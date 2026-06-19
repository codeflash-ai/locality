# Agent Guidance Installation

AFS installs a small agent guidance pack during desktop onboarding so local agents know how to use the mounted Notion filesystem without the user repeating setup instructions.

## Supported Local Agents

| Agent | Install target | Status |
| --- | --- | --- |
| Claude Code / Claude Desktop / Claude Cowork | `~/.claude/skills/afs/SKILL.md`, `~/.claude.json`, and Claude Desktop MCP config when present | Automatic when Claude is detected. |
| Codex | `~/.codex/skills/afs/SKILL.md` and `~/.codex/config.toml` | Automatic when Codex is detected. |
| Warp | `~/.agents/skills/afs/SKILL.md` | Automatic when Warp is detected. Warp also reads project rules such as `AGENTS.md` and `WARP.md`; AFS keeps connector-local `AGENTS.md` under `/AFS/notion`. |
| OpenCode | `~/.agents/skills/afs/SKILL.md` | Automatic when OpenCode is detected. |
| Gemini CLI | `~/.gemini/GEMINI.md` | Automatic managed section when Gemini is detected. |
| Cline / Roo Code / Cursor / Windsurf / Zed | `~/.agents/AGENTS.md` plus `/AFS/notion/AGENTS.md`; Cursor and Windsurf also get global MCP config when present | Automatic fallback when one of these agents is detected. |
| GitHub Copilot CLI | `~/.copilot/copilot-instructions.md`; Copilot MCP config when present | Automatic managed section when Copilot-capable local tooling is detected. |

AFS does not edit opaque app databases. It only updates documented, file-backed
agent instruction or MCP config files, and rewrites its own `afs` entry
idempotently. If an agent only supports UI-managed global rules, AFS relies on
the shared `AGENTS.md` fallback and the connector-local guidance in the mounted
folder.

## Installed Skill Behavior

The skill tells agents:

- Notion files live under `~/Library/CloudStorage/AFS/notion` on macOS by default.
- Online-only files hydrate automatically when opened.
- Agents should edit Markdown files directly and leave changes pending for AFS review.
- Agents should not edit AFS identity frontmatter, block IDs, `::afs{...}` directives, `_schema.yaml`, `AGENTS.md`, or `CLAUDE.md` unless explicitly asked.
- For Notion, agents should read the mount-local `AGENTS.md` for the concrete page and row creation contract: pages are directories, a new child page is created by writing `parent-page/new-page/page.md`, new page frontmatter needs `title: "..."`, and generated `afs:` identity frontmatter is omitted until AFS adds it after push.
- `afs status` is optional and only needed when the agent needs to inspect pending changes.
- Agents should only push when the user explicitly asks. The safe sequence is `afs diff <file>`, then `afs push <file> -y` for safe plans.
- If push reports that the remote changed since last sync, the recovery sequence is `afs pull <file>`, resolve any inline conflict markers, rerun `afs diff <file>`, then push again.
- If the agent sandbox cannot execute the host `afs` CLI, it should use the MCP fallback tool named `afs` with CLI-style `argv` arguments. AFS installs the required local MCP credentials for supported agents.

## MCP Fallback

AFS exposes one MCP tool named `afs` so sandboxed agents can still use the same
CLI contract:

```json
{"argv":["status","~/Library/CloudStorage/AFS/notion","--json"]}
```

This bridge is intended as a fallback, not the preferred path. Agents that can
run `afs` directly should keep using the CLI. Claude Desktop is configured as a
local stdio MCP server by launching `afs mcp`. Other supported local agents that
accept URL-based MCP configs use the daemon-hosted HTTP endpoint, which requires
a per-install capability token stored under the AFS state root and copied into
their MCP configs by the desktop installer. Set `AFS_MCP_ADDR=off` before
starting `afsd` to disable the daemon-hosted MCP endpoint, or set
`AFS_MCP_ADDR=<host:port>` to move it.

## Onboarding UX

After the Notion mount is created, the desktop app runs the installer and shows which local agents were updated. The final onboarding screen also offers this suggested prompt:

```text
Use AFS to edit my Notion workspace. Open the Notion files under ~/Library/CloudStorage/AFS/notion, make the requested edits directly in Markdown, and leave the changes pending for AFS review.
```

Users can rerun the installer from Settings > Agent Instructions after installing
a new local agent. The desktop app also refreshes agent guidance and MCP config
periodically while it is running, so newly installed agents are picked up without
another onboarding pass.
