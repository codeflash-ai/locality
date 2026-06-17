use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const MANAGED_START: &str = "<!-- AFS_AGENT_GUIDANCE_START -->";
const MANAGED_END: &str = "<!-- AFS_AGENT_GUIDANCE_END -->";
const DEFAULT_NOTION_MOUNT: &str = "~/Library/CloudStorage/AFS/notion";

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

struct AgentTargetSpec {
    agent: &'static str,
    path: PathBuf,
    kind: InstallKind,
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
    mount_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(DEFAULT_NOTION_MOUNT)
        .trim_end_matches('/')
        .to_string()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
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
