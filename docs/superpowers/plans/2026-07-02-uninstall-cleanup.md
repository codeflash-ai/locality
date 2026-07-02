# Uninstall Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an explicit desktop action that prepares Locality for uninstall by stopping runtime processes and removing Locality-managed agent/MCP integration entries.

**Architecture:** Reuse reset cleanup for daemon/provider state, add agent-guidance uninstall helpers beside the installer, and expose a Tauri command plus Settings UI button. Package uninstall hooks can call the same CLI/helper path after the shared cleanup exists.

**Tech Stack:** Rust Tauri backend, existing `agent_guidance.rs` JSON/TOML config helpers, React Settings view, Cargo and npm tests.

---

### Task 1: Agent/MCP Removal Helpers

**Files:**
- Modify: `apps/desktop/src-tauri/src/agent_guidance.rs`

- [ ] Write tests proving Locality removes only the `loc` MCP server entries and managed instruction sections while preserving unrelated user config.
- [ ] Implement `uninstall_agent_guidance()` plus JSON/TOML removal helpers beside the existing install helpers.
- [ ] Run `cargo test -p locality-desktop agent_guidance`.

### Task 2: Desktop Uninstall Preparation Command

**Files:**
- Modify: `apps/desktop/src-tauri/src/main.rs`

- [ ] Write tests for terminal CLI link cleanup where possible.
- [ ] Add `prepare_locality_uninstall` Tauri command that stops the daemon, resets provider state, removes desktop support state, removes managed agent/MCP integrations, and removes Locality-managed terminal command links.
- [ ] Register the command in the Tauri invoke handler.
- [ ] Run focused desktop Rust tests.

### Task 3: Settings UI

**Files:**
- Modify: `apps/desktop/src/App.tsx`

- [ ] Add a destructive Settings button labelled `Prepare for Uninstall`.
- [ ] Confirm with the user before calling the new Tauri command.
- [ ] Show success/error messages and refresh desktop state on success.
- [ ] Run desktop typecheck/tests if available.

### Task 4: Distribution Docs And Hooks

**Files:**
- Modify: `docs/macos-distribution.md`
- Modify: `docs/linux-distribution.md`
- Modify: `docs/windows-distribution.md`
- Modify package hook scripts if the repo has a supported hook point.

- [ ] Document that DMG deletion has no OS uninstall hook and users should run Settings > Prepare for Uninstall first.
- [ ] Add package uninstall hook calls where packaging supports them without harming upgrades.
- [ ] Run formatting and focused verification.
