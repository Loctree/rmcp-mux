use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::config::{Config, ServerConfig, expand_path, safe_read_to_string};
use crate::runtime::atomic_write;

// ─────────────────────────────────────────────────────────────────────────────
// CLI arg structs (CLI surface)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone)]
pub struct ScanArgs {
    /// Optional manifest output path.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Manifest format: toml|json|yaml
    #[arg(long, default_value = "toml")]
    pub manifest_format: String,
    /// Optional snippet output path (per-host a suffix is added).
    #[arg(long)]
    pub snippet: Option<PathBuf>,
    /// Snippet format: toml|json|yaml
    #[arg(long, default_value = "toml")]
    pub snippet_format: String,
    /// Socket directory for generated services.
    #[arg(long, default_value = "~/.rmcp-servers/rmcp-mux/sockets")]
    pub socket_dir: String,
    /// Do not write files; print to stdout.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RewireArgs {
    /// Explicit path to host config; otherwise auto-discovery is used.
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Host kind to target (codex|claude|claude-desktop|junie|gemini|cursor|vscode|jetbrains).
    #[arg(long)]
    pub host: Option<String>,
    /// Socket directory used for proxy args.
    #[arg(long, default_value = "~/.rmcp-servers/rmcp-mux/sockets")]
    pub socket_dir: String,
    /// Proxy command used in rewritten config.
    #[arg(long, default_value = "rmcp-mux-proxy")]
    pub proxy_cmd: String,
    /// Extra args passed before --socket.
    #[arg(long, value_delimiter = ' ')]
    pub proxy_args: Vec<String>,
    /// Only show planned changes.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// Explicit host config path.
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Host kind (codex|claude|claude-desktop|junie|gemini|cursor|vscode|jetbrains).
    #[arg(long)]
    pub host: Option<String>,
    /// Expected proxy command (default rmcp-mux-proxy).
    #[arg(long, default_value = "rmcp-mux-proxy")]
    pub proxy_cmd: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Core type model: kinds, formats, schemas, confidence, sources
// ─────────────────────────────────────────────────────────────────────────────

/// Kind of MCP client whose config we are looking at.
///
/// Reflects the real-world client landscape: Claude has a Code config and
/// a Desktop config that live in different places, Junie ships several
/// possible config locations, Gemini is best driven by its own CLI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HostKind {
    Codex,
    Claude,
    ClaudeDesktop,
    Junie,
    Gemini,
    Cursor,
    VSCode,
    JetBrains,
    /// User-provided custom config path.
    Custom,
    /// Unknown / legacy bucket.
    Unknown,
}

impl HostKind {
    pub fn as_label(&self) -> &'static str {
        match self {
            HostKind::Codex => "codex",
            HostKind::Claude => "claude",
            HostKind::ClaudeDesktop => "claude-desktop",
            HostKind::Junie => "junie",
            HostKind::Gemini => "gemini",
            HostKind::Cursor => "cursor",
            HostKind::VSCode => "vscode",
            HostKind::JetBrains => "jetbrains",
            HostKind::Custom => "custom",
            HostKind::Unknown => "unknown",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            HostKind::Codex => "Codex CLI",
            HostKind::Claude => "Claude Code",
            HostKind::ClaudeDesktop => "Claude Desktop",
            HostKind::Junie => "Junie",
            HostKind::Gemini => "Gemini CLI",
            HostKind::Cursor => "Cursor",
            HostKind::VSCode => "VS Code",
            HostKind::JetBrains => "JetBrains IDEs",
            HostKind::Custom => "Custom",
            HostKind::Unknown => "Unknown",
        }
    }
}

/// Wire format used by the host config file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HostFormat {
    Toml,
    Json,
}

/// Logical schema inside the host config, independent of file format.
///
/// Different MCP clients store their server map under different keys:
/// `mcpServers` (Claude / Junie / Cursor / VSCode / JetBrains JSON),
/// `servers` (Gemini settings, generic), or `[mcp_servers.<name>]`
/// (Codex TOML, snake_case).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfigSchema {
    /// JSON object with top-level `mcpServers` map.
    McpServersJson,
    /// JSON object with top-level `servers` map.
    ServersJson,
    /// TOML with `[mcp_servers.<name>]` tables (snake_case key).
    McpServersToml,
    /// JSON: try `mcpServers` first, then `servers`. Used for generic / unknown JSON files.
    AutoJson,
}

/// How sure we are that this path actually represents a real client config.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// A single discovered or user-provided client config source.
#[derive(Debug, Clone, Serialize)]
pub struct HostFile {
    pub kind: HostKind,
    pub path: PathBuf,
    pub format: HostFormat,
    /// Logical key/shape inside the file.
    pub schema: ConfigSchema,
    pub confidence: Confidence,
    /// Writable safely if the user opts in to the danger flow.
    pub writable: bool,
    /// Eligible for the [DANGER] auto-rewrite flow.
    /// Some clients (e.g. Gemini) lack a robust strict-config flag and
    /// prefer being driven through their own CLI rather than file rewrite.
    pub eligible_for_danger: bool,
}

/// One MCP server discovered inside a client config.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HostService {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub socket: Option<String>,
    pub env: Option<HashMap<String, String>>,
    /// Optional `enabled` flag if the client's schema exposes one.
    pub enabled: Option<bool>,
}

/// Parsed result of one config source.
#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub host: HostFile,
    pub services: Vec<HostService>,
}

/// Result of merging all discovered services across sources, including
/// any conflicts where the same server name has divergent commands or env.
#[derive(Debug, Clone, Serialize)]
pub struct MergeOutcome {
    pub services: Vec<HostService>,
    pub conflicts: Vec<ConflictReport>,
}

/// A conflict where the same `name` appears with different `command`/`args`/`env`
/// across two or more sources.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictReport {
    pub name: String,
    pub variants: Vec<ConflictVariant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConflictVariant {
    pub source_path: PathBuf,
    pub source_kind: HostKind,
    pub service: HostService,
}

/// Outcome from rewriting a single host config.
#[derive(Debug, Clone, Serialize)]
pub struct RewireOutcome {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub written: bool,
}

/// Built-in MCP server source discovered outside a client config file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DefaultServerSource {
    VibecraftedMcp,
}

/// A first-class MCP server candidate surfaced by the wizard before any
/// operator-owned client config is rewired.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveredMcpServer {
    pub name: String,
    pub source: DefaultServerSource,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
}

impl DiscoveredMcpServer {
    pub fn into_host_service(self) -> HostService {
        HostService {
            name: self.name,
            command: self.command,
            args: self.args,
            cwd: self.cwd.map(|p| p.to_string_lossy().into_owned()),
            socket: None,
            env: self.env,
            enabled: Some(true),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Default sources (canonical list per real-world MCP clients)
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical list of well-known MCP client config sources, exactly as we
/// document them.
///
/// These are *candidates*: the caller filters down to those that exist on
/// disk via [`discover_hosts`].
pub fn default_sources() -> Vec<HostFile> {
    vec![
        // ───── Claude Code (global JSON config) ─────
        HostFile {
            kind: HostKind::Claude,
            path: expand_path("~/.claude.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        },
        // ───── Claude Desktop (macOS) ─────
        HostFile {
            kind: HostKind::ClaudeDesktop,
            path: expand_path("~/Library/Application Support/Claude/claude_desktop_config.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        },
        // ───── Codex CLI (TOML, snake_case mcp_servers) ─────
        HostFile {
            kind: HostKind::Codex,
            path: expand_path("~/.codex/config.toml"),
            format: HostFormat::Toml,
            schema: ConfigSchema::McpServersToml,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        },
        // ───── Junie (high-confidence path) ─────
        HostFile {
            kind: HostKind::Junie,
            path: expand_path("~/.junie/mcp/mcp.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        },
        // ───── Junie (generic agent path) ─────
        HostFile {
            kind: HostKind::Junie,
            path: expand_path("~/.agents/mcp.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        HostFile {
            kind: HostKind::Junie,
            path: expand_path("~/.ai/mcp.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        // ───── Gemini ─────
        // Gemini exposes `gemini mcp list/add/remove/enable/disable`. There's
        // no observed Claude-style strict config flag, so we discover the
        // settings file but mark it as ineligible for the danger rewrite by
        // default. The wizard prefers generated `gemini mcp add ...` commands.
        HostFile {
            kind: HostKind::Gemini,
            path: expand_path("~/.gemini/settings.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: false,
        },
        // ───── Legacy editor hosts (kept for back-compat with `scan` CLI) ─────
        HostFile {
            kind: HostKind::Cursor,
            path: expand_path("~/Library/Application Support/Cursor/User/settings.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        HostFile {
            kind: HostKind::Cursor,
            path: expand_path("~/.config/Cursor/User/settings.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        HostFile {
            kind: HostKind::VSCode,
            path: expand_path("~/Library/Application Support/Code/User/settings.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        HostFile {
            kind: HostKind::VSCode,
            path: expand_path("~/.config/Code/User/settings.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
        HostFile {
            kind: HostKind::JetBrains,
            path: expand_path("~/Library/Application Support/JetBrains/LLM/mcp.json"),
            format: HostFormat::Json,
            schema: ConfigSchema::AutoJson,
            confidence: Confidence::Medium,
            writable: true,
            eligible_for_danger: true,
        },
    ]
}

/// Discover the local Vibecrafted MCP server through the two supported paths:
/// the framework dev checkout, or an installed `pip show vibecrafted-mcp`.
pub fn discover_vibecrafted_mcp() -> Option<DiscoveredMcpServer> {
    let dev_path = expand_path("~/Libraxis/vibecrafted/vibecrafted-mcp");
    discover_vibecrafted_mcp_with(&dev_path, pip_show_vibecrafted_mcp)
}

fn discover_vibecrafted_mcp_with(
    dev_path: &Path,
    pip_show: impl Fn() -> bool,
) -> Option<DiscoveredMcpServer> {
    if is_vibecrafted_mcp_project(dev_path) {
        return Some(DiscoveredMcpServer {
            name: "vibecrafted-mcp".into(),
            source: DefaultServerSource::VibecraftedMcp,
            command: "python".into(),
            args: vec!["-m".into(), "vibecrafted_mcp".into()],
            cwd: Some(dev_path.to_path_buf()),
            env: None,
        });
    }

    if pip_show() {
        return Some(DiscoveredMcpServer {
            name: "vibecrafted-mcp".into(),
            source: DefaultServerSource::VibecraftedMcp,
            command: "python".into(),
            args: vec!["-m".into(), "vibecrafted_mcp".into()],
            cwd: None,
            env: None,
        });
    }

    None
}

fn pip_show_vibecrafted_mcp() -> bool {
    Command::new("pip")
        .args(["show", "vibecrafted-mcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn is_vibecrafted_mcp_project(path: &Path) -> bool {
    pyproject_name(path)
        .as_deref()
        .is_some_and(|name| name == "vibecrafted-mcp")
}

fn pyproject_name(path: &Path) -> Option<String> {
    let pyproject = path.join("pyproject.toml");
    let data = fs::read_to_string(pyproject).ok()?;
    let root: toml::Value = toml::from_str(&data).ok()?;
    root.get("project")?
        .get("name")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Filter [`default_sources`] down to candidates that actually exist on disk.
pub fn discover_hosts() -> Vec<HostFile> {
    default_sources()
        .into_iter()
        .filter(|hf| hf.path.exists())
        .collect()
}

/// Build a custom [`HostFile`] from a user-provided path, inferring format
/// and schema from the extension. The danger-rewrite flag is set to `true`
/// because the user explicitly pointed us at this file.
pub fn host_file_from_custom_path(path: &Path) -> HostFile {
    let format = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("toml") => HostFormat::Toml,
        _ => HostFormat::Json,
    };
    let schema = match format {
        HostFormat::Toml => ConfigSchema::McpServersToml,
        HostFormat::Json => ConfigSchema::AutoJson,
    };
    HostFile {
        kind: HostKind::Custom,
        path: path.to_path_buf(),
        format,
        schema,
        confidence: Confidence::Medium,
        writable: true,
        eligible_for_danger: true,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-schema parser
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RawServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    socket: Option<String>,
    env: Option<HashMap<String, String>>,
    enabled: Option<bool>,
}

/// Parse a single host config file into a list of MCP services.
///
/// Dispatches on the file's [`HostFile::schema`]. If the schema is set to
/// [`ConfigSchema::AutoJson`] the parser tries `mcpServers` first, then
/// falls back to `servers` if the shape is clearly an MCP server map.
pub fn scan_host_file(file: &HostFile) -> Result<ScanResult> {
    let data = safe_read_to_string(&file.path)
        .with_context(|| format!("failed to read {}", file.path.display()))?;
    let services = match (file.format, file.schema) {
        (HostFormat::Json, ConfigSchema::McpServersJson) => {
            parse_json_servers_under_key(&data, &file.path, "mcpServers", true)?
        }
        (HostFormat::Json, ConfigSchema::ServersJson) => {
            parse_json_servers_under_key(&data, &file.path, "servers", true)?
        }
        (HostFormat::Json, ConfigSchema::AutoJson)
        | (HostFormat::Json, ConfigSchema::McpServersToml) => {
            // `McpServersToml` with JSON format is nonsensical; treat as auto.
            let primary = parse_json_servers_under_key(&data, &file.path, "mcpServers", false)?;
            if !primary.is_empty() {
                primary
            } else {
                parse_json_servers_under_key(&data, &file.path, "servers", false)?
            }
        }
        (HostFormat::Toml, ConfigSchema::McpServersToml)
        | (HostFormat::Toml, ConfigSchema::AutoJson) => parse_toml_mcp_servers(&data, &file.path)?,
        (HostFormat::Toml, ConfigSchema::McpServersJson)
        | (HostFormat::Toml, ConfigSchema::ServersJson) => {
            // Same misalignment guard: TOML never holds JSON-shape `mcpServers`.
            parse_toml_mcp_servers(&data, &file.path)?
        }
    };

    Ok(ScanResult {
        host: file.clone(),
        services,
    })
}

fn parse_json_servers_under_key(
    data: &str,
    path: &Path,
    key: &str,
    require_present: bool,
) -> Result<Vec<HostService>> {
    let root: serde_json::Value = serde_json::from_str(data)
        .with_context(|| format!("failed to parse json {}", path.display()))?;
    let map = match root.get(key) {
        Some(serde_json::Value::Object(m)) => m,
        Some(_) => return Err(anyhow!("`{}` is not an object in {}", key, path.display())),
        None if require_present => {
            return Err(anyhow!("missing `{}` in {}", key, path.display()));
        }
        None => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for (name, raw) in map {
        // Skip entries that aren't MCP-shaped (e.g. ide settings under `servers`)
        let raw: RawServer = match serde_json::from_value::<RawServer>(raw.clone()) {
            Ok(r) if r.command.is_some() => r,
            _ => continue,
        };
        let Some(command) = raw.command else { continue };
        out.push(HostService {
            name: name.clone(),
            command,
            args: raw.args.unwrap_or_default(),
            cwd: raw.cwd,
            socket: raw.socket,
            env: raw.env,
            enabled: raw.enabled,
        });
    }
    Ok(out)
}

fn parse_toml_mcp_servers(data: &str, path: &Path) -> Result<Vec<HostService>> {
    let root: toml::Value =
        toml::from_str(data).with_context(|| format!("failed to parse toml {}", path.display()))?;
    // Codex TOML uses `[mcp_servers.<name>]` (snake_case).
    let table = match root.get("mcp_servers") {
        Some(toml::Value::Table(t)) => t.clone(),
        _ => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for (name, raw) in table {
        let raw_str =
            toml::to_string(&raw).with_context(|| format!("re-serialize toml entry {}", name))?;
        let parsed: RawServer = match toml::from_str::<RawServer>(&raw_str) {
            Ok(r) if r.command.is_some() => r,
            _ => continue,
        };
        let Some(command) = parsed.command else {
            continue;
        };
        out.push(HostService {
            name: name.clone(),
            command,
            args: parsed.args.unwrap_or_default(),
            cwd: parsed.cwd,
            socket: parsed.socket,
            env: parsed.env,
            enabled: parsed.enabled,
        });
    }
    Ok(out)
}

/// Scan all auto-discovered host config files. Failures are logged and the
/// failing source is skipped; callers see only successfully-parsed sources.
pub fn scan_hosts() -> Vec<ScanResult> {
    discover_hosts()
        .into_iter()
        .filter_map(|hf| match scan_host_file(&hf) {
            Ok(res) => Some(res),
            Err(err) => {
                tracing::warn!("failed to scan {}: {err}", hf.path.display());
                None
            }
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Dedup + conflict detection
// ─────────────────────────────────────────────────────────────────────────────

/// Merge services across sources, deduplicating identical entries by
/// `(name, command, args, env)` and surfacing conflicts where the same
/// name has different command/args/env.
///
/// Conflicting servers are renamed with deterministic `-from-<source>`
/// suffixes so both variants survive the merge.
pub fn merge_services(scans: &[ScanResult]) -> MergeOutcome {
    // group by name -> Vec<(source, service)>
    let mut by_name: BTreeMap<String, Vec<(HostFile, HostService)>> = BTreeMap::new();
    for scan in scans {
        for svc in &scan.services {
            by_name
                .entry(svc.name.clone())
                .or_default()
                .push((scan.host.clone(), svc.clone()));
        }
    }

    let mut services = Vec::new();
    let mut conflicts = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (name, mut variants) in by_name {
        // Identical entries (same command/args/env) collapse into one.
        let mut unique: Vec<(HostFile, HostService)> = Vec::new();
        for (src, svc) in variants.drain(..) {
            if !unique
                .iter()
                .any(|(_, existing)| services_equivalent(existing, &svc))
            {
                unique.push((src, svc));
            }
        }

        if unique.len() == 1 {
            let (_src, svc) = unique.into_iter().next().expect("len 1");
            used_names.insert(name.clone());
            services.push(svc);
            continue;
        }

        // Multiple distinct variants -> conflict. Surface every variant with
        // a deterministic suffix so the operator can keep both.
        let report = ConflictReport {
            name: name.clone(),
            variants: unique
                .iter()
                .map(|(src, svc)| ConflictVariant {
                    source_path: src.path.clone(),
                    source_kind: src.kind,
                    service: svc.clone(),
                })
                .collect(),
        };
        conflicts.push(report);

        for (src, svc) in unique {
            let candidate = format!("{}-from-{}", name, src.kind.as_label());
            let unique_name = ensure_unique(&used_names, candidate);
            used_names.insert(unique_name.clone());
            let mut renamed = svc;
            renamed.name = unique_name;
            services.push(renamed);
        }
    }

    MergeOutcome {
        services,
        conflicts,
    }
}

fn services_equivalent(a: &HostService, b: &HostService) -> bool {
    a.command == b.command
        && a.args == b.args
        && a.cwd == b.cwd
        && env_equal(a.env.as_ref(), b.env.as_ref())
        && a.socket == b.socket
}

fn env_equal(a: Option<&HashMap<String, String>>, b: Option<&HashMap<String, String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(am), Some(bm)) => am == bm,
        _ => false,
    }
}

fn ensure_unique(used: &std::collections::HashSet<String>, candidate: String) -> String {
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

// ─────────────────────────────────────────────────────────────────────────────
// Manifest + snippet helpers (used by the legacy `scan` and `rewire` CLIs)
// ─────────────────────────────────────────────────────────────────────────────

pub fn format_for_host(host: &HostFile) -> &'static str {
    match host.format {
        HostFormat::Json => "json",
        HostFormat::Toml => "toml",
    }
}

pub fn build_manifest(scans: &[ScanResult], socket_dir: &Path) -> Config {
    let merged = merge_services(scans);
    let mut cfg = Config::default();
    for svc in merged.services {
        let socket = svc.socket.clone().unwrap_or_else(|| {
            socket_dir
                .join(format!("{}.sock", svc.name))
                .to_string_lossy()
                .to_string()
        });
        cfg.servers.insert(
            svc.name.clone(),
            ServerConfig {
                socket: Some(socket),
                cmd: Some(svc.command.clone()),
                args: Some(svc.args.clone()),
                cwd: svc.cwd.clone(),
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
                env: svc.env.clone(),
                heartbeat_interval_ms: Some(30_000),
                heartbeat_timeout_ms: Some(30_000),
                heartbeat_max_failures: Some(3),
                // Off by default: most MCP servers (npx wrappers, custom stdio
                // binaries) do not implement rmcp-mux ping/pong probes, so a
                // global `true` triggers endless restart loops. Operators flip
                // this to `true` per-service for backends that explicitly
                // support heartbeat.
                heartbeat_enabled: Some(false),
            },
        );
    }
    cfg
}

pub fn generate_snippet(
    scans: &[ScanResult],
    socket_dir: &Path,
    proxy_cmd: &str,
    proxy_args: &[String],
) -> HashMap<HostKind, serde_json::Value> {
    let merged = merge_services(scans);
    let mut servers = serde_json::Map::new();
    for svc in &merged.services {
        let socket = svc.socket.clone().unwrap_or_else(|| {
            socket_dir
                .join(format!("{}.sock", svc.name))
                .to_string_lossy()
                .to_string()
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
        servers.insert(svc.name.clone(), serde_json::Value::Object(server));
    }

    let mut root = serde_json::Map::new();
    root.insert("mcpServers".to_string(), serde_json::Value::Object(servers));
    let snippet_value = serde_json::Value::Object(root);

    let mut snippets = HashMap::new();
    for scan in scans {
        snippets.insert(scan.host.kind, snippet_value.clone());
    }
    snippets
}

// ─────────────────────────────────────────────────────────────────────────────
// Legacy single-host rewrite (used by `rmcp-mux rewire` CLI)
//
// Note: the *wizard* uses the safer flow defined in `crate::danger`, which
// always takes timestamped backups, previews changes, and refuses to touch
// invalid files. This function is preserved for the CLI subcommand and for
// callers that already opted in via explicit `--path`.
// ─────────────────────────────────────────────────────────────────────────────

pub fn rewire_host(
    host: &HostFile,
    socket_dir: &Path,
    proxy_cmd: &str,
    proxy_args: &[String],
    dry_run: bool,
) -> Result<RewireOutcome> {
    let scan = scan_host_file(host)?;
    let snippets = generate_snippet(&[scan], socket_dir, proxy_cmd, proxy_args);
    let snippet = snippets
        .get(&host.kind)
        .ok_or_else(|| anyhow!("no snippet generated for host"))?;
    let format = format_for_host(host);
    let snippet_text = serialize_snippet(snippet, format)?;
    let data = safe_read_to_string(&host.path)
        .with_context(|| format!("failed to read {}", host.path.display()))?;

    let merged = match host.format {
        HostFormat::Json => {
            let mut root: serde_json::Value = serde_json::from_str(&data)
                .with_context(|| format!("failed to parse json {}", host.path.display()))?;
            let obj = root
                .as_object_mut()
                .ok_or_else(|| anyhow!("host json must be an object"))?;
            let snippet_json: serde_json::Value =
                serde_json::from_str(&snippet_text).context("parse snippet json")?;
            let mcp = snippet_json
                .get("mcpServers")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
            obj.insert("mcpServers".into(), mcp);
            serde_json::to_string_pretty(&root).context("serialize merged json")?
        }
        HostFormat::Toml => {
            let mut root: toml::Value = toml::from_str(&data)
                .with_context(|| format!("failed to parse toml {}", host.path.display()))?;
            let snippet_toml: toml::Value =
                toml::from_str(&snippet_text).context("parse snippet toml")?;
            let mcp = snippet_toml
                .get("mcpServers")
                .cloned()
                .unwrap_or_else(|| toml::Value::Table(Default::default()));
            let table = root
                .as_table_mut()
                .ok_or_else(|| anyhow!("host toml must be a table"))?;
            // Use the schema-correct key for TOML clients (Codex expects mcp_servers).
            let target_key = if matches!(host.kind, HostKind::Codex) {
                "mcp_servers"
            } else {
                "mcpServers"
            };
            table.insert(target_key.into(), mcp);
            toml::to_string_pretty(&root).context("serialize merged toml")?
        }
    };

    let backup = write_with_backup(&host.path, &merged, dry_run)?;
    Ok(RewireOutcome {
        path: host.path.clone(),
        backup,
        written: !dry_run,
    })
}

/// Write `contents` to `path`, creating a `<path>.bak` backup of the previous
/// contents. Used by the legacy `rewire` CLI subcommand.
pub fn write_with_backup(path: &Path, contents: &str, dry_run: bool) -> Result<Option<PathBuf>> {
    if dry_run {
        println!("--- {} (dry-run) ---\n{}", path.display(), contents);
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let backup = path.with_extension("bak");
    if path.exists() {
        let current = safe_read_to_string(path)?;
        atomic_write(&backup, current.as_bytes())
            .with_context(|| format!("failed to create backup {}", backup.display()))?;
    }
    atomic_write(path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(Some(backup))
}

/// Banner prepended to TOML manifests written by `rmcp-mux scan --manifest`.
/// Keeps the heartbeat-default rationale next to the on-disk artifact so
/// operators learn the gotcha without grepping the source tree.
const MANIFEST_TOML_BANNER: &str = "\
# Generated by `rmcp-mux scan` — daemon manifest of upstream MCP servers.
#
# Heartbeat policy:
#   `heartbeat_enabled` defaults to `false` because the MCP protocol does not
#   require ping/pong probes; most MCP servers (npx-wrapped, custom stdio
#   binaries) do not respond to rmcp-mux heartbeats and would be restarted in
#   a loop. Flip to `true` per-service for backends you have verified support
#   the rmcp-mux heartbeat probe.
\n";

pub fn serialize_config(config: &Config, format: &str) -> Result<String> {
    match format {
        "json" => serde_json::to_string_pretty(config).context("serialize json"),
        "yaml" | "yml" => serde_yaml::to_string(config).context("serialize yaml"),
        "toml" => {
            let body = toml::to_string_pretty(config).context("serialize toml")?;
            Ok(format!("{MANIFEST_TOML_BANNER}{body}"))
        }
        other => Err(anyhow!("unsupported format {other}")),
    }
}

pub fn serialize_snippet(snippet: &serde_json::Value, format: &str) -> Result<String> {
    match format {
        "json" => serde_json::to_string_pretty(snippet).context("serialize snippet json"),
        "yaml" | "yml" => serde_yaml::to_string(snippet).context("serialize snippet yaml"),
        "toml" => toml::to_string_pretty(snippet).context("serialize snippet toml"),
        other => Err(anyhow!("unsupported format {other}")),
    }
}

pub fn resolve_host_from_args(args: &RewireArgs) -> Result<HostFile> {
    if let Some(path) = &args.path {
        return Ok(host_file_from_custom_path(path));
    }

    let discovered = discover_hosts();
    if discovered.is_empty() {
        return Err(anyhow!("no host configs found"));
    }
    if let Some(host) = &args.host {
        let lower = host.to_ascii_lowercase();
        let target = discovered.into_iter().find(|h| h.kind.as_label() == lower);
        return target.ok_or_else(|| anyhow!("host {host} not found"));
    }

    Ok(discovered[0].clone())
}

pub fn resolve_status_host(args: &StatusArgs) -> Result<HostFile> {
    if let Some(path) = &args.path {
        return Ok(host_file_from_custom_path(path));
    }
    let discovered = discover_hosts();
    if discovered.is_empty() {
        return Err(anyhow!("no host configs found"));
    }
    if let Some(host) = &args.host {
        let lower = host.to_ascii_lowercase();
        if let Some(h) = discovered.into_iter().find(|h| h.kind.as_label() == lower) {
            return Ok(h);
        }
        return Err(anyhow!("host {host} not found"));
    }
    Ok(discovered[0].clone())
}

pub fn run_scan_cmd(args: ScanArgs) -> Result<()> {
    let socket_dir = expand_path(args.socket_dir);
    let scans = scan_hosts();
    if scans.is_empty() {
        println!("No host configs discovered.");
        return Ok(());
    }

    let manifest = build_manifest(&scans, &socket_dir);

    if let Some(path) = args.manifest {
        let text = serialize_config(&manifest, &args.manifest_format.to_lowercase())?;
        let backup = write_with_backup(&path, &text, args.dry_run)?;
        println!(
            "Manifest {}written to {}{}",
            if args.dry_run { "(dry-run) " } else { "" },
            path.display(),
            backup
                .as_ref()
                .map(|b| format!(" (backup {})", b.display()))
                .unwrap_or_default()
        );
    } else {
        println!(
            "Discovered {} host(s), {} service(s). Use --manifest to save mux config.",
            scans.len(),
            manifest.servers.len()
        );
    }

    if args.snippet.is_some() || args.dry_run {
        let snippets = generate_snippet(&scans, &socket_dir, "rmcp-mux-proxy", &[]);
        for (kind, snippet) in snippets {
            let fmt = args.snippet_format.to_lowercase();
            let text = serialize_snippet(&snippet, &fmt)?;
            if let Some(base) = &args.snippet {
                let mut path = base.clone();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("snippet")
                    .to_string();
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                path = parent.join(format!("{stem}-{}.{}", kind.as_label(), fmt));
                let backup = write_with_backup(&path, &text, args.dry_run)?;
                println!(
                    "Snippet for {} {}written to {}{}",
                    kind.as_label(),
                    if args.dry_run { "(dry-run) " } else { "" },
                    path.display(),
                    backup
                        .as_ref()
                        .map(|b| format!(" (backup {})", b.display()))
                        .unwrap_or_default()
                );
            } else {
                println!("--- snippet ({}) ---\n{}", kind.as_label(), text);
            }
        }
    }

    Ok(())
}

pub fn run_rewire_cmd(args: RewireArgs) -> Result<()> {
    let target = resolve_host_from_args(&args)?;
    let socket_dir = expand_path(&args.socket_dir);
    let outcome = rewire_host(
        &target,
        &socket_dir,
        &args.proxy_cmd,
        &args.proxy_args,
        args.dry_run,
    )?;
    println!(
        "{}rewired {} (backup: {})",
        if args.dry_run { "Would have " } else { "" },
        outcome.path.display(),
        outcome
            .backup
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none".into())
    );
    Ok(())
}

pub fn run_status_cmd(args: StatusArgs) -> Result<()> {
    let target = resolve_status_host(&args)?;
    let scan = scan_host_file(&target)?;
    if scan.services.is_empty() {
        println!("{}: no services found in config", target.path.display());
        return Ok(());
    }
    println!(
        "Checking {} ({})",
        target.path.display(),
        target.kind.as_label()
    );
    for svc in &scan.services {
        let uses_proxy = svc.command == args.proxy_cmd;
        if uses_proxy {
            println!(" - {}: OK (via {})", svc.name, svc.command);
        } else {
            println!(
                " - {}: NOT rewired (command='{}', args={:?})",
                svc.name, svc.command, svc.args
            );
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_text(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent dir")).expect("create parent");
        atomic_write(path, body.as_bytes()).expect("write file");
    }

    fn json_host(path: PathBuf, kind: HostKind, schema: ConfigSchema) -> HostFile {
        HostFile {
            kind,
            path,
            format: HostFormat::Json,
            schema,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        }
    }

    fn toml_host(path: PathBuf, kind: HostKind) -> HostFile {
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
    fn defaults_cover_required_clients() {
        let kinds: std::collections::HashSet<HostKind> =
            default_sources().iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&HostKind::Claude), "Claude Code missing");
        assert!(
            kinds.contains(&HostKind::ClaudeDesktop),
            "Claude Desktop missing"
        );
        assert!(kinds.contains(&HostKind::Codex), "Codex missing");
        assert!(kinds.contains(&HostKind::Junie), "Junie missing");
        assert!(kinds.contains(&HostKind::Gemini), "Gemini missing");
    }

    #[test]
    fn defaults_use_canonical_paths() {
        let by_kind: std::collections::HashMap<HostKind, Vec<PathBuf>> = default_sources()
            .iter()
            .fold(Default::default(), |mut acc, s| {
                acc.entry(s.kind).or_default().push(s.path.clone());
                acc
            });

        let claude_paths = by_kind.get(&HostKind::Claude).expect("claude");
        assert!(
            claude_paths.iter().any(|p| p.ends_with(".claude.json")),
            "expected ~/.claude.json among Claude paths, got {:?}",
            claude_paths
        );

        let desktop_paths = by_kind
            .get(&HostKind::ClaudeDesktop)
            .expect("claude desktop");
        assert!(
            desktop_paths
                .iter()
                .any(|p| p.ends_with("claude_desktop_config.json")),
            "expected claude_desktop_config.json"
        );

        let codex_paths = by_kind.get(&HostKind::Codex).expect("codex");
        assert!(codex_paths.iter().any(|p| p.ends_with("config.toml")));

        let junie_paths = by_kind.get(&HostKind::Junie).expect("junie");
        assert!(
            junie_paths
                .iter()
                .any(|p| p.ends_with(".junie/mcp/mcp.json"))
        );
        assert!(junie_paths.iter().any(|p| p.ends_with(".agents/mcp.json")));
        assert!(junie_paths.iter().any(|p| p.ends_with(".ai/mcp.json")));

        let gemini_paths = by_kind.get(&HostKind::Gemini).expect("gemini");
        assert!(
            gemini_paths
                .iter()
                .any(|p| p.ends_with(".gemini/settings.json"))
        );
    }

    #[test]
    fn discover_vibecrafted_mcp_dev_path_found() {
        let dir = tempdir().expect("tempdir");
        write_text(
            &dir.path().join("pyproject.toml"),
            r#"
            [project]
            name = "vibecrafted-mcp"
            "#,
        );

        let server = discover_vibecrafted_mcp_with(dir.path(), || false).expect("server");

        assert_eq!(server.name, "vibecrafted-mcp");
        assert_eq!(server.source, DefaultServerSource::VibecraftedMcp);
        assert_eq!(server.command, "python");
        assert_eq!(server.args, vec!["-m", "vibecrafted_mcp"]);
        assert_eq!(server.cwd.as_deref(), Some(dir.path()));
    }

    #[test]
    fn discover_vibecrafted_mcp_pip_show_found() {
        let dir = tempdir().expect("tempdir");
        let server = discover_vibecrafted_mcp_with(dir.path(), || true).expect("server");

        assert_eq!(server.name, "vibecrafted-mcp");
        assert_eq!(server.command, "python");
        assert_eq!(server.args, vec!["-m", "vibecrafted_mcp"]);
        assert_eq!(server.cwd, None);
    }

    #[test]
    fn discover_vibecrafted_mcp_returns_none_when_neither() {
        let dir = tempdir().expect("tempdir");
        write_text(
            &dir.path().join("pyproject.toml"),
            r#"
            [project]
            name = "something-else"
            "#,
        );

        assert!(discover_vibecrafted_mcp_with(dir.path(), || false).is_none());
    }

    #[test]
    fn parse_json_mcpservers_for_claude() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        write_text(
            &path,
            r#"{
              "other": true,
              "mcpServers": {
                "memory": {
                  "command": "npx",
                  "args": ["@modelcontextprotocol/server-memory"],
                  "env": {"FOO": "bar"}
                }
              }
            }"#,
        );

        let host = json_host(path, HostKind::Claude, ConfigSchema::McpServersJson);
        let scan = scan_host_file(&host).expect("scan");
        assert_eq!(scan.services.len(), 1);
        assert_eq!(scan.services[0].name, "memory");
        assert_eq!(scan.services[0].command, "npx");
        assert_eq!(
            scan.services[0]
                .env
                .as_ref()
                .and_then(|m| m.get("FOO"))
                .map(|s| s.as_str()),
            Some("bar")
        );
    }

    #[test]
    fn parse_toml_mcp_servers_for_codex() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        write_text(
            &path,
            r#"
            [other]
            unrelated = "keep me"

            [mcp_servers.memory]
            command = "npx"
            args = ["@modelcontextprotocol/server-memory"]

            [mcp_servers.memory.env]
            HOME = "/tmp"
            "#,
        );

        let host = toml_host(path, HostKind::Codex);
        let scan = scan_host_file(&host).expect("scan");
        assert_eq!(scan.services.len(), 1);
        let svc = &scan.services[0];
        assert_eq!(svc.name, "memory");
        assert_eq!(svc.command, "npx");
        assert_eq!(svc.args, vec!["@modelcontextprotocol/server-memory"]);
        assert_eq!(
            svc.env
                .as_ref()
                .and_then(|m| m.get("HOME"))
                .map(String::as_str),
            Some("/tmp")
        );
    }

    #[test]
    fn parse_junie_json_mcpservers() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("mcp.json");
        write_text(
            &path,
            r#"{"mcpServers": {"fs": {"command": "npx", "args": ["@modelcontextprotocol/server-filesystem", "/tmp"]}}}"#,
        );

        let host = json_host(path, HostKind::Junie, ConfigSchema::McpServersJson);
        let scan = scan_host_file(&host).expect("scan");
        assert_eq!(scan.services.len(), 1);
        assert_eq!(scan.services[0].name, "fs");
    }

    #[test]
    fn parse_generic_json_servers_when_present() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("custom.json");
        write_text(
            &path,
            r#"{"servers": {"echo": {"command": "echo", "args": ["hi"]}}}"#,
        );

        let host = json_host(path, HostKind::Custom, ConfigSchema::AutoJson);
        let scan = scan_host_file(&host).expect("scan");
        assert_eq!(scan.services.len(), 1);
        assert_eq!(scan.services[0].command, "echo");
    }

    #[test]
    fn merge_dedupes_identical_services() {
        let dir = tempdir().expect("tempdir");
        let path_a = dir.path().join("a.json");
        let path_b = dir.path().join("b.json");

        let svc = HostService {
            name: "memory".into(),
            command: "npx".into(),
            args: vec!["@modelcontextprotocol/server-memory".into()],
            cwd: None,
            socket: None,
            env: None,
            enabled: None,
        };
        let scans = vec![
            ScanResult {
                host: json_host(path_a, HostKind::Claude, ConfigSchema::McpServersJson),
                services: vec![svc.clone()],
            },
            ScanResult {
                host: json_host(
                    path_b,
                    HostKind::ClaudeDesktop,
                    ConfigSchema::McpServersJson,
                ),
                services: vec![svc],
            },
        ];

        let merged = merge_services(&scans);
        assert_eq!(
            merged.services.len(),
            1,
            "identical entries should collapse"
        );
        assert!(merged.conflicts.is_empty());
    }

    #[test]
    fn merge_surfaces_conflicting_services() {
        let dir = tempdir().expect("tempdir");
        let path_a = dir.path().join("a.json");
        let path_b = dir.path().join("b.json");

        let scans = vec![
            ScanResult {
                host: json_host(path_a, HostKind::Claude, ConfigSchema::McpServersJson),
                services: vec![HostService {
                    name: "memory".into(),
                    command: "npx".into(),
                    args: vec!["@modelcontextprotocol/server-memory".into()],
                    cwd: None,
                    socket: None,
                    env: None,
                    enabled: None,
                }],
            },
            ScanResult {
                host: json_host(path_b, HostKind::Junie, ConfigSchema::McpServersJson),
                services: vec![HostService {
                    name: "memory".into(),
                    command: "uv".into(),
                    args: vec!["run".into(), "memory-server".into()],
                    cwd: None,
                    socket: None,
                    env: None,
                    enabled: None,
                }],
            },
        ];

        let merged = merge_services(&scans);
        assert_eq!(
            merged.conflicts.len(),
            1,
            "expected one conflict report for `memory`"
        );
        let names: Vec<&str> = merged.services.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("memory-from-claude")));
        assert!(names.iter().any(|n| n.starts_with("memory-from-junie")));
    }

    #[test]
    fn build_manifest_populates_defaults() {
        let scans = vec![ScanResult {
            host: toml_host(PathBuf::from("dummy"), HostKind::Codex),
            services: vec![HostService {
                name: "memory".into(),
                command: "npx".into(),
                args: vec!["@mcp/server-memory".into()],
                cwd: None,
                socket: None,
                env: None,
                enabled: None,
            }],
        }];
        let cfg = build_manifest(&scans, Path::new("/tmp/sockets"));
        let svc = cfg.servers.get("memory").expect("memory svc");
        assert_eq!(svc.cmd.as_deref(), Some("npx"));
        assert_eq!(
            svc.args.as_ref().expect("args"),
            &vec!["@mcp/server-memory"]
        );
        assert!(
            svc.socket
                .as_ref()
                .expect("socket")
                .contains("/tmp/sockets/memory.sock")
        );
    }

    #[test]
    fn build_manifest_disables_heartbeat_by_default() {
        // Regression: scan-generated configs used to default heartbeat_enabled
        // = true, which forced rmcp-mux to ping every upstream MCP. Most MCP
        // servers (npx wrappers, custom stdio binaries) do not respond to
        // rmcp-mux probes, so the daemon spiralled into restart loops. The
        // safe semantic default is `false`; operators flip it per-service for
        // backends that actually support the heartbeat protocol.
        let scans = vec![ScanResult {
            host: toml_host(PathBuf::from("dummy"), HostKind::Codex),
            services: vec![
                HostService {
                    name: "memory".into(),
                    command: "npx".into(),
                    args: vec!["@mcp/server-memory".into()],
                    cwd: None,
                    socket: None,
                    env: None,
                    enabled: None,
                },
                HostService {
                    name: "loctree".into(),
                    command: "loctree-mcp".into(),
                    args: vec![],
                    cwd: None,
                    socket: None,
                    env: None,
                    enabled: None,
                },
            ],
        }];
        let cfg = build_manifest(&scans, Path::new("/tmp/sockets"));
        for (name, svc) in &cfg.servers {
            assert_eq!(
                svc.heartbeat_enabled,
                Some(false),
                "service {name} should default heartbeat_enabled = Some(false)",
            );
        }

        // The serialized TOML must not advertise heartbeat as on for any
        // service, and must carry the operator-facing rationale banner so the
        // gotcha is visible without grepping the source tree.
        let rendered = serialize_config(&cfg, "toml").expect("serialize toml");
        assert!(
            rendered.contains("heartbeat_enabled = false"),
            "expected heartbeat_enabled = false in rendered TOML, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("heartbeat_enabled = true"),
            "rendered TOML must not advertise heartbeat_enabled = true:\n{rendered}",
        );
        assert!(
            rendered.contains("Heartbeat policy"),
            "rendered TOML must carry the heartbeat-policy banner:\n{rendered}",
        );
    }

    #[test]
    fn generate_snippet_uses_proxy() {
        let scans = vec![ScanResult {
            host: toml_host(PathBuf::from("dummy"), HostKind::Codex),
            services: vec![HostService {
                name: "svc".into(),
                command: "npx".into(),
                args: vec!["x".into()],
                cwd: None,
                socket: None,
                env: None,
                enabled: None,
            }],
        }];

        let snippets = generate_snippet(
            &scans,
            Path::new("/tmp/sockets"),
            "rmcp-mux-proxy",
            &["proxy".into()],
        );
        let node = snippets.get(&HostKind::Codex).expect("codex snippet");
        let servers = node
            .get("mcpServers")
            .and_then(|m| m.as_object())
            .expect("mcpServers map");
        let svc = servers
            .get("svc")
            .expect("svc entry")
            .as_object()
            .expect("svc object");
        assert_eq!(svc.get("command").expect("command"), "rmcp-mux-proxy");
        let args = svc
            .get("args")
            .expect("args")
            .as_array()
            .expect("args array")
            .iter()
            .map(|v| v.as_str().expect("string").to_string())
            .collect::<Vec<_>>();
        assert!(args.contains(&"proxy".to_string()));
        assert!(args.iter().any(|s| s == "--socket"));
    }

    #[test]
    fn rewire_updates_mcpservers_in_json() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_text(
            &path,
            r#"{"other": true, "mcpServers": {"memory": {"command": "npx", "args": ["@mcp/server-memory"]}}}"#,
        );
        let host = json_host(path.clone(), HostKind::Claude, ConfigSchema::McpServersJson);
        rewire_host(
            &host,
            Path::new("/tmp/sockets"),
            "rmcp-mux-proxy",
            &[],
            false,
        )
        .expect("rewire");
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        let servers = updated
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .expect("mcpServers");
        let mem = servers
            .get("memory")
            .expect("memory")
            .as_object()
            .expect("memory obj");
        assert_eq!(mem.get("command").expect("command"), "rmcp-mux-proxy");
        assert_eq!(updated.get("other").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn host_file_from_custom_path_infers_format() {
        let json = host_file_from_custom_path(Path::new("/tmp/custom.json"));
        assert_eq!(json.format, HostFormat::Json);
        let toml = host_file_from_custom_path(Path::new("/tmp/custom.toml"));
        assert_eq!(toml.format, HostFormat::Toml);
        assert_eq!(toml.kind, HostKind::Custom);
        assert!(toml.eligible_for_danger);
    }
}
