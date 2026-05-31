//! Runtime module for the mux daemon.
//!
//! This module contains the core mux server logic that can be embedded
//! in other applications via the library interface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::ResolvedParams;
use crate::state::{
    MuxState, MuxStateConfig, StatusSnapshot, error_response, publish_status, snapshot_for_state,
};
#[cfg(feature = "tray")]
use crate::tray::{find_tray_icon, spawn_tray};

mod client;
pub mod heartbeat;
mod proxy;
mod server;
mod status;
mod types;

pub use heartbeat::{
    HeartbeatConfig, HeartbeatEvent, HeartbeatInspectorContext, is_heartbeat_response,
    spawn_heartbeat_inspector,
};
pub use proxy::run_proxy;
#[cfg(feature = "cli")]
pub(crate) use status::atomic_write;
pub(crate) use status::spawn_status_writer;
pub use status::{
    DEFAULT_STATUS_SOCKET, DaemonStatus, ServerRef, StatusState, print_status_table, query_status,
    run_status_listener, status_socket_for_config,
};
pub use types::MAX_PENDING;
pub use types::MAX_QUEUE;

use client::handle_client;
use server::{ServerManagerChannels, ServerManagerConfig, ServerManagerState, server_manager};
use types::ServerEvent;

/// Handle events from the server with heartbeat response detection.
///
/// This wraps the normal server event handling and filters out heartbeat
/// responses, forwarding them to the heartbeat inspector instead of clients.
async fn handle_server_events_with_heartbeat(
    state: Arc<Mutex<MuxState>>,
    active_clients: Arc<Semaphore>,
    status_tx: watch::Sender<StatusSnapshot>,
    mut rx: mpsc::UnboundedReceiver<ServerEvent>,
    heartbeat_tx: mpsc::UnboundedSender<HeartbeatEvent>,
) {
    while let Some(evt) = rx.recv().await {
        match evt {
            ServerEvent::Message(msg) => {
                // Check if this is a heartbeat response
                if let Some(id) = msg.get("id").and_then(|v| v.as_str())
                    && is_heartbeat_response(id)
                {
                    // Forward to heartbeat inspector
                    let _ = heartbeat_tx.send(HeartbeatEvent::Response { id: id.to_string() });
                    continue;
                }
                // Normal message handling
                if let Err(e) =
                    server::handle_server_message(msg, &state, &active_clients, &status_tx).await
                {
                    warn!("server message routing failed: {e}");
                }
            }
            ServerEvent::Reset(reason) => {
                crate::state::reset_state(&state, &reason, &active_clients, &status_tx).await;
            }
        }
    }
}

/// Lightweight health check: verifies the mux socket is reachable.
pub async fn health_check(params: &ResolvedParams) -> Result<()> {
    let mut stream = UnixStream::connect(&params.socket)
        .await
        .with_context(|| format!("failed to connect to {}", params.socket.display()))?;
    stream
        .shutdown()
        .await
        .context("failed to shutdown health check stream")?;
    Ok(())
}

/// Reap timed-out pending requests.
async fn reap_timeouts(
    state: Arc<Mutex<MuxState>>,
    active_clients: Arc<Semaphore>,
    status_tx: watch::Sender<StatusSnapshot>,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                let mut expired = Vec::new();
                let timeout = {
                    let st = state.lock().await;
                    st.request_timeout
                };
                {
                    let mut st = state.lock().await;
                    let now = Instant::now();
                    st.pending.retain(|_, p| {
                        if now.duration_since(p.started_at) > timeout {
                            expired.push((p.client_id, p.local_id.clone()));
                            false
                        } else {
                            true
                        }
                    });
                }
                if !expired.is_empty() {
                    let st = state.lock().await;
                    for (cid, lid) in expired {
                        if let Some(tx) = st.clients.get(&cid) {
                            tx.send(error_response(lid, "request timeout")).ok();
                        }
                    }
                }
                publish_status(&state, &active_clients, &status_tx).await;
            }
        }
    }
}

/// Start the mux daemon with external shutdown control.
///
/// This is the library-facing entry point used by the binary and embedding API.
/// Callers own the [`CancellationToken`] so multiple mux instances can be
/// supervised from a shared runtime.
pub async fn run_mux(params: ResolvedParams, shutdown: CancellationToken) -> Result<()> {
    run_mux_internal(params, shutdown).await
}

/// Start the mux daemon with external shutdown control.
///
/// Backward-compatible alias for callers that explicitly name the internal
/// variant. New code should call [`run_mux`].
///
/// # Example
/// ```rust,no_run
/// use rmcp_mux::config::ResolvedParams;
/// use tokio_util::sync::CancellationToken;
///
/// async fn run_embedded(params: ResolvedParams) {
///     let shutdown = CancellationToken::new();
///     let shutdown_clone = shutdown.clone();
///
///     // Trigger shutdown from elsewhere
///     tokio::spawn(async move {
///         tokio::time::sleep(std::time::Duration::from_secs(60)).await;
///         shutdown_clone.cancel();
///     });
///
///     rmcp_mux::runtime::run_mux_internal(params, shutdown).await.unwrap();
/// }
/// ```
pub async fn run_mux_internal(params: ResolvedParams, shutdown: CancellationToken) -> Result<()> {
    run_mux_internal_with_status(params, shutdown, None).await
}

/// Start the mux daemon with status registration.
///
/// Like [`run_mux_internal`] but also registers with a shared [`StatusState`]
/// for daemon-wide status monitoring.
pub async fn run_mux_internal_with_status(
    params: ResolvedParams,
    shutdown: CancellationToken,
    status_state: Option<Arc<Mutex<status::StatusState>>>,
) -> Result<()> {
    let service_name = Arc::new(params.service_name.clone());
    let socket_path = params.socket.clone();
    let cmd = params.cmd.clone();
    let args = params.args.clone();
    let env = params.env.clone();
    let max_clients = params.max_clients;
    let tray_enabled = params.tray_enabled;
    let lazy_start = params.lazy_start;
    let max_request_bytes = params.max_request_bytes;
    let request_timeout = params.request_timeout;
    let restart_backoff = params.restart_backoff;
    let restart_backoff_max = params.restart_backoff_max;
    let max_restarts = params.max_restarts;

    // Build heartbeat configuration from params
    let heartbeat_config = HeartbeatConfig {
        interval: params.heartbeat_interval,
        timeout: params.heartbeat_timeout,
        max_failures: params.heartbeat_max_failures,
        enabled: params.heartbeat_enabled,
    };

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create socket parent dir")?;
    }
    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket {}", socket_path.display()))?;
    info!("rmcp_mux listening on {}", socket_path.display());

    #[cfg(feature = "cli")]
    let (event_tx, _) = tokio::sync::broadcast::channel(100);

    let state = Arc::new(Mutex::new(MuxState::new(MuxStateConfig {
        max_active_clients: max_clients,
        service_name: service_name.as_ref().clone(),
        max_request_bytes,
        request_timeout,
        restart_backoff,
        restart_backoff_max,
        max_restarts,
        queue_depth: 0,
        child_pid: None,
        #[cfg(feature = "cli")]
        event_tx: Some(event_tx.clone()),
    })));

    #[cfg(feature = "cli")]
    {
        let ipc_ctx = Arc::new(crate::ipc::server::MuxControlContext::new(
            state.clone(),
            Some(event_tx),
        ));
        tokio::spawn(async move {
            if let Err(e) = crate::ipc::server::run_server(ipc_ctx).await {
                error!("IPC server error: {}", e);
            }
        });
    }

    // Initialize heartbeat metrics with enabled state
    {
        let mut st = state.lock().await;
        st.heartbeat_metrics.enabled = heartbeat_config.enabled;
    }

    let active_clients = Arc::new(Semaphore::new(max_clients));

    // Register with status state if provided (for daemon-wide monitoring)
    if let Some(ref ss) = status_state {
        let server_ref = status::ServerRef {
            name: service_name.as_ref().clone(),
            state: state.clone(),
            active_clients: active_clients.clone(),
            max_active_clients: max_clients,
        };
        let mut ss_guard = ss.lock().await;
        ss_guard.register_server(server_ref);
    }

    let (status_tx, status_rx) = {
        let st = state.lock().await;
        let initial = snapshot_for_state(&st, 0);
        drop(st);
        watch::channel(initial)
    };
    #[cfg(not(feature = "tray"))]
    let _ = &status_rx;

    #[cfg(feature = "tray")]
    let tray_icon = find_tray_icon();
    #[cfg(feature = "tray")]
    let tray_handle: Option<std::thread::JoinHandle<()>> = if tray_enabled {
        Some(spawn_tray(status_rx.clone(), shutdown.clone(), tray_icon))
    } else {
        None
    };
    #[cfg(not(feature = "tray"))]
    let _tray_handle: Option<()> = if tray_enabled {
        warn!("tray support compiled out; ignoring --tray");
        None
    } else {
        None
    };

    let _status_file_handle: Option<tokio::task::JoinHandle<()>> = params
        .status_file
        .clone()
        .map(|path| spawn_status_writer(status_rx.clone(), path));

    let (to_server_tx, to_server_rx) = mpsc::channel::<Value>(MAX_QUEUE);
    let (server_events_tx, server_events_rx) = mpsc::unbounded_channel::<ServerEvent>();

    // Heartbeat channels
    let (heartbeat_event_tx, heartbeat_event_rx) = mpsc::unbounded_channel::<HeartbeatEvent>();
    let (heartbeat_restart_tx, heartbeat_restart_rx) = mpsc::unbounded_channel::<String>();

    // Server -> clients router (with heartbeat response detection)
    let router_state = state.clone();
    let router_active = active_clients.clone();
    let status_for_router = status_tx.clone();
    let heartbeat_event_tx_for_router = heartbeat_event_tx.clone();
    tokio::spawn(async move {
        handle_server_events_with_heartbeat(
            router_state,
            router_active,
            status_for_router,
            server_events_rx,
            heartbeat_event_tx_for_router,
        )
        .await;
    });

    // Child process manager (with heartbeat restart signal)
    let server_state = state.clone();
    let server_shutdown = shutdown.clone();
    let server_active = active_clients.clone();
    let status_for_server = status_tx.clone();
    let to_server_tx_for_server = to_server_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = server_manager(
            ServerManagerConfig {
                cmd: cmd.clone(),
                args: args.clone(),
                cwd: params.cwd.clone(),
                env: env.clone().unwrap_or_default(),
                lazy_start,
                restart_backoff,
                restart_backoff_max,
                max_restarts,
            },
            ServerManagerChannels {
                to_server_rx,
                to_server_meter: to_server_tx_for_server,
                server_events_tx,
                heartbeat_restart_rx,
            },
            ServerManagerState {
                state: server_state,
                active_clients: server_active,
                status_tx: status_for_server,
                shutdown: server_shutdown,
            },
        )
        .await
        {
            error!("server manager exited with error: {e}");
        }
    });

    // Heartbeat inspector
    let heartbeat_state = state.clone();
    let heartbeat_active = active_clients.clone();
    let heartbeat_status = status_tx.clone();
    let heartbeat_shutdown = shutdown.clone();
    let heartbeat_to_server = to_server_tx.clone();
    let _heartbeat_handle = spawn_heartbeat_inspector(
        heartbeat_config,
        HeartbeatInspectorContext {
            to_server_tx: heartbeat_to_server,
            heartbeat_rx: heartbeat_event_rx,
            state: heartbeat_state,
            active_clients: heartbeat_active,
            status_tx: heartbeat_status,
            restart_tx: heartbeat_restart_tx,
            shutdown: heartbeat_shutdown,
        },
    );

    // Timeout reaper
    let reaper_state = state.clone();
    let reaper_active = active_clients.clone();
    let reaper_status = status_tx.clone();
    let reaper_shutdown = shutdown.clone();
    tokio::spawn(async move {
        reap_timeouts(reaper_state, reaper_active, reaper_status, reaper_shutdown).await;
    });

    // Accept clients
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("shutdown requested; closing listener");
                break;
            }
            accept_res = listener.accept() => {
                let (stream, _) = match accept_res {
                    Ok(v) => v,
                    Err(e) => { warn!("accept failed: {e}"); continue; }
                };
                let state = state.clone();
                let to_server_tx = to_server_tx.clone();
                let active_clients = active_clients.clone();
                let shutdown = shutdown.clone();
                let status_tx = status_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state, to_server_tx, active_clients, status_tx, shutdown).await {
                        warn!("client handler error: {e}");
                    }
                });
            }
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;
    #[cfg(feature = "tray")]
    if let Some(handle) = tray_handle {
        let _ = handle.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
