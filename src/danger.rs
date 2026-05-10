//! `[DANGER]` automatic client configuration.
//!
//! This module rewrites the user's *existing* MCP client configs so each
//! known server starts `rust-mux-proxy` instead of the upstream command.
//! It does so in a backup-first, preview-first, explicit-confirmation-only
//! way:
//!
//! 1. `plan_danger_rewrite` inspects every eligible source, computes the
//!    change, and assembles a [`DangerPlan`] with one [`DangerAction`] per
//!    file. Sources that fail to parse are recorded as actions with
//!    `status = SkippedInvalid` — they are *never* mutated.
//! 2. `format_preview` turns the plan into human-readable text the wizard
//!    shows before any disk write.
//! 3. `execute_plan` is the only function that touches disk: it requires
//!    `confirmed = true`, copies each target file to a timestamped
//!    `<file>.<unix_seconds>.bak`, then writes the rewritten content. If
//!    the timestamped backup already exists it falls back to a
//!    `<file>.<unix_seconds>-<n>.bak` form so two runs in the same second
//!    never collide.
//!
//! JSON rules: only the `mcpServers` (or `servers`) entries we actually
//! discovered are replaced. All other top-level keys are preserved exactly.
//!
//! TOML rules: only the `mcp_servers` table is replaced. All other tables
//! are preserved at the value level. Comments and key order are not
//! preserved — the preview always says so and the backup is the source of
//! truth for rollback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::config::safe_read_to_string;
use crate::runtime::atomic_write;
use crate::scan::{
    ConfigSchema, HostFile, HostFormat, HostKind, HostService, ScanResult, scan_host_file,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum DangerStatus {
    /// Source parses cleanly and a rewrite has been planned.
    Planned,
    /// Source could not be parsed; rewrite is skipped (file untouched).
    SkippedInvalid,
    /// Source is ineligible for the danger flow (e.g. Gemini settings).
    SkippedIneligible,
    /// No services to rewrite for this source.
    SkippedEmpty,
}

#[derive(Debug, Clone, Serialize)]
pub struct DangerAction {
    pub source: HostFile,
    pub status: DangerStatus,
    /// Rewritten file contents. `None` when status != Planned.
    pub new_contents: Option<String>,
    /// One-line per-server change summary lines (for preview).
    pub change_lines: Vec<String>,
    /// Reason the source was skipped, when applicable.
    pub skip_reason: Option<String>,
    /// Existing services we were about to rewrite (for preview).
    pub existing_services: Vec<HostService>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DangerPlan {
    pub actions: Vec<DangerAction>,
    pub proxy_cmd: String,
    pub proxy_args: Vec<String>,
    pub socket_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct DangerExecutionOutcome {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub written: bool,
    pub status: DangerStatus,
    pub error: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Plan
// ─────────────────────────────────────────────────────────────────────────────

/// Plan a danger rewrite for the given list of eligible sources. Sources
/// flagged as `eligible_for_danger = false` are recorded as
/// `SkippedIneligible` and never planned for mutation.
pub fn plan_danger_rewrite(
    sources: &[HostFile],
    proxy_cmd: &str,
    proxy_args: &[String],
    socket_dir: &Path,
) -> DangerPlan {
    let mut actions = Vec::with_capacity(sources.len());
    for source in sources {
        if !source.eligible_for_danger {
            actions.push(DangerAction {
                source: source.clone(),
                status: DangerStatus::SkippedIneligible,
                new_contents: None,
                change_lines: Vec::new(),
                skip_reason: Some(format!(
                    "{} has no verified strict-config flag; use the safe path instructions instead",
                    source.kind.display_name()
                )),
                existing_services: Vec::new(),
            });
            continue;
        }

        let scan = match scan_host_file(source) {
            Ok(s) => s,
            Err(err) => {
                actions.push(DangerAction {
                    source: source.clone(),
                    status: DangerStatus::SkippedInvalid,
                    new_contents: None,
                    change_lines: Vec::new(),
                    skip_reason: Some(format!("parse error: {err}")),
                    existing_services: Vec::new(),
                });
                continue;
            }
        };

        if scan.services.is_empty() {
            push_empty_action(&mut actions, source.clone());
            continue;
        }

        push_rewrite_action(&mut actions, scan, proxy_cmd, proxy_args, socket_dir);
    }

    DangerPlan {
        actions,
        proxy_cmd: proxy_cmd.to_string(),
        proxy_args: proxy_args.to_vec(),
        socket_dir: socket_dir.to_path_buf(),
    }
}

/// Plan danger rewrites from already-scanned sources.
///
/// The wizard uses this after STEP 2 so operator deselections are preserved:
/// callers can filter `scan.services` before planning, while unknown or
/// unselected entries in the original file remain untouched by the rewriter.
pub fn plan_danger_rewrite_for_scans(
    scans: &[ScanResult],
    proxy_cmd: &str,
    proxy_args: &[String],
    socket_dir: &Path,
) -> DangerPlan {
    let mut actions = Vec::with_capacity(scans.len());
    for scan in scans {
        let source = &scan.host;
        if !source.eligible_for_danger {
            actions.push(DangerAction {
                source: source.clone(),
                status: DangerStatus::SkippedIneligible,
                new_contents: None,
                change_lines: Vec::new(),
                skip_reason: Some(format!(
                    "{} has no verified strict-config flag; use the safe path instructions instead",
                    source.kind.display_name()
                )),
                existing_services: scan.services.clone(),
            });
            continue;
        }

        if scan.services.is_empty() {
            push_empty_action(&mut actions, source.clone());
            continue;
        }

        push_rewrite_action(
            &mut actions,
            scan.clone(),
            proxy_cmd,
            proxy_args,
            socket_dir,
        );
    }

    DangerPlan {
        actions,
        proxy_cmd: proxy_cmd.to_string(),
        proxy_args: proxy_args.to_vec(),
        socket_dir: socket_dir.to_path_buf(),
    }
}

fn push_empty_action(actions: &mut Vec<DangerAction>, source: HostFile) {
    actions.push(DangerAction {
        source,
        status: DangerStatus::SkippedEmpty,
        new_contents: None,
        change_lines: Vec::new(),
        skip_reason: Some(
            "no selected MCP servers found in this config; nothing to rewrite".into(),
        ),
        existing_services: Vec::new(),
    });
}

fn push_rewrite_action(
    actions: &mut Vec<DangerAction>,
    scan: ScanResult,
    proxy_cmd: &str,
    proxy_args: &[String],
    socket_dir: &Path,
) {
    let source = &scan.host;
    let original = match safe_read_to_string(&source.path) {
        Ok(s) => s,
        Err(err) => {
            actions.push(DangerAction {
                source: source.clone(),
                status: DangerStatus::SkippedInvalid,
                new_contents: None,
                change_lines: Vec::new(),
                skip_reason: Some(format!("read error: {err}")),
                existing_services: Vec::new(),
            });
            return;
        }
    };

    let rewrite = match source.format {
        HostFormat::Json => rewrite_json_keep_other_keys(
            &original,
            &source.path,
            source.schema,
            &scan.services,
            proxy_cmd,
            proxy_args,
            socket_dir,
        ),
        HostFormat::Toml => rewrite_toml_keep_other_tables(
            &original,
            &source.path,
            source.kind,
            &scan.services,
            proxy_cmd,
            proxy_args,
            socket_dir,
        ),
    };

    match rewrite {
        Ok((new_contents, change_lines)) => actions.push(DangerAction {
            source: source.clone(),
            status: DangerStatus::Planned,
            new_contents: Some(new_contents),
            change_lines,
            skip_reason: None,
            existing_services: scan.services,
        }),
        Err(err) => actions.push(DangerAction {
            source: source.clone(),
            status: DangerStatus::SkippedInvalid,
            new_contents: None,
            change_lines: Vec::new(),
            skip_reason: Some(format!("rewrite failed: {err}")),
            existing_services: scan.services,
        }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Preview
// ─────────────────────────────────────────────────────────────────────────────

/// Render a human-readable preview of the plan. The wizard shows this before
/// asking for confirmation; nothing has been written to disk yet.
pub fn format_preview(plan: &DangerPlan) -> String {
    let mut out = String::new();
    out.push_str("⚠️  [DANGER] Automatic client configuration plan\n");
    out.push_str("====================================================\n");
    out.push_str(&format!("Proxy command : {}\n", plan.proxy_cmd));
    if !plan.proxy_args.is_empty() {
        out.push_str(&format!("Proxy pre-args: {}\n", plan.proxy_args.join(" ")));
    }
    out.push_str(&format!(
        "Socket dir    : {}\n\n",
        plan.socket_dir.display()
    ));

    let planned: Vec<&DangerAction> = plan
        .actions
        .iter()
        .filter(|a| a.status == DangerStatus::Planned)
        .collect();
    let skipped: Vec<&DangerAction> = plan
        .actions
        .iter()
        .filter(|a| a.status != DangerStatus::Planned)
        .collect();

    out.push_str(&format!("Planned changes ({} file(s)):\n", planned.len()));
    for action in &planned {
        out.push_str(&format!(
            "  • {} ({} format, {} services)\n",
            action.source.path.display(),
            match action.source.format {
                HostFormat::Json => "JSON",
                HostFormat::Toml => "TOML",
            },
            action.existing_services.len()
        ));
        for change in &action.change_lines {
            out.push_str(&format!("      - {change}\n"));
        }
        out.push_str(&format!(
            "      backup target: {}.<unix_seconds>.bak\n",
            action.source.path.display()
        ));
    }

    if !skipped.is_empty() {
        out.push_str(&format!("\nSkipped ({}):\n", skipped.len()));
        for action in skipped {
            let reason = action.skip_reason.as_deref().unwrap_or("(unspecified)");
            out.push_str(&format!(
                "  • {}: {} ({})\n",
                action.source.path.display(),
                short_status(&action.status),
                reason
            ));
        }
    }

    out.push_str("\nNotes:\n");
    out.push_str("  - Every modified file gets a timestamped .bak next to it before mutation.\n");
    out.push_str("  - TOML rewrites lose comments and key order; .bak preserves the original.\n");
    out.push_str("  - Files with parse errors are NEVER modified.\n");
    out.push_str("  - Unrelated config keys/tables are preserved.\n");
    out
}

fn short_status(status: &DangerStatus) -> &'static str {
    match status {
        DangerStatus::Planned => "planned",
        DangerStatus::SkippedInvalid => "invalid",
        DangerStatus::SkippedIneligible => "ineligible",
        DangerStatus::SkippedEmpty => "empty",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

/// Execute the plan. Requires `confirmed = true`; without it the call is a
/// no-op that returns an explicit error so accidental dispatches cannot
/// silently mutate user files.
pub fn execute_plan(plan: &DangerPlan, confirmed: bool) -> Result<Vec<DangerExecutionOutcome>> {
    if !confirmed {
        return Err(anyhow!(
            "danger plan execution refused: explicit confirmation required"
        ));
    }
    let mut outcomes = Vec::with_capacity(plan.actions.len());
    for action in &plan.actions {
        match (&action.status, action.new_contents.as_deref()) {
            (DangerStatus::Planned, Some(new_contents)) => {
                let res = write_with_timestamped_backup(&action.source.path, new_contents);
                match res {
                    Ok(backup) => outcomes.push(DangerExecutionOutcome {
                        path: action.source.path.clone(),
                        backup: Some(backup),
                        written: true,
                        status: DangerStatus::Planned,
                        error: None,
                    }),
                    Err(err) => outcomes.push(DangerExecutionOutcome {
                        path: action.source.path.clone(),
                        backup: None,
                        written: false,
                        status: DangerStatus::SkippedInvalid,
                        error: Some(err.to_string()),
                    }),
                }
            }
            _ => {
                outcomes.push(DangerExecutionOutcome {
                    path: action.source.path.clone(),
                    backup: None,
                    written: false,
                    status: action.status.clone(),
                    error: action.skip_reason.clone(),
                });
            }
        }
    }
    Ok(outcomes)
}

/// Rollback hint lines for the operator: the exact `cp` commands to restore
/// each backed-up file to its previous state.
pub fn rollback_commands(outcomes: &[DangerExecutionOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter_map(|o| {
            o.backup
                .as_ref()
                .map(|b| format!("cp -p {} {}", shell_quote(b), shell_quote(&o.path)))
        })
        .collect()
}

fn shell_quote(p: &Path) -> String {
    let s = p.display().to_string();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~'))
    {
        s
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn write_with_timestamped_backup(path: &Path, contents: &str) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut backup = backup_path(path, secs, None);
    let mut counter = 1u32;
    while backup.exists() {
        backup = backup_path(path, secs, Some(counter));
        counter += 1;
        if counter > 1000 {
            return Err(anyhow!(
                "could not find unique backup path for {}",
                path.display()
            ));
        }
    }

    if path.exists() {
        let current = safe_read_to_string(path)?;
        atomic_write(&backup, current.as_bytes())
            .with_context(|| format!("failed to create backup {}", backup.display()))?;
    }
    atomic_write(path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(backup)
}

fn backup_path(path: &Path, secs: u64, suffix: Option<u32>) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "danger".to_string());
    match suffix {
        Some(n) => name.push_str(&format!(".{secs}-{n}.bak")),
        None => name.push_str(&format!(".{secs}.bak")),
    }
    let mut p = path.to_path_buf();
    p.set_file_name(name);
    p
}

// ─────────────────────────────────────────────────────────────────────────────
// Format-specific surgical rewrites
// ─────────────────────────────────────────────────────────────────────────────

fn rewrite_json_keep_other_keys(
    original: &str,
    path: &Path,
    schema: ConfigSchema,
    services: &[HostService],
    proxy_cmd: &str,
    proxy_args: &[String],
    socket_dir: &Path,
) -> Result<(String, Vec<String>)> {
    let mut root: serde_json::Value = serde_json::from_str(original)
        .with_context(|| format!("failed to parse json {}", path.display()))?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: top-level JSON must be an object", path.display()))?;

    // Determine which key to update: schema's stated key, or whichever exists.
    let key = match schema {
        ConfigSchema::McpServersJson => "mcpServers",
        ConfigSchema::ServersJson => "servers",
        ConfigSchema::AutoJson => {
            if obj.contains_key("mcpServers") {
                "mcpServers"
            } else if obj.contains_key("servers") {
                "servers"
            } else {
                "mcpServers"
            }
        }
        ConfigSchema::McpServersToml => "mcpServers",
    };

    let existing = obj
        .get(key)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut change_lines = Vec::with_capacity(services.len());
    let mut new_map = serde_json::Map::new();

    // Preserve any entries that we did NOT discover as MCP-shaped (best-effort
    // defensive behaviour: don't drop unfamiliar keys under mcpServers).
    let discovered_names: std::collections::HashSet<&str> =
        services.iter().map(|s| s.name.as_str()).collect();
    for (name, value) in &existing {
        if !discovered_names.contains(name.as_str()) {
            new_map.insert(name.clone(), value.clone());
        }
    }

    for svc in services {
        let socket = svc.socket.clone().unwrap_or_else(|| {
            socket_dir
                .join(format!("{}.sock", svc.name))
                .to_string_lossy()
                .into_owned()
        });
        let mut args: Vec<String> = proxy_args.to_owned();
        args.push("--socket".to_string());
        args.push(socket);

        let mut server = serde_json::Map::new();
        server.insert(
            "command".to_string(),
            serde_json::Value::String(proxy_cmd.to_string()),
        );
        server.insert(
            "args".to_string(),
            serde_json::Value::Array(args.into_iter().map(serde_json::Value::String).collect()),
        );
        if let Some(env) = &svc.env {
            server.insert(
                "env".to_string(),
                serde_json::Value::Object(
                    env.iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect(),
                ),
            );
        }

        change_lines.push(format!(
            "rewrite `{}`: {} -> {}",
            svc.name, svc.command, proxy_cmd
        ));
        new_map.insert(svc.name.clone(), serde_json::Value::Object(server));
    }

    obj.insert(key.into(), serde_json::Value::Object(new_map));
    let serialized = serde_json::to_string_pretty(&root).context("serialize rewritten json")?;
    Ok((serialized, change_lines))
}

fn rewrite_toml_keep_other_tables(
    original: &str,
    path: &Path,
    kind: HostKind,
    services: &[HostService],
    proxy_cmd: &str,
    proxy_args: &[String],
    socket_dir: &Path,
) -> Result<(String, Vec<String>)> {
    let mut root: toml::Value = toml::from_str(original)
        .with_context(|| format!("failed to parse toml {}", path.display()))?;
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("{}: top-level TOML must be a table", path.display()))?;

    let target_key = match kind {
        HostKind::Codex => "mcp_servers",
        _ => "mcp_servers",
    };

    let existing = table
        .get(target_key)
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();

    let discovered_names: std::collections::HashSet<&str> =
        services.iter().map(|s| s.name.as_str()).collect();
    let mut new_table = toml::value::Table::new();
    for (name, value) in &existing {
        if !discovered_names.contains(name.as_str()) {
            new_table.insert(name.clone(), value.clone());
        }
    }

    let mut change_lines = Vec::with_capacity(services.len());
    for svc in services {
        let socket = svc.socket.clone().unwrap_or_else(|| {
            socket_dir
                .join(format!("{}.sock", svc.name))
                .to_string_lossy()
                .into_owned()
        });
        let mut args: Vec<String> = proxy_args.to_owned();
        args.push("--socket".to_string());
        args.push(socket);

        let mut entry = toml::value::Table::new();
        entry.insert("command".into(), toml::Value::String(proxy_cmd.to_string()));
        entry.insert(
            "args".into(),
            toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
        );
        if let Some(env) = &svc.env {
            let env_map: HashMap<String, toml::Value> = env
                .iter()
                .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                .collect();
            let mut env_table = toml::value::Table::new();
            for (k, v) in env_map {
                env_table.insert(k, v);
            }
            entry.insert("env".into(), toml::Value::Table(env_table));
        }

        change_lines.push(format!(
            "rewrite `{}`: {} -> {}",
            svc.name, svc.command, proxy_cmd
        ));
        new_table.insert(svc.name.clone(), toml::Value::Table(entry));
    }

    table.insert(target_key.into(), toml::Value::Table(new_table));
    let serialized = toml::to_string_pretty(&root).context("serialize rewritten toml")?;
    Ok((serialized, change_lines))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat, HostKind};
    use tempfile::tempdir;

    fn write_text(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        atomic_write(path, body.as_bytes()).expect("write");
    }

    fn json_source(path: PathBuf, kind: HostKind) -> HostFile {
        HostFile {
            kind,
            path,
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        }
    }

    fn toml_source(path: PathBuf, kind: HostKind) -> HostFile {
        HostFile {
            kind,
            path,
            format: HostFormat::Toml,
            schema: ConfigSchema::McpServersToml,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        }
    }

    #[test]
    fn execute_refuses_without_confirmation() {
        let plan = DangerPlan {
            actions: Vec::new(),
            proxy_cmd: "rust-mux-proxy".into(),
            proxy_args: Vec::new(),
            socket_dir: PathBuf::from("/tmp"),
        };
        let res = execute_plan(&plan, false);
        assert!(
            res.is_err(),
            "execute_plan should refuse without confirmation"
        );
    }

    #[test]
    fn json_rewrite_creates_timestamped_backup_and_keeps_other_keys() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        write_text(
            &path,
            r#"{
              "trustedTools": ["one", "two"],
              "mcpServers": {
                "memory": {
                  "command": "npx",
                  "args": ["@modelcontextprotocol/server-memory"]
                }
              }
            }"#,
        );

        let plan = plan_danger_rewrite(
            &[json_source(path.clone(), HostKind::Claude)],
            "rust-mux-proxy",
            &[],
            Path::new("/tmp/sockets"),
        );
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].status, DangerStatus::Planned);

        let outcomes = execute_plan(&plan, true).expect("execute");
        let outcome = &outcomes[0];
        assert!(outcome.written, "json file should have been written");
        let backup = outcome.backup.as_ref().expect("backup path");
        assert!(backup.exists(), "backup must exist on disk");
        assert!(
            backup
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.ends_with(".bak"))
                .unwrap_or(false),
            "backup name should end with .bak: {:?}",
            backup
        );

        // After rewrite: top-level `trustedTools` preserved, mcpServers points to proxy.
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert!(
            updated.get("trustedTools").is_some(),
            "unrelated key dropped"
        );
        let mem = updated
            .get("mcpServers")
            .and_then(|v| v.get("memory"))
            .and_then(|v| v.as_object())
            .expect("memory entry");
        assert_eq!(
            mem.get("command").and_then(|v| v.as_str()),
            Some("rust-mux-proxy")
        );
    }

    #[test]
    fn toml_rewrite_creates_backup_and_keeps_other_tables() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        write_text(
            &path,
            r#"
            [history]
            persistence = "save-all"

            [mcp_servers.memory]
            command = "npx"
            args = ["@modelcontextprotocol/server-memory"]
            "#,
        );

        let plan = plan_danger_rewrite(
            &[toml_source(path.clone(), HostKind::Codex)],
            "rust-mux-proxy",
            &[],
            Path::new("/tmp/sockets"),
        );
        assert_eq!(plan.actions[0].status, DangerStatus::Planned);

        let outcomes = execute_plan(&plan, true).expect("execute");
        let outcome = &outcomes[0];
        assert!(outcome.backup.as_ref().expect("backup").exists());

        let updated: toml::Value =
            toml::from_str(&fs::read_to_string(&path).expect("read")).expect("parse toml");
        assert!(
            updated.get("history").is_some(),
            "unrelated [history] table dropped"
        );
        let mem = updated
            .get("mcp_servers")
            .and_then(|v| v.get("memory"))
            .expect("memory entry");
        assert_eq!(
            mem.get("command").and_then(|v| v.as_str()),
            Some("rust-mux-proxy")
        );
    }

    #[test]
    fn invalid_json_is_never_modified() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("broken.json");
        write_text(&path, "{ not valid json");
        let original = fs::read_to_string(&path).expect("read");

        let plan = plan_danger_rewrite(
            &[json_source(path.clone(), HostKind::Claude)],
            "rust-mux-proxy",
            &[],
            Path::new("/tmp/sockets"),
        );
        assert_eq!(plan.actions[0].status, DangerStatus::SkippedInvalid);

        let _ = execute_plan(&plan, true).expect("execute");
        let after = fs::read_to_string(&path).expect("read after");
        assert_eq!(original, after, "invalid file must be untouched");
    }

    #[test]
    fn ineligible_sources_are_recorded_but_not_planned() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_text(&path, r#"{"mcpServers": {"x": {"command": "npx"}}}"#);
        let mut source = json_source(path, HostKind::Gemini);
        source.eligible_for_danger = false;

        let plan = plan_danger_rewrite(&[source], "rust-mux-proxy", &[], Path::new("/tmp/sockets"));
        assert_eq!(plan.actions[0].status, DangerStatus::SkippedIneligible);
    }

    #[test]
    fn preview_mentions_backup_pattern_and_proxy() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        write_text(&path, r#"{"mcpServers": {"x": {"command": "npx"}}}"#);
        let plan = plan_danger_rewrite(
            &[json_source(path, HostKind::Claude)],
            "rust-mux-proxy",
            &[],
            Path::new("/tmp/sockets"),
        );
        let preview = format_preview(&plan);
        assert!(preview.contains("rust-mux-proxy"));
        assert!(preview.contains(".bak"));
        assert!(preview.contains("DANGER"));
    }

    #[test]
    fn rollback_commands_use_backup_paths() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        write_text(
            &path,
            r#"{"mcpServers": {"memory": {"command": "npx", "args": []}}}"#,
        );
        let plan = plan_danger_rewrite(
            &[json_source(path.clone(), HostKind::Claude)],
            "rust-mux-proxy",
            &[],
            Path::new("/tmp/sockets"),
        );
        let outcomes = execute_plan(&plan, true).expect("execute");
        let cmds = rollback_commands(&outcomes);
        assert_eq!(cmds.len(), 1);
        let backup = outcomes[0].backup.as_ref().expect("backup");
        assert!(
            cmds[0].contains(&backup.display().to_string())
                && cmds[0].contains(&path.display().to_string()),
            "rollback should reference both backup and target paths: {}",
            cmds[0]
        );
    }
}
