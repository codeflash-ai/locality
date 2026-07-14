use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

const MANAGED_START: &str = "<!-- LOCALITY_AGENT_GUIDANCE_START -->";
const MANAGED_END: &str = "<!-- LOCALITY_AGENT_GUIDANCE_END -->";
const DEFAULT_NOTION_MOUNT: &str = "~/Library/CloudStorage/Locality/notion";
const MCP_SERVER_NAME: &str = "loc";
const SKILL_DIR_NAME: &str = "locality";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGuidanceInstallReport {
    pub ok: bool,
    pub command: &'static str,
    pub targets: Vec<AgentGuidanceTarget>,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGuidanceTarget {
    pub agent: String,
    pub status: String,
    pub path: Option<String>,
    pub detail: String,
}

#[derive(Clone, Copy)]
enum InstallKind {
    Skill,
    ManagedInstructions,
}

#[derive(Clone, Copy)]
enum McpInstallKind {
    ClaudeDesktopJson,
    CodexToml,
    McpServersJson,
    CopilotServersJson,
}

struct AgentTargetSpec {
    agent: &'static str,
    path: PathBuf,
    kind: InstallKind,
    detected: bool,
    detail: &'static str,
}

struct McpTargetSpec {
    agent: &'static str,
    path: PathBuf,
    kind: McpInstallKind,
    detected: bool,
    detail: &'static str,
}

pub fn install_agent_guidance(mount_path: Option<&str>) -> AgentGuidanceInstallReport {
    let mount_path = normalized_mount_path(mount_path);
    let prompt = suggested_agent_prompt(&mount_path);
    let Some(home) = home_dir() else {
        return AgentGuidanceInstallReport {
            ok: false,
            command: "install_agent_guidance",
            targets: vec![AgentGuidanceTarget {
                agent: "Local agents".to_string(),
                status: "failed".to_string(),
                path: None,
                detail: "Could not find the home directory for agent instruction installation."
                    .to_string(),
            }],
            prompt,
        };
    };

    let mut targets = Vec::new();
    for spec in agent_target_specs(&home) {
        if !spec.detected {
            continue;
        }
        targets.push(install_target(&spec, &mount_path));
    }
    targets.extend(install_mcp_targets(&home));

    if targets.is_empty() {
        targets.push(AgentGuidanceTarget {
            agent: "Locality Notion folder".to_string(),
            status: "available".to_string(),
            path: Some(format!("{mount_path}/AGENTS.md")),
            detail: "No supported local agent install was detected. Locality guidance is still available inside the Notion folder.".to_string(),
        });
    }

    AgentGuidanceInstallReport {
        ok: targets.iter().all(|target| target.status != "failed"),
        command: "install_agent_guidance",
        targets,
        prompt,
    }
}

pub fn uninstall_agent_guidance() -> AgentGuidanceInstallReport {
    let Some(home) = home_dir() else {
        return AgentGuidanceInstallReport {
            ok: false,
            command: "uninstall_agent_guidance",
            targets: vec![AgentGuidanceTarget {
                agent: "Local agents".to_string(),
                status: "failed".to_string(),
                path: None,
                detail: "Could not find the home directory for agent instruction removal."
                    .to_string(),
            }],
            prompt: String::new(),
        };
    };

    let mut targets = Vec::new();
    for spec in agent_target_specs(&home) {
        targets.push(uninstall_target(&spec));
    }
    for spec in mcp_target_specs(&home) {
        targets.push(uninstall_mcp_target(&spec));
    }

    AgentGuidanceInstallReport {
        ok: targets.iter().all(|target| target.status != "failed"),
        command: "uninstall_agent_guidance",
        targets,
        prompt: String::new(),
    }
}

fn install_mcp_targets(home: &Path) -> Vec<AgentGuidanceTarget> {
    let specs = mcp_target_specs(home)
        .into_iter()
        .filter(|spec| spec.detected)
        .collect::<Vec<_>>();
    if specs.is_empty() {
        return Vec::new();
    }

    let token = match localityd::mcp::ensure_mcp_token(&default_state_root()) {
        Ok(token) => token,
        Err(error) => {
            return vec![AgentGuidanceTarget {
                agent: "Locality MCP".to_string(),
                status: "failed".to_string(),
                path: Some(display_path(&localityd::mcp::mcp_token_path(
                    &default_state_root(),
                ))),
                detail: error,
            }];
        }
    };

    specs
        .iter()
        .map(|spec| install_mcp_target(spec, &token))
        .collect()
}

fn agent_target_specs(home: &Path) -> Vec<AgentTargetSpec> {
    let claude_detected = path_exists(home.join(".claude"))
        || command_exists("claude")
        || mac_app_exists(home, "Claude.app");
    let codex_detected = path_exists(home.join(".codex"))
        || command_exists("codex")
        || mac_app_exists(home, "Codex.app");
    let warp_detected =
        command_exists("warp") || mac_app_exists(home, "Warp.app") || warp_state_exists(home);
    let opencode_detected =
        command_exists("opencode") || path_exists(home.join(".config/opencode"));
    let gemini_detected = command_exists("gemini") || path_exists(home.join(".gemini"));
    let copilot_detected = gh_copilot_extension_exists(home) || path_exists(home.join(".copilot"));
    let shared_agents_detected = warp_detected
        || opencode_detected
        || cline_or_roo_detected(home)
        || cursor_detected(home)
        || windsurf_detected(home)
        || zed_detected(home);

    vec![
        AgentTargetSpec {
            agent: "Claude Code / Claude Desktop / Claude Cowork",
            path: skill_path(home, ".claude/skills"),
            kind: InstallKind::Skill,
            detected: claude_detected,
            detail: "Installed the Locality skill for Claude local agents.",
        },
        AgentTargetSpec {
            agent: "Codex",
            path: skill_path(home, ".codex/skills"),
            kind: InstallKind::Skill,
            detected: codex_detected,
            detail: "Installed the Locality skill for Codex.",
        },
        AgentTargetSpec {
            agent: "Warp",
            path: skill_path(home, ".agents/skills"),
            kind: InstallKind::Skill,
            detected: warp_detected,
            detail: "Installed the Locality skill for Warp and Oz skills discovery.",
        },
        AgentTargetSpec {
            agent: "OpenCode",
            path: skill_path(home, ".agents/skills"),
            kind: InstallKind::Skill,
            detected: opencode_detected,
            detail: "Installed the Locality skill for OpenCode's shared skill discovery.",
        },
        AgentTargetSpec {
            agent: "Gemini CLI",
            path: home.join(".gemini/GEMINI.md"),
            kind: InstallKind::ManagedInstructions,
            detected: gemini_detected,
            detail: "Installed a managed Locality section in Gemini's global instructions.",
        },
        AgentTargetSpec {
            agent: "Shared AGENTS.md agents",
            path: home.join(".agents/AGENTS.md"),
            kind: InstallKind::ManagedInstructions,
            detected: shared_agents_detected,
            detail: "Installed fallback Locality instructions for agents that read global AGENTS.md.",
        },
        AgentTargetSpec {
            agent: "GitHub Copilot CLI",
            path: home.join(".copilot/copilot-instructions.md"),
            kind: InstallKind::ManagedInstructions,
            detected: copilot_detected,
            detail: "Installed a managed Locality section in Copilot CLI instructions.",
        },
    ]
}

fn mcp_target_specs(home: &Path) -> Vec<McpTargetSpec> {
    let claude_detected = path_exists(home.join(".claude"))
        || path_exists(home.join(".claude.json"))
        || command_exists("claude")
        || mac_app_exists(home, "Claude.app");
    let codex_detected = path_exists(home.join(".codex"))
        || command_exists("codex")
        || mac_app_exists(home, "Codex.app");
    let cursor_detected = cursor_detected(home);
    let windsurf_detected = windsurf_detected(home);
    let copilot_detected = gh_copilot_extension_exists(home) || path_exists(home.join(".copilot"));

    vec![
        McpTargetSpec {
            agent: "Claude MCP",
            path: home.join(".claude.json"),
            kind: McpInstallKind::McpServersJson,
            detected: claude_detected,
            detail: "Configured the Locality MCP fallback for Claude local agents.",
        },
        McpTargetSpec {
            agent: "Claude Desktop MCP",
            path: claude_desktop_config_path(home),
            kind: McpInstallKind::ClaudeDesktopJson,
            detected: claude_desktop_detected(home),
            detail: "Configured the local Locality MCP fallback for Claude Desktop.",
        },
        McpTargetSpec {
            agent: "Codex MCP",
            path: home.join(".codex/config.toml"),
            kind: McpInstallKind::CodexToml,
            detected: codex_detected,
            detail: "Configured the Locality MCP fallback for Codex.",
        },
        McpTargetSpec {
            agent: "Cursor MCP",
            path: home.join(".cursor/mcp.json"),
            kind: McpInstallKind::McpServersJson,
            detected: cursor_detected,
            detail: "Configured the Locality MCP fallback for Cursor.",
        },
        McpTargetSpec {
            agent: "Windsurf MCP",
            path: home.join(".windsurf/mcp.json"),
            kind: McpInstallKind::McpServersJson,
            detected: windsurf_detected,
            detail: "Configured the Locality MCP fallback for Windsurf.",
        },
        McpTargetSpec {
            agent: "GitHub Copilot MCP",
            path: home.join(".config/github-copilot/intellij/mcp.json"),
            kind: McpInstallKind::CopilotServersJson,
            detected: copilot_detected,
            detail: "Configured the Locality MCP fallback for GitHub Copilot.",
        },
    ]
}

fn install_target(spec: &AgentTargetSpec, mount_path: &str) -> AgentGuidanceTarget {
    let contents = match spec.kind {
        InstallKind::Skill => skill_markdown(mount_path),
        InstallKind::ManagedInstructions => managed_instruction_block(mount_path),
    };
    let result = match spec.kind {
        InstallKind::Skill => write_if_changed(&spec.path, &contents),
        InstallKind::ManagedInstructions => write_managed_section(&spec.path, &contents),
    };

    match result {
        Ok(action) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: action.to_string(),
            path: Some(display_path(&spec.path)),
            detail: spec.detail.to_string(),
        },
        Err(error) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: "failed".to_string(),
            path: Some(display_path(&spec.path)),
            detail: error,
        },
    }
}

fn uninstall_target(spec: &AgentTargetSpec) -> AgentGuidanceTarget {
    let result = match spec.kind {
        InstallKind::Skill => remove_locality_skill(&spec.path),
        InstallKind::ManagedInstructions => remove_managed_section_file(&spec.path),
    };

    match result {
        Ok(action) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: action.to_string(),
            path: Some(display_path(&spec.path)),
            detail: "Removed Locality-managed agent guidance when present.".to_string(),
        },
        Err(error) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: "failed".to_string(),
            path: Some(display_path(&spec.path)),
            detail: error,
        },
    }
}

fn skill_path(home: &Path, skills_root: &str) -> PathBuf {
    home.join(skills_root).join(SKILL_DIR_NAME).join("SKILL.md")
}

fn install_mcp_target(spec: &McpTargetSpec, token: &str) -> AgentGuidanceTarget {
    let result = match spec.kind {
        McpInstallKind::ClaudeDesktopJson => install_claude_desktop_mcp_config(&spec.path),
        McpInstallKind::CodexToml => install_codex_mcp_config(&spec.path, token),
        McpInstallKind::McpServersJson => install_mcp_servers_json_config(&spec.path, token),
        McpInstallKind::CopilotServersJson => {
            install_copilot_servers_json_config(&spec.path, token)
        }
    };

    match result {
        Ok(action) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: action.to_string(),
            path: Some(display_path(&spec.path)),
            detail: spec.detail.to_string(),
        },
        Err(error) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: "failed".to_string(),
            path: Some(display_path(&spec.path)),
            detail: error,
        },
    }
}

fn uninstall_mcp_target(spec: &McpTargetSpec) -> AgentGuidanceTarget {
    let result = match spec.kind {
        McpInstallKind::ClaudeDesktopJson | McpInstallKind::McpServersJson => {
            remove_mcp_servers_json_config(&spec.path)
        }
        McpInstallKind::CodexToml => remove_codex_mcp_config(&spec.path),
        McpInstallKind::CopilotServersJson => remove_copilot_servers_json_config(&spec.path),
    };

    match result {
        Ok(action) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: action.to_string(),
            path: Some(display_path(&spec.path)),
            detail: "Removed the Locality MCP fallback entry when present.".to_string(),
        },
        Err(error) => AgentGuidanceTarget {
            agent: spec.agent.to_string(),
            status: "failed".to_string(),
            path: Some(display_path(&spec.path)),
            detail: error,
        },
    }
}

fn skill_markdown(mount_path: &str) -> String {
    format!(
        r#"---
name: locality
description: Use Locality when the user wants to find, read, or edit Notion/company docs through local filesystem files.
---

# Locality

Locality projects connected company sources, including Notion, into the local filesystem so agents can edit normal Markdown files and let the user review/push the changes back.

## Where to work

- Notion files are under `{mount_path}`.
- Connector-local guidance is available at `{mount_path}/AGENTS.md` and `{mount_path}/CLAUDE.md`.
- Online-only files hydrate automatically when opened by the filesystem.

## Safe workflow

1. If the user gives a Notion URL, run `loc locate <url>` and edit the printed local Markdown path.
2. Edit the local Markdown file directly.
3. Do not edit Locality identity frontmatter, block IDs, `::loc{{...}}` directives, `_schema.yaml`, `AGENTS.md`, or `CLAUDE.md` unless explicitly asked.
4. Unless the user asked you to sync back to Notion, leave edits pending for Locality review and tell the user what changed.
5. Use `loc status` only when you need to inspect pending changes; regular clean files hydrate automatically on open.
6. If desktop Live Mode is on, safe local edits may sync automatically. Do not run routine `loc pull` or `loc push` after every edit.
7. If the user asks you to sync back to Notion, update Notion, publish, or apply the edit remotely, do not stop after local edits. Run `loc diff <file>` first, then `loc push <file> -y` for safe plans.
8. If push says the remote changed since last sync, run `loc pull <file>`, resolve any inline conflict markers in the Markdown, rerun `loc diff <file>`, then push again.

## Creating Notion Content

- Read `{mount_path}/AGENTS.md` for connector-specific creation rules.
- Prefer `loc create page --title "New Page" --parent <parent-directory>` for new pages.
- Pages are directories; edit or create the `page.md` inside the page directory.
- To create a child page, create `parent-page/new-page/page.md`.
- New `page.md` files need YAML frontmatter with `title: "..."` and no `loc:` identity block.
- Existing files already have an `loc:` block; preserve it and edit only the body, `title`, and supported property frontmatter.
- Database rows can be created as `database/new-row/page.md` or, where supported, direct `database/new-row.md` files.

## MCP fallback

If your sandbox cannot run the host `loc` CLI, such as in Claude Cowork or
another isolated agent runtime, fall back to the Locality MCP tool named `loc`.
Pass the same CLI arguments as JSON `argv`, for example:

```json
{{"argv":["status","{mount_path}","--json"]}}
```

Locality configures this MCP fallback automatically for supported local agents.
Prefer direct CLI execution whenever it is available.

## Suggested user prompt

```text
{}
```
"#,
        suggested_agent_prompt(mount_path)
    )
}

fn managed_instruction_block(mount_path: &str) -> String {
    format!(
        "{MANAGED_START}\n{}\n{MANAGED_END}\n",
        skill_markdown(mount_path).trim()
    )
}

fn suggested_agent_prompt(mount_path: &str) -> String {
    format!(
        "Use Locality to edit my Notion workspace. Open the Notion files under {mount_path}, make the requested edits directly in Markdown, and leave the changes pending for Locality review unless I ask you to sync back to Notion. When I do, run `loc diff <file>` and then `loc push <file> -y` for safe plans."
    )
}

fn write_if_changed(path: &Path, contents: &str) -> Result<&'static str, String> {
    if let Ok(existing) = fs::read_to_string(path)
        && existing == contents
    {
        return Ok("installed");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create `{}`: {error}", parent.display()))?;
    }
    fs::write(path, contents)
        .map_err(|error| format!("Could not write `{}`: {error}", path.display()))?;
    Ok("installed")
}

fn write_managed_section(path: &Path, block: &str) -> Result<&'static str, String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let next = replace_managed_section(&existing, block);
    if existing == next {
        return Ok("installed");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create `{}`: {error}", parent.display()))?;
    }
    fs::write(path, next)
        .map_err(|error| format!("Could not write `{}`: {error}", path.display()))?;
    Ok("installed")
}

fn remove_locality_skill(path: &Path) -> Result<&'static str, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("unchanged"),
        Err(error) => return Err(format!("Could not read `{}`: {error}", path.display())),
    };
    if !contents.contains("name: locality") {
        return Ok("unchanged");
    }
    fs::remove_file(path)
        .map_err(|error| format!("Could not remove `{}`: {error}", path.display()))?;
    Ok("removed")
}

fn remove_managed_section_file(path: &Path) -> Result<&'static str, String> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("unchanged"),
        Err(error) => return Err(format!("Could not read `{}`: {error}", path.display())),
    };
    let next = remove_managed_section(&existing);
    if existing == next {
        return Ok("unchanged");
    }
    fs::write(path, next)
        .map_err(|error| format!("Could not write `{}`: {error}", path.display()))?;
    Ok("removed")
}

fn remove_codex_mcp_config(path: &Path) -> Result<&'static str, String> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("unchanged"),
        Err(error) => return Err(format!("Could not read `{}`: {error}", path.display())),
    };
    let next = remove_toml_table(&existing, "mcp_servers.loc");
    if existing == next {
        return Ok("unchanged");
    }
    write_private_if_changed(path, &next)?;
    Ok("removed")
}

fn install_codex_mcp_config(path: &Path, token: &str) -> Result<&'static str, String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let stripped = remove_toml_table(&existing, "mcp_servers.loc");
    let block = format!(
        "[mcp_servers.{MCP_SERVER_NAME}]\nurl = \"{}\"\nhttp_headers = {{ Authorization = \"Bearer {token}\" }}\n",
        mcp_endpoint()
    );
    let next = append_toml_block(&stripped, &block);
    write_private_if_changed(path, &next)
}

fn remove_toml_table(contents: &str, table: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in contents.lines() {
        if let Some(name) = toml_table_name(line) {
            if name == table || name.starts_with(&format!("{table}.")) {
                skipping = true;
                continue;
            }
            if skipping {
                skipping = false;
            }
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut next = out.join("\n");
    if contents.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    next
}

fn toml_table_name(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        Some(trimmed.trim_start_matches('[').trim_end_matches(']').trim())
    } else {
        None
    }
}

fn append_toml_block(existing: &str, block: &str) -> String {
    let trimmed = existing.trim_end();
    if trimmed.is_empty() {
        block.to_string()
    } else {
        format!("{trimmed}\n\n{block}")
    }
}

fn install_mcp_servers_json_config(path: &Path, token: &str) -> Result<&'static str, String> {
    let mut root = read_json_config(path)?;
    let object = ensure_object(&mut root);
    let servers = object
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let servers = ensure_object(servers);
    servers.insert(MCP_SERVER_NAME.to_string(), mcp_server_json(token));
    write_json_config(path, &root)
}

fn remove_mcp_servers_json_config(path: &Path) -> Result<&'static str, String> {
    remove_json_mcp_server(path, "mcpServers")
}

fn install_claude_desktop_mcp_config(path: &Path) -> Result<&'static str, String> {
    let mut root = read_json_config(path)?;
    let object = ensure_object(&mut root);
    let servers = object
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let servers = ensure_object(servers);
    servers.insert(
        MCP_SERVER_NAME.to_string(),
        claude_desktop_mcp_server_json(),
    );
    write_json_config(path, &root)
}

fn install_copilot_servers_json_config(path: &Path, token: &str) -> Result<&'static str, String> {
    let mut root = read_json_config(path)?;
    let object = ensure_object(&mut root);
    let servers = object
        .entry("servers".to_string())
        .or_insert_with(|| json!({}));
    let servers = ensure_object(servers);
    servers.insert(
        MCP_SERVER_NAME.to_string(),
        json!({
            "url": mcp_endpoint(),
            "requestInit": {
                "headers": {
                    "Authorization": format!("Bearer {token}")
                }
            }
        }),
    );
    write_json_config(path, &root)
}

fn remove_copilot_servers_json_config(path: &Path) -> Result<&'static str, String> {
    remove_json_mcp_server(path, "servers")
}

fn remove_json_mcp_server(path: &Path, container_key: &str) -> Result<&'static str, String> {
    if !path.exists() {
        return Ok("unchanged");
    }
    let mut root = read_json_config(path)?;
    let Some(object) = root.as_object_mut() else {
        return Ok("unchanged");
    };
    let Some(servers) = object.get_mut(container_key).and_then(Value::as_object_mut) else {
        return Ok("unchanged");
    };
    if servers.remove(MCP_SERVER_NAME).is_none() {
        return Ok("unchanged");
    }
    write_json_config(path, &root)?;
    Ok("removed")
}

fn mcp_server_json(token: &str) -> Value {
    json!({
        "type": "http",
        "url": mcp_endpoint(),
        "headers": {
            "Authorization": format!("Bearer {token}")
        }
    })
}

fn claude_desktop_mcp_server_json() -> Value {
    json!({
        "command": loc_cli_command(),
        "args": ["mcp"],
        "env": {
            "LOCALITY_STATE_DIR": default_state_root().display().to_string()
        }
    })
}

fn loc_cli_command() -> String {
    if let Ok(current) = env::current_exe() {
        return loc_cli_command_for_current_exe(&current);
    }
    "loc".to_string()
}

fn loc_cli_command_for_current_exe(current: &Path) -> String {
    if let Some(parent) = current.parent() {
        let sibling = parent.join(binary_name("loc"));
        if sibling.is_file() {
            return sibling.display().to_string();
        }
    }
    "loc".to_string()
}

fn binary_name(name: &str) -> String {
    #[cfg(windows)]
    {
        format!("{name}.exe")
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

fn read_json_config(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read `{}`: {error}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(json!({}));
    }
    let without_line_comments = strip_json_line_comments(&contents);
    serde_json::from_str(&without_line_comments)
        .map_err(|error| format!("Could not parse `{}` as JSON: {error}", path.display()))
}

fn strip_json_line_comments(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("JSON value is object")
}

fn write_json_config(path: &Path, value: &Value) -> Result<&'static str, String> {
    let contents = serde_json::to_string_pretty(value)
        .map_err(|error| format!("Could not serialize `{}`: {error}", path.display()))?;
    write_private_if_changed(path, &(contents + "\n"))
}

fn write_private_if_changed(path: &Path, contents: &str) -> Result<&'static str, String> {
    if let Ok(existing) = fs::read_to_string(path)
        && existing == contents
    {
        protect_private_file(path)?;
        return Ok("installed");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create `{}`: {error}", parent.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not write `{}`: {error}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("Could not write `{}`: {error}", path.display()))?;
    protect_private_file(path)?;
    Ok("installed")
}

fn protect_private_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect `{}`: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn replace_managed_section(existing: &str, block: &str) -> String {
    let Some(start) = existing.find(MANAGED_START) else {
        let trimmed = existing.trim_end();
        return if trimmed.is_empty() {
            block.to_string()
        } else {
            format!("{trimmed}\n\n{block}")
        };
    };
    let Some(relative_end) = existing[start..].find(MANAGED_END) else {
        let trimmed = existing.trim_end();
        return format!("{trimmed}\n\n{block}");
    };
    let end = start + relative_end + MANAGED_END.len();
    let mut next = String::new();
    next.push_str(existing[..start].trim_end());
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(block);
    let suffix = existing[end..].trim_start_matches(['\r', '\n']);
    if !suffix.is_empty() {
        next.push('\n');
        next.push_str(suffix);
    }
    next
}

fn remove_managed_section(existing: &str) -> String {
    let Some(start) = existing.find(MANAGED_START) else {
        return existing.to_string();
    };
    let Some(relative_end) = existing[start..].find(MANAGED_END) else {
        return existing.to_string();
    };
    let end = start + relative_end + MANAGED_END.len();
    let mut next = String::new();
    next.push_str(existing[..start].trim_end());
    let suffix = existing[end..].trim_start_matches(['\r', '\n']);
    if !next.is_empty() && !suffix.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(suffix);
    next
}

fn normalized_mount_path(mount_path: Option<&str>) -> String {
    let trimmed = mount_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(DEFAULT_NOTION_MOUNT)
        .trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    locality_platform::user_home()
}

fn default_state_root() -> PathBuf {
    if let Ok(value) = env::var("LOCALITY_STATE_DIR") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        if let Ok(current_dir) = env::current_dir() {
            return current_dir.join(path);
        }
    }

    locality_platform::default_state_root()
}

fn mcp_endpoint() -> String {
    format!("http://{}/mcp", localityd::mcp::DEFAULT_MCP_ADDR)
}

fn command_exists(command: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    command_exists_in_paths(command, paths)
}

fn command_exists_in_paths(command: &str, paths: OsString) -> bool {
    env::split_paths(&paths).any(|path| path.join(command).is_file())
}

fn mac_app_exists(home: &Path, app_name: &str) -> bool {
    path_exists(PathBuf::from("/Applications").join(app_name))
        || path_exists(home.join("Applications").join(app_name))
}

fn claude_desktop_config_path(home: &Path) -> PathBuf {
    claude_desktop_config_dir(home).join("claude_desktop_config.json")
}

#[cfg(windows)]
fn claude_desktop_config_dir(home: &Path) -> PathBuf {
    if let Some(msix_root) = windows_claude_desktop_msix_package_root(home) {
        return msix_root.join("LocalCache/Roaming/Claude");
    }
    env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"))
        .join("Claude")
}

#[cfg(not(windows))]
fn claude_desktop_config_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Claude")
}

#[cfg(windows)]
fn claude_desktop_detected(home: &Path) -> bool {
    path_exists(claude_desktop_config_dir(home)) || windows_claude_desktop_app_exists(home)
}

#[cfg(not(windows))]
fn claude_desktop_detected(home: &Path) -> bool {
    mac_app_exists(home, "Claude.app")
}

#[cfg(windows)]
fn windows_claude_desktop_app_exists(home: &Path) -> bool {
    let local_appdata = env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Local"));
    path_exists(local_appdata.join("Programs/Claude/Claude.exe"))
        || windows_claude_desktop_msix_package_root_in(&local_appdata).is_some()
}

#[cfg(windows)]
fn windows_claude_desktop_msix_package_root(home: &Path) -> Option<PathBuf> {
    let local_appdata = env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Local"));
    windows_claude_desktop_msix_package_root_in(&local_appdata)
}

#[cfg(windows)]
fn windows_claude_desktop_msix_package_root_in(local_appdata: &Path) -> Option<PathBuf> {
    fs::read_dir(local_appdata.join("Packages"))
        .ok()?
        .flatten()
        .find_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("claude_") || name.starts_with("anthropic.claude") {
                Some(entry.path())
            } else {
                None
            }
        })
}

fn warp_state_exists(home: &Path) -> bool {
    path_exists(home.join("Library/Application Support/dev.warp.Warp-Stable"))
        || path_exists(home.join("Library/Application Support/Warp"))
        || path_exists(home.join(".warp"))
}

fn cline_or_roo_detected(home: &Path) -> bool {
    path_exists(home.join("Documents/Cline"))
        || path_exists(home.join(".cline"))
        || path_exists(home.join(".roo"))
        || vscode_extension_exists(home, "cline")
        || vscode_extension_exists(home, "roo")
}

fn cursor_detected(home: &Path) -> bool {
    path_exists(home.join(".cursor")) || mac_app_exists(home, "Cursor.app")
}

fn windsurf_detected(home: &Path) -> bool {
    path_exists(home.join(".windsurf"))
        || path_exists(home.join(".codeium/windsurf"))
        || mac_app_exists(home, "Windsurf.app")
}

fn zed_detected(home: &Path) -> bool {
    path_exists(home.join(".config/zed")) || mac_app_exists(home, "Zed.app")
}

fn gh_copilot_extension_exists(home: &Path) -> bool {
    path_exists(home.join(".config/gh/extensions/gh-copilot"))
        || path_exists(home.join(".local/share/gh/extensions/gh-copilot"))
}

fn vscode_extension_exists(home: &Path, needle: &str) -> bool {
    let dirs = [
        home.join(".vscode/extensions"),
        home.join(".vscode-insiders/extensions"),
        home.join("Library/Application Support/Code/User/globalStorage"),
    ];
    dirs.iter().any(|dir| {
        fs::read_dir(dir).ok().is_some_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(needle)
            })
        })
    })
}

fn path_exists(path: PathBuf) -> bool {
    path.exists()
}

fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn skill_mentions_mount_and_review_workflow() {
        let skill = skill_markdown("~/Library/CloudStorage/Locality/notion");

        assert!(skill.contains("name: locality"));
        assert!(skill.contains("~/Library/CloudStorage/Locality/notion"));
        assert!(skill.contains("pending for Locality review"));
        assert!(skill.contains("sync back to Notion"));
        assert!(skill.contains("If desktop Live Mode is on"));
        assert!(skill.contains("Do not run routine `loc pull` or `loc push`"));
        assert!(skill.contains("loc diff <file>"));
        assert!(skill.contains("Creating Notion Content"));
        assert!(skill.contains("loc create page --title"));
        assert!(skill.contains("parent-page/new-page/page.md"));
        assert!(skill.contains("no `loc:` identity block"));
        assert!(skill.contains("remote changed since last sync"));
        assert!(skill.contains("fall back to the Locality MCP tool named `loc`"));
        assert!(skill.contains("Claude Cowork"));
        assert!(skill.contains("Locality configures this MCP fallback automatically"));
    }

    #[test]
    fn managed_section_replace_preserves_user_content() {
        let existing = "Before\n\n<!-- LOCALITY_AGENT_GUIDANCE_START -->\nold\n<!-- LOCALITY_AGENT_GUIDANCE_END -->\n\nAfter\n";
        let next = replace_managed_section(existing, "new block\n");

        assert!(next.contains("Before"));
        assert!(next.contains("new block"));
        assert!(next.contains("After"));
        assert!(!next.contains("\nold\n"));
    }

    #[test]
    fn managed_section_remove_preserves_user_content() {
        let existing = "Before\n\n<!-- LOCALITY_AGENT_GUIDANCE_START -->\nold\n<!-- LOCALITY_AGENT_GUIDANCE_END -->\n\nAfter\n";
        let next = remove_managed_section(existing);

        assert_eq!(next, "Before\n\nAfter\n");
    }

    #[test]
    fn normalized_blank_mount_path_uses_default_notion_mount() {
        assert_eq!(normalized_mount_path(Some("  ")), DEFAULT_NOTION_MOUNT);
        assert_eq!(
            normalized_mount_path(Some("~/Library/CloudStorage/Locality/notion/")),
            "~/Library/CloudStorage/Locality/notion"
        );
    }

    #[test]
    fn normalized_mount_preserves_root_path() {
        assert_eq!(normalized_mount_path(Some("/")), "/");
    }

    #[test]
    fn install_target_writes_locality_skill_name() {
        let temp = temp_root("locality-agent-guidance-skill-name");
        let spec = AgentTargetSpec {
            agent: "Codex",
            path: temp.join(".codex/skills/locality/SKILL.md"),
            kind: InstallKind::Skill,
            detected: true,
            detail: "Installed the Locality skill for Codex.",
        };
        let target = install_target(&spec, "~/Library/CloudStorage/Locality/notion");
        let skill = fs::read_to_string(temp.join(".codex/skills/locality/SKILL.md"))
            .expect("read locality skill");

        assert_eq!(target.status, "installed");
        assert!(skill.contains("name: locality"));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn command_detection_respects_path() {
        let temp = temp_root("loc-agent-guidance-path");
        let bin = temp.join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        fs::write(bin.join("codex"), "").expect("write command");
        assert!(command_exists_in_paths(
            "codex",
            OsString::from(bin.as_os_str())
        ));
        assert!(!command_exists_in_paths(
            "missing-agent",
            OsString::from(bin.as_os_str())
        ));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn copilot_detection_requires_copilot_state() {
        let temp = temp_root("loc-agent-guidance-copilot");

        assert!(!gh_copilot_extension_exists(&temp));
        fs::create_dir_all(temp.join(".config/gh/extensions/gh-copilot"))
            .expect("create gh copilot extension");
        assert!(gh_copilot_extension_exists(&temp));

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(windows)]
    #[test]
    fn windows_claude_desktop_mcp_uses_roaming_appdata_config() {
        let temp = temp_root("loc-agent-guidance-windows-claude-desktop");
        let appdata = temp.join("AppData/Roaming");
        let localappdata = temp.join("AppData/Local");
        fs::create_dir_all(appdata.join("Claude")).expect("create Claude appdata");

        let specs = mcp_target_specs_with_windows_appdata(&temp, &appdata, Some(&localappdata));

        let spec = specs
            .iter()
            .find(|spec| spec.agent == "Claude Desktop MCP")
            .expect("Claude Desktop MCP target");

        assert_eq!(
            spec.path,
            temp.join("AppData/Roaming/Claude/claude_desktop_config.json")
        );
        assert!(spec.detected);

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(windows)]
    #[test]
    fn windows_claude_desktop_mcp_detects_installed_app_without_roaming_config() {
        let temp = temp_root("loc-agent-guidance-windows-claude-desktop-installed");
        let appdata = temp.join("AppData/Roaming");
        let localappdata = temp.join("AppData/Local");
        let claude_exe = localappdata.join("Programs/Claude/Claude.exe");
        fs::create_dir_all(claude_exe.parent().expect("claude exe parent"))
            .expect("create Claude install dir");
        fs::write(&claude_exe, "").expect("write Claude.exe");

        let specs = mcp_target_specs_with_windows_appdata(&temp, &appdata, Some(&localappdata));

        let spec = specs
            .iter()
            .find(|spec| spec.agent == "Claude Desktop MCP")
            .expect("Claude Desktop MCP target");

        assert_eq!(
            spec.path,
            temp.join("AppData/Roaming/Claude/claude_desktop_config.json")
        );
        assert!(spec.detected);

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(windows)]
    #[test]
    fn windows_claude_desktop_mcp_detects_msix_package_without_roaming_config() {
        let temp = temp_root("loc-agent-guidance-windows-claude-desktop-msix");
        let appdata = temp.join("AppData/Roaming");
        let localappdata = temp.join("AppData/Local");
        let msix_root = localappdata.join("Packages/Claude_pzs8sxrjxfjjc");
        fs::create_dir_all(&msix_root).expect("create Claude MSIX package dir");

        let specs = mcp_target_specs_with_windows_appdata(&temp, &appdata, Some(&localappdata));

        let spec = specs
            .iter()
            .find(|spec| spec.agent == "Claude Desktop MCP")
            .expect("Claude Desktop MCP target");

        assert_eq!(
            spec.path,
            msix_root.join("LocalCache/Roaming/Claude/claude_desktop_config.json")
        );
        assert!(spec.detected);

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(windows)]
    fn mcp_target_specs_with_windows_appdata(
        home: &Path,
        appdata: &Path,
        localappdata: Option<&Path>,
    ) -> Vec<McpTargetSpec> {
        let _env_lock = ENV_LOCK.lock().expect("env lock");
        let previous_appdata = env::var_os("APPDATA");
        let previous_localappdata = env::var_os("LOCALAPPDATA");
        // SAFETY: This unit test temporarily points APPDATA and LOCALAPPDATA at
        // private temp directories, then restores the previous values before
        // returning.
        unsafe {
            env::set_var("APPDATA", appdata);
            if let Some(localappdata) = localappdata {
                env::set_var("LOCALAPPDATA", localappdata);
            }
        }

        let specs = mcp_target_specs(home);
        // SAFETY: Restores APPDATA and LOCALAPPDATA after the scoped test
        // mutation above.
        unsafe {
            if let Some(previous) = previous_appdata {
                env::set_var("APPDATA", previous);
            } else {
                env::remove_var("APPDATA");
            }
            if localappdata.is_some() {
                if let Some(previous) = previous_localappdata {
                    env::set_var("LOCALAPPDATA", previous);
                } else {
                    env::remove_var("LOCALAPPDATA");
                }
            }
        }
        specs
    }

    #[cfg(windows)]
    #[test]
    fn windows_claude_desktop_mcp_command_prefers_installed_loc_exe() {
        let bin_dir = temp_root("loc-agent-guidance-windows-current-exe-bin");
        let loc_exe = bin_dir.join("loc.exe");
        fs::write(&loc_exe, "").expect("write test loc.exe");
        let current_exe = bin_dir.join("locality.exe");

        let command = loc_cli_command_for_current_exe(&current_exe);

        let _ = fs::remove_dir_all(bin_dir);

        assert_eq!(command, loc_exe.display().to_string());
    }

    #[test]
    fn codex_mcp_config_replaces_existing_loc_table() {
        let temp = temp_root("loc-agent-guidance-codex-mcp");
        let config = temp.join("config.toml");
        fs::write(
            &config,
            "model = \"gpt\"\n\n[mcp_servers.loc]\nurl = \"old\"\n\n[mcp_servers.other]\nurl = \"keep\"\n",
        )
        .expect("write config");

        install_codex_mcp_config(&config, "secret-token").expect("install config");
        let contents = fs::read_to_string(&config).expect("read config");

        assert!(contents.contains("model = \"gpt\""));
        assert!(contents.contains("[mcp_servers.other]"));
        assert!(contents.contains("[mcp_servers.loc]"));
        assert!(contents.contains("Authorization = \"Bearer secret-token\""));
        assert!(!contents.contains("url = \"old\""));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn mcp_json_config_preserves_other_servers() {
        let temp = temp_root("loc-agent-guidance-json-mcp");
        let config = temp.join("mcp.json");
        fs::write(
            &config,
            r#"{"mcpServers":{"other":{"url":"https://example.test/mcp"}}}"#,
        )
        .expect("write config");

        install_mcp_servers_json_config(&config, "secret-token").expect("install config");
        let contents = fs::read_to_string(&config).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("json");

        assert_eq!(
            json["mcpServers"]["other"]["url"],
            "https://example.test/mcp"
        );
        assert_eq!(json["mcpServers"]["loc"]["type"], "http");
        assert_eq!(
            json["mcpServers"]["loc"]["headers"]["Authorization"],
            "Bearer secret-token"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn claude_desktop_mcp_config_uses_local_stdio_command() {
        let temp = temp_root("loc-agent-guidance-claude-desktop-mcp");
        let config = temp.join("claude_desktop_config.json");
        fs::write(
            &config,
            r#"{"mcpServers":{"other":{"command":"node","args":["server.js"]},"loc":{"type":"http","url":"http://127.0.0.1:38568/mcp","headers":{"Authorization":"Bearer stale"}}}}"#,
        )
        .expect("write config");

        install_claude_desktop_mcp_config(&config).expect("install config");
        let contents = fs::read_to_string(&config).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("json");

        assert_eq!(json["mcpServers"]["other"]["command"], "node");
        assert_eq!(json["mcpServers"]["loc"]["command"], "loc");
        assert_eq!(json["mcpServers"]["loc"]["args"], json!(["mcp"]));
        assert!(json["mcpServers"]["loc"]["env"]["LOCALITY_STATE_DIR"].is_string());
        assert!(json["mcpServers"]["loc"].get("url").is_none());
        assert!(json["mcpServers"]["loc"].get("headers").is_none());
        assert!(json["mcpServers"]["loc"].get("type").is_none());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn copilot_json_config_accepts_comment_only_template() {
        let temp = temp_root("loc-agent-guidance-copilot-mcp");
        let config = temp.join("mcp.json");
        fs::write(
            &config,
            "{\n  \"servers\": {\n    // add your servers here\n  }\n}\n",
        )
        .expect("write config");

        install_copilot_servers_json_config(&config, "secret-token").expect("install config");
        let contents = fs::read_to_string(&config).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("json");

        assert_eq!(
            json["servers"]["loc"]["requestInit"]["headers"]["Authorization"],
            "Bearer secret-token"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_codex_mcp_config_preserves_other_tables() {
        let temp = temp_root("loc-agent-guidance-remove-codex-mcp");
        let config = temp.join("config.toml");
        fs::write(
            &config,
            "model = \"gpt\"\n\n[mcp_servers.loc]\nurl = \"old\"\n\n[mcp_servers.other]\nurl = \"keep\"\n",
        )
        .expect("write config");

        let action = remove_codex_mcp_config(&config).expect("remove loc MCP");
        let contents = fs::read_to_string(&config).expect("read config");

        assert_eq!(action, "removed");
        assert!(contents.contains("model = \"gpt\""));
        assert!(contents.contains("[mcp_servers.other]"));
        assert!(!contents.contains("[mcp_servers.loc]"));
        assert!(!contents.contains("url = \"old\""));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_json_mcp_config_preserves_other_servers() {
        let temp = temp_root("loc-agent-guidance-remove-json-mcp");
        let config = temp.join("mcp.json");
        fs::write(
            &config,
            r#"{"mcpServers":{"other":{"url":"https://example.test/mcp"},"loc":{"url":"http://127.0.0.1:38568/mcp"}}}"#,
        )
        .expect("write config");

        let action = remove_mcp_servers_json_config(&config).expect("remove loc MCP");
        let contents = fs::read_to_string(&config).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("json");

        assert_eq!(action, "removed");
        assert_eq!(
            json["mcpServers"]["other"]["url"],
            "https://example.test/mcp"
        );
        assert!(json["mcpServers"]["loc"].is_null());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_copilot_mcp_config_preserves_other_servers() {
        let temp = temp_root("loc-agent-guidance-remove-copilot-mcp");
        let config = temp.join("mcp.json");
        fs::write(
            &config,
            r#"{"servers":{"other":{"url":"https://example.test/mcp"},"loc":{"url":"http://127.0.0.1:38568/mcp"}}}"#,
        )
        .expect("write config");

        let action = remove_copilot_servers_json_config(&config).expect("remove loc MCP");
        let contents = fs::read_to_string(&config).expect("read config");
        let json: Value = serde_json::from_str(&contents).expect("json");

        assert_eq!(action, "removed");
        assert_eq!(json["servers"]["other"]["url"], "https://example.test/mcp");
        assert!(json["servers"]["loc"].is_null());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_locality_skill_deletes_only_locality_skill() {
        let temp = temp_root("loc-agent-guidance-remove-skill");
        let skill = temp.join("SKILL.md");
        fs::write(&skill, "---\nname: locality\n---\n# Locality\n").expect("write skill");

        let action = remove_locality_skill(&skill).expect("remove skill");

        assert_eq!(action, "removed");
        assert!(!skill.exists());

        let other = temp.join("OTHER.md");
        fs::write(&other, "---\nname: other\n---\n# Other\n").expect("write other skill");
        let action = remove_locality_skill(&other).expect("skip other skill");

        assert_eq!(action, "unchanged");
        assert!(other.exists());
        let _ = fs::remove_dir_all(temp);
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp root");
        root
    }
}
