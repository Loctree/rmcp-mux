# rmcp-mux – shared MCP server daemon

Small Rust daemon that lets many MCP clients reuse a single STDIO server process (e.g. `npx @modelcontextprotocol/server-memory`) over a Unix socket. It rewrites JSON-RPC IDs per client, caches `initialize`, restarts the child on failure, and cleans up the socket on exit.

## Features
- One child process per service (spawned from `--cmd ...`).
- Many clients via Unix socket; ID rewriting keeps responses matched to the right client.
- `initialize` is executed once; later clients get the cached response immediately.
- Concurrent requests allowed; active client slots limited by `--max-active-clients` (default 5).
- Notifications are broadcast to all connected clients.
- Restart-on-exit for the child; pending/waiting requests receive an error on reset.
- Ctrl+C stops the mux, kills the child, and removes the socket file.
- Optional JSON status snapshots (`--status-file`) for tray/automation (PID, restarts, queue depth).
- Optional tray indicator (`--tray`) shows live server status (running/restarting), client and pending counts, initialize cache state, and restart reason.

## Build
```
cargo build --release
```
Binaries live in `target/release/rmcp-mux`.

## Install (curl | sh)
```
curl -fsSL https://raw.githubusercontent.com/Loctree/rmcp-mux/main/tools/install.sh | sh
```
- Places wrapper in `$HOME/.local/bin/rmcp-mux` and ensures PATH contains cargo bin + wrapper dir.
- Env overrides: `INSTALL_DIR`, `CARGO_HOME`, `MUX_REF` (branch/tag, default main), `MUX_NO_LOCK=1` to skip `--locked`.

### Built-in proxy (no socat required)
If your MCP host wants a STDIO command, use the bundled proxy:
```
rmcp-mux-proxy --socket /tmp/mcp-memory.sock
```
Point host config to `rmcp-mux-proxy` with the matching socket path.

## Run (example: memory server)
```
./target/release/rmcp-mux \
  --socket /tmp/mcp-memory.sock \
  --cmd npx -- @modelcontextprotocol/server-memory \
  --max-active-clients 5 \
  --log-level info

```

## Config-driven run (JSON/YAML/TOML)
- Default config path: `~/.codex/mcp.json` (override via `--config <path>`). Parser auto-detects by extension (`.json`, `.yaml`/`.yml`, `.toml`).
- JSON:
```
{
  "servers": {
    "general-memory": {
      "socket": "~/mcp-sockets/general-memory.sock",
      "cmd": "npx",
      "args": ["@modelcontextprotocol/server-memory"],
      "max_active_clients": 5,
      "max_request_bytes": 1048576,
      "request_timeout_ms": 30000,
      "restart_backoff_ms": 1000,
      "restart_backoff_max_ms": 30000,
      "max_restarts": 5,
      "status_file": "~/.rmcp_servers/rmcp_mux/status.json",
      "lazy_start": false,
      "tray": true,
      "service_name": "general-memory"
    }
  }
}
```
- YAML:
```
servers:
  general-memory:
    socket: "~/mcp-sockets/general-memory.sock"
    cmd: "npx"
    args: ["@modelcontextprotocol/server-memory"]
    max_active_clients: 5
    max_request_bytes: 1048576
    request_timeout_ms: 30000
    restart_backoff_ms: 1000
    restart_backoff_max_ms: 30000
    max_restarts: 5
    status_file: "~/.rmcp_servers/rmcp_mux/status.json"
    lazy_start: false
    tray: true
    service_name: "general-memory"
```
- TOML:
```
[servers.general-memory]
socket = "~/mcp-sockets/general-memory.sock"
cmd = "npx"
args = ["@modelcontextprotocol/server-memory"]
max_active_clients = 5
tray = true

[servers.brave-search]
socket = "~/mcp-sockets/brave.sock"
cmd = "npx"
args = ["-y", "@anthropic/mcp-server-brave-search"]
env = { BRAVE_API_KEY = "your-api-key" }
request_timeout_ms = 60000

[servers.filesystem]
socket = "~/mcp-sockets/fs.sock"
cmd = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/docs"]
lazy_start = true

[servers.rmcp-memex]
socket = "~/.rmcp-servers/sockets/rmcp-memex.sock"
cmd = "/path/to/rmcp-memex"
args = ["serve", "--config", "config.toml", "--db-path", "~/.ai-memories/lancedb"]
env = { SLED_PATH = "~/.rmcp-servers/sled/memex" }
max_request_bytes = 1048576
request_timeout_ms = 30000
restart_backoff_ms = 1000
restart_backoff_max_ms = 30000
max_restarts = 5
status_file = "~/.rmcp_servers/rmcp_mux/status.json"
lazy_start = false
tray = true
service_name = "general-memory"
```
### Parameter reference

| Parameter | Default | Description |
|-----------|---------|-------------|
| `socket` | required | Unix socket path (supports `~` expansion) |
| `cmd` | required | MCP server command |
| `args` | `[]` | Arguments for command |
| `env` | `{}` | Environment variables for child process |
| `max_active_clients` | `5` | Concurrent client limit |
| `lazy_start` | `false` | Defer child spawn until first request |
| `max_request_bytes` | `1048576` | Max request size (1 MiB) |
| `request_timeout_ms` | `30000` | Request timeout (30s) |
| `restart_backoff_ms` | `1000` | Initial restart delay (1s) |
| `restart_backoff_max_ms` | `30000` | Max restart delay (30s) |
| `max_restarts` | `5` | Restart limit (0 = unlimited) |
| `tray` | `false` | Enable tray icon for this server |
| `status_file` | none | Path for JSON status snapshots |

### Client Configuration (Claude Desktop, etc.)

MCP hosts expecting STDIO communication connect through `rmcp-mux-proxy`:

```
./target/release/rmcp-mux --config ~/.codex/mcp.json --service general-memory
```
- CLI flags still override config (e.g. `--socket`, `--cmd`, `--tray`).

### Resolution order & defaults
- `socket` / `cmd`: required (either CLI or config). `--service` is required when `--config` is provided.
- `args`: CLI `--` tail wins, otherwise config, otherwise empty.
- `max_active_clients`: CLI default 5 unless overridden by config entry.
- `lazy_start`: default `false`.
- `max_request_bytes`: default `1_048_576` (1 MiB).
- `request_timeout_ms`: default `30_000` (30 s).
- `restart_backoff_ms`: default `1_000` (1 s), capped by `restart_backoff_max_ms` (default `30_000`).
- `max_restarts`: default `5` (0 = unlimited).
- `tray`: default `false`.
- `service_name`: CLI `--service-name`, else config, else socket file stem, else `rmcp_mux`.
- `status_file`: optional; accepts `~` and absolute/relative paths.

### Interactive wizard (TUI)
- Launch a guided editor (ratatui) to build/update your mux config:
```
rmcp-mux wizard --config ~/.codex/mcp-mux.toml --service general-memory
```
- Controls: `↑/↓` move, `Enter` edit field, `Space` toggle tray, `s` save, `q` quit. Saves JSON/YAML/TOML based on the extension; creates a `.bak` before overwriting.
- `--dry-run` runs the wizard without writing files.
- `--import-config <path>` (repeatable) imports a workspace-local or otherwise non-default MCP config file (JSON or TOML). The wizard auto-detects the schema (`mcpServers`/`servers` for JSON, `[mcp_servers.*]` for TOML).

#### Step 3 — confirm dialog actions

| Action          | What it does |
|-----------------|--------------|
| `SAFE GEN`      | Writes `~/.config/mux/{config.toml,mcp.json,mcp.toml}`. `config.toml` is the daemon truth (original upstream commands), `mcp.json`/`mcp.toml` are client-facing snippets where every server runs `rmcp-mux-proxy --socket <path>`. Never modifies any existing client config. Prints per-client setup commands. |
| `MUX ONLY`      | Writes the legacy mux config to whichever path you passed via `--config`. |
| `CLIPBOARD`     | Copies the mux TOML to the macOS clipboard (`pbcopy`). |
| `[DANGER] auto` | Backup-first preview-first rewrite of *existing* MCP server blocks in known client configs to use `rmcp-mux-proxy`. Wizard leaves the alternate screen, prints a full preview (planned changes per file, skipped sources with reasons), and refuses to mutate anything until the user types `CONFIRM`. Each modified file gets a timestamped `<file>.<unix_seconds>.bak` next to it; rollback commands are printed at the end. Files that fail to parse are *never* modified. |

The `[DANGER]` flow understands real-world client realities:
- **Claude Code** & **Claude Desktop** are eligible (JSON `mcpServers` schema, surgical update keeps unrelated keys).
- **Codex** is eligible (TOML `[mcp_servers.<name>]` schema).
- **Junie** is eligible (`~/.junie/mcp/mcp.json` plus generic `~/.agents/mcp.json` / `~/.ai/mcp.json`).
- **Gemini** is by default *ineligible* — there's no observed strict-config flag, so the wizard prefers generated `gemini mcp add ...` commands in the safe path. You can still aim a `--import-config` at a Gemini settings file for inspection.

#### Per-client guidance the safe path prints

After `SAFE GEN`, run the printed commands per client. Quick reference:
- Claude Code: `claude --strict-mcp-config --mcp-config "$HOME/.config/mux/mcp.json"`
- Claude Desktop: merge the `mcpServers` block from `~/.config/mux/mcp.json` into `~/Library/Application Support/Claude/claude_desktop_config.json` (no strict-config CLI flag in this variant).
- Codex CLI: merge the `[mcp_servers]` block from `~/.config/mux/mcp.toml` into `~/.codex/config.toml`, or run `codex mcp add ...` per server. (`codex --config k=v` is a key-value override, not a config-file flag — the wizard will not invent one.)
- Junie: `junie --mcp-location "$HOME/.config/mux/mcp.json"` (or `--mcp-default-locations` to keep additive.)
- Gemini CLI: one printed `gemini mcp add <name> -- rmcp-mux-proxy --socket <path>` per discovered service.

See `docs/WIZARD.md` for the full guided walk-through, conflict handling, and rollback procedure.

### Dependency notes
- `ratatui` + `crossterm` power the TUI wizard; both are pure-Rust and optional (build with `--no-default-features` to skip).
- `tempfile` is dev-only for isolated FS fixtures in tests.

### Scan and rewire host configs
- Detect MCP hosts (Codex, Cursor/VSCode, Claude, JetBrains paths) and build a mux manifest + host snippets that point to the bundled proxy:
```
rmcp-mux scan --manifest ~/.codex/mcp-mux.toml --snippet ~/.codex/mcp-mux
```
#### `scan` – Discover and generate configs
```
rmcp-mux rewire --host codex --socket-dir ~/.rmcp-servers/rmcp-mux/sockets
```
- Snippets use the installed `rmcp-mux-proxy` binary: `command = "rmcp-mux-proxy"; args = ["--socket", "<service.sock>"]`.
- Check whether a host is already pointed at the mux proxy:
```
rmcp-mux status --host codex --proxy-cmd rmcp-mux-proxy
```

### Health check
- Verify that config resolves and the mux socket is reachable:
```
rmcp-mux health --socket /tmp/mcp-memory.sock --cmd npx -- @modelcontextprotocol/server-memory
```
- With a config file:
```
rmcp-mux health --config ~/.codex/mcp.json --service general-memory
```

## Tray status (optional)
- Run with `--tray` to spawn a small status icon. The drawer lists service name, server state, connected/active clients, pending requests, initialize cache state, and restart count/reason.
- Click “Quit mux” in the tray menu to stop the daemon (propagates shutdown to the child and cleans the socket).
- To feed your own UI/monitor, write status snapshots to JSON: `rmcp-mux --status-file ~/.rmcp_servers/rmcp_mux/status.json ...`. The file is updated on every state change.
```

### Proxy config for MCP hosts
Use the bundled proxy instead of `socat`:
```
rmcp-mux-proxy --socket /tmp/mcp-memory.sock
```
Do this per service (memory, brave-search, etc.) with distinct sockets and mux instances.

### launchd (macOS) example
A template lives at `tools/launchd/rmcp-mux.sample.plist`. Copy to `~/Library/LaunchAgents/`, replace paths/user, then:
```
launchctl load -w ~/Library/LaunchAgents/rmcp-mux.general-memory.plist
```
Label should be unique per service; logs go to the paths defined in the plist.

## Runtime behavior
- New client → assigned `client_id`, messages get `global_id = c<client>:<seq>`.
- Responses are demuxed back to the original client/local ID.
- First `initialize` hits the server; the response is cached and fanned out to waiters. Later `initialize` calls are answered from cache.
- Guards: max request size (default 1 MiB), request timeout (default 30 s) with cleanup of pending calls, exponential restart backoff (1 s → 30 s) with a default limit of 5 restarts, and optional lazy start (defer child spawn until the first request).
- If the child exits or write/read fails, the mux restarts it, clears cache/pending, and sends error responses to affected clients.
- On shutdown (Ctrl+C), the mux stops the child and deletes the socket.

## Options
- `--socket <path>`: Unix socket path.
- `--cmd <prog>` `-- <args>`: command to run the MCP server.
- `--max-active-clients <n>`: limit of concurrently active clients (default 5).
- `--log-level <level>`: trace|debug|info|warn|error (default info).

## Tests and coverage
```
cargo test
cargo clippy --all-targets --all-features
cargo tarpaulin --all-targets --timeout 120
```
Current unit tests cover ID rewriting, initialize caching, and reset fan-out. Integration tests with a fake server can be added to raise coverage.

## Notes and TODOs
- Extend health to include initialize ping and optional metrics (per client / per request).
- Consider persistent initialize params after child restart (auto re-init).
- Add configurable child restart backoff and max retries.
- Expand host detection/rewire coverage and add automated host-side validation.
