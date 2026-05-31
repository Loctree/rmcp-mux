# rust-mux wizard — five-step flow

> **Version:** 0.4.0
> **Last updated:** 2026-05-06

The wizard takes you from "I have N MCP clients with overlapping servers" to
"every client multiplexes through one rust-mux daemon" without surprising you.
Five steps. Three strategies. Backups before any rewrite.

```
DiscoverySources → ServerReview → StrategyChoice → SummaryConfirm → ResultAndTray
```

```bash
make wizard                           # uses default ~/.codex/mcp-mux.toml
make wizard-dry-run                   # preview only, no writes

# Or directly:
cargo run --bin rust-mux -- wizard --config ~/.codex/mcp-mux.toml
cargo run --bin rust-mux -- wizard --dry-run
cargo run --bin rust-mux -- wizard --import-config ~/.workspace/mcp.json
```

## Source of truth

The wizard discovers MCP servers from your **client config files**, not from
running processes. That matters: Claude Code and Codex spawn MCP servers on
demand, so a `ps`-based scan would show "zero servers" outside an active
session. Reading the configs gives a true picture regardless of runtime
state. ps-scan is still used, but only as **enrichment** — it stamps PIDs on
matching entries and surfaces ps-only orphans.

Default sources (auto-discovered if the file exists):

| Path | Client | Schema |
|---|---|---|
| `~/.claude.json` | Claude Code | JSON `mcpServers` |
| `~/Library/Application Support/Claude/claude_desktop_config.json` | Claude Desktop | JSON `mcpServers` |
| `~/.codex/config.toml` | Codex CLI | TOML `[mcp_servers.<name>]` |
| `~/.junie/mcp/mcp.json` | Junie | JSON `mcpServers` |
| `~/.agents/mcp.json` | Junie (generic) | JSON `mcpServers` / `servers` |
| `~/.ai/mcp.json` | Junie (generic) | JSON `mcpServers` / `servers` |
| `~/.gemini/settings.json` | Gemini CLI | JSON `mcpServers` / `servers` |
| `~/Library/Application Support/Cursor/User/settings.json` | Cursor | JSON (legacy) |
| `~/Library/Application Support/Code/User/settings.json` | VS Code | JSON (legacy) |
| `~/Library/Application Support/JetBrains/LLM/mcp.json` | JetBrains IDEs | JSON (legacy) |

Use `--import-config <path>` (repeatable) on the CLI to pre-add custom
sources before the wizard starts. Inside STEP 1, `i` opens the custom-path
input field for ad-hoc additions.

## STEP 1 — Discovery sources

```
┌─Sources [4/8]─────────────────────────────────────┐ ┌─Custom path───────────────┐
│ ▶ [x] [Claude Code     ] ~/.claude.json 5 servers │ │ Add custom path            │
│   [x] [Codex CLI       ] ~/.codex/config.toml   7 │ │                            │
│   [x] [Junie           ] ~/.junie/mcp/mcp.json  3 │ │ > <empty>                  │
│   [ ] [Gemini CLI      ] ~/.gemini/settings.json  │ │                            │
│   [ ] [Claude Desktop  ] …/claude_desktop_config… │ │ Keys                       │
│   [ ] [Cursor          ] …/Cursor/User/settings.… │ │   Up/Down  navigate        │
│   [ ] [VS Code         ] …/Code/User/settings.js… │ │   Space    toggle          │
│   [ ] [JetBrains       ] …/LLM/mcp.json not found │ │   i        custom path     │
└───────────────────────────────────────────────────┘ │   Enter    add custom path │
                                                     │   n        next step       │
                                                     │   q        quit            │
                                                     └────────────────────────────┘
```

Statuses surface the parse outcome (`N servers`, `empty`, `invalid`,
`not found`). Sources that don't exist are deselected by default; you can
flip them on if you plan to populate the file later.

The custom-path field opens with `i`. Type a path (`~` is expanded), then
`Enter` to add it, `Esc` to abandon. The newly added source is selected
automatically.

## STEP 2 — Server review

```
┌─Servers [9/15]─────────────────────────┐ ┌─Review───────────────────────┐
│ ─ claude                               │ │ Summary                      │
│ ▶ [x] aicx-mcp                         │ │                              │
│   [x] brave-search                     │ │   Total entries  : 15        │
│   [x] loctree-mcp                      │ │   Unique names   : 9         │
│   [x] context7                         │ │   Sources scanned: 3         │
│   [x] memex                            │ │                              │
│ ─ codex                                │ │ Keys                         │
│   [x] playwright                       │ │   Up/Down  navigate          │
│   [x] chrome-devtools (pid 21470)      │ │   Space    toggle            │
│   [x] curl                             │ │   n        next step         │
│   [x] youtube                          │ │   p        previous step     │
│ ─ junie                                │ │   q        quit              │
│   (deduped against claude/junie items) │ │                              │
└────────────────────────────────────────┘ └──────────────────────────────┘
```

Servers are grouped by their originating client. Identical entries across
clients (same `command + args + env`) collapse into one. If two clients
disagree on how to launch the same logical server, both variants are kept
and renamed with deterministic `-from-<kind>` suffixes; the right panel
surfaces the conflict count.

PID badges (`pid 21470`) appear on entries whose `(cmd, args)` match a
running process — the `enrich_running_state` pass.

You can untick any entry to drop it from the mux output. Defaults to all
selected.

## STEP 3 — Strategy

```
┌─Strategy────────────────────────────────────────────────────────────────┐
│ How do you want to use mux?                                             │
│                                                                         │
│   (•) 1. Unified config                                                 │
│       Write one ~/.config/mux/{config.toml,mcp.json,mcp.toml} with      │
│       every selected server. Recommended.                               │
│                                                                         │
│   ( ) 2. Per-client configs                                             │
│       Write a separate file per client kind (claude.json, codex.toml,   │
│       junie.json, …) under ~/.config/mux/.                              │
│                                                                         │
│   ( ) 3. [DANGER] Auto-rewire existing client configs                   │
│       Backup-first preview-first rewrite of your real client configs    │
│       to route through rust-mux-proxy.                                  │
│                                                                         │
│ Up/Down to choose, Enter or n to continue, p to go back, q to quit.     │
└─────────────────────────────────────────────────────────────────────────┘
```

`1`, `2`, `3` quick-pick. Up/Down navigate. Enter or `n` advances.

### Unified

Writes three files into `~/.config/mux/`:

| File | Role |
|---|---|
| `config.toml` | Daemon truth — what `rust-mux` should run upstream. |
| `mcp.json` | Client-facing JSON; every server's `command` is `rust-mux-proxy`. |
| `mcp.toml` | Same shape as `mcp.json` but in TOML for Codex-style tooling. |

Per-client startup snippets are printed on STEP 5:

```
claude --strict-mcp-config --mcp-config "$HOME/.config/mux/mcp.json"
junie  --mcp-location      "$HOME/.config/mux/mcp.json"
gemini mcp add aicx-mcp -- rust-mux-proxy --socket $HOME/.config/mux/sockets/aicx-mcp.sock
```

(Codex has no flag to swap the entire config file; the snippet tells you
to merge `mcp.toml` into `~/.codex/config.toml` or use `codex mcp add`.)

### Per-client

Writes one mux config per originating client kind, in that client's
native format:

```
~/.config/mux/
  config.toml      # daemon truth (merged across every selected source)
  claude.json      # only Claude's servers, in Claude Desktop's mcpServers shape
  codex.toml       # only Codex's servers, in [mcp_servers.<name>] shape
  junie.json       # only Junie's servers, in mcpServers shape
  …
```

Useful when you want different stacks for different agents — e.g. give
Claude five servers but Codex only two. Each client points at its own mux
file with the per-kind startup commands STEP 5 prints.

### [DANGER] Auto-rewire

Rewrites the user's existing client configs in-place to route through
`rust-mux-proxy`. Discipline:

1. Every eligible source gets a timestamped backup
   (`<file>.<unix_seconds>.bak`).
2. Before any disk write the wizard prints a full preview and prompts
   for `CONFIRM` (uppercase) on cooked stdin. Anything else cancels.
3. Sources that fail to parse are recorded as `SkippedInvalid` and never
   touched.
4. Sources flagged `eligible_for_danger = false` (currently Gemini's
   `settings.json`) are listed but skipped — the safe path snippets are
   the supported route for those clients.

The result panel includes one rollback command per backup:

```
cp -p '~/.claude.json.1714956123.bak' '~/.claude.json'
```

## STEP 4 — Summary & confirm

```
┌─Summary─────────────────────────────────────────┐ ┌─Action──────────┐
│ About to:                                       │ │ Choose action   │
│                                                 │ │                 │
│   Strategy : Unified                            │ │ ▶ Confirm       │
│   Outputs  : ~/.config/mux/config.toml          │ │   Back          │
│              ~/.config/mux/mcp.json             │ │   Cancel        │
│              ~/.config/mux/mcp.toml             │ │                 │
│   Sockets  : ~/.config/mux/sockets              │ │ Up/Down: choose │
│   Servers  : 9 selected                         │ │ Enter: do it    │
│                                                 │ └─────────────────┘
│   DRY-RUN: no files will be modified.           │
└─────────────────────────────────────────────────┘
```

Strategy-specific previews:

- **Unified** — three concrete output paths.
- **Per-client** — daemon `config.toml` plus one predicted file per
  selected source kind.
- **Auto-rewire** — explicit list of files that *will* be rewritten,
  plus the list of sources skipped because of `eligible_for_danger =
  false`.

Choose `Confirm`, `Back`, or `Cancel`. Confirm queues the strategy as a
`PendingAction` so it can run on cooked stdout/stdin once the alt screen
is dropped (the danger flow needs that for its `CONFIRM` prompt).

## STEP 5 — Result & tray daemon

```
┌─Result──────────────────────────────────────────┐ ┌─Action─────────────┐
│ Result                                          │ │ Tray daemon        │
│                                                 │ │                    │
│ Wrote rust-mux config under ~/.config/mux:      │ │ Run a multi-       │
│   - .../config.toml (daemon truth)              │ │ service tray       │
│   - .../mcp.json    (client JSON)               │ │ monitor for the    │
│   - .../mcp.toml    (client TOML)               │ │ sockets you just   │
│                                                 │ │ configured?        │
│ Use it from your AI clients:                    │ │                    │
│ • Claude Code (strict config)                   │ │ ▶ Start tray       │
│     claude --strict-mcp-config …                │ │   daemon now       │
│ • Codex CLI                                     │ │   No, exit         │
│     # merge [mcp_servers] from …/mcp.toml       │ │                    │
│ • Junie                                         │ │ Up/Down: choose    │
│     junie --mcp-location …                      │ │ Enter: confirm     │
│ • Gemini CLI                                    │ │                    │
│     gemini mcp add aicx-mcp …                   │ └────────────────────┘
└─────────────────────────────────────────────────┘
```

`Start tray daemon now` spawns `rust-mux --tray --config <generated>`
detached from this terminal (stdin/stdout/stderr → `/dev/null`). `No`
exits cleanly. A persistent launchd-managed tray service is on the
roadmap; today the spawn is per-session.

## Tray monitoring (after the wizard exits)

The wizard's STEP 5 spawn is the convenience path. For long-running
workflows you'll usually want one of:

- `rust-mux --config ~/.config/mux/config.toml` — run the multi-service
  daemon in the foreground so you can see its logs.
- `rust-mux --tray --config ~/.config/mux/config.toml` — same, with the
  tray-icon UI; quit it from the menu.
- `rust-mux daemon-status` (or `make daemon-status`) — query the
  running multi-service daemon over its Unix socket and dump per-service
  status.
- `rust-mux dashboard` (or `make dashboard`) — multi-service status
  view in the system tray, reading the daemon's status snapshots.
- `rust-mux health --config ~/.config/mux/config.toml --service <name>`
  — single-service socket reachability probe.

## CLI flags

```
rust-mux wizard --config <PATH>         # mux daemon config (default ~/.codex/mcp-mux.toml)
                --import-config <PATH>  # pre-load a custom MCP file as STEP 1 source (repeatable)
                --dry-run               # plan everything, write nothing
```

The legacy `--service`, `--socket`, `--cmd`, `--args`, `--max-clients`,
`--log-level`, `--tray` flags are accepted for backwards compatibility
and ignored by the 5-step flow.

## Troubleshooting

- **Empty source list on STEP 1.** No client config was found at the
  default paths. Use `i` to add a custom path, or pass
  `--import-config <PATH>` on the CLI.

- **A source is "invalid".** STEP 1 surfaces the parser error in the
  status panel after `i`. Common causes: trailing commas in JSON,
  duplicate keys in TOML, `mcpServers` value that isn't an object.

- **STEP 4 lists a file under "Skipped (no strict-config flag for
  danger flow)".** That client (typically Gemini) doesn't have a
  documented way to swap its entire MCP config file via a flag, so the
  danger flow refuses to rewrite it. Use the safe-path snippets STEP 5
  prints instead — they cover that client's recommended `mcp add`-style
  setup.

- **`CONFIRM` prompt didn't appear after Auto-rewire.** It appears on
  cooked stdout *after* the alt screen is dropped. If you see no
  prompt, the plan had zero `Planned` actions (every selected source
  was either invalid or ineligible).

- **Tray daemon spawn says "Could not start … run manually".** Either
  `rust-mux` isn't on `$PATH` or the config file you pointed at doesn't
  exist. `cargo install --path .` (or copy the release binary onto
  your `$PATH`) and try again.

## See also

- `docs/integration.md` — library use of `MuxConfig` / `spawn_mux_server`.
- `docs/vc-agents-client-discovery-plan.md` — original plan for the
  multi-client discovery layer this wizard now consumes.
- `AGENTS.md` — repo-wide doctrine, including the
  wizard's Unified / Per-client / Auto-rewire split.

---

_𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI_
