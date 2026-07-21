# Agent Guidance Installation

Locality installs a small agent guidance pack during desktop onboarding so local agents know how to use connected source filesystems and the `loc` CLI without the user repeating setup instructions.

## Supported Local Agents

| Agent | Install target | Status |
| --- | --- | --- |
| Claude Code / Claude Desktop / Claude Cowork | `~/.claude/skills/locality/SKILL.md`, `~/.claude.json`, and Claude Desktop MCP config when present (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on classic Windows installs, or `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\claude_desktop_config.json` for packaged Windows Claude) | Installed when the guidance installer runs and Claude is detected. |
| Codex | `~/.codex/skills/locality/SKILL.md` and `~/.codex/config.toml` | Installed when the guidance installer runs and Codex is detected. |
| Warp | `~/.agents/skills/locality/SKILL.md` | Installed when the guidance installer runs and Warp is detected. Warp also reads project rules such as `AGENTS.md` and `WARP.md`; Locality keeps mount-point-local `AGENTS.md` files under each connected source mount. |
| OpenCode | `~/.agents/skills/locality/SKILL.md` | Installed when the guidance installer runs and OpenCode is detected. |
| Gemini CLI | `~/.gemini/GEMINI.md` | Managed section installed when the guidance installer runs and Gemini is detected. |
| Cline / Roo Code / Cursor / Windsurf / Zed | `~/.agents/AGENTS.md` plus each mount-local `AGENTS.md`; Cursor and Windsurf also get global MCP config when present | Fallback installed when the guidance installer runs and one of these agents is detected. |
| GitHub Copilot CLI | `~/.copilot/copilot-instructions.md`; Copilot MCP config when present | Managed section installed when the guidance installer runs and Copilot-capable local tooling is detected. |

Locality does not edit opaque app databases. It only updates documented, file-backed
agent instruction or MCP config files. Skill installs use the `locality` skill
name, and MCP config installs rewrite the `loc` server entry idempotently. If an
agent only supports UI-managed global rules, Locality relies on the shared
`AGENTS.md` fallback and the mount-point-local guidance in the mounted folder.

The `loc` name is reserved for the CLI and MCP server entry.

## Installed Skill Behavior

The skill tells agents:

- Connected source files live under `~/Library/CloudStorage/Locality` on macOS by default, with connector-specific rules in the nearest mount-local `AGENTS.md`.
- Supported sources can include Notion, Google Docs, Google Calendar, Gmail, Linear, Slack, and Granola; writable and read-only behavior depends on the connector.
- Online-only files hydrate automatically when opened.
- Agents should use `loc info <path>` for mount context, `loc search <query>` for broad discovery, and `loc locate <url-or-title>` when the user gives a remote URL or title.
- Agents should edit mounted Markdown directly for writable sources and leave changes pending for Locality review unless the user asks them to apply changes remotely.
- Agents should use `loc status <path>`, `loc inspect <path>`, and `loc diff <path>` to inspect local state, remote comparison, and planned operations.
- Agents should not edit Locality identity frontmatter, block IDs, `::loc{...}` directives, `_schema.yaml`, `AGENTS.md`, or `CLAUDE.md` unless explicitly asked.
- For Notion, pages are directories with `page.md`; for Calendar and Gmail, outbound operations use draft folders; for Linear, supported edits include issue body/frontmatter changes and status moves; Slack and Granola are read-only.
- If desktop Live Mode is on, agents should expect safe local edits and clean remote changes to sync in the background. They can inspect state with `loc live-mode status <file>`, but should not run routine `loc pull` or `loc push` after every edit.
- If the user asks the agent to sync, send, publish, update the source, or apply the edit remotely, the agent should not stop after local edits. The safe sequence is `loc diff <path>`, then `loc push <path> -y` for safe plans.
- Agents should also push when Live Mode pauses for review and the user approves the scoped plan.
- If push reports that the remote changed since last sync, the recovery sequence is `loc pull <path>`, resolve any inline conflict markers, rerun `loc diff <path>`, then push again.
- If the agent sandbox cannot execute the host `loc` CLI, it should use the MCP fallback tool named `loc` with CLI-style `argv` arguments. Locality installs the required local MCP credentials for supported agents.

## MCP Fallback

Locality exposes one MCP tool named `loc` so sandboxed agents can still use the same
CLI contract:

```json
{"argv":["status","~/Library/CloudStorage/Locality","--json"]}
```

This bridge is intended as a fallback, not the preferred path. Agents that can
run `loc` directly should keep using the CLI. Claude Desktop is configured as a
local stdio MCP server by launching `loc mcp`. Other supported local agents that
accept URL-based MCP configs use the daemon-hosted HTTP endpoint, which requires
a per-install capability token stored under the Locality state root and copied into
their MCP configs by the desktop installer. Set `LOCALITY_MCP_ADDR=off` before
starting `localityd` to disable the daemon-hosted MCP endpoint, or set
`LOCALITY_MCP_ADDR=<host:port>` to move it.

On Windows, the desktop installer detects both the current MSIX Claude Desktop
package and the legacy per-user EXE install before writing the config path that
install uses: the package-local Roaming path for MSIX installs, or
`%APPDATA%\Claude\claude_desktop_config.json` for legacy installs.

## Onboarding UX

After the first source mount is created, the desktop app runs the installer and shows which local agents were updated. The final onboarding screen also offers this suggested prompt:

```text
Use Locality to work with my connected sources under ~/Library/CloudStorage/Locality. Find the relevant mounted file with `loc locate <url-or-title>` for a URL or title, or `loc search <query>` for broader discovery. Edit mounted Markdown directly, use `loc status <path>` and `loc diff <path>` to inspect pending work, and leave changes pending for Locality review unless I ask you to apply them remotely. When I do, follow the nearest mount-local `AGENTS.md` and run `loc push <path> -y` for safe plans.
```

Users can rerun the installer from Settings > Agent Instructions after installing
a new local agent. The desktop app also runs the installer once after a new
install or upgrade is acknowledged. It does not periodically scan or rewrite
other apps' guidance files in the background.
