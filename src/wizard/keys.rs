//! Key handling for the 5-step wizard flow.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use super::services::{
    append_default_discovered_services, build_services_from_scans, check_health,
    enrich_running_state,
};
use super::types::{
    AppState, PendingAction, SourceEntry, SourceStatus, Strategy, SummaryAction, TrayChoice,
    WizardStep,
};

/// Top-level key dispatcher. Returns `Ok(true)` to break the TUI loop.
pub fn handle_key(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    match app.wizard_step {
        WizardStep::DiscoverySources => handle_step1(app, key),
        WizardStep::ServerReview => handle_step2(app, key),
        WizardStep::StrategyChoice => handle_step3(app, key),
        WizardStep::SummaryConfirm => handle_step4(app, key),
        WizardStep::ResultAndTray => handle_step5(app, key),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 1: Discovery sources
// ─────────────────────────────────────────────────────────────────────────────

fn handle_step1(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    // While editing the custom path, every keystroke goes into the buffer.
    if app.custom_path.editing {
        match key.code {
            KeyCode::Esc => {
                app.custom_path.editing = false;
                update_step1_message(app);
            }
            KeyCode::Enter => {
                let path = std::mem::take(&mut app.custom_path.buffer);
                app.custom_path.editing = false;
                if path.trim().is_empty() {
                    app.custom_path.status = Some("Path was empty.".into());
                } else {
                    add_custom_source(app, &path);
                }
                update_step1_message(app);
            }
            KeyCode::Backspace => {
                app.custom_path.buffer.pop();
            }
            KeyCode::Char(c) => {
                app.custom_path.buffer.push(c);
            }
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('i') => {
            app.custom_path.editing = true;
            app.message = "Editing custom path… type a path, Enter to add, Esc to cancel.".into();
        }
        KeyCode::Up if app.selected_source > 0 => {
            app.selected_source -= 1;
        }
        KeyCode::Down if app.selected_source + 1 < app.sources.len() => {
            app.selected_source += 1;
        }
        KeyCode::Char(' ') if !app.sources.is_empty() => {
            let idx = app.selected_source.min(app.sources.len() - 1);
            app.sources[idx].selected = !app.sources[idx].selected;
            update_step1_message(app);
        }
        KeyCode::Char('n') | KeyCode::Enter | KeyCode::Right => {
            advance_to_step2(app);
        }
        _ => {}
    }
    Ok(false)
}

fn add_custom_source(app: &mut AppState, raw_path: &str) {
    let expanded = crate::config::expand_path(raw_path);
    let path = PathBuf::from(&expanded);
    let host_file = crate::scan::host_file_from_custom_path(&path);
    let status = if !path.exists() {
        SourceStatus::Missing
    } else {
        match crate::scan::scan_host_file(&host_file) {
            Ok(scan) if scan.services.is_empty() => SourceStatus::Empty,
            Ok(scan) => SourceStatus::Ok {
                servers_found: scan.services.len(),
            },
            Err(err) => SourceStatus::InvalidFormat {
                details: err.to_string(),
            },
        }
    };
    app.custom_path.status = Some(format!(
        "Added {}: {}",
        host_file.path.display(),
        status.short_label()
    ));
    app.sources.push(SourceEntry {
        host_file,
        status,
        selected: true,
    });
    app.selected_source = app.sources.len() - 1;
}

fn update_step1_message(app: &mut AppState) {
    if app.custom_path.editing {
        app.message = "Editing custom path… type a path, Enter to add, Esc to cancel.".into();
        return;
    }
    let selected = app.sources.iter().filter(|s| s.selected).count();
    let total = app.sources.len();
    app.message = format!(
        "STEP 1: {selected}/{total} sources selected | Space toggle | i custom path | n next | q quit"
    );
}

fn advance_to_step2(app: &mut AppState) {
    let scans: Vec<_> = app
        .sources
        .iter()
        .filter(|s| s.selected && matches!(s.status, SourceStatus::Ok { .. }))
        .filter_map(|s| crate::scan::scan_host_file(&s.host_file).ok())
        .collect();

    let mut services = build_services_from_scans(&scans);
    append_default_discovered_services(&mut services);
    enrich_running_state(&mut services);

    // Cheap health checks on entries with sockets, so STEP 2 has badge data
    // available if a future view turns the column on.
    for svc in &mut services {
        svc.health = check_health(&svc.config);
    }

    app.services = services;
    app.selected_service = 0;
    app.wizard_step = WizardStep::ServerReview;
    update_step2_message(app);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 2: Server review
// ─────────────────────────────────────────────────────────────────────────────

fn handle_step2(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Up if app.selected_service > 0 => {
            app.selected_service -= 1;
        }
        KeyCode::Down if app.selected_service + 1 < app.services.len() => {
            app.selected_service += 1;
        }
        KeyCode::Char(' ') if !app.services.is_empty() => {
            let idx = app.selected_service.min(app.services.len() - 1);
            app.services[idx].selected = !app.services[idx].selected;
            update_step2_message(app);
        }
        KeyCode::Char('n') | KeyCode::Right | KeyCode::Enter => {
            if app.services.iter().filter(|s| s.selected).count() == 0 {
                app.message = "Select at least one server (Space) before continuing.".into();
            } else {
                app.wizard_step = WizardStep::StrategyChoice;
                update_step3_message(app);
            }
        }
        KeyCode::Char('p') | KeyCode::Left => {
            app.wizard_step = WizardStep::DiscoverySources;
            update_step1_message(app);
        }
        _ => {}
    }
    Ok(false)
}

fn update_step2_message(app: &mut AppState) {
    let selected = app.services.iter().filter(|s| s.selected).count();
    app.message = format!(
        "STEP 2: {selected}/{} servers selected | Space toggle | n next | p prev | q quit",
        app.services.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 3: Strategy
// ─────────────────────────────────────────────────────────────────────────────

fn handle_step3(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    let order = [Strategy::Unified, Strategy::PerClient, Strategy::AutoRewire];
    let idx = order.iter().position(|s| *s == app.strategy).unwrap_or(0);
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Up if idx > 0 => app.strategy = order[idx - 1],
        KeyCode::Down if idx + 1 < order.len() => app.strategy = order[idx + 1],
        KeyCode::Char('1') => app.strategy = Strategy::Unified,
        KeyCode::Char('2') => app.strategy = Strategy::PerClient,
        KeyCode::Char('3') => app.strategy = Strategy::AutoRewire,
        KeyCode::Char('n') | KeyCode::Right | KeyCode::Enter => {
            app.wizard_step = WizardStep::SummaryConfirm;
            app.summary_action = SummaryAction::Confirm;
            update_step4_message(app);
        }
        KeyCode::Char('p') | KeyCode::Left => {
            app.wizard_step = WizardStep::ServerReview;
            update_step2_message(app);
        }
        _ => {}
    }
    Ok(false)
}

fn update_step3_message(app: &mut AppState) {
    app.message = "STEP 3: Up/Down to choose strategy | 1/2/3 quick pick | n next | p prev".into();
    let _ = app;
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 4: Summary + confirm
// ─────────────────────────────────────────────────────────────────────────────

fn handle_step4(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    let order = [
        SummaryAction::Confirm,
        SummaryAction::Back,
        SummaryAction::Cancel,
    ];
    let idx = order
        .iter()
        .position(|a| *a == app.summary_action)
        .unwrap_or(0);
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Up if idx > 0 => app.summary_action = order[idx - 1],
        KeyCode::Down if idx + 1 < order.len() => app.summary_action = order[idx + 1],
        KeyCode::Char('p') | KeyCode::Left => {
            app.wizard_step = WizardStep::StrategyChoice;
            update_step3_message(app);
        }
        KeyCode::Enter => match app.summary_action {
            SummaryAction::Confirm => {
                queue_pending_for_strategy(app);
                return Ok(true);
            }
            SummaryAction::Back => {
                app.wizard_step = WizardStep::StrategyChoice;
                update_step3_message(app);
            }
            SummaryAction::Cancel => return Ok(true),
        },
        _ => {}
    }
    Ok(false)
}

fn update_step4_message(app: &mut AppState) {
    app.message = "STEP 4: Up/Down to pick action | Enter to do it | p back | q quit".into();
    let _ = app;
}

fn queue_pending_for_strategy(app: &mut AppState) {
    app.pending_action = Some(match app.strategy {
        Strategy::Unified => PendingAction::GenerateUnified,
        Strategy::PerClient => PendingAction::GeneratePerClient,
        Strategy::AutoRewire => PendingAction::AutoRewire,
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 5: Result + tray prompt
// ─────────────────────────────────────────────────────────────────────────────

fn handle_step5(app: &mut AppState, key: KeyEvent) -> Result<bool> {
    let order = [TrayChoice::StartNow, TrayChoice::No];
    let idx = order
        .iter()
        .position(|t| *t == app.tray_choice)
        .unwrap_or(0);
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Up if idx > 0 => app.tray_choice = order[idx - 1],
        KeyCode::Down if idx + 1 < order.len() => app.tray_choice = order[idx + 1],
        KeyCode::Enter => match app.tray_choice {
            TrayChoice::StartNow => {
                app.pending_action = Some(PendingAction::StartTrayDaemon);
                return Ok(true);
            }
            TrayChoice::No => return Ok(true),
        },
        _ => {}
    }
    Ok(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::types::{
        CustomPathInput, HealthStatus, ServiceEntry, ServiceSource, SourceEntry,
    };
    use super::*;
    use crate::config::ServerConfig;
    use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat, HostKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // ───── Fixtures ─────

    /// Minimal `ServerConfig` for synthesising STEP 2 entries.
    fn dummy_server_config(name: &str) -> ServerConfig {
        ServerConfig {
            socket: None,
            cmd: Some("npx".into()),
            args: Some(vec![format!("@modelcontextprotocol/server-{name}")]),
            cwd: None,
            env: None,
            max_active_clients: Some(5),
            tray: Some(false),
            service_name: Some(name.into()),
            log_level: Some("info".into()),
            lazy_start: Some(false),
            max_request_bytes: Some(1_048_576),
            request_timeout_ms: Some(30_000),
            restart_backoff_ms: Some(1_000),
            restart_backoff_max_ms: Some(30_000),
            max_restarts: Some(5),
            status_file: None,
            heartbeat_interval_ms: Some(30_000),
            heartbeat_timeout_ms: Some(30_000),
            heartbeat_max_failures: Some(3),
            heartbeat_enabled: Some(false),
        }
    }

    /// Build an AppState wired for a given step. Skips `run_wizard` entirely
    /// (which requires a TTY); produces just enough state for dispatcher
    /// tests.
    fn make_app(step: WizardStep) -> AppState {
        AppState {
            wizard_step: step,
            config_path: PathBuf::from("/tmp/rust-mux-test-config.toml"),
            sources: Vec::new(),
            selected_source: 0,
            custom_path: CustomPathInput::default(),
            services: Vec::new(),
            selected_service: 0,
            strategy: Strategy::Unified,
            summary_action: SummaryAction::Confirm,
            tray_choice: TrayChoice::No,
            message: String::new(),
            dry_run: true,
            pending_action: None,
            strategy_result: None,
        }
    }

    /// Synthesise a `SourceEntry` whose `host_file.path` is a real on-disk
    /// JSON file so `advance_to_step2` can re-scan it through
    /// `scan_host_file`.
    fn ok_source_at(path: PathBuf, servers_found: usize) -> SourceEntry {
        SourceEntry {
            host_file: HostFile {
                kind: HostKind::Claude,
                path,
                format: HostFormat::Json,
                schema: ConfigSchema::McpServersJson,
                confidence: Confidence::High,
                writable: true,
                eligible_for_danger: true,
            },
            status: SourceStatus::Ok { servers_found },
            selected: true,
        }
    }

    /// Build an AppState on STEP 1 with one selected source backed by a
    /// real tempdir-hosted JSON config containing one MCP server.
    fn make_app_with_sources() -> (AppState, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        fs::write(
            &path,
            r#"{"mcpServers": {"memory": {"command": "npx", "args": ["@modelcontextprotocol/server-memory"]}}}"#,
        )
        .expect("write fixture");

        let mut app = make_app(WizardStep::DiscoverySources);
        app.sources.push(ok_source_at(path, 1));
        (app, dir)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_service(name: &str, selected: bool) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            config: dummy_server_config(name),
            health: HealthStatus::Unknown,
            source: ServiceSource::Client {
                kind: HostKind::Claude,
                path: PathBuf::from("/tmp/test-claude.json"),
            },
            pid: None,
            selected,
        }
    }

    // ───── STEP 1 → STEP 2 (happy path) ─────

    #[test]
    fn step1_n_advances_to_step2_when_a_source_is_selected() {
        let (mut app, _dir) = make_app_with_sources();
        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done, "n should not exit the loop");
        assert_eq!(app.wizard_step, WizardStep::ServerReview);
        // `advance_to_step2` runs `enrich_running_state`, which scans the
        // host's `ps` output for live MCP processes. The fixture contributes
        // one entry ("memory") tagged `Client { kind: Claude, .. }`; any
        // additional entries observed at test time are orphan
        // `DetectedRunning` workers from the host. Assert on the fixture
        // signal, not the total count, so the test stays stable across
        // machines.
        let memory = app
            .services
            .iter()
            .find(|s| s.name == "memory")
            .expect("memory service must be derived from the fixture");
        assert!(memory.selected);
        assert!(matches!(
            memory.source,
            ServiceSource::Client {
                kind: HostKind::Claude,
                ..
            }
        ));
    }

    // ───── STEP 2 → STEP 3 (gated by selection count) ─────

    #[test]
    fn step2_blocks_advance_when_zero_services_selected() {
        let mut app = make_app(WizardStep::ServerReview);
        app.services.push(make_service("memory", false));
        app.services.push(make_service("filesystem", false));

        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done);
        assert_eq!(
            app.wizard_step,
            WizardStep::ServerReview,
            "must not advance with zero selected services"
        );
        assert!(app.message.contains("Select at least one server"));
    }

    #[test]
    fn step2_advances_when_at_least_one_service_selected() {
        let mut app = make_app(WizardStep::ServerReview);
        app.services.push(make_service("memory", true));
        app.services.push(make_service("filesystem", false));

        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done);
        assert_eq!(app.wizard_step, WizardStep::StrategyChoice);
    }

    // ───── STEP 3 → STEP 4 (per strategy) ─────

    #[test]
    fn step3_advances_to_step4_for_unified() {
        let mut app = make_app(WizardStep::StrategyChoice);
        app.strategy = Strategy::Unified;
        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done);
        assert_eq!(app.wizard_step, WizardStep::SummaryConfirm);
        assert_eq!(app.summary_action, SummaryAction::Confirm);
    }

    #[test]
    fn step3_advances_to_step4_for_per_client() {
        let mut app = make_app(WizardStep::StrategyChoice);
        app.strategy = Strategy::PerClient;
        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done);
        assert_eq!(app.wizard_step, WizardStep::SummaryConfirm);
        assert_eq!(app.summary_action, SummaryAction::Confirm);
    }

    #[test]
    fn step3_advances_to_step4_for_auto_rewire() {
        let mut app = make_app(WizardStep::StrategyChoice);
        app.strategy = Strategy::AutoRewire;
        let done = handle_key(&mut app, key(KeyCode::Char('n'))).expect("dispatch");
        assert!(!done);
        assert_eq!(app.wizard_step, WizardStep::SummaryConfirm);
        assert_eq!(app.summary_action, SummaryAction::Confirm);
    }

    #[test]
    fn step3_quick_pick_keys_set_strategy() {
        let mut app = make_app(WizardStep::StrategyChoice);
        handle_key(&mut app, key(KeyCode::Char('1'))).expect("1");
        assert_eq!(app.strategy, Strategy::Unified);
        handle_key(&mut app, key(KeyCode::Char('2'))).expect("2");
        assert_eq!(app.strategy, Strategy::PerClient);
        handle_key(&mut app, key(KeyCode::Char('3'))).expect("3");
        assert_eq!(app.strategy, Strategy::AutoRewire);
    }

    // ───── STEP 4 Confirm/Cancel/Back ─────

    #[test]
    fn step4_confirm_unified_queues_generate_unified_and_exits_loop() {
        let mut app = make_app(WizardStep::SummaryConfirm);
        app.strategy = Strategy::Unified;
        app.summary_action = SummaryAction::Confirm;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done, "Confirm must break the TUI loop");
        assert_eq!(app.pending_action, Some(PendingAction::GenerateUnified));
    }

    #[test]
    fn step4_confirm_per_client_queues_generate_per_client() {
        let mut app = make_app(WizardStep::SummaryConfirm);
        app.strategy = Strategy::PerClient;
        app.summary_action = SummaryAction::Confirm;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done);
        assert_eq!(app.pending_action, Some(PendingAction::GeneratePerClient));
    }

    #[test]
    fn step4_confirm_auto_rewire_queues_auto_rewire() {
        let mut app = make_app(WizardStep::SummaryConfirm);
        app.strategy = Strategy::AutoRewire;
        app.summary_action = SummaryAction::Confirm;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done);
        assert_eq!(app.pending_action, Some(PendingAction::AutoRewire));
    }

    #[test]
    fn step4_cancel_exits_with_no_pending_action() {
        let mut app = make_app(WizardStep::SummaryConfirm);
        app.strategy = Strategy::Unified;
        app.summary_action = SummaryAction::Cancel;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done, "Cancel must break the TUI loop");
        assert!(
            app.pending_action.is_none(),
            "Cancel must not queue any persistence action"
        );
    }

    #[test]
    fn step4_back_returns_to_strategy_choice_without_action() {
        let mut app = make_app(WizardStep::SummaryConfirm);
        app.strategy = Strategy::Unified;
        app.summary_action = SummaryAction::Back;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(!done, "Back must keep the loop running");
        assert_eq!(app.wizard_step, WizardStep::StrategyChoice);
        assert!(app.pending_action.is_none());
    }

    // ───── STEP 5 tray prompt ─────

    #[test]
    fn step5_start_now_queues_start_tray_daemon() {
        let mut app = make_app(WizardStep::ResultAndTray);
        app.tray_choice = TrayChoice::StartNow;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done);
        assert_eq!(app.pending_action, Some(PendingAction::StartTrayDaemon));
    }

    #[test]
    fn step5_no_exits_with_no_pending_action() {
        let mut app = make_app(WizardStep::ResultAndTray);
        app.tray_choice = TrayChoice::No;

        let done = handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(done);
        assert!(app.pending_action.is_none());
    }

    // ───── STEP 1 custom-path edit mode ─────

    #[test]
    fn step1_i_enters_custom_path_editing_mode() {
        let mut app = make_app(WizardStep::DiscoverySources);
        handle_key(&mut app, key(KeyCode::Char('i'))).expect("dispatch");
        assert!(app.custom_path.editing);
        assert!(app.message.contains("Editing custom path"));
    }

    #[test]
    fn step1_editing_buffers_typed_chars_and_handles_backspace() {
        let mut app = make_app(WizardStep::DiscoverySources);
        app.custom_path.editing = true;

        for c in "/tmp/x".chars() {
            handle_key(&mut app, key(KeyCode::Char(c))).expect("dispatch");
        }
        assert_eq!(app.custom_path.buffer, "/tmp/x");

        handle_key(&mut app, key(KeyCode::Backspace)).expect("dispatch");
        assert_eq!(app.custom_path.buffer, "/tmp/");
        assert!(
            app.custom_path.editing,
            "Backspace must not exit editing mode"
        );
    }

    #[test]
    fn step1_enter_on_empty_buffer_reports_path_was_empty() {
        let mut app = make_app(WizardStep::DiscoverySources);
        app.custom_path.editing = true;
        app.custom_path.buffer.clear();

        handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");
        assert!(
            !app.custom_path.editing,
            "Enter must close the edit field even on empty input"
        );
        assert_eq!(
            app.custom_path.status.as_deref(),
            Some("Path was empty."),
            "empty Enter must surface the explicit status string"
        );
        assert!(
            app.sources.is_empty(),
            "empty Enter must not append a SourceEntry"
        );
    }

    #[test]
    fn step1_enter_on_non_empty_path_appends_source_entry() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("custom.json");
        fs::write(
            &path,
            r#"{"mcpServers": {"foo": {"command": "echo", "args": ["foo"]}}}"#,
        )
        .expect("write fixture");

        let mut app = make_app(WizardStep::DiscoverySources);
        app.custom_path.editing = true;
        app.custom_path.buffer = path.to_string_lossy().into_owned();

        handle_key(&mut app, key(KeyCode::Enter)).expect("dispatch");

        assert!(!app.custom_path.editing);
        assert_eq!(
            app.sources.len(),
            1,
            "custom path must be added as a source"
        );
        assert!(app.sources[0].selected);
        match &app.sources[0].status {
            SourceStatus::Ok { servers_found } => assert_eq!(*servers_found, 1),
            other => panic!("expected SourceStatus::Ok with 1 server, got {other:?}"),
        }
    }

    #[test]
    fn step1_esc_in_edit_mode_cancels_editing() {
        let mut app = make_app(WizardStep::DiscoverySources);
        app.custom_path.editing = true;
        app.custom_path.buffer = "/tmp/abc".into();

        handle_key(&mut app, key(KeyCode::Esc)).expect("dispatch");
        assert!(!app.custom_path.editing, "Esc must exit edit mode");
        // Esc preserves the buffer (only Enter consumes it via mem::take).
        assert_eq!(app.custom_path.buffer, "/tmp/abc");
    }

    // ───── STEP 1 Up/Down clamping ─────

    #[test]
    fn step1_up_clamps_at_zero() {
        let (mut app, _dir) = make_app_with_sources();
        // Add a second source so cursor can move at all.
        let path = _dir.path().join("second.json");
        fs::write(&path, r#"{"mcpServers": {}}"#).expect("write");
        app.sources.push(SourceEntry {
            host_file: HostFile {
                kind: HostKind::Claude,
                path,
                format: HostFormat::Json,
                schema: ConfigSchema::McpServersJson,
                confidence: Confidence::High,
                writable: true,
                eligible_for_danger: true,
            },
            status: SourceStatus::Empty,
            selected: false,
        });

        app.selected_source = 0;
        handle_key(&mut app, key(KeyCode::Up)).expect("dispatch");
        assert_eq!(app.selected_source, 0, "Up at index 0 must clamp");
    }

    #[test]
    fn step1_down_clamps_at_last_index() {
        let (mut app, dir) = make_app_with_sources();
        let path = dir.path().join("second.json");
        fs::write(&path, r#"{"mcpServers": {}}"#).expect("write");
        app.sources.push(SourceEntry {
            host_file: HostFile {
                kind: HostKind::Claude,
                path,
                format: HostFormat::Json,
                schema: ConfigSchema::McpServersJson,
                confidence: Confidence::High,
                writable: true,
                eligible_for_danger: true,
            },
            status: SourceStatus::Empty,
            selected: false,
        });

        app.selected_source = app.sources.len() - 1;
        handle_key(&mut app, key(KeyCode::Down)).expect("dispatch");
        assert_eq!(
            app.selected_source,
            app.sources.len() - 1,
            "Down at last index must clamp"
        );
    }

    // ───── STEP 2 Up/Down clamping ─────

    #[test]
    fn step2_up_down_clamps_at_boundaries() {
        let mut app = make_app(WizardStep::ServerReview);
        app.services.push(make_service("a", true));
        app.services.push(make_service("b", true));

        app.selected_service = 0;
        handle_key(&mut app, key(KeyCode::Up)).expect("dispatch");
        assert_eq!(app.selected_service, 0, "Up at 0 must clamp");

        app.selected_service = app.services.len() - 1;
        handle_key(&mut app, key(KeyCode::Down)).expect("dispatch");
        assert_eq!(
            app.selected_service,
            app.services.len() - 1,
            "Down at last must clamp"
        );
    }

    // ───── q quits on every step ─────

    #[test]
    fn q_quits_from_any_step() {
        for step in [
            WizardStep::DiscoverySources,
            WizardStep::ServerReview,
            WizardStep::StrategyChoice,
            WizardStep::SummaryConfirm,
            WizardStep::ResultAndTray,
        ] {
            let mut app = make_app(step);
            let done = handle_key(&mut app, key(KeyCode::Char('q'))).expect("dispatch");
            assert!(done, "q must break the loop on {:?}", step);
        }
    }

    // ───── Render smoke tests (no .snap files; just confirm draw_ui never
    //       panics on each step's state with TestBackend). ─────

    #[test]
    fn draw_ui_renders_without_panic_on_every_step() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use super::super::ui::draw_ui;

        let steps = [
            WizardStep::DiscoverySources,
            WizardStep::ServerReview,
            WizardStep::StrategyChoice,
            WizardStep::SummaryConfirm,
            WizardStep::ResultAndTray,
        ];
        for step in steps {
            let mut app = make_app(step);
            // Populate enough state for non-empty rendering.
            app.services.push(make_service("memory", true));
            app.strategy_result = Some("dry-run summary".into());
            app.message = format!("rendering {step:?}");

            let backend = TestBackend::new(120, 40);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|f| draw_ui(f, &app))
                .unwrap_or_else(|e| panic!("draw_ui panicked on {step:?}: {e}"));
        }
    }
}
