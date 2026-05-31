//! Type definitions for the wizard module.
//!
//! v0.4.0 5-step flow:
//!
//! 1. **DiscoverySources** — pick which client config files to scan, plus
//!    optional custom paths.
//! 2. **ServerReview** — per-client tree of discovered servers, dedup count,
//!    conflict hints. Read-only.
//! 3. **StrategyChoice** — Unified vs Per-client vs Auto-rewire (DANGER).
//! 4. **SummaryConfirm** — preview of what will be written and to where,
//!    then Confirm / Back / Cancel.
//! 5. **ResultAndTray** — show what was actually done, per-client setup
//!    snippets, and offer to start a tray daemon now.

use std::path::PathBuf;

use crate::config::ServerConfig;
use crate::scan::{DefaultServerSource, HostFile, HostKind};

// ─────────────────────────────────────────────────────────────────────────────
// Wizard flow
// ─────────────────────────────────────────────────────────────────────────────

/// Wizard step in the five-step flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    /// Step 1: Choose which client config files to scan, optional custom paths.
    DiscoverySources,
    /// Step 2: Read-only review of discovered servers, grouped by client.
    ServerReview,
    /// Step 3: Pick the strategy (Unified / Per-client / Auto-rewire).
    StrategyChoice,
    /// Step 4: Preview what will happen, confirm or go back.
    SummaryConfirm,
    /// Step 5: Show the result and offer to start the tray daemon.
    ResultAndTray,
}

/// Strategy on STEP 3. Drives what STEP 4 previews and STEP 5 reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// One unified mux config under `~/.config/mux/{config.toml, mcp.json, mcp.toml}`.
    /// Every selected server, deduplicated. Recommended.
    Unified,
    /// Separate per-client mux configs under `~/.config/mux/<client>.{json,toml}`,
    /// only that client's servers. Native format per client.
    PerClient,
    /// `[DANGER]` Auto-rewire existing client configs in-place to route
    /// through `rmcp-mux-proxy`. Backup-first, preview-first, rollback-ready.
    AutoRewire,
}

/// Action chosen on STEP 4 (SummaryConfirm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryAction {
    Confirm,
    Back,
    Cancel,
}

/// Tray daemon prompt on STEP 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayChoice {
    /// Spawn `rmcp-mux --tray --multi-service` in the background detached
    /// from this terminal.
    StartNow,
    /// Skip the tray daemon, exit cleanly.
    No,
}

/// Action queued by an in-step Enter handler that needs to run *outside* the
/// raw-mode TUI loop (anything that prints to stdout, prompts via stdin, or
/// spawns a long-running detached process).
///
/// `keys.rs` sets this and exits the loop; `wizard/mod.rs::run_tui` drains it
/// after restoring the cooked terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// Strategy::Unified — write `~/.config/mux/{config.toml, mcp.json, mcp.toml}`.
    GenerateUnified,
    /// Strategy::PerClient — write per-kind native files in `~/.config/mux/`.
    GeneratePerClient,
    /// Strategy::AutoRewire — backup-first preview-first rewrite of existing
    /// client configs to route through `rmcp-mux-proxy`.
    AutoRewire,
    /// Spawn the tray daemon detached; runs after the strategy result is
    /// printed.
    StartTrayDaemon,
}

/// Source of a `ServiceEntry`. Drives UI labels and dedup decisions.
///
/// Custom-path imports (`--import-config`, wizard custom-path field) flow
/// through `scan::host_file_from_custom_path`, which tags the host file with
/// `HostKind::Custom`; the resulting `ServiceEntry::source` is therefore
/// `ServiceSource::Client { kind: HostKind::Custom, .. }`. There is no
/// separate `Custom` variant — that would be a parallel surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceSource {
    /// Discovered inside a known MCP client config file (well-known clients
    /// or `HostKind::Custom` for user-provided paths).
    Client { kind: HostKind, path: PathBuf },
    /// Built-in discovered server, such as the local Vibecrafted MCP package.
    Default { source: DefaultServerSource },
    /// Detected as a running process but not present in any scanned config.
    DetectedRunning,
}

impl ServiceSource {
    pub fn short_label(&self) -> String {
        match self {
            ServiceSource::Client { kind, .. } => kind.as_label().to_string(),
            ServiceSource::Default { .. } => "default".into(),
            ServiceSource::DetectedRunning => "running".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Unknown,
    Healthy,
    Unhealthy,
}

// ─────────────────────────────────────────────────────────────────────────────
// Source-step state (STEP 1)
// ─────────────────────────────────────────────────────────────────────────────

/// Status of a discovery source after we tried to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// File present and parsed cleanly.
    Ok { servers_found: usize },
    /// File present but contained no MCP server entries.
    Empty,
    /// File present but failed to parse — `details` carries the error.
    InvalidFormat { details: String },
    /// File not present on disk.
    Missing,
}

impl SourceStatus {
    pub fn short_label(&self) -> String {
        match self {
            SourceStatus::Ok { servers_found } => format!("{servers_found} servers"),
            SourceStatus::Empty => "empty".into(),
            SourceStatus::InvalidFormat { .. } => "invalid".into(),
            SourceStatus::Missing => "not found".into(),
        }
    }
}

/// One row on STEP 1: a candidate source the operator can include or skip.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub host_file: HostFile,
    pub status: SourceStatus,
    /// Whether the operator wants this source included.
    pub selected: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Service-review state (STEP 2)
// ─────────────────────────────────────────────────────────────────────────────

/// One service entry surfaced in STEP 2. Always selected by default; the
/// operator can untick to drop it from the mux output.
#[derive(Debug, Clone)]
pub struct ServiceEntry {
    pub name: String,
    pub config: ServerConfig,
    pub health: HealthStatus,
    pub source: ServiceSource,
    /// PID of running process (set by ps-scan enrichment).
    pub pid: Option<u32>,
    /// Whether this server is selected for inclusion in mux config.
    pub selected: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// AppState — the wizard's working set
// ─────────────────────────────────────────────────────────────────────────────

/// Editing the custom-path input field on STEP 1.
#[derive(Debug, Clone, Default)]
pub struct CustomPathInput {
    pub buffer: String,
    /// Whether we are currently in raw-keystroke editing of the buffer.
    pub editing: bool,
    /// Latest validation message ("file not found", "parsed N servers", ...)
    pub status: Option<String>,
}

pub struct AppState {
    /// Current wizard step.
    pub wizard_step: WizardStep,
    /// Path to mux config file (for the legacy mux-only persist path).
    pub config_path: PathBuf,
    /// Discovery sources surfaced on STEP 1.
    pub sources: Vec<SourceEntry>,
    /// Currently highlighted source on STEP 1.
    pub selected_source: usize,
    /// Custom-path input on STEP 1.
    pub custom_path: CustomPathInput,
    /// All discovered services (computed when leaving STEP 1).
    pub services: Vec<ServiceEntry>,
    /// Currently highlighted service on STEP 2.
    pub selected_service: usize,
    /// STEP 3 strategy radio.
    pub strategy: Strategy,
    /// STEP 4 confirm choice.
    pub summary_action: SummaryAction,
    /// STEP 5 tray prompt.
    pub tray_choice: TrayChoice,
    /// Status bar message.
    pub message: String,
    pub dry_run: bool,
    /// Action to perform after the TUI loop exits and the terminal is back
    /// to cooked mode.
    pub pending_action: Option<PendingAction>,
    /// Free-text result that STEP 5 displays after the strategy ran.
    pub strategy_result: Option<String>,
}
