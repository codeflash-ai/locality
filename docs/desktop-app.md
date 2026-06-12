# Desktop App Plan

This document captures the product and interaction plan for the AFS desktop app.
It is subordinate to `plan.md`: the desktop app is a user-facing orchestration
surface over the existing CLI, daemon, connector, store, and projection layers.

## Product Goal

The desktop app should make AFS feel useful within the first session. The target
experience is not "configure a sync tool"; it is:

1. Install AFS.
2. Connect Notion.
3. Mount the user's Notion workspace as local files.
4. Locate a specific Notion page as a local file path.
5. Open the mounted workspace in Finder or paste the file path into an agent.
6. Safely make and push changes back to Notion.

The product should optimize for time-to-aha while preserving trust. The aha
moment is the user seeing that their Notion workspace is available as local files
that agents can inspect and edit with the generated `AGENTS.md` and `CLAUDE.md`
guidance already present in the mount, then having an agent safely edit those
files and push the reviewed changes back into Notion.

## Product Principles

- Workspace first: onboarding should mount a Notion workspace or selected
  workspace scope, not start with a single page URL.
- One obvious next action: screens should avoid multiple competing primary
  buttons. Use one primary action and secondary links only when needed.
- Do not block on full initial sync: setup should become useful quickly, with
  specific pages prepared on demand when the user locates them.
- Hide sync internals: users should not need to reason about hydration queues,
  polling, or last-sync timestamps.
- Safe by default: local edits are staged as pending changes until the user or
  agent explicitly pushes them.
- Agent ready: the mounted directory should include concise local instructions
  for coding agents, so the app does not need to over-explain agent behavior.
- Technical depth on demand: advanced state, logs, and diagnostics should exist,
  but not dominate first-run or tray surfaces.

## First-Run Onboarding

The first-run flow should be a compact wizard with a single primary path.

### 1. Welcome

Purpose: establish what AFS does in one sentence.

Primary action: `Connect Notion`.

Supporting copy should stay concrete: Notion becomes local files for agents and
editors. Avoid abstract connector-marketplace language during first run.

### 2. Connect Notion

Clicking `Connect Notion` on the welcome screen should immediately open the
broker-backed Notion OAuth flow. The second onboarding screen should help the
user complete that browser flow rather than ask them to open Notion again.
Notion's picker is where the user grants access to the pages, teamspaces, or
workspace content that AFS can see.

The app should explain the security model in plain terms:

- AFS only sees Notion content the user grants.
- Credentials are stored securely in the OS credential store.
- The broker protects the Notion OAuth client secret.
- Local edits do not change Notion until push.

### 3. Choose Mount Location

Default mount base:

```text
~/Documents
```

Suggested concrete folder:

```text
~/Documents/AFS/Notion
```

The mount should live under `~/Documents` because it is visible and familiar to
normal users. The app can still keep the folder organized under an `AFS`
subdirectory to avoid cluttering `Documents` directly. The user can change this,
but the default should be good enough for most users. This step should be framed
as "Where should your Notion files appear?" rather than "configure mount root."

### 4. Create Workspace Mount

The desktop product target is a workspace-level Notion mount using the access
granted during OAuth. The current CLI is root-page-oriented, so this requires a
desktop-facing setup API or CLI extension before the polished app ships.

Target orchestration:

```text
connect notion
begin reading accessible top-level Notion structure
create notion workspace mount
start daemon
begin workspace projection in the background
open mount or continue to locate
```

The app should show a short progress sequence with human concepts:

- Connecting Notion
- Finding top-level workspace pages
- Creating local folder
- Preparing files for agents
- Starting background sync

Do not show hydration queues, polling intervals, or low-level daemon concepts in
the onboarding UI. Do not make the user wait for the full workspace projection
or initial sync to finish before moving forward. Once Notion is connected, AFS
should begin prefetching top-level directories and files so the chosen mount
point feels populated quickly. The UI can show a simple checkpoint flow while
the background work is already underway.

### 5. Ready

The completion screen should have exactly one primary action:

```text
Open Notion Folder
```

Secondary actions can be lower-emphasis links:

- Copy path
- Open a Notion page
- Open settings
- Add another connection

Do not present Finder, editor, and terminal as three equal primary choices. The
mount already includes `AGENTS.md` and `CLAUDE.md`, so agent guidance belongs in
the folder rather than as a separate onboarding decision.

After setup, the app should make it easy to continue into the locate flow. A
user should be able to paste a Notion URL immediately into a text field with
placeholder guidance, and AFS should prioritize that page's local file
preparation instead of waiting for the rest of the workspace to finish.

## Locate By Notion URL

Pasting a Notion URL should be a locate workflow, not the start of onboarding.
It is useful after the workspace is mounted.

User story:

1. User pastes a Notion page or database URL.
2. AFS resolves the remote ID through the mounted workspace.
3. AFS prioritizes that item if it is still being prepared locally.
4. The app shows the corresponding local file or directory path.
5. The user can copy the path for an agent or reveal it in Finder.
6. If the item is not yet present locally, AFS updates the projection and then
   reveals or copies the path when ready.

This should be available from:

- the tray menu;
- the main dashboard;
- a global app shortcut later;
- CLI/agent workflows through a stable command.

Suggested command/API shape:

```text
afs locate notion <url> --json
```

The response should include the mount, local path, entity type, whether the file
already existed, whether any projection update was needed, and whether the file
was prioritized for local preparation.

## Tray And Mini-Dashboard

The tray app should communicate availability and actionability, not internal
sync mechanics.

### Tray Status

- Green: AFS is running and all mounts are ready.
- Yellow: attention needed, such as pending changes or reconnect required.
- Red: AFS is stopped or an action failed.

### Tray Menu

Primary items:

- Open Notion Folder
- Notion URL input for locating a page
- Pending Changes
- Add Connection...
- Settings
- Quit Options

Avoid showing last sync time. Users should be able to assume AFS syncs
intelligently. Avoid showing hydration queue counts; hydration is an
implementation detail.

`Quit Options` should be a submenu, not a top-level destructive action:

- Don't Show in Menubar
- Quit Completely

Quitting completely stops background sync, so it should be harder to do by
accident than closing a normal app window.

### Mini-Dashboard

Recommended sections:

- Connections: connected Notion workspace and future connectors.
- Mounts: local folders with quick open and reveal actions.
- Pending Changes: files that need push, conflicts, or blocked edits.
- Suggestions: useful next connectors or setup improvements.

Avoid internal file-state terms in user-facing UI. Use "pending changes",
"needs review", "conflict", or "not ready to push" depending on context.

## Safety UX

Safety should be visible but not anxiety-inducing. The app should give users
confidence that company docs are protected.

Core messages:

- Local edits become pending changes first.
- Push shows a clear plan before updating Notion.
- Large or destructive pushes require confirmation.
- AFS keeps a journal so pushes can be inspected and, where supported, undone.
- Read-only mounts are available for research-only use.
- Credentials are stored outside project files.

Important language:

- Use "pending changes" for local edits that have not been pushed.
- Use "review push" instead of "sync now" when remote writes are involved.
- Use "restore local file" for local recovery from shadow state.
- Use "undo push" only for journal-backed remote undo.

## Main App Structure

### Home

Shows the current state in product terms:

- connected workspaces;
- mounted folders;
- a Notion URL input for opening a page as a local file;
- pending changes;
- attention items;
- connector suggestions.

### Mount Detail

Shows one mounted workspace:

- source workspace/account;
- local folder and folder location controls;
- open/reveal actions;
- pending changes;
- conflicts;
- read-only state;
- advanced diagnostics behind disclosure.

### Pending Changes

Shows files that will affect Notion on push. The user can review the plan, push
safe changes, handle conflicts, or open files locally.

### Activity

Shows push history, failed actions, and undo availability. Keep low-level daemon
logs behind a diagnostics affordance.

### Settings

Includes launch at login, default mount directory, update channel, and advanced
daemon controls.

## Desktop API Needs

To support this app cleanly, the Rust side should expose a desktop-oriented API
instead of forcing the UI to compose many low-level commands.

Initial target operations:

- list app health;
- connect Notion;
- create workspace-level Notion mount;
- list mounts;
- open/reveal mount folder;
- locate Notion URL in an existing mount;
- prioritize local preparation for a located Notion item;
- list pending changes;
- review push plan;
- push approved changes;
- show push history;
- undo supported pushes;
- reconnect or disconnect a workspace.

The first implementation can call existing `afs --json` commands as a sidecar.
Once the shape stabilizes, the same operations can move behind a shared Rust API
or daemon IPC client.

## Staged Build Plan

### Stage 1: Product Contract

- Define the desktop-facing command/API contract.
- Add workspace-level Notion mount support if the CLI cannot already express it.
- Add `locate notion <url>` for the post-onboarding URL workflow.
- Make locate prioritize the requested page or database during initial
  workspace preparation.
- Normalize user-facing status terms around pending changes and conflicts.

### Stage 2: Tauri Shell

- Add a Tauri app that bundles `afs` and `afsd` as sidecars.
- Implement tray status and menu.
- Implement first-run Notion onboarding.
- Implement open/reveal folder actions.

### Stage 3: Trust And Writes

- Build the pending changes view.
- Show push plans in clear language.
- Wire push confirmation and supported undo.
- Surface read-only mount mode and reconnect flows.

### Stage 4: Distribution

- Ship signed/notarized macOS DMG.
- Add Homebrew installation path for power users.
- Add signed auto-update.

### Stage 5: Connector Expansion

- Introduce the connector suggestion surface.
- Add Linear after Notion once the desktop abstractions are stable.
- Keep connector UI generic: connection, mount, locate, pending changes, and
  source-specific settings.

## Open Product Decisions

- The first public desktop beta should target macOS File Provider as the primary
  projection, matching `plan.md`.
- First-run should stop on a ready screen with `Open Notion Folder` as the
  single primary action.
- Workspace-level Notion mounts should follow Notion's grouping semantics:
  workspace first, then top-level pages and nested page/database hierarchy. The
  page a user wants to edit should live in that hierarchy rather than as a
  special one-off mount.
- How much push-plan detail belongs in the tray mini-dashboard versus the full
  app window remains open. It may be valuable to show which files are modified
  while leaving detailed review/editing to richer file viewers or the full app.
