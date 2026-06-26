# Desktop App Plan

This document captures the product and interaction plan for the Locality desktop app.
It is subordinate to `plan.md`: the desktop app is a user-facing orchestration
surface over the existing CLI, daemon, connector, store, and projection layers.

## Product Goal

The desktop app should make Locality feel useful within the first session. The target
experience is not "configure a sync tool"; it is:

1. Install Locality.
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

Onboarding also installs local agent guidance for detected agents such as Claude
Code, Codex, Warp, Cursor-compatible tools, Gemini CLI, and Cline/Roo. See
[agent-guidance.md](agent-guidance.md) for install targets and fallback
behavior. The same installer also configures the local authenticated MCP
fallback for supported agents and refreshes periodically while Locality is running,
so agents installed after Locality can still discover the fallback without another
setup flow.

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

Purpose: establish what Locality does in one sentence.

Primary action: `Connect Notion`.

Supporting copy should stay concrete: Notion becomes local files for agents and
editors. Avoid abstract connector-marketplace language during first run.

### 2. Connect Notion

Clicking `Connect Notion` on the welcome screen should immediately open the
broker-backed Notion OAuth flow. The second onboarding screen should help the
user complete that browser flow rather than ask them to open Notion again.
Notion's picker is where the user grants access to the pages, teamspaces, or
workspace content that Locality can see.

The app should explain the security model in plain terms:

- Locality only sees Notion content the user grants.
- Credentials are stored securely in the OS credential store.
- The broker protects the Notion OAuth client secret.
- Local edits do not change Notion until push.

### 3. Choose Mount Location

Default mount base:

```text
~/Library/CloudStorage/Locality
```

Suggested concrete folder:

```text
~/Library/CloudStorage/Locality/notion-main
```

On macOS, the real Locality root is assigned by File Provider and must be read from
`NSFileProviderManager.getUserVisibleURL`. Packaged builds and the local
development bundle identify the host app as `Locality`, so new installs should
resolve to `~/Library/CloudStorage/Locality`. Older roots such as `Locality` and
`Locality-Locality` are repair aliases only. Each mount gets a stable mount-point
folder such as `notion-main`, rather than a connector-derived child folder such as
`notion`. Locality should not create a Documents alias or symlink; the app should
show the actual CloudStorage folder directly. This step should be framed as
"Where should your Notion files appear?" rather than "configure mount root."

Local diagnostics live under the Locality state root in `logs/`. Desktop actions and
File Provider repair failures are mirrored to `desktop.log` with event markers
such as `[file_provider.open_domain_failed]`; daemon-managed runs continue to
write localityd logs in the same folder. The UI should point support and power users
to that one directory instead of asking them to inspect terminal output,
launchd, and helper logs separately.

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

The app can track setup internally with human concepts:

- Connecting Notion
- Finding top-level workspace pages
- Creating local folder
- Preparing files for agents
- Starting background sync

Do not show hydration queues, polling intervals, or low-level daemon concepts in
the onboarding UI. Do not make the user wait for the full workspace projection
or initial sync to finish before moving forward. Once Notion is connected, Locality
should begin prefetching top-level directories and files so the chosen mount
point feels populated quickly. The UI should not show an extra checklist screen
where most items complete instantly; once the folder and agent instructions are
ready, route directly to the final ready screen and show background sync as a
short supporting detail rather than a task the user waits on.

### 5. Ready

The completion screen is the final onboarding step and should have exactly one
primary action:

```text
Open Notion Folder
```

Secondary actions can be lower-emphasis controls:

- Copy path
- Open a Notion page
- Copy prompt

Do not present Finder, editor, and terminal as three equal primary choices. The
mount already includes `AGENTS.md` and `CLAUDE.md`, so agent guidance belongs in
the folder rather than as a separate onboarding decision. The final screen can
include a small agent prompt example, but it should feel human and be visually
secondary to opening the folder. Its copy action should be a compact inline
button, not a second full-width button.

After setup, the app should make it easy to continue into the locate flow. A
user should be able to paste a Notion URL immediately into a text field with
placeholder guidance, and Locality should prioritize that page's local file
preparation instead of waiting for the rest of the workspace to finish.

## Locate And Search

Finding work should feel closer to Notion search than filesystem browsing. Locality
should support direct URL locate, title search, and path-fragment search from the
same input so users do not need to remember whether they are holding a Notion URL
or only a page name. This is especially important once a workspace has hundreds
or thousands of pages.

User story:

1. User pastes a Notion page/database URL or types a page title/path fragment.
2. Locality searches the local SQLite entity index first.
3. If the input contains a Notion ID, Locality resolves that ID exactly through the
   mounted workspace.
4. If the item is present but online-only, Locality prioritizes it for local
   preparation.
5. The app shows the corresponding local file or directory path.
6. The user can copy the path for an agent or reveal it in Finder.
7. If the item is not yet present locally, Locality explains that the page must be
   granted in Notion and synced before it can appear.

This should be available from:

- the tray menu;
- the main dashboard;
- a global app shortcut later;
- CLI/agent workflows through a stable command.

Suggested command/API shape:

```text
loc locate notion <url> --json
loc search <query> --connector notion --json
```

The response should include the mount, local path, entity type, whether the file
already existed, whether any projection update was needed, and whether the file
was prioritized for local preparation.

Initial desktop implementation can search metadata already stored in SQLite:
remote ID, title, and projected path. The next step is a dedicated local search
index that also covers Markdown body text, frontmatter properties, breadcrumbs,
recently opened pages, and aliases from Notion mentions. That index should be
updated by daemon reconciliation and virtual filesystem mutations, not by reading
every file in the mounted folder.

Main app and tray search surfaces should show ranked suggestions while the user
types, including the projected path and state label. A user should be able to
select a suggestion, copy the path, or reveal it in Finder without waiting for a
full workspace sync.

## Large Workspace Navigation

The product should assume a normal workspace can contain 1,000+ accessible
Notion pages and databases. The virtual filesystem should stay useful without
forcing users to manually browse deep directory trees.

Required behavior:

- Projection is metadata-first: directory listings come from SQLite and remain
  fast even when bodies are not hydrated.
- Search is local-first: title/path results should appear instantly from the
  entity index; body search can be eventually consistent.
- Hydration is intent-driven: opening, searching, pinning, or agent-targeting a
  page should raise its priority above background sync.
- Navigation preserves hierarchy: search results should show enough path context
  to distinguish pages with the same title.
- Recent and important pages should be promoted: recently opened, recently
  edited, dirty, conflicted, and pending-push files deserve first-class surfaces.
- Missing access is explicit: if a URL cannot be found, the app should say
  whether the likely issue is missing Notion permission, no sync yet, or no
  mounted workspace.

The "magical" behavior is not a large first sync. It is that the user can paste
or search for the thing they care about and Locality makes the right local file ready
without asking them to understand hydration, indexing, or daemon queues.

## Local Index Roadmap

Use progressive indexing so Locality remains reliable while becoming more useful:

1. Metadata index: mount ID, remote ID, kind, title, projected path, hydration
   state, remote edited time, dirty/conflict/pending-push flags.
2. Breadcrumb index: parent chain, database name, teamspace/workspace labels,
   and stable display path independent of current filename.
3. Body FTS index: extracted Markdown body, headings, list items, table text,
   frontmatter values, and Notion mentions. Store tokenized/searchable text in a
   local SQLite FTS table, never in credential storage.
4. Activity index: recent opens, copies, reveals, pushes, restores, failed
   pushes, and access changes.
5. Agent handoff index: a compact "best file targets" API that can answer
   agent-oriented queries like "find the Q4 launch plan" with ranked local paths.

Index updates should be event-sourced from daemon-owned state transitions:
enumerate, fetch/render, scheduled pull, virtual create/rename/delete, push
reconcile, restore, and conflict creation. Rebuilding the index should be safe
and deterministic from the durable store plus content cache.

## Tray And Mini-Dashboard

The tray app should communicate availability and actionability, not internal
sync mechanics.

### Tray Status

- Green: Locality is running and all mounts are ready.
- Yellow: attention needed, such as pending changes or reconnect required.
- Red: Locality is stopped or an action failed.

### Tray Menu

Primary items:

- Open Notion Folder
- Notion URL input for locating a page
- Pending Changes
- Add Connection...
- Settings
- Quit Options

Avoid showing last sync time. Users should be able to assume Locality syncs
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
- Locality keeps a journal so pushes can be inspected and, where supported, undone.
- Read-only mounts are available for research-only use.
- Credentials are stored outside project files.

Important language:

- Use "pending changes" for local edits that have not been pushed.
- Use "review push" instead of "sync now" when remote writes are involved.
- Use "restore local file" for local recovery from shadow state.
- Use "undo push" only for journal-backed remote undo.
- Live Mode is opt-in, rate-limited to one sync action per tick, and must keep
  the same push guardrails: stop on conflicts, blocked files, and
  review-required changes. The normal local-write path comes from File Provider
  callbacks; a visible-file reconciliation fallback is throttled and scoped to
  the active already-hydrated page. When there is no local pending change, Live
  Mode fetches one already-hydrated page into the daemon content cache and
  compares the rendered shadow before touching the visible CloudStorage
  projection, so stale Notion metadata does not hide body edits and unchanged
  files are not repeatedly read or rewritten.

## Main App Structure

### Home

Shows the current state in product terms:

- connected workspaces;
- mounted folders;
- a Notion URL input for opening a page as a local file;
- an explicit Live Mode toggle for users who want clean hydrated pages checked
  for remote changes and safe pending changes pushed continuously without
  opening the review flow;
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

The first implementation can call existing `loc --json` commands as a sidecar.
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

- Add a Tauri app that bundles `loc` and `localityd` as sidecars.
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
