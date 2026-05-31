use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::ipc::event::IpcEvent;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    offset: u64,
}

pub async fn run_jsonl_bridge(
    vibecrafted_home: PathBuf,
    tx: broadcast::Sender<IpcEvent>,
) -> Result<()> {
    let control_plane = vibecrafted_home.join("control_plane");
    tokio::fs::create_dir_all(&control_plane)
        .await
        .with_context(|| format!("create {}", control_plane.display()))?;

    let events_path = control_plane.join("events.jsonl");
    let cursor_path = cursor_path_for(&events_path)?;
    if let Some(parent) = cursor_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::channel::<()>(32);
    let mut watcher = notify::recommended_watcher({
        let watch_tx = watch_tx.clone();
        let events_path = events_path.clone();
        move |result: notify::Result<Event>| match result {
            Ok(event) if touches_events_jsonl(&event, &events_path) => {
                let _ = watch_tx.blocking_send(());
            }
            Ok(_) => {}
            Err(error) => {
                warn!("events.jsonl watcher error: {error}");
                let _ = watch_tx.blocking_send(());
            }
        }
    })
    .context("create events.jsonl watcher")?;

    watcher
        .watch(&control_plane, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", control_plane.display()))?;

    drain_events(&events_path, &cursor_path, &tx).await?;

    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = poll.tick() => {
                drain_events(&events_path, &cursor_path, &tx).await?;
            }
            Some(()) = watch_rx.recv() => {
                drain_events(&events_path, &cursor_path, &tx).await?;
            }
            else => break,
        }
    }

    Ok(())
}

async fn drain_events(
    events_path: &Path,
    cursor_path: &Path,
    tx: &broadcast::Sender<IpcEvent>,
) -> Result<()> {
    let bytes = match tokio::fs::read(events_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", events_path.display())),
    };

    let mut cursor = read_cursor(cursor_path).await.unwrap_or_default();
    if cursor.offset as usize > bytes.len() {
        cursor.offset = 0;
    }

    let start = cursor.offset as usize;
    for raw_line in bytes[start..].split(|byte| *byte == b'\n') {
        let line = match std::str::from_utf8(raw_line) {
            Ok(line) => line.trim(),
            Err(error) => {
                warn!("skipping non-utf8 events.jsonl line: {error}");
                continue;
            }
        };
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                if let Some(event) = spawn_update_from_value(&value)
                    && tx.send(event).is_err()
                {
                    debug!("no IPC subscribers for spawn update");
                }
            }
            Err(error) => warn!("skipping malformed events.jsonl line: {error}"),
        }
    }

    cursor.offset = bytes.len() as u64;
    write_cursor(cursor_path, &cursor).await
}

fn spawn_update_from_value(value: &Value) -> Option<IpcEvent> {
    if value.get("kind").and_then(Value::as_str) != Some("spawn-update") {
        return None;
    }

    let payload = value.get("payload").unwrap_or(&Value::Null);
    Some(IpcEvent::SpawnUpdate {
        run_id: string_field(value, "run_id").unwrap_or_else(|| "unknown".to_string()),
        agent: string_field(payload, "agent").unwrap_or_else(|| "unknown".to_string()),
        skill: string_field(payload, "skill").unwrap_or_else(|| "unknown".to_string()),
        mode: string_field(payload, "mode").unwrap_or_else(|| "default".to_string()),
        state: string_field(payload, "state")
            .or_else(|| string_field(payload, "status"))
            .unwrap_or_else(|| "running".to_string()),
        session_id: string_field(payload, "session_id"),
        exit_code: i32_field(payload, "exit_code"),
        launcher_pid: u32_field(payload, "launcher_pid"),
        transcript: path_field(payload, "transcript"),
        report: path_field(payload, "report"),
        ts: string_field(value, "ts").unwrap_or_else(|| "unknown".to_string()),
    })
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn i32_field(value: &Value, field: &str) -> Option<i32> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|number| i32::try_from(number).ok())
}

fn u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

fn path_field(value: &Value, field: &str) -> Option<PathBuf> {
    string_field(value, field).map(PathBuf::from)
}

fn touches_events_jsonl(event: &Event, events_path: &Path) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
    ) && event.paths.iter().any(|path| path == events_path)
}

async fn read_cursor(path: &Path) -> Result<Cursor> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes).context("decode jsonl bridge cursor")
}

async fn write_cursor(path: &Path, cursor: &Cursor) -> Result<()> {
    let bytes = serde_json::to_vec(cursor)?;
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("write {}", path.display()))
}

fn cursor_path_for(events_path: &Path) -> Result<PathBuf> {
    let mut hasher = DefaultHasher::new();
    events_path.hash(&mut hasher);
    let digest = hasher.finish();
    Ok(runtime_dir()?
        .join("jsonl-bridge")
        .join(format!("{digest:x}.cursor")))
}

fn runtime_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RMCP_MUX_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set and RMCP_MUX_RUNTIME_DIR was not provided")?;
    Ok(home.join(".rmcp-mux"))
}
