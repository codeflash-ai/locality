use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

const MANAGED_START: &str = "<!-- AFS_AGENT_GUIDANCE_START -->";
const MANAGED_END: &str = "<!-- AFS_AGENT_GUIDANCE_END -->";
const DEFAULT_NOTION_MOUNT: &str = "~/Library/CloudStorage/AFS/notion";
const MCP_SERVER_NAME: &str = "afs";

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
            agent: "AFS Notion folder".to_string(),
            status: "available".to_string(),
            path: Some(format!("{mount_path}/AGENTS.md")),
            detail: "No supported local agent install was detected. AFS guidance is still available inside the Notion folder.".to_string(),
        });
    }

    AgentGuidanceInstallReport {
        ok: targets.iter().all(|target| target.status != "failed"),
        command: "install_agent_guidance",
        targets,
        prompt,
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

    let token = match afsd::mcp::ensure_mcp_token(&default_state_root()) {
        Ok(token) => token,
        Err(error) => {
            return vec![AgentGuidanceTarget {
                agent: "AFS MCP".to_string(),
                status: "failed".to_string(),
                path: Some(display_path(&afsd::mcp::mcp_token_path(
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
            path: home.join(".claude/skills/afs/SKILL.md"),
            kind: InstallKind::Skill,
            detected: claude_detected,
            detail: "Installed the AFS skill for Claude local agents.",
        },
        AgentTargetSpec {
            agent: "Codex",
            path: home.join(".codex/skills/afs/SKILL.md"),
            kind: InstallKind::Skill,
            detected: codex_detected,
            detail: "Installed the AFS skill for Codex.",
        },
        AgentTargetSpec {
            agent: "Warp",
            path: home.join(".agents/skills/afs/SKILL.md"),
            kind: InstallKind::Skill,
            detected: warp_detected,
            detail: "Installed the AFS skill for Warp and Oz skills discovery.",
        },
        AgentTargetSpec {
            agent: "OpenCode",
            path: home.join(".agents/skills/afs/SKILL.md"),
            kind: InstallKind::Skill,
            detected: opencode_detected,
            detail: "Installed the AFS skill for OpenCode's shared skill discovery.",
        },
        AgentTargetSpec {
            agent: "Gemini CLI",
            path: home.join(".gemini/GEMINI.md"),
            kind: InstallKind::ManagedInstructions,
            detected: gemini_detected,
            detail: "Installed a managed AFS section in Gemini's global instructions.",
        },
        AgentTargetSpec {
            agent: "Shared AGENTS.md agents",
            path: home.join(".agents/AGENTS.md"),
            kind: InstallKind::ManagedInstructions,
            detected: shared_agents_detected,
            detail: "Installed fallback AFS instructions for agents that read global AGENTS.md.",
        },
        AgentTargetSpec {
            agent: "GitHub Copilot CLI",
            path: home.join(".copilot/copilot-instructions.md"),
            kind: InstallKind::ManagedInstructions,
            detected: copilot_detected,
            detail: "Installed a managed AFS section in Copilot CLI instructions.",
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
            detail: "Configured the AFS MCP fallback for Claude local agents.",
        },
        McpTargetSpec {
            agent: "Claude Desktop MCP",
            path: home.join("Library/Application Support/Claude/claude_desktop_config.json"),
            kind: McpInstallKind::McpServersJson,
            detected: mac_app_exists(home, "Claude.app"),
            detail: "Configured the AFS MCP fallback for Claude Desktop.",
        },
        McpTargetSpec {
            agent: "Codex MCP",
            path: home.join(".codex/config.toml"),
            kind: McpInstallKind::CodexToml,
            detected: codex_detected,
            detail: "Configured the AFS MCP fallback for Codex.",
        },
        McpTargetSpec {
            agent: "Cursor MCP",
            path: home.join(".cursor/mcp.json"),
            kind: McpInstallKind::McpServersJson,
            detected: cursor_detected,
            detail: "Configured the AFS MCP fallback for Cursor.",
        },
        McpTargetSpec {
            agent: "Windsurf MCP",
            path: home.join(".windsurf/mcp.json"),
            kind: McpInstallKind::McpServersJson,
            detected: windsurf_detected,
            detail: "Configured the AFS MCP fallback for Windsurf.",
        },
        McpTargetSpec {
            agent: "GitHub Copilot MCP",
            path: home.join(".config/github-copilot/intellij/mcp.json"),
            kind: McpInstallKind::CopilotServersJson,
            detected: copilot_detected,
            detail: "Configured the AFS MCP fallback for GitHub Copilot.",
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

fn install_mcp_target(spec: &McpTargetSpec, token: &str) -> AgentGuidanceTarget {
    let result = match spec.kind {
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

fn skill_markdown(mount_path: &str) -> String {
    format!(
        r#"---
name: afs
description: Use AFS when the user wants to find, read, or edit Notion/company docs through local filesystem files.
---

# AFS

AFS projects connected company sources, including Notion, into the local filesystem so agents can edit normal Markdown files and let the user review/push the changes back.

## Where to work

- Notion files are under `{mount_path}`.
- Connector-local guidance is available at `{mount_path}/AGENTS.md` and `{mount_path}/CLAUDE.md`.
- Online-only files hydrate automatically when opened by the filesystem.

## Safe workflow

1. If the user gives a Notion URL, locate the matching local Markdown file before editing.
2. Edit the local Markdown file directly.
3. Do not edit AFS identity frontmatter, block IDs, `::afs{{...}}` directives, `_schema.yaml`, `AGENTS.md`, or `CLAUDE.md` unless explicitly asked.
4. Leave edits pending for AFS review and tell the user what changed.
5. If an AFS CLI is available, use `afs status` only when you need to inspect pending changes; regular clean files hydrate automatically on open.
6. Only push when the user explicitly asks. Run `afs diff <file>` first, then `afs push <file> -y` for safe plans.
7. If push says the remote changed since last sync, run `afs pull <file>`, resolve any inline conflict markers in the Markdown, rerun `afs diff <file>`, then push again.

## MCP fallback

If your sandbox cannot run the host `afs` CLI, use the MCP tool named `afs`.
Pass the same CLI arguments as JSON `argv`, for example:

```json
{{"argv":["status","{mount_path}","--json"]}}
```

AFS configures this fallback automatically for supported local agents. Prefer
direct CLI execution whenever it is available.

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
        "Use AFS to edit my Notion workspace. Open the Notion files under {mount_path}, make the requested edits directly in Markdown, and leave the changes pending for AFS review."
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

fn install_codex_mcp_config(path: &Path, token: &str) -> Result<&'static str, String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let stripped = remove_toml_table(&existing, "mcp_servers.afs");
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

fn mcp_server_json(token: &str) -> Value {
    json!({
        "type": "http",
        "url": mcp_endpoint(),
        "headers": {
            "Authorization": format!("Bearer {token}")
        }
    })
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
    env::var_os("HOME").map(PathBuf::from)
}

fn default_state_root() -> PathBuf {
    if let Ok(value) = env::var("AFS_STATE_DIR") {
        return PathBuf::from(value);
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".afs");
    }

    PathBuf::from(".afs")
}

fn mcp_endpoint() -> String {
    format!("http://{}/mcp", afsd::mcp::DEFAULT_MCP_ADDR)
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

    #[test]
    fn skill_mentions_mount_and_review_workflow() {
        let skill = skill_markdown("~/Library/CloudStorage/AFS/notion");

        assert!(skill.contains("name: afs"));
        assert!(skill.contains("~/Library/CloudStorage/AFS/notion"));
        assert!(skill.contains("pending for AFS review"));
        assert!(skill.contains("afs diff <file>"));
        assert!(skill.contains("remote changed since last sync"));
        assert!(skill.contains("AFS configures this fallback automatically"));
    }

    #[test]
    fn managed_section_replace_preserves_user_content() {
        let existing = "Before\n\n<!-- AFS_AGENT_GUIDANCE_START -->\nold\n<!-- AFS_AGENT_GUIDANCE_END -->\n\nAfter\n";
        let next = replace_managed_section(existing, "new block\n");

        assert!(next.contains("Before"));
        assert!(next.contains("new block"));
        assert!(next.contains("After"));
        assert!(!next.contains("\nold\n"));
    }

    #[test]
    fn normalized_mount_uses_default_for_empty_input() {
        assert_eq!(normalized_mount_path(Some("  ")), DEFAULT_NOTION_MOUNT);
        assert_eq!(
            normalized_mount_path(Some("~/Library/CloudStorage/AFS/notion/")),
            "~/Library/CloudStorage/AFS/notion"
        );
    }

    #[test]
    fn normalized_mount_preserves_root_path() {
        assert_eq!(normalized_mount_path(Some("/")), "/");
    }

    #[test]
    fn command_detection_respects_path() {
        let temp = temp_root("afs-agent-guidance-path");
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
        let temp = temp_root("afs-agent-guidance-copilot");

        assert!(!gh_copilot_extension_exists(&temp));
        fs::create_dir_all(temp.join(".config/gh/extensions/gh-copilot"))
            .expect("create gh copilot extension");
        assert!(gh_copilot_extension_exists(&temp));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn codex_mcp_config_replaces_existing_afs_table() {
        let temp = temp_root("afs-agent-guidance-codex-mcp");
        let config = temp.join("config.toml");
        fs::write(
            &config,
            "model = \"gpt\"\n\n[mcp_servers.afs]\nurl = \"old\"\n\n[mcp_servers.other]\nurl = \"keep\"\n",
        )
        .expect("write config");

        install_codex_mcp_config(&config, "secret-token").expect("install config");
        let contents = fs::read_to_string(&config).expect("read config");

        assert!(contents.contains("model = \"gpt\""));
        assert!(contents.contains("[mcp_servers.other]"));
        assert!(contents.contains("[mcp_servers.afs]"));
        assert!(contents.contains("Authorization = \"Bearer secret-token\""));
        assert!(!contents.contains("url = \"old\""));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn mcp_json_config_preserves_other_servers() {
        let temp = temp_root("afs-agent-guidance-json-mcp");
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
        assert_eq!(json["mcpServers"]["afs"]["type"], "http");
        assert_eq!(
            json["mcpServers"]["afs"]["headers"]["Authorization"],
            "Bearer secret-token"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn copilot_json_config_accepts_comment_only_template() {
        let temp = temp_root("afs-agent-guidance-copilot-mcp");
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
            json["servers"]["afs"]["requestInit"]["headers"]["Authorization"],
            "Bearer secret-token"
        );
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
