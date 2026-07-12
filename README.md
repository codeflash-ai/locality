# Locality

Locality turns apps and systems of record into local files and keeps them in sync. You or 
you agents only need to work with local files - everything else is taken care of by Locality.
There is no need to directly run api calls or use MCP - upon opening files Locality
gets the upto date version, one edits only the local files, and updates it back 
on file saves or on `push` command.

This file approach simplifies external apps for your agents, and collaborating with agents for work.

The first supported platform is Notion: pages become directories, page bodies live
in `page.md`, child pages become child directories, and database rows become
page-like folders with frontmatter. Humans, editors, scripts, and coding agents
can search, read, and edit those files with ordinary filesystem tools while keeping
Notion as the source of truth.

```text
~/Library/CloudStorage/Locality/notion
└── Company Wiki
    ├── page.md
    ├── Product
    │   ├── Roadmap
    │   │   └── page.md
    │   └── Launch Plan
    │       └── page.md
    └── Meetings
        ├── page.md
        └── Tasks
            ├── _schema.yaml
            └── Follow up with design
                └── page.md
```

Locality is local-first, but not reckless. It keeps durable sync state in
SQLite, handles merges and conflicts, validates changes before
remote writes, journals push plans, and pauses for review when it cannot safely
decide what to do.

## Why Locality Exists

Knowledge work increasingly happens across remote apps, local editors, and AI
agents. Most apps are excellent collaboration databases but poor
local programming surfaces. Locality gives those systems a filesystem interface:

- let agents edit mounted Markdown directly with repo-local guidance;
- use `rg`, `grep`, editors, scripts, and agents against real Markdown files;
- review local edits with `loc diff` before anything touches the remote source;
- push safe changes back through file operations instead of browser automation;
- collaborate with other people in realtime by keeping content fresh with Live Mode.

The goal is not to export a workspace and fork reality. The goal is to make the
remote workspace feel locally available while preserving remote identity,
structure, permissions, and safety.

## What It Does Today

Locality currently includes:

- a desktop app for connecting Notion, creating a local mount, opening the
  mounted folder, and managing Live Mode;
- a `loc` CLI for connecting, mounting, locating, pulling, diffing, pushing,
  restoring, inspecting, and debugging mounts;
- a per-user daemon process, `localityd`, that owns hydration, background freshness,
  virtual filesystem requests, local write tracking, and Live Mode;
- a Notion connector that renders pages/databases to canonical Markdown,
  supports conservative block/property writes, handles media under `.loc/media`,
  and reconciles changed pages after pushes;
- virtual filesystem projections through macOS File Provider, Linux FUSE, and
  Windows Cloud Files.
- generated `AGENTS.md` and `CLAUDE.md` guidance inside mounts so coding agents
  understand the filesystem contract.

## Install

The most reliable user install path is downloading the installer from the (Downloads page)[https://www.locality.dev/downloads].
The app auto-updates as new versions are released.

For development builds, use the source workflow below.

## Quick Start

Install dependencies and build from a fresh checkout:

```sh
make setup
make build
```

Start the desktop app in development mode:

```sh
make dev-tauri
```

Or use the CLI directly. The OAuth flow is the normal product path:

```sh
loc connect notion --name work
loc mount notion ~/Locality/notion --workspace --connection work --projection plain-files
loc pull ~/Locality/notion
```

Then locate a page from a Notion URL, title, path fragment, or remote id:

```sh
loc locate "https://app.notion.com/..."
```

The command prints the resolved local path, for example:

```text
/Users/alice/Library/CloudStorage/Locality/notion/Product/Initial Idea/page.md
```

Edit the file in your editor, then review and push:

```sh
loc status /path/to/page.md
loc diff /path/to/page.md
loc push /path/to/page.md -y
```

With Live Mode enabled for a file, safe local edits can push automatically:

```sh
loc live-mode on /path/to/page.md
loc live-mode status /path/to/page.md
```

Live Mode remains conservative. It pauses for conflicts, remote drift that needs
review, unsupported plans, destructive or large changes, and anything requiring
explicit user approval.


## Command Line Workflow

Common commands:

| Command | Purpose |
| --- | --- |
| `loc locate <query>` | Find a Notion page/database and print its local path. |
| `loc status [path]` | Show local state, pending edits, conflicts, and known remote drift. |
| `loc diff <path>` | Review the planned connector operations and readable Markdown diff. |
| `loc push <path> -y` | Apply a safe plan to the remote source and reconcile local state. |
| `loc pull <path>` | Refresh a mount, folder, page directory, or `page.md`. |
| `loc inspect <path>` | Fetch the current remote page and explain local-vs-remote drift. |
| `loc restore <path>` | Reset a local file from the last synced shadow without calling Notion. |
| `loc log --diff` | Review journaled pushes and their readable diffs. |
| `loc daemon status` | Inspect the background daemon. |

For agents, the preferred path is usually:

1. Use `loc locate` or normal filesystem search to find the page.
2. Edit mounted Markdown directly.
3. Stop unless the user asked for review or push.
4. Use `loc status` and `loc diff` for inspection.
5. Use `loc push` only when explicitly requested or when recovering a known
   pending local change.

## Sync Engine

Locality uses a connector-neutral three-tree model:

- **Remote Tree**: the latest source-side state Locality has observed.
- **Local Tree**: the current local file or virtual projection content.
- **Synced Tree**: the last accepted version shared by remote and local,
  stored as a canonical shadow.

The planner compares those trees and chooses one of the safe outcomes:

```text
Remote == Synced, Local == Synced  -> clean
Remote != Synced, Local == Synced  -> fast-forward possible
Remote == Synced, Local != Synced  -> local pending change
Remote != Synced, Local != Synced  -> review or conflict
```

`loc push` re-checks the current remote version before it
applies mutations.

Freshness work is budgeted. Locality prioritizes push preflight, pending files,
opened files, recently listed folders, pasted URLs, active workspace navigation,
and then cold background sampling. It avoids crawling a whole workspace just to
keep idle state warm.

## Live Mode

Live Mode is the desktop background sync loop. It combines local filesystem
signals, virtual-provider callbacks, recent user activity, bounded remote
freshness checks, and the same push planner used by the CLI.

When Live Mode is enabled and a file is safe:

- local edits can be auto-pushed;
- clean files can fast-forward when the remote changes;
- active files and folders are checked at higher priority;
- remote-only updates can hydrate into the local cache and visible projection.

When the situation is not obviously safe, Live Mode does not guess. It reports a
paused, review-needed, or conflicted state that humans and agents can inspect
with `loc status`, `loc diff`, and `loc inspect`.

## Architecture

Locality is a Rust workspace with user-facing desktop and CLI surfaces over a
shared sync core.

```text
desktop app / loc CLI / editor / agent
        |
        v
platform projection
  macOS File Provider | Linux FUSE | Windows Cloud Files | plain files
        |
        v
localityd daemon
  hydration, freshness, virtual FS, write tracking, Live Mode
        |
        v
locality-core + locality-store
  three-tree planner, validation, journals, SQLite state
        |
        v
connector SDK
        |
        v
locality-notion -> Notion API
```

Core crates and directories:

| Path | Responsibility |
| --- | --- |
| `apps/desktop` | Tauri desktop app and tray UI. |
| `crates/loc-cli` | Stable `loc` command surface for humans and agents. |
| `crates/localityd` | Per-user daemon for mounts, hydration, freshness, virtual filesystem IPC, and Live Mode. |
| `crates/locality-core` | Connector-neutral sync model, canonical Markdown, diff planning, validation, guardrails, conflicts, and journals. |
| `crates/locality-connector` | Connector trait and data types for enumerate, fetch, render, parse, apply, and reverse apply. |
| `crates/locality-notion` | Notion API client, DTOs, renderer, parser/apply support, database schema handling, media, and OAuth integration. |
| `crates/locality-store` | SQLite state store, migrations, mounts, entities, shadows, journals, credentials metadata, and freshness state. |
| `platform/linux/locality-fuse` | Linux FUSE helper for online-only virtual mounts. |
| `platform/windows/locality-cloud-files` | Windows Cloud Files provider runtime. |
| `platform/macos/LocalityFileProvider` | macOS File Provider extension and helper. |
| `templates/mount/AGENTS.md` | Generated mount guidance for coding agents. |
| `docs/` | Engineering notes, architecture, sync behavior, platform internals, and release references. |
| `docs-site/` | Public documentation site. |

## Notion Support

The Notion connector supports broad read/render coverage and conservative
writes. It can render paragraphs, headings, lists, to-dos, quotes, callouts,
code blocks, simple tables, dividers, equations, bookmark/embed/link-preview
URLs, child-page links, database rows, supported rich text, page/database
mentions, and file-like media. Unsupported or lossy Notion blocks are preserved
as `::loc{...}` directives with remote identity metadata when possible.

Writable support is intentionally narrower. Locality can update, append, move
safe directive-backed blocks, archive supported blocks, upload supported local
media, update supported page properties, create database rows, and reconcile
changed pages back into local shadows. Unsupported shapes fail before mutation
or pause for review rather than silently degrading the source.

## Safety And State

Locality treats local state as durable user state, not disposable cache:

- SQLite state lives under `~/.loc/`;
- credentials live in the OS credential store with metadata in SQLite;
- hydrated virtual-file content lives in Locality-managed content roots;
- push plans are journaled before remote apply;
- failed journals remain visible for recovery and audit;
- schema and compatibility changes are migrated or repaired rather than asking
  users to reset state.

For local recovery:

```sh
loc status /path/to/mount
loc inspect /path/to/page.md
loc restore /path/to/page.md
loc log --diff
```

For destructive local-state cleanup during development:

```sh
loc reset --yes
```

## Development

The root `Makefile` is the easiest entry point:

```sh
make help
make setup
make build
make test
```

Common targets:

| Target | What it does |
| --- | --- |
| `make build` | Builds the Rust workspace and desktop frontend. |
| `make check` | Runs Rust checks plus the desktop TypeScript/Vite build. |
| `make test` | Runs the default Rust workspace test suite. |
| `make ci` | Runs formatting and workspace tests similar to GitHub Actions. |
| `make lint` | Runs Rust formatting checks and clippy with warnings denied. |
| `make fmt` | Formats all Rust code. |
| `make dev-desktop` | Starts the desktop Vite dev server. |
| `make dev-tauri` | Builds debug sidecars and starts the Tauri desktop app. |
| `make build-tauri` | Builds the packaged Tauri desktop app locally. |
| `make run-cli ARGS='status --json'` | Runs the `loc` CLI with custom arguments. |
| `make clean` | Removes Rust and desktop build outputs. |


## Testing

Locality has fixture-backed unit and integration tests for the sync core, store,
CLI, daemon, Notion rendering/apply behavior, virtual filesystem paths, desktop
commands, and platform packaging checks.

Live Notion tests use scratch content in a disposable workspace. They are wired
into the `notion-live-e2e` GitHub Actions workflow when the repository secrets
are configured. The live jobs exercise connector behavior, mounted workflows,
Linux FUSE, Windows Cloud Files, and desktop Live Mode against real Notion API
calls.

For local live Notion testing, configure a writable parent page and use the
ignored tests or scripts documented in `docs/notion-connector.md` and
`docs/linux-fuse.md`.
