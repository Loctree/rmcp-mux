use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc, watch};

#[cfg_attr(not(feature = "tray"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerStatus {
    Starting,
    Running,
    Restarting,
    Failed(String),
    Stopped,
    Lazy,
    Backoff,
}

pub type HealthStatus = ServerStatus;

use crate::multi::StatusLevel;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub servers: Vec<StatusSnapshot>,
    pub version: String,
    pub uptime: String,
    pub server_count: usize,
    pub running_count: usize,
    pub error_count: usize,
}

#[cfg_attr(not(feature = "tray"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub service_name: String,
    pub name: String, // Alias for service_name
    pub server_status: ServerStatus,
    pub status_text: String,
    pub level: StatusLevel,
    pub restarts: u64,
    pub connected_clients: usize,
    pub active_clients: usize,
    pub max_active_clients: usize,
    pub pending_requests: usize,
    pub cached_initialize: bool,
    pub initializing: bool,
    pub last_reset: Option<String>,
    pub queue_depth: usize,
    pub child_pid: Option<u32>,
    pub max_request_bytes: usize,
    pub health_status: HealthStatus,
    pub heartbeat_latency_ms: Option<u64>,
    pub heartbeat: HeartbeatMetrics,
    pub uptime_ms: u64,
    pub in_backoff: bool,
    pub restart_backoff_ms: u64,
    pub restart_backoff_max_ms: u64,
    pub max_restarts: u64,
}
#[derive(Clone, Debug)]
pub struct Pending {
    pub client_id: u64,
    pub local_id: serde_json::Value,
    pub is_initialize: bool,
    pub started_at: std::time::Instant,
}

/// Central runtime state shared between the async mux loops.
///
/// - `queue_depth` caps queued client messages to avoid unbounded memory growth
///   under bursty hosts.
/// - `max_request_bytes` and `request_timeout` are enforced per forwarded
///   request to prevent slowloris/DoS patterns.
/// - Restart backoff (`restart_backoff`..`restart_backoff_max`) and
///   `max_restarts` gate child respawns so a flapping server cannot burn CPU.
#[derive(Clone)]
pub struct MuxState {
    pub next_client_id: u64,
    pub next_global_id: u64,
    pub clients: HashMap<u64, mpsc::UnboundedSender<Value>>,
    pub pending: HashMap<String, Pending>,
    pub cached_initialize: Option<Value>,
    pub init_waiting: Vec<(u64, Value)>,
    pub initializing: bool,
    pub server_status: ServerStatus,
    pub restarts: u64,
    pub last_reset: Option<String>,
    pub max_active_clients: usize,
    pub service_name: String,
    pub max_request_bytes: usize,
    pub request_timeout: Duration,
    pub restart_backoff: Duration,
    pub restart_backoff_max: Duration,
    pub max_restarts: u64,
    pub queue_depth: usize,
    pub child_pid: Option<u32>,
    pub heartbeat_metrics: HeartbeatMetrics,
    pub client_handshakes: HashMap<u64, ClientHandshake>,
    pub server_initialized: bool,
    pub started_at: Option<Instant>,
    pub in_backoff: bool,
    #[cfg(feature = "cli")]
    pub event_tx: Option<tokio::sync::broadcast::Sender<crate::ipc::event::IpcEvent>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct HeartbeatMetrics {
    pub enabled: bool,
    pub latency_ms: Option<u64>,
    pub last_heartbeat_ms: Option<u64>,
    pub avg_response_ms: Option<u64>,
    pub total_success: u64,
    pub total_failures: u64,
    pub consecutive_failures: u32,
}

#[derive(Clone, Debug)]
pub struct ClientHandshake {
    pub connected_at: Instant,
    pub initialized: bool,
    pub initialize_pending: bool,
    pub buffered_messages: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct MuxStateConfig {
    pub max_active_clients: usize,
    pub service_name: String,
    pub max_request_bytes: usize,
    pub request_timeout: Duration,
    pub restart_backoff: Duration,
    pub restart_backoff_max: Duration,
    pub max_restarts: u64,
    pub queue_depth: usize,
    pub child_pid: Option<u32>,
    #[cfg(feature = "cli")]
    pub event_tx: Option<tokio::sync::broadcast::Sender<crate::ipc::event::IpcEvent>>,
}

impl MuxState {
    pub fn new(config: MuxStateConfig) -> Self {
        Self {
            next_client_id: 1,
            next_global_id: 1,
            clients: HashMap::new(),
            pending: HashMap::new(),
            cached_initialize: None,
            init_waiting: Vec::new(),
            initializing: false,
            server_status: ServerStatus::Starting,
            restarts: 0,
            last_reset: None,
            max_active_clients: config.max_active_clients,
            service_name: config.service_name,
            max_request_bytes: config.max_request_bytes,
            request_timeout: config.request_timeout,
            restart_backoff: config.restart_backoff,
            restart_backoff_max: config.restart_backoff_max,
            max_restarts: config.max_restarts,
            queue_depth: config.queue_depth,
            child_pid: config.child_pid,
            heartbeat_metrics: HeartbeatMetrics::default(),
            client_handshakes: HashMap::new(),
            server_initialized: false,
            started_at: None,
            in_backoff: false,
            #[cfg(feature = "cli")]
            event_tx: config.event_tx,
        }
    }

    pub fn register_client(&mut self, tx: mpsc::UnboundedSender<Value>) -> u64 {
        let id = self.next_client_id;
        self.next_client_id += 1;
        self.clients.insert(id, tx);
        self.client_handshakes.insert(
            id,
            ClientHandshake {
                connected_at: Instant::now(),
                initialized: false,
                initialize_pending: false,
                buffered_messages: Vec::new(),
            },
        );
        id
    }

    pub fn unregister_client(&mut self, client_id: u64) {
        self.clients.remove(&client_id);
        self.client_handshakes.remove(&client_id);
        self.pending.retain(|_, p| p.client_id != client_id);
        self.init_waiting.retain(|(cid, _)| *cid != client_id);
    }

    pub fn next_request_id(&mut self) -> u64 {
        let id = self.next_global_id;
        self.next_global_id += 1;
        id
    }

    pub fn mark_handshake_complete(&mut self, client_id: u64) -> Vec<Value> {
        if let Some(handshake) = self.client_handshakes.get_mut(&client_id) {
            handshake.initialized = true;
            std::mem::take(&mut handshake.buffered_messages)
        } else {
            Vec::new()
        }
    }

    pub fn complete_handshake(&mut self, client_id: u64) -> Vec<Value> {
        self.mark_handshake_complete(client_id)
    }

    pub fn is_handshake_complete(&self, client_id: u64) -> bool {
        self.client_handshakes
            .get(&client_id)
            .is_none_or(|handshake| handshake.initialized)
    }

    pub fn is_handshake_timed_out(&self, client_id: u64) -> bool {
        self.client_handshakes
            .get(&client_id)
            .is_some_and(|handshake| {
                !handshake.initialized && handshake.connected_at.elapsed() > HANDSHAKE_TIMEOUT
            })
    }

    pub fn buffer_message(&mut self, client_id: u64, msg: Value) {
        if let Some(handshake) = self.client_handshakes.get_mut(&client_id) {
            handshake.buffered_messages.push(msg);
        }
    }

    pub fn get_handshake_mut(&mut self, client_id: u64) -> Option<&mut ClientHandshake> {
        self.client_handshakes.get_mut(&client_id)
    }
}

pub fn set_id(msg: &mut Value, id: Value) {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("id".to_string(), id);
    }
}

pub fn error_response(id: Value, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32000,
            "message": message,
        }
    })
}

pub fn snapshot_for_state(st: &MuxState, active_clients: usize) -> StatusSnapshot {
    StatusSnapshot {
        service_name: st.service_name.clone(),
        name: st.service_name.clone(),
        server_status: st.server_status.clone(),
        status_text: format!("{:?}", st.server_status),
        level: StatusLevel::Ok,
        restarts: st.restarts,
        connected_clients: st.clients.len(),
        active_clients,
        max_active_clients: st.max_active_clients,
        pending_requests: st.pending.len(),
        cached_initialize: st.cached_initialize.is_some(),
        initializing: st.initializing,
        last_reset: st.last_reset.clone(),
        queue_depth: st.queue_depth,
        child_pid: st.child_pid,
        max_request_bytes: st.max_request_bytes,
        health_status: st.server_status.clone(),
        heartbeat_latency_ms: st.heartbeat_metrics.latency_ms,
        heartbeat: st.heartbeat_metrics.clone(),
        uptime_ms: st
            .started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0),
        in_backoff: st.in_backoff,
        restart_backoff_ms: st.restart_backoff.as_millis() as u64,
        restart_backoff_max_ms: st.restart_backoff_max.as_millis() as u64,
        max_restarts: st.max_restarts,
    }
}

pub async fn publish_status(
    state: &Arc<Mutex<MuxState>>,
    active_clients: &Arc<Semaphore>,
    status_tx: &watch::Sender<StatusSnapshot>,
) {
    let st = state.lock().await;
    let active = st
        .max_active_clients
        .saturating_sub(active_clients.available_permits());
    let snapshot = snapshot_for_state(&st, active);
    #[cfg(feature = "cli")]
    if let Some(ref tx) = st.event_tx {
        let _ = tx.send(crate::ipc::event::IpcEvent::StateChange {
            service: st.service_name.clone(),
            from: "Unknown".into(),
            to: format!("{:?}", st.server_status),
        });
    }
    drop(st);
    let _ = status_tx.send(snapshot);
}

pub async fn reset_state(
    state: &Arc<Mutex<MuxState>>,
    reason: &str,
    active_clients: &Arc<Semaphore>,
    status_tx: &watch::Sender<StatusSnapshot>,
) {
    let mut st = state.lock().await;
    let pending = std::mem::take(&mut st.pending);
    let waiters = std::mem::take(&mut st.init_waiting);
    st.cached_initialize = None;
    st.initializing = false;
    st.last_reset = Some(reason.to_string());
    st.queue_depth = 0;
    st.child_pid = None;

    for (_, p) in pending {
        if let Some(tx) = st.clients.get(&p.client_id) {
            tx.send(error_response(p.local_id.clone(), reason)).ok();
        }
    }
    for (cid, lid) in waiters {
        if let Some(tx) = st.clients.get(&cid) {
            tx.send(error_response(lid, reason)).ok();
        }
    }
    drop(st);
    publish_status(state, active_clients, status_tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_request_id_increments_sequentially() {
        let mut state = MuxState::new(MuxStateConfig {
            max_active_clients: 5,
            service_name: "test-service".into(),
            max_request_bytes: 1_048_576,
            request_timeout: Duration::from_secs(30),
            restart_backoff: Duration::from_millis(1_000),
            restart_backoff_max: Duration::from_millis(30_000),
            max_restarts: 5,
            queue_depth: 0,
            child_pid: None,
            #[cfg(feature = "cli")]
            event_tx: None,
        });

        let first = state.next_request_id();
        let second = state.next_request_id();

        assert_eq!(first + 1, second);
    }
}
