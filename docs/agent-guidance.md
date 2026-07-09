# Agent Guidance Installation

Locality installs a small agent guidance pack during desktop onboarding so local agents know how to use the mounted Notion filesystem without the user repeating setup instructions.

## Supported Local Agents

| Agent | Install target | Status |
| --- | --- | --- |
| Claude Code / Claude Desktop / Claude Cowork | `~/.claude/skills/locality/SKILL.md`, `~/.claude.json`, and Claude Desktop MCP config when present (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on Windows) | Automatic when Claude is detected. |
| Codex | `~/.codex/skills/locality/SKILL.md` and `~/.codex/config.toml` | Automatic when Codex is detected. |
| Warp | `~/.agents/skills/locality/SKILL.md` | Automatic when Warp is detected. Warp also reads project rules such as `AGENTS.md` and `WARP.md`; Locality keeps mount-point-local `AGENTS.md` under `/Locality/notion-main`. |
| OpenCode | `~/.agents/skills/locality/SKILL.md` | Automatic when OpenCode is detected. |
| Gemini CLI | `~/.gemini/GEMINI.md` | Automatic managed section when Gemini is detected. |
| Cline / Roo Code / Cursor / Windsurf / Zed | `~/.agents/AGENTS.md` plus `/Locality/notion-main/AGENTS.md`; Cursor and Windsurf also get global MCP config when present | Automatic fallback when one of these agents is detected. |
| GitHub Copilot CLI | `~/.copilot/copilot-instructions.md`; Copilot MCP config when present | Automatic managed section when Copilot-capable local tooling is detected. |

Locality does not edit opaque app databases. It only updates documented, file-backed
agent instruction or MCP config files. Skill installs use the `locality` skill
name, and MCP config installs rewrite the `loc` server entry idempotently. If an
agent only supports UI-managed global rules, Locality relies on the shared
`AGENTS.md` fallback and the mount-point-local guidance in the mounted folder.

The `loc` name is reserved for the CLI and MCP server entry.

## Installed Skill Behavior

The skill tells agents:

- Notion files live under `~/Library/CloudStorage/Locality/notion` on macOS by default.
- Online-only files hydrate automatically when opened.
- Agents should edit Markdown files directly and leave changes pending for Locality review.
- Agents should not edit Locality identity frontmatter, block IDs, `::loc{...}` directives, `_schema.yaml`, `AGENTS.md`, or `CLAUDE.md` unless explicitly asked.
- For Notion, agents should read the mount-local `AGENTS.md` for the concrete page and row creation contract. Prefer `loc create page --title "New Page" --parent <parent-directory>` for new pages, and add `--private` when the remote page should be created in Notion's Private section; manually, pages are directories, a new child page is created by writing `parent-page/new-page/page.md`, new page frontmatter needs `title: "..."`, and generated `loc:` identity frontmatter is omitted until Locality adds it after push.
- `loc status` is optional and only needed when the agent needs to inspect pending changes.
- If desktop Live Mode is on, agents should expect safe local edits and clean remote changes to sync in the background. They should not run routine `loc pull` or `loc push` after every edit.
- Agents should only push when the user explicitly asks or when Live Mode pauses for review. The safe sequence is `loc diff <file>`, then `loc push <file> -y` for safe plans.
- If push reports that the remote changed since last sync, the recovery sequence is `loc pull <file>`, resolve any inline conflict markers, rerun `loc diff <file>`, then push again.
- If the agent sandbox cannot execute the host `loc` CLI, it should use the MCP fallback tool named `loc` with CLI-style `argv` arguments. Locality installs the required local MCP credentials for supported agents.

## MCP Fallback

Locality exposes one MCP tool named `loc` so sandboxed agents can still use the same
CLI contract:

```json
{"argv":["status","~/Library/CloudStorage/Locality/notion-main","--json"]}
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
package and the legacy per-user EXE install before writing
`%APPDATA%\Claude\claude_desktop_config.json`.

## Onboarding UX

After the Notion mount is created, the desktop app runs the installer and shows which local agents were updated. The final onboarding screen also offers this suggested prompt:

```text
Use Locality to edit my Notion workspace. Open the Notion files under ~/Library/CloudStorage/Locality/notion-main, make the requested edits directly in Markdown, and leave the changes pending for Locality review.
```

Users can rerun the installer from Settings > Agent Instructions after installing
a new local agent. The desktop app also refreshes agent guidance and MCP config
periodically while it is running, so newly installed agents are picked up without
another onboarding pass.
