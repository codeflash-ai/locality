# Desktop UI Screen Designs

This document translates `docs/desktop-app.md` into concrete screen designs for
the Tauri desktop app. It focuses on product behavior, layout, copy, and state
handling rather than implementation details.

## Design Philosophy

Locality should feel like a calm native desktop utility: compact, trustworthy,
fast, and precise. The app should not feel like a SaaS dashboard wrapped in a
desktop shell. It should help users connect a workspace, find a local path, and
understand pending changes without exposing sync machinery.

Core rules:

- one primary action per screen;
- workspace-first onboarding;
- opening a Notion page is the fastest path from a Notion URL to an
  agent-usable local file;
- use product states instead of daemon/scheduler/hydration terminology;
- make safety visible through concrete guarantees, not warning-heavy copy;
- keep diagnostics and advanced controls behind disclosure.

## Visual System

### Overall Feel

- Native desktop density: compact spacing, clear hierarchy, no oversized hero
  sections.
- Neutral base palette with purposeful status color only.
- Avoid decorative gradients, floating card stacks, and marketing layout.
- Use familiar icon buttons for repeated actions such as copy, reveal, settings,
  reconnect, review, and undo.

### App Surfaces

| Surface | Suggested size | Purpose |
|---|---:|---|
| First-run window | 720 x 560 | Focused onboarding wizard |
| Main window | 960 x 680 | Home, sources, review center, files, activity, settings |
| Tray popover | 360 x 520 | Quick status and actions |
| Modal dialog | 520 x variable | Focused confirmations or short forms |

### Navigation

The main window should use a compact left sidebar:

```text
Locality
  Home
  Sources
  Review Center
  Settings
```

`Files` and `Activity` are contextual pages. Files is reached from Home search,
recent file rows, source detail, tray search, and review actions. Activity is
reached from Settings and post-push success flows.

The first-run flow should not show the full sidebar. It should use a simple
step indicator and a single content panel.

### Status Language

Use these labels in normal UI:

- `Ready`
- `Preparing`
- `Review Center`
- `Needs Review`
- `Conflict`
- `Reconnect Needed`
- `Read Only`
- `Stopped`

Do not use implementation terms such as hydration queue, polling, daemon job,
shadow, or journal in normal surfaces. These can appear only in diagnostics.

## Screen Map

```text
First Run
  Welcome
  Connect Notion
  Choose Folder
  Prepare Workspace
  Ready

Daily Use
  Tray Icon States
  Tray Popover
  Home
  Files
  Review Center
  Push Review
  Sources
  Source Detail
  Settings
  Activity
  Diagnostics
```

## Daily Use Sources

The sidebar label is `Sources`. The first screen lists every registered connected
source from the desktop snapshot instead of immediately showing only the
preferred Notion folder.

The list uses compact source cards so long filesystem paths do not dominate the
screen. The header includes a `+ Add Source` action for adding future connectors
from the same place. Source setup is serialized, but progress belongs only to
the connector being configured: other source actions retain their normal labels
while disabled. The dialog Close action remains available during setup; closing
the dialog does not cancel the background connection and File Provider work.
Each card shows:

- Source: connector name, workspace label, mount id, and a `Primary` marker for
  the preferred source used by Home, tray, and Review Center.
- Local path: a compact middle-truncated path with the full path available on
  hover.
- Projection: plain files, macOS File Provider, Linux FUSE, or Windows Cloud
  Files.
- Access: read-only or edit-enabled.
- Content: indexed item count and review item count.
- Status: source/provider status label.
- Actions: copy path, open folder, and open details.

Clicking the source name or Details opens Source Detail for that source. The
detail screen keeps path controls, per-source sync controls, and diagnostics.
All path-based actions use the selected source rather than the preferred snapshot
source. The breadcrumb is:

```text
Home / Sources / <mount-id>
```

The `Sources` breadcrumb returns to the table without changing the selected
source in backend state. No backend source selection command is introduced by
this screen.

Source Detail keeps destructive maintenance inside a collapsed Danger Zone.
Both actions require a phrase containing the selected mount id:

- `Reset Source State` preserves pending local changes in the recovery folder,
  clears only that mount's cached/source-scoped state, and rebuilds it from the
  remote connector. It does not change remote data or other mounts.
- `Disconnect Source` deletes and revokes the selected mount's saved connection
  credential while retaining cached files and mount registration for a later
  reconnect. The retained source is hidden from the connected Sources list and
  the connector returns to the connect/reconnect path. If several mounts
  explicitly share that connection, the confirmation explains that they will all
  require reconnection.

The backend validates the typed phrase as well as the UI; invoking the desktop
command directly cannot bypass confirmation.

## First-Run Onboarding

### Screen 1: Welcome

Goal: establish what Locality does and get the user to connect Notion.

Primary action: `Connect Notion`

Secondary action: `I already connected Notion` only if a connection is detected.

Layout:

```text
┌──────────────────────────────────────────────────────────────┐
│ Locality                                            1 of 4        │
│                                                              │
│ Let your agents edit Notion as local files.                  │
│                                                              │
│ Locality mounts your Notion workspace in CloudStorage. Agents edit│
│ local files, then Locality syncs reviewed changes back to Notion. │
│                                                              │
│ [ Connect Notion ]                                           │
│                                                              │
│  Local edits stay pending until you review and push.          │
└──────────────────────────────────────────────────────────────┘
```

Notes:

- Keep the screen sparse. Do not mention every future connector here.
- The safety line should be a quiet footer, not a warning banner.

States:

- no existing connection: show `Connect Notion`;
- existing active Notion connection: show `Continue`;
- existing revoked connection: show `Reconnect Notion`.

### Screen 2: Connect Notion

Goal: keep the user oriented while the Notion OAuth browser flow is open. The
previous `Connect Notion` action already opened Notion, so this screen should
not ask the user to open Notion again.

Primary action while waiting: disabled `Waiting for Notion`

Secondary action: `Open browser again`

Layout:

```text
┌──────────────────────────────────────────────────────────────┐
│ Locality                                            2 of 4        │
│                                                              │
│ Finish connecting in Notion                                  │
│                                                              │
│ A browser window is open. Choose your workspace, pick the    │
│ pages Locality can use, then approve access.                      │
│                                                              │
│ ✓ Browser opened                                             │
│ ○ Select workspace and pages                                 │
│ ○ Approve access                                             │
│                                                              │
│ [ Waiting for Notion ]                                       │
│ Open browser again                                           │
│                                                              │
│  Credentials are stored securely in the OS credential store.  │
└──────────────────────────────────────────────────────────────┘
```

Error states:

- browser failed to open: show `Copy authorization link`;
- broker unreachable: show `Try again` and a brief "Could not start Notion
  connection";
- Notion denied access: show `Try again`.

### Screen 3: Choose Folder

Goal: choose where the workspace appears locally.

Default value:

```text
~/Library/CloudStorage/Locality/notion-main
```

Primary action: `Continue`

Secondary action: `Choose...`

Layout:

```text
┌──────────────────────────────────────────────────────────────┐
│ Locality                                            3 of 4        │
│                                                              │
│ Where should your Notion files appear?                       │
│                                                              │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ ~/Library/CloudStorage/Locality/notion-main                              [... ]│ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ [ Continue ]                                                 │
│                                                              │
│  This folder will include AGENTS.md and CLAUDE.md to help     │
│  your agents edit files natively.                            │
└──────────────────────────────────────────────────────────────┘
```

Validation:

- empty path: disable primary action;
- path outside user home: allow only with an explicit confirmation;
- existing non-empty folder: allow if it is already an Locality mount, otherwise ask
  the user to choose another folder or confirm creating `Locality/Notion 2`.

### Screen 4: Ready, Syncing In Background

Goal: complete onboarding as soon as the folder and agent guidance are ready,
while making it clear that workspace sync continues in the background. Do not
show an intermediate checklist that appears to finish instantly or asks the user
to wait on background preparation.

Primary action: `Open Notion Folder`

Secondary actions: `Open a Notion page`, `Copy folder path`, `Copy agent prompt`

Layout:

```text
┌──────────────────────────────────────────────────────────────┐
│ Locality                                            4 of 4        │
│                                                              │
│  ✓                                                           │
│ Locality is ready                                                 │
│ Your Notion folder is mounted. Locality syncs quietly while        │
│ agents edit local Markdown.                                  │
│                                                              │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Notion folder                         [ Copy ]           │ │
│ │ ~/Library/CloudStorage/Locality/notion-main                                  │ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ [ Open Notion Folder ]                                      │
│                                                              │
│ Open a Notion page                                           │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Paste a Notion URL to get the local file path [Open Page]│ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Try this with an agent                                   │ │
│ │ Find the Q4 launch plan and make it sharper for           │ │
│ │ leadership review.                              [ Copy ]  │ │
│ └──────────────────────────────────────────────────────────┘ │
│                                                              │
│ Agents can use Locality                                           │
│ Now your agents know how to use `loc` to view and edit        │
│ Notion. Installed for Claude, Codex, Warp, AGENTS.md,         │
│ and Copilot.                                                 │
└──────────────────────────────────────────────────────────────┘
```

Behavior:

- Locality should begin reading accessible top-level Notion structure immediately
  after connection, before the user reaches this screen.
- Do not block on full workspace enumeration.
- Show setup success as a small celebratory status pill. Mention background
  sync once in supporting copy, not as a task the user has to wait on.
- There is only one primary action.
- `Open a Notion page` should be a real text input, not a link-only action.
- The locate flow should prioritize the pasted page's preparation and show a
  copyable local path when ready.
- Include a small, human demo prompt that helps users understand the agent
  workflow. Keep its copy button inline on the right side of the prompt so it
  does not become another large action.

## Tray Popover

Goal: give daily status and fast access without making the user open the full
app.

## Tray Icon States

Goal: make the menu bar icon communicate enough state at a glance without
becoming visually noisy.

The base icon should remain monochrome so it feels native on macOS. The default
state should be clean, with no green badge and no activity animation. Use the
short Locality mark so the tray, popover, app icon, and installer surfaces share
the same recognizable silhouette. State badges should appear only when the user
needs to review something or fix a broken connection:

```text
Default           Working            Needs Review       Reconnect Needed
┌──────────────┐  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
│  mark        │  │  mark        │   │  mark   ●    │   │  mark   ●    │
│              │  │              │   │       amber  │   │        red   │
└──────────────┘  └──────────────┘   └──────────────┘   └──────────────┘
```

States:

- Default: monochrome icon with no badge.
- Working: monochrome icon with no badge.
- Needs Review: monochrome icon with amber dot.
- Reconnect Needed: monochrome icon with red dot.
- Hidden from menubar: no icon; background process keeps running.
- Stopped: no running icon unless the app window itself is open.

Chosen mark:

- Use the short Locality mark from the `_short.svg` logo pair. Use the dark
  mark on light UI surfaces and the light mark on dark/accent surfaces or dark
  icon backgrounds.
- Do not show alternate tray mark concepts in the final product plan.
- Do not use a default green badge or animated working badge.

Open behavior:

- left click opens the popover;
- right click can show a compact native menu if needed;
- quitting completely must be inside `Quit Options`.

Layout:

```text
┌──────────────────────────────────────┐
│ Locality                           Ready  │
│                                      │
│ Notion                               │
│ CodeFlash                            │
│ ~/Library/CloudStorage/Locality/notion-main               │
│                                      │
│ [ Open Notion Folder ]               │
│                                      │
│ Open a Notion page                   │
│ ┌──────────────────────────────────┐ │
│ │ Paste Notion URL                 │ │
│ └──────────────────────────────────┘ │
│                                      │
│ Review Center                    0   │
│                                      │
│ Settings                             │
│ Quit Options ›                       │
└──────────────────────────────────────┘
```

Attention state:

```text
┌──────────────────────────────────────┐
│ Locality                    Needs Review  │
│                                      │
│ Review Center                    3   │
│ roadmap.md                           │
│ launch-plan.md                       │
│ customer-notes.md                    │
│                                      │
│ [ Open Review Center ]               │
│ Open Notion Folder                   │
│                                      │
│ Open a Notion page                   │
│ ┌──────────────────────────────────┐ │
│ │ Paste Notion URL                 │ │
│ └──────────────────────────────────┘ │
│                                      │
│ Settings                             │
│ Quit Options ›                       │
└──────────────────────────────────────┘
```

Quit submenu:

```text
Quit Options
  Don't Show in Menubar
  Quit Completely...
```

`Quit Completely...` should show a confirmation dialog:

```text
Quit Locality completely?

Background sync will stop until Locality is opened again.

[ Cancel ]  [ Quit Completely ]
```

## Main Window Shell

Goal: richer control surface than the tray without feeling heavy.

Layout:

```text
┌───────────────┬──────────────────────────────────────────────┐
│ Locality           │ Home                                         │
│               │                                              │
│ Home          │ ...                                          │
│ Sources       │                                              │
│ Review Center │                                              │
│ Settings      │                                              │
│               │                                              │
│ Notion Ready  │                                              │
└───────────────┴──────────────────────────────────────────────┘
```

Sidebar bottom status:

- `Notion Ready`
- `Needs Review`
- `Reconnect Needed`
- `Locality Stopped`

## Home

Goal: show the current workspace and the next useful actions.

Primary action when ready: `Open Page` from the inline Notion URL field

Secondary action: `Open Notion Folder`

Layout:

```text
Home

Notion workspace
CodeFlash
~/Library/CloudStorage/Locality/notion-main

Open a Notion page
┌──────────────────────────────────────────────────────────────┐
│ Paste Notion URL to get the local file path                  │
└──────────────────────────────────────────────────────────────┘
[ Open Page ]    Open Notion Folder

Recent Files
Standups with Locality
~/Library/CloudStorage/Locality/notion-main/Engineering Wiki/Standups with Locality/page.md

Review Center
No review needed
```

Attention state:

```text
Home

Needs Review
3 files need review.

[ Open Review Center ]
Open Notion Folder
```

Empty state:

- no connections: primary `Connect Notion`;
- connected but no mount: primary `Create Notion Folder`;
- daemon stopped: primary `Start Locality`.

### Open Notion Page Result

The Home and tray URL inputs should resolve inline. The resolved path should be
clearly copyable so the user can paste it into Codex, Claude Code, Cursor, or
another agent.

Preparing result:

```text
Preparing this page
Locality is making the local file available now.

Roadmap 2026
~/Library/CloudStorage/Locality/notion-main/Engineering/Roadmap 2026/page.md

[ Copy Path ]    Reveal in Finder
```

Ready result:

```text
Roadmap 2026
Page

┌──────────────────────────────────────────────────────────────┐
│ ~/Library/CloudStorage/Locality/notion-main/Engineering/Roadmap 2026/page.md      │
└──────────────────────────────────────────────────────────────┘

[ Copy Path ]    Reveal in Finder
```

Error states:

- URL not recognized: "Paste a Notion page or database URL.";
- no matching mount: "This page is not in a mounted workspace.";
- no access: "Locality does not have access to this Notion page.";
- preparation failed: "Locality could not prepare this file. Try again."

## Files

Goal: help users and agents find files that are usable in the current workspace
access without surfacing old disconnected Notion access by default.

Default search scope:

```text
Current access only
```

This means:

- active workspaces only;
- active connections only;
- files under those workspace roots only;
- no disconnected or revoked access unless an advanced/diagnostic scope is
  explicitly selected.

Layout:

```text
Files

Current Workspace
CodeFlash
~/Library/CloudStorage/Locality/notion-main      [ Copy ] [ Reveal ] [ VS Code ]
[ Change Access ] [ Pull Latest ]

Search current files
┌──────────────────────────────────────────────────────────────┐
│ Search current Notion files                                  │
└──────────────────────────────────────────────────────────────┘

Recent Files
@Last Friday
General / Engineering Wiki / Standups with Locality
~/Library/CloudStorage/Locality/notion-main/engineering-wiki/standups-with-locality/last-friday/page.md
[ Copy Path ] [ Reveal ]
```

Recent files should come from current active workspace state, such as opened
files, local changes, or files needing review. If the workspace is tied to a
revoked or inactive connection, recent files should be hidden and the user
should be routed to reconnect or change access.

The current workspace section owns the everyday file controls. Keep path actions
next to the path itself as compact icon buttons: copy path, reveal in Finder, and
open in editor. Do not repeat the full source list in Files; the Sources screen
is the management place for all registered local folders. Advanced diagnostics can
live behind disclosure inside Files or Settings.

Search results should label safety states clearly:

- `Ready`
- `Online Only`
- `Needs Review`
- `Remote Update`
- `Conflict`

Normal search should not show stale entities from old access scopes. Recovery
or diagnostics can expose old access with a deliberate advanced filter.

### Workspace States

These states appear inside the current workspace section rather than on a
separate mount screen.

Read-only state:

- status label: `Read Only`;
- push actions hidden or disabled;
- copy: "This workspace is for reading and locating files."

Reconnect state:

- primary action changes to `Reconnect Notion`;
- keep folder actions available if local files still exist.

## Review Center

Goal: show local edits and sync exceptions that can affect Notion and route the
user to review or resolution.

Primary action when review items exist: `Review Push`

Secondary actions: `Push Safe Changes`, `Open File`, `Pull Latest`, `Reset to
Remote`, and per-file Live Mode controls when applicable.

Layout:

```text
Review Center

3 items need attention.

┌──────────────────────────────────────────────────────────────┐
│ Roadmap 2026                                                 │
│ Engineering/Roadmap 2026/page.md                             │
│ 2 text edits                                                 │
│                                                [ Open ]      │
├──────────────────────────────────────────────────────────────┤
│ Launch Plan                                                  │
│ Marketing/Launch Plan/page.md                                │
│ needs review: large deletion                                 │
│                                                [ Open ]      │
└──────────────────────────────────────────────────────────────┘

[ Review Push ]
```

Item states:

- approval required;
- needs review;
- conflict;
- read-only blocked;
- unsupported change.

Empty state:

```text
Review Center

No review needed.

Safe Live Mode work can sync automatically. Items that need approval will appear
here.
```

## Push Review

Goal: show exactly what will update in Notion before remote writes happen.

Primary action for safe plans: `Push to Notion`

Primary action for dangerous plans: `Confirm and Push`

Secondary actions: `Cancel`, `Open File`

Layout:

```text
Review Push

3 files will update Notion.

Summary
2 pages updated
1 database row updated
0 pages deleted

Files
Roadmap 2026              2 block edits
Launch Plan              needs review: large deletion
Customer Notes           1 property edit

[ Push to Notion ]
```

Dangerous plan state:

```text
Review Push

Needs Review
This push would delete 18 blocks from Notion.

Files
Launch Plan              18 block deletions

[ Confirm and Push ]      Cancel
```

Validation failure state:

```text
Review Push

Not ready to push
Fix these issues first.

Launch Plan
Status must be one of: Draft, In Progress, Done

[ Open File ]
```

Completion:

```text
Pushed to Notion

3 files updated successfully.

[ Done ]
```

## Activity

Goal: show recent meaningful actions without exposing raw logs by default.

Layout:

```text
Activity

Today
Pushed Roadmap 2026 to Notion
Located Launch Plan
Created Notion folder

Earlier
Connected Notion workspace CodeFlash
```

Activity item details:

- action type;
- file or workspace name;
- result;
- time.

Activity is read-only history in the normal desktop UI. Do not show an undo
button here unless the item carries a specific journal id, the app can preview
the reverse plan, and the user can confirm the remote write. Until then, undo
belongs in CLI/history recovery or a dedicated push-history flow, not passive
activity.

Failed item:

```text
Push failed
Launch Plan was changed in Notion before Locality could update it.

[ Review Conflict ]
```

## Settings

Goal: normal preferences without development-only controls.

Sections:

```text
Settings

General
[x] Launch Locality at login
[x] Show Locality in the menu bar
Default folder: ~/Library/CloudStorage/Locality

Connections
Notion        CodeFlash        [ Manage ]

Safety
Push confirmation: Require for large changes
Default new mount mode: Edit enabled

Updates
Automatically keep Locality up to date

Advanced
Diagnostics...
```

Do not show broker URL or OAuth internals in normal settings.

## Diagnostics

Goal: help support and power users inspect the system without polluting normal
UI.

Layout:

```text
Diagnostics

Locality process        Running
State folder       ~/.loc
Logs folder        ~/.loc/logs
Mounts watched     1
Projection         macOS File Provider

[ Copy Diagnostic Summary ]
[ Open Logs Folder ]
[ Restart Locality ]
```

Allowed technical terms here:

- daemon;
- state folder;
- projection;
- File Provider;
- logs;
- socket.

## Component Inventory

Use these components consistently:

- `PrimaryButton`
- `SecondaryButton`
- `IconButton`
- `PathField` with copy action
- `StatusPill`
- `FileChangeList`
- `MountSummary`
- `ConnectionSummary`
- `SafetyNote`
- `ProgressChecklist`
- `ConfirmDialog`
- `DisclosureSection`

Likely icons:

- folder open: open/reveal folder;
- copy: copy path;
- search: locate;
- check circle: ready/success;
- alert triangle: needs review/conflict;
- refresh/reconnect: reconnect;
- settings: settings;
- history: activity;
- eye off: hide from menubar;
- power: quit completely.

## Data Requirements

The UI needs desktop-facing data shaped around product concepts:

```text
app_health
  state: ready | preparing | needs_review | reconnect_needed | stopped | runtime_stopped
  attention_count

connection
  connector
  workspace_name
  account_label
  status

mount
  connector
  workspace_name
  local_path
  projection
  read_only
  status

located_item
  title
  kind
  local_path
  state: ready | preparing | no_access | not_found

pending_change
  title
  local_path
  summary
  state: safe | needs_review | conflict | blocked

push_plan
  summary
  file_items
  guardrail_state
  can_push
```

## Copy Guidelines

Preferred words:

- Notion folder
- local path
- pending changes
- review push
- open folder
- reveal in Finder
- reconnect
- read only

Avoid in normal UI:

- internal file-state labels
- hydration
- scheduler
- daemon job
- shadow
- socket
- broker
- polling

## Build Order

1. First-run onboarding through ready screen.
2. Inline Open Notion Page fields and path-copy result.
3. Tray popover and quit options.
4. Home screen.
5. Review Center and push review.
6. Files screen with current workspace controls.
7. Activity.
8. Settings and diagnostics.
