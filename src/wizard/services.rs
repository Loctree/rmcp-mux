//! Service loading, detection, and health check logic.
//!
//! v0.4.0 source-of-truth model:
//!
//! 1. **Discovery comes from client configs.** [`load_all_services`] runs
//!    [`crate::scan::scan_hosts`] across every well-known MCP client config
//!    (Claude / ClaudeDesktop / Codex / Junie / Gemini / Cursor / VSCode /
//!    JetBrains) and adds every server it finds, tagged with the originating
//!    [`ServiceSource::Client`].
//! 2. **ps-scan enrichment is optional and last** ([`enrich_running_state`]).
//!    It only sets the `pid` field on entries whose `(cmd, args)` match a
//!    running process; orphans (running but not in any config) are surfaced
//!    as `ServiceSource::DetectedRunning` so the operator can see them.
//!
//! The legacy ps-scan-as-source-of-truth path is gone. The hardcoded
//! `MCP_PATTERNS` whitelist below is used **only** by the enrichment helper
//! to bound the process scan; misses there mean a missed PID badge, never a
//! missing server entry.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::Command;

use crate::config::{ServerConfig, expand_path};
use crate::scan::{HostKind, HostService, ScanResult, merge_services};

use super::types::{HealthStatus, ServiceEntry, ServiceSource};

// ─────────────────────────────────────────────────────────────────────────────
// Service loading
// ─────────────────────────────────────────────────────────────────────────────

/// Build the wizard's list of `ServiceEntry` from a set of `ScanResult`s.
///
/// - Runs `merge_services` to dedup identical entries.
/// - Attributes each merged entry to its **first** originating client kind+path
///   (sources are ordered by `default_sources()` priority).
/// - Synthesises a `ServerConfig` with upstream command/args/env only. Socket
///   paths stay `None` unless the source explicitly supplied one; `mux_gen`
///   owns the mux socket path assignment under `~/.config/mux/sockets`.
///
/// Callers append to the result and run `enrich_running_state` afterwards
/// to stamp PIDs and surface ps-only orphans.
pub fn build_services_from_scans(scans: &[ScanResult]) -> Vec<ServiceEntry> {
    let merged = merge_services(scans);

    // Index from (cmd, args, env) -> (HostKind, path). Keeps the first source
    // encountered (default_sources is ordered by priority).
    let mut origin_index: HashMap<String, (HostKind, std::path::PathBuf)> = HashMap::new();
    for scan in scans {
        for svc in &scan.services {
            origin_index
                .entry(svc_key(svc))
                .or_insert_with(|| (scan.host.kind, scan.host.path.clone()));
        }
    }

    let mut out = Vec::with_capacity(merged.services.len());
    for svc in &merged.services {
        let origin = origin_index
            .get(&svc_key(svc))
            .cloned()
            .map(|(kind, path)| ServiceSource::Client { kind, path })
            .unwrap_or(ServiceSource::DetectedRunning);

        let config = ServerConfig {
            socket: svc.socket.clone(),
            cmd: Some(svc.command.clone()),
            args: Some(svc.args.clone()),
            cwd: svc.cwd.clone(),
            env: svc.env.clone(),
            max_active_clients: Some(5),
            tray: Some(false),
            service_name: Some(svc.name.clone()),
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
            // Off by default — see `mux_gen::DAEMON_CONFIG_BANNER` rationale.
            heartbeat_enabled: Some(false),
        };

        out.push(ServiceEntry {
            name: svc.name.clone(),
            config,
            health: HealthStatus::Unknown,
            source: origin,
            pid: None,
            selected: true,
        });
    }
    out
}

pub(super) fn append_default_discovered_services(services: &mut Vec<ServiceEntry>) {
    if let Some(server) = crate::scan::discover_vibecrafted_mcp() {
        let source = server.source;
        let svc = server.into_host_service();
        services.push(ServiceEntry {
            name: svc.name.clone(),
            config: ServerConfig {
                socket: svc.socket.clone(),
                cmd: Some(svc.command),
                args: Some(svc.args),
                cwd: svc.cwd,
                env: svc.env,
                max_active_clients: Some(5),
                tray: Some(false),
                service_name: Some(svc.name),
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
                // Off by default — see `mux_gen::DAEMON_CONFIG_BANNER` rationale.
                heartbeat_enabled: Some(false),
            },
            health: HealthStatus::Unknown,
            source: ServiceSource::Default { source },
            pid: None,
            selected: true,
        });
    }
}

fn svc_key(svc: &HostService) -> String {
    format!(
        "{}|{}|{}|{}",
        svc.command,
        svc.args.join(" "),
        svc.cwd.as_deref().unwrap_or(""),
        env_signature(svc.env.as_ref())
    )
}

fn env_signature(env: Option<&HashMap<String, String>>) -> String {
    let Some(env) = env else {
        return String::new();
    };
    let mut entries: Vec<(&String, &String)> = env.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

// ─────────────────────────────────────────────────────────────────────────────
// ps-scan enrichment (running-process awareness, *not* discovery)
// ─────────────────────────────────────────────────────────────────────────────

/// Patterns used to bound the ps-scan to plausible MCP processes. Misses
/// here only mean a missed PID badge — discovery itself is driven by
/// [`scan_hosts`] regardless of what is currently running.
const MCP_PATTERNS: &[&str] = &[
    "@modelcontextprotocol/",
    "mcp-server-",
    "-mcp-server",
    "mcp_server",
    "/mcp-",
    "-mcp/",
    "/loctree-mcp",
    "/aicx-mcp",
    "claude-mcp",
];

/// Stamp `pid` on every entry whose `(cmd, args)` matches a running process,
/// and append entries for processes that match an MCP heuristic but do not
/// match anything already in the list (`ServiceSource::DetectedRunning`).
pub fn enrich_running_state(services: &mut Vec<ServiceEntry>) {
    let running = list_running_mcp_processes();
    if running.is_empty() {
        return;
    }

    for proc in &running {
        // Try to match an existing entry by command+first-arg.
        let mut matched = false;
        for svc in services.iter_mut() {
            if proc_matches_entry(proc, svc) {
                svc.pid = Some(proc.pid);
                matched = true;
                break;
            }
        }
        if !matched {
            // Orphan: visible as a running MCP-shaped process but not in any
            // discovered config. Surface it so the operator can decide.
            services.push(ServiceEntry {
                name: proc.synthetic_name.clone(),
                config: ServerConfig {
                    socket: None,
                    cmd: Some(proc.cmd.clone()),
                    args: Some(proc.args.clone()),
                    cwd: None,
                    env: None,
                    max_active_clients: Some(5),
                    tray: Some(false),
                    service_name: Some(proc.synthetic_name.clone()),
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
                    // Off by default — see `mux_gen::DAEMON_CONFIG_BANNER` rationale.
                    heartbeat_enabled: Some(false),
                },
                health: HealthStatus::Healthy,
                source: ServiceSource::DetectedRunning,
                pid: Some(proc.pid),
                selected: false,
            });
        }
    }
}

#[derive(Debug, Clone)]
struct RunningMcpProcess {
    pid: u32,
    cmd: String,
    args: Vec<String>,
    synthetic_name: String,
}

fn list_running_mcp_processes() -> Vec<RunningMcpProcess> {
    let output = match Command::new("ps").args(["-eo", "pid,args"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let reader = BufReader::new(&output.stdout[..]);
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.starts_with("PID") {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() < 2 {
            continue;
        }
        let pid: u32 = match parts[0].trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let args = parts[1].trim();

        if !MCP_PATTERNS.iter().any(|p| args.contains(p)) {
            continue;
        }
        // Skip rmcp-mux itself, its proxy, and the legacy rmcp_mux binary names.
        if args.contains("rmcp-mux") || args.contains("rmcp_mux") {
            continue;
        }

        let name = extract_service_name(args);
        let unique = ensure_unique_name(&seen_names, name);
        seen_names.insert(unique.clone());

        let (cmd, cmd_args) = extract_cmd_and_args(args);
        out.push(RunningMcpProcess {
            pid,
            cmd,
            args: cmd_args,
            synthetic_name: unique,
        });
    }
    out
}

fn proc_matches_entry(proc: &RunningMcpProcess, svc: &ServiceEntry) -> bool {
    let svc_cmd = svc.config.cmd.as_deref().unwrap_or("");
    let svc_args = svc.config.args.as_deref().unwrap_or(&[]);
    if !cmds_equivalent(&proc.cmd, svc_cmd) {
        return false;
    }
    // Match if the running process args contain the first non-empty service arg
    // (heuristic — covers `npx -y @x/y` vs `npx @x/y`).
    if let Some(probe) = svc_args.iter().find(|a| !a.is_empty()) {
        return proc.args.iter().any(|a| a.contains(probe.as_str()));
    }
    true
}

fn cmds_equivalent(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_tail = a.rsplit('/').next().unwrap_or(a);
    let b_tail = b.rsplit('/').next().unwrap_or(b);
    a_tail == b_tail
}

fn ensure_unique_name(used: &std::collections::HashSet<String>, candidate: String) -> String {
    if !used.contains(&candidate) {
        return candidate;
    }
    let mut counter = 2usize;
    loop {
        let next = format!("{candidate}-{counter}");
        if !used.contains(&next) {
            return next;
        }
        counter += 1;
    }
}

fn extract_service_name(args: &str) -> String {
    if let Some(idx) = args.find("@modelcontextprotocol/") {
        let rest = &args[idx + "@modelcontextprotocol/".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !name.is_empty() {
            return name;
        }
    }
    if let Some(idx) = args.find("mcp-server-") {
        let rest = &args[idx + "mcp-server-".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !name.is_empty() {
            return format!("mcp-{}", name);
        }
    }
    if let Some(idx) = args.find("server-") {
        let rest = &args[idx + "server-".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !name.is_empty() {
            return name;
        }
    }
    "detected-mcp".into()
}

fn extract_cmd_and_args(args: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        return ("unknown".into(), vec![]);
    }
    let cmd = if parts[0].contains('/') {
        parts[0].rsplit('/').next().unwrap_or(parts[0]).to_string()
    } else {
        parts[0].to_string()
    };
    let cmd_args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    (cmd, cmd_args)
}

// ─────────────────────────────────────────────────────────────────────────────
// Health check
// ─────────────────────────────────────────────────────────────────────────────

pub fn check_health(config: &ServerConfig) -> HealthStatus {
    let socket_path = match &config.socket {
        Some(s) => expand_path(s),
        None => return HealthStatus::Unknown,
    };

    match UnixStream::connect(&socket_path) {
        Ok(_) => HealthStatus::Healthy,
        Err(_) => HealthStatus::Unhealthy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_from_merge_attribute_first_source_kind() {
        use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat};
        let host = HostFile {
            kind: HostKind::Claude,
            path: std::path::PathBuf::from("/tmp/test-claude.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        };
        let scan = ScanResult {
            host: host.clone(),
            services: vec![HostService {
                name: "memory".into(),
                command: "npx".into(),
                args: vec!["@modelcontextprotocol/server-memory".into()],
                cwd: None,
                socket: None,
                env: None,
                enabled: None,
            }],
        };
        let entries = build_services_from_scans(std::slice::from_ref(&scan));
        assert_eq!(entries.len(), 1);
        match &entries[0].source {
            ServiceSource::Client { kind, path } => {
                assert_eq!(*kind, HostKind::Claude);
                assert_eq!(path, &host.path);
            }
            other => panic!("expected Client origin, got {other:?}"),
        }
    }

    #[test]
    fn discovered_service_without_socket_leaves_mux_socket_to_generator() {
        use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat};
        let scan = ScanResult {
            host: HostFile {
                kind: HostKind::Claude,
                path: std::path::PathBuf::from("/tmp/test-claude.json"),
                format: HostFormat::Json,
                schema: ConfigSchema::McpServersJson,
                confidence: Confidence::High,
                writable: true,
                eligible_for_danger: true,
            },
            services: vec![HostService {
                name: "memory".into(),
                command: "npx".into(),
                args: vec!["@modelcontextprotocol/server-memory".into()],
                cwd: None,
                socket: None,
                env: None,
                enabled: None,
            }],
        };

        let entries = build_services_from_scans(&[scan]);

        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].config.socket.is_none(),
            "wizard discovery must not inject a fallback socket before mux_gen assigns ~/.config/mux/sockets"
        );
    }

    #[test]
    fn cmds_equivalent_matches_basename() {
        assert!(cmds_equivalent("/usr/local/bin/npx", "npx"));
        assert!(cmds_equivalent("npx", "npx"));
        assert!(!cmds_equivalent("npx", "node"));
    }
}
