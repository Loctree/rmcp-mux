//! Status file writing and daemon status socket.
//!
//! Provides:
//! - File-based status snapshots for single servers
//! - Unix socket endpoint for querying all managed servers

use std::collections::HashMap;
use std::fs as std_fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::multi::{MultiServerStatus, StatusLevel, format_uptime};
use crate::state::{MuxState, ServerStatus, StatusSnapshot};

/// Monotonic counter to disambiguate concurrent status-file tmp writes
/// inside the same process (multiple `spawn_status_writer` instances may
/// race on the same target path when several services share `status_file`).
static STATUS_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a unique sibling tmp filename for atomic-replace.
///
/// Sibling (same parent dir) avoids cross-device rename failures.
/// Process id + monotonic counter + nanosecond timestamp ensure no two
/// concurrent writers ever pick the same tmp path, so a slow `fs::write`
/// can't be silently clobbered by a peer's rename.
fn unique_tmp_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "status".to_string());
    let pid = std::process::id();
    let counter = STATUS_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{stem}.{pid}.{counter}.{nanos}.tmp"))
}

/// Atomically write a user-visible file via a unique sibling tmp file.
///
/// The target is replaced only after the full contents land in the tmp path.
/// If the final rename fails, the previous target remains in place and the
/// tmp file is removed on a best-effort basis.
pub(crate) fn atomic_write(target: &Path, content: &[u8]) -> std::io::Result<()> {
    atomic_write_with_rename(target, content, |tmp, target| std_fs::rename(tmp, target))
}

fn atomic_write_with_rename<F>(target: &Path, content: &[u8], rename: F) -> std::io::Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    if let Some(parent) = target.parent() {
        std_fs::create_dir_all(parent)?;
    }
    let tmp = unique_tmp_path(target);
    if let Err(e) = std_fs::write(&tmp, content) {
        let _ = std_fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = rename(&tmp, target) {
        let _ = std_fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Write a status snapshot to a file atomically.
///
/// Uses a unique sibling tmp file (`<parent>/.<name>.<pid>.<n>.<nanos>.tmp`)
/// to avoid mid-rename collisions when multiple writers share one target.
pub async fn write_status_file(path: &Path, snapshot: &StatusSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create status dir {}", parent.display()))?;
    }
    let tmp = unique_tmp_path(path);
    let data = serde_json::to_vec_pretty(snapshot)?;
    if let Err(e) = fs::write(&tmp, data).await {
        return Err(e).with_context(|| format!("failed to write status tmp {}", tmp.display()));
    }
    if let Err(e) = fs::rename(&tmp, path).await {
        // Best-effort cleanup of orphan tmp; ignore failure.
        let _ = fs::remove_file(&tmp).await;
        return Err(e)
            .with_context(|| format!("failed to atomically replace status {}", path.display()));
    }
    Ok(())
}

/// Spawn a background task that writes status snapshots to a file whenever they change.
///
/// Concurrent writers sharing one target path are expected (multi-service mode);
/// rename races surface as transient `NotFound`/`AlreadyExists` and are demoted
/// to debug-level. Genuine I/O errors stay at warn-level.
pub fn spawn_status_writer(
    mut rx: watch::Receiver<StatusSnapshot>,
    path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // write initial snapshot
        let mut current = rx.borrow().clone();
        if let Err(e) = write_status_file(&path, &current).await {
            log_status_write_error("initial status file", &e);
        }
        while rx.changed().await.is_ok() {
            current = rx.borrow().clone();
            if let Err(e) = write_status_file(&path, &current).await {
                log_status_write_error("status file", &e);
            }
        }
    })
}

/// Demote expected races (NotFound/AlreadyExists) to debug; keep real
/// errors at warn so operators still see disk-full / permission issues.
fn log_status_write_error(context: &str, err: &anyhow::Error) {
    let transient = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| matches!(io.kind(), ErrorKind::NotFound | ErrorKind::AlreadyExists));
    if transient {
        debug!("transient race writing {context}: {err}");
    } else {
        warn!("failed to write {context}: {err}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon status socket
// ─────────────────────────────────────────────────────────────────────────────

/// Default status socket path.
pub const DEFAULT_STATUS_SOCKET: &str = "/tmp/rust-mux.status.sock";

/// Deterministically derive a per-config status socket path.
///
/// When `rust-mux --config <path>` is started, the daemon binds the status
/// listener at `<config_dir>/daemon.sock`. `daemon-status --config <same>`
/// connects to the same path without needing a separate flag or env var.
///
/// Falls back to [`DEFAULT_STATUS_SOCKET`] if the config path has no parent
/// directory (e.g. a bare filename or root-only path).
pub fn status_socket_for_config(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("daemon.sock"),
        _ => PathBuf::from(DEFAULT_STATUS_SOCKET),
    }
}

/// Response from the status endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version
    pub version: String,
    /// Total uptime in milliseconds
    pub uptime_ms: u64,
    /// Formatted uptime string
    pub uptime: String,
    /// Number of configured servers
    pub server_count: usize,
    /// Number of servers currently running
    pub running_count: usize,
    /// Number of servers with errors
    pub error_count: usize,
    /// Per-server status
    pub servers: Vec<MultiServerStatus>,
}

/// Managed server reference for status collection.
pub struct ServerRef {
    pub name: String,
    pub state: Arc<Mutex<MuxState>>,
    pub active_clients: Arc<Semaphore>,
    pub max_active_clients: usize,
}

/// Shared state for status collection.
pub struct StatusState {
    pub servers: HashMap<String, ServerRef>,
    pub start_time: Instant,
}

impl StatusState {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            start_time: Instant::now(),
        }
    }

    pub fn register_server(&mut self, server_ref: ServerRef) {
        self.servers.insert(server_ref.name.clone(), server_ref);
    }

    /// Collect status from all servers.
    pub async fn collect_status(&self) -> DaemonStatus {
        let mut servers = Vec::with_capacity(self.servers.len());
        let mut running_count = 0;
        let mut error_count = 0;

        for (name, server_ref) in &self.servers {
            let st = server_ref.state.lock().await;
            let active = server_ref
                .max_active_clients
                .saturating_sub(server_ref.active_clients.available_permits());

            let (level, status_text) = match &st.server_status {
                ServerStatus::Running => {
                    running_count += 1;
                    if st.heartbeat_metrics.consecutive_failures > 0 {
                        (StatusLevel::Warn, "Running (heartbeat issues)".to_string())
                    } else {
                        (StatusLevel::Ok, "Running".to_string())
                    }
                }
                ServerStatus::Starting => {
                    running_count += 1;
                    (StatusLevel::Ok, "Starting".to_string())
                }
                ServerStatus::Restarting => {
                    running_count += 1;
                    (StatusLevel::Warn, "Restarting".to_string())
                }
                ServerStatus::Failed(reason) => {
                    error_count += 1;
                    (StatusLevel::Error, format!("Failed: {}", reason))
                }
                ServerStatus::Stopped => (StatusLevel::Lazy, "Stopped".to_string()),
                ServerStatus::Lazy => (StatusLevel::Lazy, "Lazy (not started)".to_string()),
                ServerStatus::Backoff => {
                    error_count += 1;
                    (StatusLevel::Error, "Backoff (restarting)".to_string())
                }
            };

            let uptime_ms = st
                .started_at
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);

            servers.push(MultiServerStatus {
                name: name.clone(),
                level,
                status_text,
                connected_clients: st.clients.len(),
                active_clients: active,
                max_active_clients: server_ref.max_active_clients,
                pending_requests: st.pending.len(),
                restarts: st.restarts,
                uptime_ms,
                in_backoff: st.in_backoff,
                heartbeat_latency_ms: st.heartbeat_metrics.avg_response_ms,
            });
        }

        // Sort servers by name for consistent output
        servers.sort_by(|a, b| a.name.cmp(&b.name));

        let uptime_ms = self.start_time.elapsed().as_millis() as u64;

        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_ms,
            uptime: format_uptime(uptime_ms),
            server_count: self.servers.len(),
            running_count,
            error_count,
            servers,
        }
    }
}

impl Default for StatusState {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the status socket listener.
///
/// Listens on the specified Unix socket and responds to status queries.
/// Each connection receives a JSON response with the current daemon status.
pub async fn run_status_listener(
    socket_path: impl AsRef<Path>,
    state: Arc<Mutex<StatusState>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let socket_path = socket_path.as_ref();

    // Clean up stale socket
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(socket = %socket_path.display(), "status listener started");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                debug!("status listener shutting down");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_status_connection(stream, state).await {
                                warn!(error = %e, "status connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "status accept error");
                    }
                }
            }
        }
    }

    // Clean up socket
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Handle a single status connection.
async fn handle_status_connection(
    mut stream: UnixStream,
    state: Arc<Mutex<StatusState>>,
) -> Result<()> {
    // Collect status
    let status = {
        let st = state.lock().await;
        st.collect_status().await
    };

    // Serialize and send
    let json = serde_json::to_string(&status)?;
    stream.write_all(json.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    Ok(())
}

/// Query the daemon status via the status socket.
pub async fn query_status(socket_path: impl AsRef<Path>) -> Result<DaemonStatus> {
    let stream = UnixStream::connect(socket_path.as_ref()).await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let status: DaemonStatus = serde_json::from_str(&line)?;
    Ok(status)
}

/// Print status in a formatted table.
pub fn print_status_table(status: &DaemonStatus) {
    println!("rust-mux v{} | uptime: {}", status.version, status.uptime);
    println!("{:─<72}", "");
    println!(
        "{:<20} {:^8} {:>8} {:>8} {:>10} {:>10}",
        "Server", "State", "Clients", "Pending", "Restarts", "Heartbeat"
    );
    println!("{:─<72}", "");

    for server in &status.servers {
        let state_icon = match server.level {
            StatusLevel::Ok => "✓",
            StatusLevel::Warn => "⚠",
            StatusLevel::Error => "✗",
            StatusLevel::Lazy => "○",
        };

        let clients = format!("{}/{}", server.active_clients, server.max_active_clients);
        let heartbeat = server
            .heartbeat_latency_ms
            .map(|ms| format!("{}ms", ms))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<20} {:^8} {:>8} {:>8} {:>10} {:>10}",
            server.name,
            format!("{} {}", state_icon, short_status(&server.status_text)),
            clients,
            server.pending_requests,
            server.restarts,
            heartbeat,
        );
    }

    println!("{:─<72}", "");
    println!(
        "Total: {} servers ({} running, {} errors)",
        status.server_count, status.running_count, status.error_count
    );
}

/// Shorten status text for table display.
fn short_status(status: &str) -> &str {
    if status.starts_with("Running") {
        "UP"
    } else if status.starts_with("Starting") {
        "START"
    } else if status.starts_with("Lazy") {
        "LAZY"
    } else if status.starts_with("Failed") {
        "FAIL"
    } else if status.starts_with("Backoff") {
        "BACK"
    } else if status.starts_with("Restarting") {
        "RSTRT"
    } else if status.starts_with("Stopped") {
        "STOP"
    } else {
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_socket_uses_rust_mux_identity() {
        assert_eq!(DEFAULT_STATUS_SOCKET, "/tmp/rust-mux.status.sock");
    }

    #[test]
    fn short_status_mapping() {
        assert_eq!(short_status("Running"), "UP");
        assert_eq!(short_status("Running (heartbeat issues)"), "UP");
        assert_eq!(short_status("Lazy (not started)"), "LAZY");
        assert_eq!(short_status("Failed: timeout"), "FAIL");
    }

    #[tokio::test]
    async fn status_state_collects_empty() {
        let state = StatusState::new();
        let status = state.collect_status().await;
        assert_eq!(status.server_count, 0);
        assert_eq!(status.running_count, 0);
    }

    #[test]
    fn unique_tmp_paths_never_collide() {
        let target = std::path::Path::new("/tmp/rust-mux-status-test/all.json");
        let a = unique_tmp_path(target);
        let b = unique_tmp_path(target);
        assert_ne!(a, b, "concurrent writers must get distinct tmp paths");
        assert_eq!(a.parent(), target.parent());
        assert_eq!(b.parent(), target.parent());
        let a_name = a.file_name().unwrap().to_string_lossy();
        assert!(
            a_name.starts_with(".all.json."),
            "unexpected stem: {a_name}"
        );
        assert!(a_name.ends_with(".tmp"), "unexpected suffix: {a_name}");
    }

    #[test]
    fn status_socket_for_config_uses_sibling_daemon_sock() {
        let cfg = std::path::Path::new("/Users/me/.config/mux/config.toml");
        let sock = status_socket_for_config(cfg);
        assert_eq!(
            sock,
            std::path::PathBuf::from("/Users/me/.config/mux/daemon.sock")
        );
    }

    #[test]
    fn status_socket_for_config_falls_back_when_no_parent() {
        let cfg = std::path::Path::new("config.toml");
        let sock = status_socket_for_config(cfg);
        assert_eq!(sock, std::path::PathBuf::from(DEFAULT_STATUS_SOCKET));
    }

    #[test]
    fn atomic_write_replaces_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        std::fs::write(&target, b"old").expect("seed target");

        atomic_write(&target, b"new").expect("atomic write");

        assert_eq!(std::fs::read(&target).expect("read target"), b"new");
    }

    #[test]
    fn atomic_write_rename_failure_keeps_target_and_cleans_tmp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        std::fs::write(&target, b"old").expect("seed target");

        let err = atomic_write_with_rename(&target, b"new", |_tmp, _target| {
            Err(std::io::Error::other("forced rename failure"))
        })
        .expect_err("rename failure should surface");

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(std::fs::read(&target).expect("read target"), b"old");
        let tmp_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| {
                let entry = entry.expect("dir entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                name.ends_with(".tmp").then_some(name)
            })
            .collect();
        assert!(tmp_files.is_empty(), "orphan tmp files: {tmp_files:?}");
    }
}
