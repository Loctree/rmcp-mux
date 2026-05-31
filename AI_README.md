# rmcp-mux – AI-facing Overview

> **Version:** 0.4.0
> **Last updated:** 2026-05-05
> **Per-repo doctrine:** see `AGENTS.md` (canonical, agent-agnostic)

This document provides a concise technical overview for AI agents working with the rmcp-mux codebase.

## Purpose

**Library-first MCP multiplexer** — share a single MCP server process across many hosts via Unix socket.

Two usage modes:
1. **As a library** — embed in Rust applications, run multiple MCP services in one process.
2. **As a CLI** — standalone daemon plus wizard, scan, rewire, health, dashboard, and proxy commands.

Core features:
- JSON-RPC ID rewriting per client
- `initialize` request caching and fan-out
- Request limits, timeouts, and size guards
- Child process restart with exponential backoff and capped max-restarts
- Heartbeat-based child health checks with explicit timeout
- Status snapshots (per-service JSON) for UI/automation
- Multi-service daemon with a status query socket and a tray dashboard

## Quick Start

### Library Usage (Recommended)

```rust
use rmcp_mux::{MuxConfig, run_mux_server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = MuxConfig::new("/tmp/mcp.sock", "npx")
        .with_args(vec!["-y".into(), "@modelcontextprotocol/server-memory".into()])
        .with_max_clients(10);
    run_mux_server(config).await
}
```

### CLI Usage

```bash
# Build (default features include cli + tray)
cargo build --release

# Run mux daemon for a single service
./target/release/rmcp-mux \
  --socket ~/.rmcp-servers/rmcp-mux/sockets/memory.sock \
  --cmd npx -- @modelcontextprotocol/server-memory \
  --max-active-clients 5 \
  --status-file ~/.rmcp-servers/rmcp-mux/status.json

# Host side: use bundled proxy (preferred over socat)
rmcp-mux-proxy --socket ~/.rmcp-servers/rmcp-mux/sockets/memory.sock
```

The canonical command surface is the `Makefile`. Use `make gates` before every commit; see `AGENTS.md` for the full target list.

## Project Structure (v0.4.0)

```
src/
├── lib.rs                  # Library entry point, public API re-exports
├── main.rs                 # CLI entry (feature: cli)
├── config.rs               # MuxConfig, ServerConfig, ResolvedParams, CliOptions trait
├── state.rs                # MuxState, StatusSnapshot, DaemonStatus, error helpers
├── common.rs               # Shared host-format helpers (extraction in progress; see GUIDELINES)
├── scan.rs                 # Host discovery + rewire (feature: cli)
├── mux_gen.rs              # Safe wizard path: emit ~/.config/mux/{mcp.json, mcp.toml, config.toml}
├── danger.rs               # [DANGER] wizard path: backup-first JSON/TOML rewrite of host configs with rollback
├── multi.rs                # Multi-service supervisor
├── multi_tui.rs            # Multi-service TUI
├── tray.rs                 # Tray icon (feature: tray)
├── tray_dashboard.rs       # Tray dashboard for multi-service status (feature: tray)
├── bin/
│   ├── rmcp-mux.rs         # CLI binary (feature: cli)
│   └── rmcp-mux-proxy.rs   # STDIO↔socket proxy (feature: cli)
├── runtime/                # Mux daemon core (modular — src/runtime.rs is gone, do not revive)
│   ├── mod.rs              # run_mux, run_mux_internal, entry points
│   ├── types.rs            # ServerEvent, MAX_QUEUE, MAX_PENDING
│   ├── client.rs           # handle_client, handle_client_message
│   ├── server.rs           # server_manager (child lifecycle, restart backoff)
│   ├── proxy.rs            # run_proxy (STDIO bridge)
│   ├── heartbeat.rs        # Heartbeat loop, child health check
│   ├── status.rs           # write_status_file, spawn_status_writer, daemon status socket
│   └── tests.rs            # Runtime tests (incl. ignored mux_transport_roundtrip_with_loctree_mcp)
└── wizard/                 # Three-step TUI wizard (feature: cli)
    ├── mod.rs              # run_wizard, run_tui (safe + [DANGER] paths)
    ├── types.rs            # WizardStep, ServiceEntry, ClientEntry, FormState
    ├── services.rs         # load_all_services, detect_running_mcp_servers
    ├── clients.rs          # detect_clients (Codex, Cursor, VSCode, Claude, JetBrains, Gemini, Junie, ~/.ai, ~/.agents)
    ├── ui.rs               # draw_ui, draw_service_list, draw_client_list
    ├── keys.rs             # handle_key, sync_form_to_service
    └── persist.rs          # persist_all, rewire_selected_clients
```

## Library API

### Core Types

| Type | Description |
|------|-------------|
| `MuxConfig` | Builder for programmatic configuration |
| `MuxHandle` | Lifecycle control for spawned servers |
| `ResolvedParams` | Merged CLI + config parameters |
| `CliOptions` | Trait for generic CLI parameter handling |

### Entry Points

```rust
// Blocking — runs until Ctrl+C
run_mux_server(config: MuxConfig) -> Result<()>

// Non-blocking — returns handle for control
spawn_mux_server(config: MuxConfig) -> Result<MuxHandle>

// External shutdown control
run_mux_with_shutdown(params: ResolvedParams, token: CancellationToken) -> Result<()>

// Health check
check_health(socket: impl AsRef<Path>) -> Result<()>
```

### MuxConfig Builder

```rust
MuxConfig::new(socket, cmd)
    .with_args(vec![...])              // Command arguments
    .with_max_clients(10)              // Max concurrent clients
    .with_service_name("my-svc")       // For logging/status
    .with_request_timeout(Duration::from_secs(60))
    .with_lazy_start(true)             // Spawn child on first request
    .with_status_file("/path")         // JSON status snapshots
```

### MuxHandle Methods

| Method | Description |
|--------|-------------|
| `shutdown()` | Request graceful shutdown (non-blocking) |
| `wait().await` | Wait for server to terminate |
| `is_running()` | Check if server is still active |

## CLI Subcommands

| Command | Purpose |
|---------|---------|
| (default) | Run mux daemon for a single service |
| `wizard` | Three-step TUI: services → clients → save (safe vs `[DANGER]` paths) |
| `scan` | Discover hosts, generate manifest/snippets |
| `rewire` | Update host config to use proxy (creates `.bak`; supports `--dry-run`) |
| `status` | Check whether a host is rewired |
| `health` | Verify socket reachability for a service |
| `daemon-status` | Query running multi-service daemon status via Unix socket |
| `dashboard` | Tray dashboard for multi-service status (feature: tray) |
| `proxy` | STDIO↔socket proxy (also exposed as the `rmcp-mux-proxy` binary) |

## Config (JSON / YAML / TOML)

Default service config: `~/.codex/mcp-mux.toml` (override with `--config`, pick `--service` key under `servers.<name>`).

**Fields per service:**
- `socket`, `cmd`, `args` — required
- `max_active_clients` — default 5
- `lazy_start` — default false
- `max_request_bytes` — default 1_048_576
- `request_timeout_ms` — default 30_000
- `restart_backoff_ms` — default 1_000
- `restart_backoff_max_ms` — default 30_000
- `max_restarts` — default 5 (0 = unlimited)
- `heartbeat_enabled` — per-server heartbeat toggle (v0.4.0)
- `heartbeat_interval_ms` — default 30_000 (v0.4.0)
- `heartbeat_timeout_ms` — timeout before marking child unhealthy (v0.4.0)
- `tray`, `service_name`, `log_level`
- `status_file` — atomic JSON snapshots for UI/automation

## Five-Step Wizard

```bash
rmcp-mux wizard --config ~/.codex/mcp-mux.toml
# or
make wizard
```

1. **DiscoverySources** — toggle which client config files to scan
   (`~/.claude.json`, `~/.codex/config.toml`, `~/.gemini/settings.json`,
   `~/.junie/`, `~/.ai/`, `~/.agents/`, plus legacy editor hosts), with
   a custom-path text input (`i`) for additional files.
2. **ServerReview** — read-only tree of discovered MCP servers grouped
   by originating client. Identical entries are deduplicated; conflicts
   surface with deterministic `-from-<kind>` rename.
3. **StrategyChoice** — pick how to use the discovery:
   - **Unified** — one mux config under `~/.config/mux/{config.toml, mcp.json, mcp.toml}` with every selected server. Recommended.
   - **Per-client** — one mux config per originating client kind, in that client's native format (`claude.json`, `codex.toml`, `junie.json`, ...).
   - **`[DANGER]` Auto-rewire** — backup-first preview-first rewrite of the user's existing client configs to route through `rmcp-mux-proxy`, with rollback commands.
4. **SummaryConfirm** — preview of what will be written and where, then `Confirm` / `Back` / `Cancel`.
5. **ResultAndTray** — show what was written with per-client startup snippets, then offer to start a tray daemon now (spawns `rmcp-mux --tray --config <generated>` detached).

Navigation: `Up/Down` choose, `Space` toggle, `Enter` / `n` next step, `p` previous, `q` quit, `i` open custom-path input on STEP 1.

Source of truth is **client configs**, not running processes. ps-scan is used as enrichment to stamp PIDs and surface running orphans, never as the discovery driver.

Detail: `docs/WIZARD.md`, `docs/vc-agents-client-discovery-plan.md`.

## Status Snapshots

Written atomically to `status_file` on every state change:

```json
{
  "service_name": "memory",
  "server_status": "Running",
  "restarts": 0,
  "connected_clients": 2,
  "active_clients": 1,
  "pending_requests": 0,
  "queue_depth": 0,
  "child_pid": 12345,
  "cached_initialize": true,
  "heartbeat_status": "Healthy",
  "last_heartbeat_ms": 12345
}
```

The multi-service daemon additionally serves a status socket; query it via `rmcp-mux daemon-status` (or `make daemon-status`).

## Testing & Quality Gates

The canonical surface is the `Makefile`:

```bash
make gates       # fmt-check + clippy -D warnings + test --all-features (REQUIRED before commit)
make test-full   # gates + ignored transport tests (needs local loctree-mcp on PATH)
make check       # cargo check --all-targets --all-features
```

Direct cargo equivalents:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features                 # 83 passed, 1 ignored at HEAD
cargo test --all-targets --all-features -- --ignored    # transport roundtrip via loctree-mcp
cargo clippy --all-targets --no-default-features -- -D warnings   # CI configuration
```

CI (`.github/workflows/ci.yml`) runs with `--no-default-features` (tray off) so headless boxes stay green.

## Key Symbols for Navigation

| Symbol | Location | Purpose |
|--------|----------|---------|
| `MuxConfig` | `lib.rs` (re-export from `config.rs`) | Builder for programmatic configuration |
| `MuxHandle` | `lib.rs` | Lifecycle control (shutdown, wait, is_running) |
| `run_mux_server` | `lib.rs` | Blocking server entry point |
| `spawn_mux_server` | `lib.rs` | Non-blocking spawn returning MuxHandle |
| `run_mux_with_shutdown` | `lib.rs` | External CancellationToken support |
| `check_health` | `lib.rs` | Socket health check |
| `CliOptions` | `config.rs` | Trait for generic CLI parameter handling |
| `ResolvedParams` | `config.rs` | Merged CLI + config parameters |
| `MuxState` | `state.rs` | Runtime state (clients, pending, cache) |
| `StatusSnapshot` / `DaemonStatus` | `state.rs` | JSON status output |
| `run_mux` / `run_mux_internal` | `runtime/mod.rs` | Main mux loop (internal vs external shutdown) |
| `server_manager` | `runtime/server.rs` | Child process lifecycle + restart backoff |
| `handle_client` | `runtime/client.rs` | Per-client connection handler |
| `heartbeat_loop` | `runtime/heartbeat.rs` | Child health probe |
| `run_proxy` | `runtime/proxy.rs` | STDIO↔socket bridge (also `rmcp-mux-proxy` binary) |
| `run_wizard` | `wizard/mod.rs` | TUI entry point (feature: cli) |
| `WizardStep` | `wizard/types.rs` | Step enum (Server / Client / Confirmation) |
| `discover_hosts` | `scan.rs` | Find host config files (feature: cli) |
| `emit_mux_config` | `mux_gen.rs` | Safe wizard path: write `~/.config/mux/*` |
| `apply_with_backup` | `danger.rs` | `[DANGER]` wizard path: backup → preview → rewrite → rollback |

## Notes for AI Agents

1. **Read `AGENTS.md` first.** It is the canonical per-repo doctrine (Living Tree convention, commit format, AGENT FAIRNESS, anti-patterns). This file is reference material; GUIDELINES is the contract.

2. **Library-first architecture.** Use `MuxConfig` + `spawn_mux_server` for embedding. CLI is feature-gated.

3. **Feature gating:**
   - `cli` → wizard, scan, binaries (`clap`, `ratatui`, `crossterm`, `tracing-subscriber`).
   - `tray` → system tray icon (`tray-icon`, `image`).
   - For library-only consumers, depend with `default-features = false`.

4. **Naming convention:**
   - Package name: `rmcp-mux` (crates.io, `Cargo.toml`).
   - Library name: `rmcp_mux` (Rust identifier, `use rmcp_mux::*`).
   - Binary names: `rmcp-mux`, `rmcp-mux-proxy`.
   - Detection still recognises legacy `rmcp_mux` patterns; do not strip without a release note.

5. **Single child model.** One MCP server per socket. Multiple services = multiple `MuxConfig` instances or multi-service daemon mode.

6. **Initialize caching.** First `initialize` is cached in `MuxState.cached_initialize`; later clients get the cached response via `init_waiting`.

7. **Error handling.** Use `anyhow::Result` and `.with_context()` for fallible operations. Avoid panics in the runtime path.

8. **Modular runtime is final.** `src/runtime.rs` (monolith) and `src/runtime_legacy.rs` / `src/wizard_legacy.rs` are deleted. If a plan references them, the plan is stale — verify against `loctree-mcp focus src/runtime` before acting.

9. **Comments / docs in English only.** Operator-facing reports under `.vibecrafted/reports/` may be in Polish.

10. **Tray feature is GUI-bound.** CI builds `--no-default-features` to avoid GUI deps; keep that path green.

11. **Prefer `rmcp-mux-proxy` over `socat`** for host STDIO integration — it's the supported bridge.

12. **`.ai-agents/**` is scratch space.** Do not commit. Root-level `AGENTS.md` (if present) is deprecated; ignore it. The canonical per-repo source is `AGENTS.md`.
