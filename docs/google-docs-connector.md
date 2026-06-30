# Google Docs Connector Summary

This document summarizes the first Google Docs connector implementation and the
follow-up fixes made during live testing.

## Connector Scope

Google Docs is registered as a first-party Locality source connector named
`google-docs`. It uses the same host semantics as the Notion connector:
connect, mount, enumerate, hydrate, diff, push, status, and live projection
paths resolve through the source registry.

The mounted remote root is a configured Google Drive workspace folder. The
folder id is stored in `MountConfig.remote_root_id`, so no SQLite schema change
was required for Google Docs mounts.

## OAuth And Drive Access

The Google Docs OAuth flow uses the Locality OAuth broker and requests:

- `openid`
- `email`
- `profile`
- `https://www.googleapis.com/auth/documents`
- `https://www.googleapis.com/auth/drive.file`
- `https://www.googleapis.com/auth/drive.metadata`

`documents` is used for Google Docs body read/write. `drive.file` keeps write
access limited to app-created or explicitly granted files. `drive.metadata`
allows Locality to discover Drive metadata for Google Docs and folders inside
the configured workspace folder, including Docs manually created in that folder.

The connector still keeps enumeration scoped to the mount workspace folder. It
does not expose arbitrary Drive traversal as a Locality mount.

## Projection

Drive folders project as local directories. Google Docs project as page
directories containing `page.md`.

Examples under a shared Locality root:

```text
~/Locality/
  google-docs-main/
    project-notes/
      page.md
    planning/
      sprint-plan/
        page.md
```

Non-Google-Docs Drive files are ignored by the V1 connector.

## Hydration And Markdown

Hydration fetches Drive metadata and Google Docs body content, then renders a
canonical Markdown document with connector-neutral Locality frontmatter. The
renderer supports common Google Docs structures:

- paragraphs and headings
- bold, italic, underline, strikethrough
- links
- inline code where representable
- bullets and numbered lists
- simple tables

Unsupported Google Docs structures are rendered as `::loc{...}` directives and
validated as push-blocking if an edit would be lossy.

## Push Behavior

Local changes use the existing shadow and push planner flow. The connector maps
operations to:

- Docs `documents.batchUpdate` for body edits
- Drive `files.update` for title, parent, and trash operations
- Drive `files.create` for new Google Docs and folders

Body updates use `writeControl.requiredRevisionId` when a synced Docs revision
is available. After apply, Locality re-fetches accepted remote state and
reconciles local Markdown and shadows.

Failed Google Docs creates now trash the just-created Drive file when body
insertion fails, preventing partial empty remote documents.

## Live Testing Fixes

Live testing found and fixed several integration issues:

- Creating a directory under the Google Docs mount-point root now uses the mount
  workspace folder id as the remote parent.
- Push planning now treats the mount remote root as a valid directory parent for
  pending creates.
- Google Docs create preconditions without a synced remote version no longer
  cause false concurrency conflicts.
- `loc diff` plain text summaries now include entity creates and archives.
- `loc status` treats Drive-only observations as equivalent to synced
  Drive-plus-Docs versions when the Drive version matches.
- Local OAuth callback handling now binds `localhost` redirects on IPv4
  loopback and launches the browser asynchronously so the callback listener can
  process redirects while the browser remains open.

## Current Limitations

- Only Google Docs and Drive folders are projected.
- Google Sheets, Slides, binary Drive files, comments, suggestions, and rich
  unsupported Docs structures are not editable through V1.
- Unsupported structures must be preserved or they block push.
- The OAuth broker project must have both Google Docs API and Google Drive API
  enabled.

## Useful Commands

Connect with the local broker:

```bash
./target/debug/loc connect google-docs --name google-docs-default --broker-url http://127.0.0.1:8787
```

Mount a workspace folder:

```bash
./target/debug/loc mount google-docs ~/Locality/google-docs-main --workspace-folder "Locality" --projection linux-fuse
```

Force enumeration and hydration:

```bash
./target/debug/loc pull --json "$HOME/Locality/google-docs-main"
```

Inspect planned pushes:

```bash
./target/debug/loc status "$HOME/Locality/google-docs-main"
./target/debug/loc diff "$HOME/Locality/google-docs-main"
```
