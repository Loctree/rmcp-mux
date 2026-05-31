use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    Claude,
    Codex,
    Gemini,
    Junie,
    Generic { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub non_mux_servers: Vec<NonMuxEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonMuxEntry {
    pub client: String,
    pub path: String,
    pub line: usize,
    pub server_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub client: String,
    pub server: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "args")]
pub enum MuxControlCommand {
    Subscribe,
    Unsubscribe,
    GetStatus,
    Verify { client_kind: ClientKind },
    RouteSnapshot,
    RestartService { name: String },
    ReloadConfig,
    Shutdown { graceful: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload")]
pub enum MuxControlResponse {
    Ok,
    Status(Box<crate::state::StatusSnapshot>),
    VerifyResult(VerifyResult),
    Routes(Vec<Route>),
    Event(crate::ipc::event::IpcEvent),
    Error(String),
    Unimplemented,
}
