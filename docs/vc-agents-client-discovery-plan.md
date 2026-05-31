---
run_id: rmcp-mux-client-discovery-config-generation
skill: vc-agents
project: rmcp-mux
status: pending
loops_completed: 0
---

# Task: rmcp-mux MCP client discovery and config generation

You are working on a living tree. Concurrent changes are expected. Adapt proactively and continue, but do not skip quality, security, or test gates.

## Product intent

`rmcp-mux` is a daemon/proxy for keeping one MCP server process alive and sharing it between many MCP clients through a socket/proxy transport. The current client discovery defaults are stale after the `rmcp_mux` → `rmcp-mux` rebrand and after real CLI verification.

Implement a client discovery and setup flow that stops pretending all MCP clients use one universal config model. The wizard must offer two explicit paths:

1. Safe default: discover real MCP servers from known client configs, generate mux-owned configs under `~/.config/mux`, and print exact per-client usage instructions. Do not modify existing client configs.
2. `[DANGER] Automatically configure my clients`: after discovery, offer a backup-first and preview-first rewrite of known MCP server blocks in existing client configs so they point to `rmcp-mux-proxy` instead of directly starting upstream MCP servers.

## Required real-world defaults

Correct the discovery defaults according to observed local CLI behavior and current research:

- Claude:
  - `~/.claude.json` for Claude Code / global config.
  - `~/Library/Application Support/Claude/claude_desktop_config.json` for Claude Desktop on macOS.
  - Claude supports `--mcp-config <configs...>` and `--strict-mcp-config`.
  - Recommended safe usage: `claude --strict-mcp-config --mcp-config "$HOME/.config/mux/mcp.json"`.
- Codex:
  - Prefer `codex mcp list --json` / `codex mcp get --json` if useful and robust.
  - File fallback: `~/.codex/config.toml`.
  - `-c/--config` is a key-value override, not a config-file flag. Do not document fake `codex --config ~/.config/mux/mcp.toml` behavior.
  - Codex TOML shape is expected around `[mcp_servers.<name>]`.
- Junie:
  - `~/.junie/mcp/mcp.json` high-confidence.
  - `~/.agents/mcp.json` and `~/.ai/mcp.json` as generic medium-confidence agent config paths.
  - Junie supports `--mcp-location` and `--mcp-default-locations`.
  - Recommended safe usage: `junie --mcp-location "$HOME/.config/mux/mcp.json"`.
- Gemini:
  - `~/.gemini/settings.json` if it contains MCP server config.
  - Gemini exposes `gemini mcp list/add/remove/enable/disable`.
  - No observed Claude-style strict config flag. Prefer generated instructions/commands or danger rewrite.
- Custom files:
  - Support user-provided JSON/TOML paths.
  - JSON with `mcpServers`.
  - JSON with `servers` only when shape is clearly MCP-like.
  - TOML with `mcp_servers`.

## Architecture requirements

- Represent discovery per client/config source with explicit metadata:
  - client kind
  - source path or CLI source
  - config format
  - confidence
  - whether writable / eligible for danger rewrite
  - discovered servers
- Represent each server with:
  - name
  - command
  - args
  - env
  - source client/path
  - enabled status if known
- Deduplicate identical servers by normalized `(name, command, args, env)`.
- Do not silently overwrite conflicts where the same server name has different command/args/env. Surface conflict and either keep both with deterministic suffixes or require explicit choice.

## Safe path requirements

Generate mux-owned files under `~/.config/mux`:

- `~/.config/mux/config.toml` — daemon/upstream truth for `rmcp-mux`, containing original upstream commands.
- `~/.config/mux/mcp.json` — client-facing JSON config where each server command is `rmcp-mux-proxy`.
- `~/.config/mux/mcp.toml` — client-facing TOML config for TOML-style clients / manual Codex merge.

The generated client-facing configs must point clients at `rmcp-mux-proxy`, not at upstream MCP servers directly.

After generation, print concise instructions:

- how to start `rmcp-mux` with generated daemon config;
- Claude strict-mode command;
- Junie `--mcp-location` command;
- Codex note that there is no verified strict config-file flag in this environment, plus generated `codex mcp add ...` or manual TOML merge instructions;
- Gemini note using `gemini mcp` commands or danger rewrite.

## Danger path requirements

`[DANGER] Automatically configure my clients` must be explicit, backup-first, preview-first, and tested.

Required behavior:

- Show scary confirmation text.
- Show dry-run preview of every file and server block to be changed.
- Require explicit confirmation before write.
- Create timestamped backup next to every modified file before mutation.
- Preserve unrelated config keys/tables.
- Modify only selected MCP server blocks.
- If parsing fails, do not modify that file.
- Print rollback commands using the exact backup paths.

JSON rules:

- Preserve unknown top-level keys.
- Modify only `mcpServers` or clearly supported `servers` entries.

TOML rules:

- Preserve unrelated tables as much as the current TOML library allows.
- If comments cannot be preserved, state that in preview and rely on backup.

## Tests required

Add tests proportional to implementation:

- discover Claude JSON `mcpServers`;
- discover Codex TOML `[mcp_servers.*]`;
- discover Junie JSON config;
- discover generic/custom JSON and TOML;
- Gemini parser only if the actual settings shape is confidently known; otherwise keep it conservative and test generic JSON import;
- deduplicate identical servers;
- detect conflicting server names;
- write mux daemon config + client JSON + client TOML into temp dir;
- danger rewrite creates backup before JSON mutation;
- danger rewrite creates backup before TOML mutation;
- invalid config is not modified.

Do not require real local user configs in normal CI. Any local-config test must be ignored/optional and read-only.

## Docs required

Update `docs/WIZARD.md` and `README.md` if needed:

- explain the safe default path step by step;
- explain the danger path, backups, rollback, and dry-run;
- document custom imports;
- document per-client commands and limitations;
- include troubleshooting for duplicate names, invalid config, original servers still starting, socket unavailable, and env/secrets handling.

## Constraints

- No `#[allow(...)]` suppressions.
- No `// nosemgrep` suppressions.
- No `--no-verify`.
- Do not delete user files.
- Do not rewrite unrelated config content.
- Do not invent unsupported CLI flags.
- Follow existing Rust style and architecture.

## Acceptance

- Discovery defaults reflect real Claude, Codex, Junie, Gemini and generic config locations above.
- Wizard offers safe generation and `[DANGER]` automatic client configuration as separate choices.
- Safe path writes mux-owned configs under `~/.config/mux` and does not mutate existing client configs.
- Danger path is backup-first, preview-first, explicit-confirmation-only, and rollback-friendly.
- Tests cover parser/generation/rewrite behavior without relying on local user config.
- Documentation gives exact setup and monitoring guidance.
- Gates pass:
  - `cargo fmt -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all-targets --all-features`
  - `make test-full` if available and environment has optional dependencies.

## Agent roles

Claude:

- Audit real-world MCP client config formats and danger-path risks.
- Verify assumptions before changing code.
- Focus on edge cases, data loss risks, rollback quality, and docs truthfulness.

Gemini:

- Challenge and simplify the UX/architecture.
- Identify overengineering, unsafe magic, and better command-generation alternatives.
- Keep user flow understandable.

Codex:

- Implement the final design after incorporating findings.
- Prioritize exact parser/generator/rewrite code and tests.
- Keep gates green.