//! Persistence and export functions for the 5-step wizard.
//!
//! Three strategies live here, plus the tray-daemon launcher:
//!
//! - `run_unified_generate` — Strategy::Unified — write
//!   `~/.config/mux/{config.toml, mcp.json, mcp.toml}` plus per-client
//!   setup snippets.
//! - `run_per_client_generate` — Strategy::PerClient — one mux config file
//!   per originating client kind, in that client's native format. Daemon
//!   `config.toml` still merged across every selected source.
//! - `run_danger_auto_configure` — Strategy::AutoRewire — backup-first
//!   preview-first rewrite of the user's existing client configs.
//! - `start_tray_daemon` — spawns `rmcp-mux --tray --multi-service` detached
//!   (STEP 5 "Yes — start now").

use std::io::{Write, stdin, stdout};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::config::expand_path;
use crate::danger::{
    DangerStatus, execute_plan, format_preview, plan_danger_rewrite_for_scans, rollback_commands,
};
use crate::mux_gen::{
    build_mux_outputs, build_per_client_outputs, default_mux_dir, per_client_instructions,
    safe_path_instructions, write_mux_outputs, write_per_client_outputs,
};
use crate::scan::{HostService, MergeOutcome, ScanResult, scan_host_file};

use super::types::{AppState, SourceStatus};

// ─────────────────────────────────────────────────────────────────────────────
// Build helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Re-scan the operator's selected sources so the strategies see the same
/// services they had on STEP 2.
fn selected_scans(app: &AppState) -> Vec<ScanResult> {
    let filter = SelectedServiceFilter::from_app(app);
    app.sources
        .iter()
        .filter(|s| s.selected && matches!(s.status, SourceStatus::Ok { .. }))
        .filter_map(|s| scan_host_file(&s.host_file).ok())
        .map(|mut scan| {
            scan.services.retain(|svc| filter.matches(svc));
            scan
        })
        .collect()
}

/// Build a [`MergeOutcome`] from the operator's STEP 2 selection, restricted
/// to entries the operator left ticked.
fn build_merge_from_services(app: &AppState) -> MergeOutcome {
    use crate::scan::HostService;
    let mut services = Vec::new();
    for svc in &app.services {
        if !svc.selected {
            continue;
        }
        services.push(HostService {
            name: svc.name.clone(),
            command: svc.config.cmd.clone().unwrap_or_default(),
            args: svc.config.args.clone().unwrap_or_default(),
            cwd: svc.config.cwd.clone(),
            socket: svc.config.socket.clone(),
            env: svc.config.env.clone(),
            enabled: None,
        });
    }
    MergeOutcome {
        services,
        conflicts: Vec::new(),
    }
}

/// Selected sources, filtered to the operator's STEP 2 server selection and
/// restricted to sources eligible for the danger flow.
fn selected_danger_scans(app: &AppState) -> Vec<ScanResult> {
    selected_scans(app)
        .into_iter()
        .filter(|scan| scan.host.eligible_for_danger)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServiceExactKey {
    name: String,
    shape: ServiceShapeKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServiceShapeKey {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    socket: Option<String>,
    env: Vec<(String, String)>,
}

struct SelectedServiceFilter {
    exact: std::collections::HashSet<ServiceExactKey>,
    conflict_shapes: std::collections::HashSet<ServiceShapeKey>,
}

impl SelectedServiceFilter {
    fn from_app(app: &AppState) -> Self {
        let mut exact = std::collections::HashSet::new();
        let mut conflict_shapes = std::collections::HashSet::new();

        for svc in app.services.iter().filter(|svc| svc.selected) {
            let shape = ServiceShapeKey {
                command: svc.config.cmd.clone().unwrap_or_default(),
                args: svc.config.args.clone().unwrap_or_default(),
                cwd: svc.config.cwd.clone(),
                socket: svc.config.socket.clone(),
                env: sorted_env(svc.config.env.as_ref()),
            };
            exact.insert(ServiceExactKey {
                name: svc.name.clone(),
                shape: shape.clone(),
            });

            // merge_services renames divergent same-name variants with
            // `<name>-from-<kind>`, while the source file still carries the
            // original name. Shape fallback keeps those explicit selections
            // routeable without making the UI store another parallel ID.
            if svc.name.contains("-from-") {
                conflict_shapes.insert(shape);
            }
        }

        Self {
            exact,
            conflict_shapes,
        }
    }

    fn matches(&self, svc: &HostService) -> bool {
        let shape = ServiceShapeKey {
            command: svc.command.clone(),
            args: svc.args.clone(),
            cwd: svc.cwd.clone(),
            socket: svc.socket.clone(),
            env: sorted_env(svc.env.as_ref()),
        };
        self.exact.contains(&ServiceExactKey {
            name: svc.name.clone(),
            shape: shape.clone(),
        }) || self.conflict_shapes.contains(&shape)
    }
}

fn sorted_env(env: Option<&std::collections::HashMap<String, String>>) -> Vec<(String, String)> {
    let Some(env) = env else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

// ─────────────────────────────────────────────────────────────────────────────
// Strategy::Unified
// ─────────────────────────────────────────────────────────────────────────────

/// Write `~/.config/mux/{config.toml, mcp.json, mcp.toml}` and return the
/// human-readable summary that STEP 5 will display.
pub fn run_unified_generate(app: &AppState) -> Result<String> {
    let merge = build_merge_from_services(app);
    if merge.services.is_empty() {
        return Ok("No services selected — nothing generated.".into());
    }
    let mux_dir = default_mux_dir();
    let outputs = build_mux_outputs(&merge, &mux_dir, "rmcp-mux-proxy", &[])?;

    if app.dry_run {
        let selected = merge
            .services
            .iter()
            .map(|svc| format!("  - {}", svc.name))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(format!(
            "(dry-run) Would generate:\n  - {}\n  - {}\n  - {}\nPer-client setup commands would be printed on completion.",
            outputs.config_toml_path.display(),
            outputs.mcp_json_path.display(),
            outputs.mcp_toml_path.display()
        ) + if selected.is_empty() {
            ""
        } else {
            "\nSelected servers:\n"
        } + &selected);
    }

    write_mux_outputs(&outputs)?;

    let mut summary = String::new();
    summary.push_str(&format!(
        "Wrote rmcp-mux config under {}:\n",
        outputs.mux_dir.display()
    ));
    summary.push_str(&format!(
        "  - {} (daemon truth)\n",
        outputs.config_toml_path.display()
    ));
    summary.push_str(&format!(
        "  - {} (client JSON)\n",
        outputs.mcp_json_path.display()
    ));
    summary.push_str(&format!(
        "  - {} (client TOML)\n",
        outputs.mcp_toml_path.display()
    ));
    summary.push('\n');
    summary.push_str(&format!(
        "Start the mux:\n  rmcp-mux --config {}\n\n",
        outputs.config_toml_path.display()
    ));
    summary.push_str("Use it from your AI clients:\n");
    for inst in safe_path_instructions(&outputs) {
        summary.push_str(&format!("• {} ({})\n", inst.headline, inst.kind.as_label()));
        for cmd in &inst.commands {
            summary.push_str(&format!("    {cmd}\n"));
        }
        summary.push_str(&format!("    note: {}\n\n", inst.note));
    }
    if !outputs.conflicts.is_empty() {
        summary.push_str(&format!(
            "⚠️  {} server-name conflict(s) surfaced — review the config.toml entries.\n",
            outputs.conflicts.len()
        ));
    }
    Ok(summary)
}

// ─────────────────────────────────────────────────────────────────────────────
// Strategy::PerClient
// ─────────────────────────────────────────────────────────────────────────────

/// Write per-client mux config files plus a shared daemon `config.toml`.
pub fn run_per_client_generate(app: &AppState) -> Result<String> {
    let scans = selected_scans(app);
    if scans.iter().all(|scan| scan.services.is_empty()) {
        return Ok("No selected services parsed cleanly — nothing generated.".into());
    }
    let mux_dir = default_mux_dir();
    let outputs = build_per_client_outputs(&scans, &mux_dir, "rmcp-mux-proxy", &[])?;

    if app.dry_run {
        let mut s = format!(
            "(dry-run) Would write under {}:\n",
            outputs.mux_dir.display()
        );
        s.push_str(&format!(
            "  - {} (daemon truth)\n",
            outputs.config_toml_path.display()
        ));
        for client in &outputs.clients {
            s.push_str(&format!(
                "  - {} ({} servers)\n",
                client.path.display(),
                client.services.len()
            ));
        }
        return Ok(s);
    }

    write_per_client_outputs(&outputs)?;

    let mut summary = String::new();
    summary.push_str(&format!(
        "Wrote rmcp-mux per-client configs under {}:\n",
        outputs.mux_dir.display()
    ));
    summary.push_str(&format!(
        "  - {} (daemon truth, {} unique servers)\n",
        outputs.config_toml_path.display(),
        outputs.total_services
    ));
    for client in &outputs.clients {
        summary.push_str(&format!(
            "  - {} ({} servers)\n",
            client.path.display(),
            client.services.len()
        ));
    }
    summary.push('\n');
    summary.push_str(&format!(
        "Start the mux:\n  rmcp-mux --config {}\n\n",
        outputs.config_toml_path.display()
    ));
    summary.push_str("Use the per-client mux files from each AI client:\n");
    for inst in per_client_instructions(&outputs) {
        summary.push_str(&format!("• {} ({})\n", inst.headline, inst.kind.as_label()));
        for cmd in &inst.commands {
            summary.push_str(&format!("    {cmd}\n"));
        }
        summary.push_str(&format!("    note: {}\n\n", inst.note));
    }
    if !outputs.conflicts.is_empty() {
        summary.push_str(&format!(
            "⚠️  {} server-name conflict(s) surfaced across clients — daemon config kept the variants apart with -from-<kind> suffixes.\n",
            outputs.conflicts.len()
        ));
    }
    Ok(summary)
}

// ─────────────────────────────────────────────────────────────────────────────
// Strategy::AutoRewire (DANGER)
// ─────────────────────────────────────────────────────────────────────────────

/// Caller is responsible for leaving the TUI's alternate screen and disabling
/// raw mode before invoking this; the function uses cooked stdin/stdout for
/// the explicit `CONFIRM` prompt.
pub fn run_danger_auto_configure(app: &AppState) -> Result<String> {
    let merge = build_merge_from_services(app);
    if merge.services.is_empty() {
        return Ok("No services selected — danger flow has nothing to do.".into());
    }
    let scans = selected_danger_scans(app);
    if scans.is_empty() {
        return Ok(
            "No selected sources are eligible for danger rewrite (or none parsed cleanly).".into(),
        );
    }

    let plan = plan_danger_rewrite_for_scans(
        &scans,
        "rmcp-mux-proxy",
        &[],
        &expand_path("~/.config/mux/sockets"),
    );

    let preview = format_preview(&plan);
    println!("\n{preview}");

    let any_planned = plan
        .actions
        .iter()
        .any(|a| matches!(a.status, DangerStatus::Planned));
    if !any_planned {
        println!("(no files planned for change — nothing to confirm)\n");
        return Ok("No eligible files to rewrite.".into());
    }

    if app.dry_run {
        println!("(dry-run) plan above would have been executed; no files modified.\n");
        return Ok("Dry-run: danger plan rendered, no writes performed.".into());
    }

    println!(
        "Type CONFIRM (uppercase) and press Enter to apply the rewrite, anything else to cancel:"
    );
    print!("> ");
    let _ = stdout().flush();
    let mut input = String::new();
    stdin()
        .read_line(&mut input)
        .context("read confirmation prompt")?;

    if input.trim() != "CONFIRM" {
        println!("Cancelled — no files modified.\n");
        return Ok("Danger flow cancelled by operator.".into());
    }

    let outcomes = execute_plan(&plan, true)?;

    println!("\nResults:");
    let mut written = 0usize;
    for o in &outcomes {
        match &o.status {
            DangerStatus::Planned if o.written => {
                written += 1;
                let backup = o
                    .backup
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(none)".into());
                println!("  ✓ wrote {} (backup: {})", o.path.display(), backup);
            }
            other => {
                let err = o.error.as_deref().unwrap_or("");
                println!(
                    "  · {} skipped ({:?}){}",
                    o.path.display(),
                    other,
                    if err.is_empty() {
                        String::new()
                    } else {
                        format!(": {err}")
                    }
                );
            }
        }
    }

    let rollback = rollback_commands(&outcomes);
    if !rollback.is_empty() {
        println!("\nRollback (paste any line to restore that file):");
        for cmd in &rollback {
            println!("  {cmd}");
        }
    }

    Ok(format!(
        "Auto-rewire applied to {written} file(s); see terminal for details and rollback commands."
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tray daemon launcher (STEP 5 "Yes — start now")
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn `rmcp-mux --tray --multi-service` detached from this terminal so the
/// wizard exit doesn't kill the daemon. Returns a short summary line.
pub fn start_tray_daemon(app: &AppState) -> Result<String> {
    if app.dry_run {
        return Ok("(dry-run) tray daemon would be started in the background.".into());
    }

    // Pick the most useful config target: prefer the freshly generated mux
    // config.toml, fall back to the wizard's --config argument.
    let mux_config = default_mux_dir().join("config.toml");
    let config_arg = if mux_config.exists() {
        mux_config
    } else {
        app.config_path.clone()
    };

    // Prefer the binary sitting next to the running wizard (covers `cargo run`
    // and bespoke install paths); fall back to PATH lookup of `rmcp-mux`.
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rmcp-mux")))
        .filter(|p| p.exists())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("rmcp-mux"));

    let mut cmd = Command::new(&bin);
    cmd.arg("--tray")
        .arg("--config")
        .arg(&config_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match cmd.spawn() {
        Ok(child) => Ok(format!(
            "Started tray daemon (pid {}) using config {}",
            child.id(),
            config_arg.display()
        )),
        Err(err) => Ok(format!(
            "Could not start tray daemon: {err}. Run manually: rmcp-mux --tray --config {}",
            config_arg.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat, HostKind};
    use crate::wizard::types::{
        AppState, CustomPathInput, ServiceEntry, ServiceSource, SourceEntry, Strategy,
        SummaryAction, TrayChoice, WizardStep,
    };
    use std::fs;
    use tempfile::tempdir;

    fn make_app(tmp: &std::path::Path) -> AppState {
        AppState {
            wizard_step: WizardStep::SummaryConfirm,
            config_path: tmp.join("mcp-mux.toml"),
            sources: Vec::new(),
            selected_source: 0,
            custom_path: CustomPathInput::default(),
            services: vec![ServiceEntry {
                name: "memory".into(),
                config: ServerConfig {
                    socket: Some(
                        tmp.join("sockets/memory.sock")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    cmd: Some("npx".into()),
                    args: Some(vec!["@modelcontextprotocol/server-memory".into()]),
                    cwd: None,
                    env: None,
                    max_active_clients: Some(5),
                    tray: Some(false),
                    service_name: Some("memory".into()),
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
                },
                health: super::super::types::HealthStatus::Unknown,
                source: ServiceSource::Client {
                    kind: HostKind::Custom,
                    path: tmp.join("custom.json"),
                },
                pid: None,
                selected: true,
            }],
            selected_service: 0,
            strategy: Strategy::Unified,
            summary_action: SummaryAction::Confirm,
            tray_choice: TrayChoice::No,
            message: String::new(),
            dry_run: false,
            pending_action: None,
            strategy_result: None,
        }
    }

    fn claude_source(path: std::path::PathBuf) -> HostFile {
        HostFile {
            kind: HostKind::Claude,
            path,
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        }
    }

    fn service_entry(name: &str, package: &str, selected: bool) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            config: ServerConfig {
                socket: None,
                cmd: Some("npx".into()),
                args: Some(vec![package.into()]),
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
            },
            health: super::super::types::HealthStatus::Unknown,
            source: ServiceSource::Client {
                kind: HostKind::Claude,
                path: std::path::PathBuf::from("/tmp/claude.json"),
            },
            pid: None,
            selected,
        }
    }

    fn app_with_two_source_services(tmp: &std::path::Path) -> (AppState, std::path::PathBuf) {
        let path = tmp.join("claude.json");
        fs::write(
            &path,
            r#"{
              "mcpServers": {
                "memory": {
                  "command": "npx",
                  "args": ["@modelcontextprotocol/server-memory"]
                },
                "brave": {
                  "command": "npx",
                  "args": ["@modelcontextprotocol/server-brave-search"]
                }
              }
            }"#,
        )
        .expect("write source");

        let host = claude_source(path.clone());
        let mut app = make_app(tmp);
        app.sources = vec![SourceEntry {
            host_file: host,
            status: SourceStatus::Ok { servers_found: 2 },
            selected: true,
        }];
        app.services = vec![
            service_entry("memory", "@modelcontextprotocol/server-memory", true),
            service_entry("brave", "@modelcontextprotocol/server-brave-search", false),
        ];
        (app, path)
    }

    #[test]
    fn unified_dry_run_does_not_write_files() {
        let dir = tempdir().expect("tempdir");
        let mut app = make_app(dir.path());
        app.dry_run = true;
        let summary = run_unified_generate(&app).expect("unified dry");
        assert!(
            summary.contains("dry-run") || summary.contains("Would"),
            "summary should announce dry-run: {summary}"
        );
    }

    #[test]
    fn unified_with_no_services_selected_is_noop() {
        let dir = tempdir().expect("tempdir");
        let mut app = make_app(dir.path());
        app.services[0].selected = false;
        let summary = run_unified_generate(&app).expect("unified empty");
        assert!(summary.contains("No services"));
    }

    #[test]
    fn per_client_with_no_sources_is_noop() {
        let dir = tempdir().expect("tempdir");
        let app = make_app(dir.path());
        let summary = run_per_client_generate(&app).expect("per-client empty");
        assert!(
            summary.contains("No selected sources") || summary.contains("nothing generated"),
            "summary: {summary}"
        );
    }

    #[test]
    fn per_client_scans_honor_step2_deselection() {
        let dir = tempdir().expect("tempdir");
        let (app, _) = app_with_two_source_services(dir.path());

        let scans = selected_scans(&app);

        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].services.len(), 1);
        assert_eq!(scans[0].services[0].name, "memory");
    }

    #[test]
    fn danger_plan_rewrites_only_step2_selected_services() {
        let dir = tempdir().expect("tempdir");
        let (app, _) = app_with_two_source_services(dir.path());

        let scans = selected_danger_scans(&app);
        let plan = plan_danger_rewrite_for_scans(&scans, "rmcp-mux-proxy", &[], dir.path());

        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0];
        assert!(matches!(action.status, DangerStatus::Planned));
        assert_eq!(action.existing_services.len(), 1);
        assert_eq!(action.existing_services[0].name, "memory");

        let rewritten: serde_json::Value =
            serde_json::from_str(action.new_contents.as_ref().expect("contents"))
                .expect("rewritten json");
        let servers = rewritten
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .expect("mcpServers");
        assert_eq!(
            servers
                .get("memory")
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("rmcp-mux-proxy")
        );
        assert_eq!(
            servers
                .get("brave")
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("npx"),
            "deselected servers must stay in the source file untouched"
        );
    }

    #[test]
    fn danger_with_no_sources_is_noop() {
        let dir = tempdir().expect("tempdir");
        let app = make_app(dir.path());
        let summary = run_danger_auto_configure(&app).expect("danger empty");
        assert!(
            summary.contains("eligible") || summary.contains("nothing"),
            "summary: {summary}"
        );
    }

    #[test]
    fn start_tray_daemon_dry_run_short_circuits() {
        let dir = tempdir().expect("tempdir");
        let mut app = make_app(dir.path());
        app.dry_run = true;
        let summary = start_tray_daemon(&app).expect("tray dry");
        assert!(summary.contains("dry-run"));
    }
}
