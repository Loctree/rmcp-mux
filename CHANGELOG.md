# Changelog

All notable changes to this project will be documented in this file.

## [0.4.2] - 2026-05-10

### Fixed
- **Status-file atomic-replace WARN spam eliminated.** `runtime::status::write_status_file`
  now writes through a unique sibling tmp path (`.<name>.<pid>.<n>.<nanos>.tmp`)
  instead of the shared `<name>.tmp`. With multiple `spawn_status_writer`
  instances pointing at the same target (default multi-service mode), the old
  shared-tmp approach raced its own peers — losing the rename ~6× per
  heartbeat tick per service and flooding the daemon log with `failed to
  atomically replace status` warnings. Transient `NotFound`/`AlreadyExists`
  rename races are now logged at `debug` level; only genuine I/O failures
  (permission, disk-full) stay at `warn`.
- **`rust-mux daemon-status --config <path>` now works.** Previously clap
  rejected the flag with `unexpected argument '--config' found`; even when
  the operator manually passed `--socket`, the multi-service daemon was
  not actually binding the status listener (the doc-comment claim on
  `run_mux_multi` was aspirational). Resolution:
  - `DaemonStatusArgs` gained `--config <PATH>`. When set, the status
    socket is derived deterministically as `<config_dir>/daemon.sock`
    via the new `runtime::status_socket_for_config` helper.
  - `run_mux_multi` now actually binds a `run_status_listener` against a
    shared `StatusState` registry. The new `run_mux_multi_with_status_socket`
    entry point lets the binary thread the per-config socket through.
  - The CLI binary computes the per-config socket from `--config <path>`
    on daemon startup, so `daemon-status --config <same>` always finds
    the listener without flag duplication.
  - Explicit `--socket` still overrides; missing both flags falls back
    to the legacy `/tmp/rust-mux.status.sock` for backwards-compat.
- Removed an accidental `print_status_table` placeholder in `lib.rs` that
  was shadowing the real `runtime::print_status_table` re-export. The
  `daemon-status` table output (non-`--json`) now actually renders.
- **`heartbeat_enabled` now defaults to `false`** in every config generator
  (`scan::build_manifest`, `mux_gen::build_mux_outputs` /
  `build_per_client_outputs`, `wizard::services::build_services_from_scans`,
  default-discovery, and ps-scan orphans) and in the resolver fallback
  (`config::resolve_params`). MCP protocol (spec 2025-11-25) does not require
  ping/pong probes; most upstream MCP servers (npx wrappers, custom Rust
  stdio binaries) do not respond to rust-mux heartbeats, so a global `true`
  forced the daemon into restart loops (heartbeat timeout 30s × 3 max
  failures → endless restart, services never stabilised). Operators flip
  `heartbeat_enabled = true` per-service for backends that explicitly
  support the rust-mux probe.
- Generated `~/.config/mux/config.toml` (both the wizard's safe path and
  `rust-mux scan --manifest --manifest-format toml`) now carries an
  operator-facing banner explaining the heartbeat policy, so the gotcha is
  discoverable without grepping the source tree.
- `--heartbeat-enabled` CLI help text updated to advertise the new default.

### Tests
- New `scan::tests::build_manifest_disables_heartbeat_by_default` and
  `mux_gen::tests::daemon_config_disables_heartbeat_by_default` lock the
  defaults plus the rationale banner.

## [0.4.1] - 2026-05-06

### Added
- **5-step interactive wizard** replacing the legacy 4-step flow:
  DiscoverySources → ServerReview → StrategyChoice → SummaryConfirm → ResultAndTray.
- **Three explicit strategies** for using mux:
  - **Unified** — one ~/.config/mux/{config.toml, mcp.json, mcp.toml}.
  - **Per-client** — separate mux config per originating client kind in
    that client's native format (claude.json, codex.toml, junie.json, ...).
  - **[DANGER] Auto-rewire** — backup-first preview-first rewrite of
    existing client configs to route through `rust-mux-proxy`, with
    rollback commands.
- **Custom-path input** on STEP 1 (`i` to enter) for client config files
  outside the default discovery list.
- **Tray daemon prompt** on STEP 5: spawn `rust-mux --tray --config
  <generated>` detached from the wizard session.
- New helpers:
  - `mux_gen::build_per_client_outputs` + `write_per_client_outputs` +
    `per_client_instructions` for the Per-client strategy.
  - `wizard::services::build_services_from_scans` /
    `enrich_running_state` / `load_services_from_custom_path` as the
    public discovery surface.
  - `config` checked file-read/copy helpers that reject parent
    traversal at the filesystem boundary.
- New canonical doc surfaces: `.vibecrafted/GUIDELINES.md` (per-repo,
  agent-agnostic doctrine) and a fully rewritten `docs/WIZARD.md`.

### Changed
- **Discovery is now driven by client config files**, not by ps-scan.
  The legacy `MCP_PATTERNS` whitelist is demoted to enrichment-only —
  it stamps PIDs on matching entries and surfaces ps-only orphans, but
  never drives the discovery list.
- **Source-of-truth model** flipped: client configs (Claude / Codex /
  Junie / Gemini / ...) are authoritative; running processes are
  side-effects.
- **Wizard title** rebranded from `rmcp_mux wizard` to `rust-mux
  wizard`. Daemon-status banner and multi-server dashboard header
  rebranded in lockstep.
- **Socket path canonicalised** to v0.4.0
  `~/.rmcp-servers/rust-mux/sockets/` everywhere (was a mix of
  `~/mcp-sockets/` and the canonical path).
- **AI_README** bumped to 0.4.0 / 2026-05-05; project structure
  reflects modular `runtime/` + `wizard/` and the new helper modules.

### Fixed
- Self-skip dedup bug in the ps-scan: `args.contains("rust-mux") ||
  args.contains("rust-mux")` was a copy/paste; the second clause now
  correctly matches the legacy `rmcp_mux` binary name.
- Per-client strategy output collisions for same-kind sources (Junie
  ×3, Cursor ×2, VSCode ×2): same-kind scans now merge before writing
  one file per kind.
- Per-client and danger strategies now honour STEP 2 server selection
  (previously rescanned the source verbatim).
- Wizard's per-client summary filenames now follow STEP 2 selected
  services instead of selected source rows.
- Socket allocation ownership returned to `mux_gen` (services.rs no
  longer injects a default socket path that mux_gen would override).
- Removed dead `#[allow(dead_code)]` carry-overs from the C2/C3
  rebuild after consumers landed in the 5-step flow.
- Documentation drift: rmcp_mux references in doc comments, status
  banners, and proxy `--socket` help text replaced with rust-mux.

### Security
- Audited dependency tree for the `tray` feature: 0 vulnerabilities, 1
  unsoundness (glib 0.18.5 RUSTSEC-2024-0429, not on rust-mux's call
  graph) and 8 unmaintained advisories (GTK3 stack via tray-icon).
  Tracked in `.vibecrafted/GUIDELINES.md` under "Tray feature
  dependency risks". CI mitigation: `--no-default-features`.
- `config::checked_read_to_string` and `checked_copy` helpers reject
  parent-traversal paths and canonicalise filesystem boundaries; used
  by `scan::scan_host_file` and `danger` plan execution.

### Coverage notes
- `cargo test --all-targets --all-features` baseline went from 83 → 87
  passing (+4 new wizard::services tests, plus persist.rs scenarios).
  `mux_transport_roundtrip_with_loctree_mcp` (ignored) passes against
  local `loctree-mcp v0.9.4`.

## [0.4.0] - 2025-12-26

### Breaking Changes
- **Default paths changed** from `~/.rmcp_servers/rmcp_mux/` to `~/.rmcp-servers/rust-mux/`.
- **Proxy command** changed from `rmcp_mux_proxy` to `rust-mux-proxy`.

### Added
- **Daemon Status Socket** - Query running daemon status via Unix socket.
- **Heartbeat System** - Configurable health checks for MCP servers.
  - `heartbeat_enabled` - Enable/disable per-server heartbeat
  - `heartbeat_interval_ms` - Check interval (default: 30s)
  - `heartbeat_timeout_ms` - Timeout before marking unhealthy
- **Tray Dashboard** - Multi-server status view in system tray.
- **Standalone Build** - Inlined common types, no workspace dependencies.

### Changed
- Default socket directory: `~/.rmcp-servers/rust-mux/sockets`.
- Default service name: `rust-mux` (hyphenated).
- Detection now matches both `rust-mux` and legacy `rmcp_mux` patterns.
- Updated to Rust Edition 2024 (stable).

### Fixed
- Consistent naming across paths, commands, and documentation.

## [0.3.4] - 2025-12-20

### Fixed
- Minor bug fixes and stability improvements.

## [0.3.0] - 2025-12-04

### Added
- **Library-first architecture** – rust-mux is now an embeddable Rust library, not just a CLI tool.
- `MuxConfig` builder for programmatic configuration:
  ```rust
  let config = MuxConfig::new("/tmp/mcp.sock", "npx")
      .with_args(vec!["@mcp/server-memory".into()])
      .with_max_clients(10);
  ```
- `run_mux_server(config)` – blocking entry point for single mux server.
- `spawn_mux_server(config)` – non-blocking spawn returning `MuxHandle` for lifecycle control.
- `MuxHandle` with `shutdown()`, `wait()`, `is_running()` methods.
- `run_mux_with_shutdown(params, token)` – external `CancellationToken` support for custom shutdown logic.
- `check_health(socket_path)` – simple health check function.
- `CliOptions` trait for generic CLI parameter handling.
- `docs/integration.md` – comprehensive library integration guide.
- Feature flags: `cli` (wizard, scan, binaries) and `tray` (system tray icon).

### Changed
- **Rebranded: `rmcp_mux` → `rust-mux`.** Crate name hyphenated on crates.io per convention; module path `rust_mux`. Binary `rmcp_mux_proxy` → `rust_mux_proxy`. All internal imports `use rmcp_mux::` → `use rust_mux::`. User-facing `RMCP_MUX_*` environment variables preserved for backward compatibility.
- **Moved to Loctree org:** `https://github.com/VetCoders/rust-mux` → `https://github.com/Loctree/rust-mux`.

### Added
- Package metadata: `description`, `repository`, `homepage`, `documentation`, `readme`, `keywords`, `categories`, `license = "MIT OR Apache-2.0"`, and `authors = ["Maciej Gad <void@div0.space>", "Monika Szymanska <hello@vetcoders.io>"]` in `Cargo.toml` for proper crates.io listing and discovery.

## 0.2.0 - 2025-11-24

### Added
- Optional tray icon (`--tray`) showing live server status, client and pending counts, and restart reasons. ([5eefde4](https://github.com/LibraxisAI/rust_mux/commit/5eefde4))
- Config file support (JSON/YAML/TOML) with auto-detection and CLI overrides. ([5eefde4](https://github.com/LibraxisAI/rust_mux/commit/5eefde4))
- `rust-mux-proxy` helper binary plus launchd template and installer tweaks for easier setup. ([04e5402](https://github.com/LibraxisAI/rust_mux/commit/04e5402))
- GitHub Actions CI workflow for formatting, linting, testing, and coverage, including an async proxy forwarding test. ([ad2b9aa](https://github.com/LibraxisAI/rust_mux/commit/ad2b9aa))
- Mux hooks, Semgrep rules, and expanded README documentation. ([e80083c](https://github.com/LibraxisAI/rust_mux/commit/e80083c))
- `health` subcommand to resolve config and assert socket reachability, plus unit tests for healthy/missing sockets.

### Changed
- Refactored mux state management and tray functionality into dedicated `state` and `tray` modules, with tray dependencies gated behind an optional `tray` feature; CI updated to run with `--no-default-features`. ([0d60764](https://github.com/LibraxisAI/rust_mux/commit/0d60764), [ad2b9aa](https://github.com/LibraxisAI/rust_mux/commit/ad2b9aa))

## 0.1.5
- Added JSON status snapshots (`--status-file` / `status_file`) including PID, queue depth, request limits, restart/backoff settings.
- Hardened runtime: lazy child start, request size guard, request timeouts, capped restart backoff, max restarts.
- Config/Wizard/Scan updated to surface new fields; defaults documented in README.
- Status writer task for tray/automation; MuxState now tracks queue depth and child PID.
- Tests cover initialize cache, resets, status snapshots, and proxy; CI runs fmt/clippy/tests/tarpaulin with `--no-default-features` (tray off in CI).
