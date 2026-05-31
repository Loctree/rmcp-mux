//! Interactive wizard for configuring rmcp-mux services and rewiring MCP clients.
//!
//! v0.4.0 5-step flow:
//!
//! 1. **DiscoverySources** — pick which client config files to scan, plus
//!    optional custom paths.
//! 2. **ServerReview** — read-only tree of discovered servers grouped by
//!    client, with dedup count.
//! 3. **StrategyChoice** — Unified / Per-client / Auto-rewire (DANGER).
//! 4. **SummaryConfirm** — preview of what will be written and where.
//! 5. **ResultAndTray** — show what happened, offer to start the tray
//!    daemon now.

use std::io::{IsTerminal, stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Args;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::expand_path;
use crate::scan::{default_sources, scan_host_file};

mod keys;
mod persist;
mod services;
mod types;
mod ui;

use keys::handle_key;
use persist::{
    run_danger_auto_configure, run_per_client_generate, run_unified_generate, start_tray_daemon,
};
use services::{append_default_discovered_services, build_services_from_scans};
use types::{
    AppState, CustomPathInput, PendingAction, SourceEntry, SourceStatus, Strategy, SummaryAction,
    TrayChoice, WizardStep,
};
use ui::draw_ui;

// ─────────────────────────────────────────────────────────────────────────────
// CLI arguments
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Args)]
pub struct WizardArgs {
    /// Path to mux daemon config (json/yaml/toml). Default: ~/.codex/mcp-mux.toml.
    /// Used as the fallback target when starting the tray daemon if no
    /// generated mux config is available.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Service key (kept for backwards compatibility; ignored by the
    /// 5-step flow which discovers from client configs).
    #[arg(long)]
    pub service: Option<String>,
    /// Socket path override (legacy; ignored).
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Command override (legacy; ignored).
    #[arg(long)]
    pub cmd: Option<String>,
    /// Args override (legacy; ignored).
    #[arg(long)]
    pub args: Vec<String>,
    /// Max clients override (legacy; ignored).
    #[arg(long)]
    pub max_clients: Option<usize>,
    /// Log level override (legacy; ignored).
    #[arg(long)]
    pub log_level: Option<String>,
    /// Tray override (legacy; the wizard offers a tray prompt on STEP 5).
    #[arg(long)]
    pub tray: Option<bool>,
    /// Do not write files; just preview.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Non-interactive strategy preview. Currently supports `unified` with
    /// `--dry-run`, so CI/agents can smoke the wizard discovery path without
    /// driving ratatui.
    #[arg(long)]
    pub strategy: Option<String>,
    /// Pre-load extra MCP client config files. Each path is added as a
    /// custom source on STEP 1 and selected by default.
    #[arg(long = "import-config")]
    pub import_configs: Vec<PathBuf>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run_wizard(args: WizardArgs) -> Result<()> {
    if let Some(strategy) = args.strategy.as_deref() {
        return run_wizard_strategy_preview(&args, strategy);
    }

    if !stdout().is_terminal() {
        return Err(anyhow!(
            "wizard requires an interactive TTY; use the CLI subcommands (scan / rewire / health) for non-interactive mode"
        ));
    }

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| expand_path("~/.codex/mcp-mux.toml"));

    let sources = build_initial_sources(&args.import_configs);

    let mut app = AppState {
        wizard_step: WizardStep::DiscoverySources,
        config_path,
        sources,
        selected_source: 0,
        custom_path: CustomPathInput::default(),
        services: Vec::new(),
        selected_service: 0,
        strategy: Strategy::Unified,
        summary_action: SummaryAction::Confirm,
        tray_choice: TrayChoice::No,
        message: String::new(),
        dry_run: args.dry_run,
        pending_action: None,
        strategy_result: None,
    };
    refresh_step1_message(&mut app);

    run_tui(&mut app)?;

    Ok(())
}

fn run_wizard_strategy_preview(args: &WizardArgs, strategy: &str) -> Result<()> {
    if !args.dry_run {
        return Err(anyhow!(
            "wizard --strategy currently supports preview only; add --dry-run"
        ));
    }
    if strategy != "unified" {
        return Err(anyhow!(
            "unsupported wizard strategy '{strategy}' (currently supported: unified)"
        ));
    }

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| expand_path("~/.codex/mcp-mux.toml"));
    let sources = build_initial_sources(&args.import_configs);
    let scans = sources
        .iter()
        .filter(|s| s.selected && matches!(s.status, SourceStatus::Ok { .. }))
        .filter_map(|s| scan_host_file(&s.host_file).ok())
        .collect::<Vec<_>>();
    let mut services = build_services_from_scans(&scans);
    append_default_discovered_services(&mut services);

    let app = AppState {
        wizard_step: WizardStep::SummaryConfirm,
        config_path,
        sources,
        selected_source: 0,
        custom_path: CustomPathInput::default(),
        services,
        selected_service: 0,
        strategy: Strategy::Unified,
        summary_action: SummaryAction::Confirm,
        tray_choice: TrayChoice::No,
        message: String::new(),
        dry_run: true,
        pending_action: None,
        strategy_result: None,
    };

    println!("{}", run_unified_generate(&app)?);
    Ok(())
}

/// Walk the canonical `default_sources()` list, classify each entry, then
/// append any `--import-config` paths the operator passed on the CLI.
fn build_initial_sources(imports: &[PathBuf]) -> Vec<SourceEntry> {
    let mut out = Vec::new();
    for source in default_sources() {
        let status = classify_source(&source);
        let exists = matches!(
            status,
            SourceStatus::Ok { .. } | SourceStatus::Empty | SourceStatus::InvalidFormat { .. }
        );
        out.push(SourceEntry {
            host_file: source,
            status,
            selected: exists,
        });
    }
    for path in imports {
        let host = crate::scan::host_file_from_custom_path(path);
        let status = classify_source(&host);
        out.push(SourceEntry {
            host_file: host,
            status,
            selected: true,
        });
    }
    out
}

fn classify_source(host: &crate::scan::HostFile) -> SourceStatus {
    if !host.path.exists() {
        return SourceStatus::Missing;
    }
    match scan_host_file(host) {
        Ok(scan) if scan.services.is_empty() => SourceStatus::Empty,
        Ok(scan) => SourceStatus::Ok {
            servers_found: scan.services.len(),
        },
        Err(err) => SourceStatus::InvalidFormat {
            details: err.to_string(),
        },
    }
}

fn refresh_step1_message(app: &mut AppState) {
    let selected = app.sources.iter().filter(|s| s.selected).count();
    let total = app.sources.len();
    app.message = format!(
        "STEP 1: {selected}/{total} sources selected | Space toggle | i custom path | n next | q quit"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// TUI loop
// ─────────────────────────────────────────────────────────────────────────────

fn run_tui(app: &mut AppState) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout_handle = stdout();
    execute!(stdout_handle, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout_handle);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    loop {
        terminal.draw(|f| draw_ui(f, app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let evt = event::read()?;
        if let Event::Key(key) = evt {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if handle_key(app, key)? {
                break;
            }
        }
    }

    // Drop the alternate screen so cooked stdout/stdin work for the
    // post-loop drain below.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    drain_pending_actions(app)?;

    Ok(())
}

/// Handles every action that needs cooked stdin/stdout (printing summaries,
/// the danger CONFIRM prompt, spawning the tray daemon).
///
/// The flow is:
/// - Strategy actions print their result, set `strategy_result`, re-enter the
///   alt screen, switch to STEP 5, and run the loop again so the operator can
///   choose the tray prompt.
/// - The tray-daemon spawn is terminal: it prints the spawn line and exits.
fn drain_pending_actions(app: &mut AppState) -> Result<()> {
    let Some(action) = app.pending_action.take() else {
        return Ok(());
    };

    match action {
        PendingAction::GenerateUnified => {
            let summary = run_unified_generate(app)?;
            println!("\n{summary}");
            advance_to_step5(app, summary)?;
        }
        PendingAction::GeneratePerClient => {
            let summary = run_per_client_generate(app)?;
            println!("\n{summary}");
            advance_to_step5(app, summary)?;
        }
        PendingAction::AutoRewire => {
            let summary = run_danger_auto_configure(app)?;
            println!("\n{summary}");
            advance_to_step5(app, summary)?;
        }
        PendingAction::StartTrayDaemon => {
            let summary = start_tray_daemon(app)?;
            println!("\n{summary}\n");
        }
    }
    Ok(())
}

/// Re-enter the alt screen on STEP 5 so the operator can pick the tray
/// prompt with the same TUI machinery.
fn advance_to_step5(app: &mut AppState, summary: String) -> Result<()> {
    app.wizard_step = WizardStep::ResultAndTray;
    app.tray_choice = TrayChoice::No;
    app.strategy_result = Some(summary);
    app.message = "STEP 5: Up/Down to choose, Enter to confirm, q to quit.".into();

    enable_raw_mode()?;
    let mut stdout_handle = stdout();
    execute!(stdout_handle, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout_handle);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    loop {
        terminal.draw(|f| draw_ui(f, app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let evt = event::read()?;
        if let Event::Key(key) = evt {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if handle_key(app, key)? {
                break;
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // STEP 5 may have queued a follow-up tray-daemon spawn.
    if let Some(PendingAction::StartTrayDaemon) = app.pending_action.take() {
        let summary = start_tray_daemon(app)?;
        println!("\n{summary}\n");
    }

    Ok(())
}
