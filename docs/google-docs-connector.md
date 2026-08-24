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

The broker uses the shared `LOCALITY_GOOGLE_CLIENT_ID` and
`LOCALITY_GOOGLE_CLIENT_SECRET` pair for Google Docs, Google Calendar, and
Gmail. The Google OAuth client must allow all Google connector localhost
callbacks.

`documents` is used for Google Docs body read/write. `drive.file` keeps write
access limited to app-created or explicitly granted files. Locality does not
request Drive metadata scopes beyond the app-file write access covered by
`drive.file`.

The connector still keeps enumeration scoped to the mount workspace folder. It
does not expose arbitrary Drive traversal as a Locality mount.

## Google OAuth Verification

Keep the Google Cloud Console verification scope list aligned with
`connectors/oauth-verification/google-docs.json`.

Submit only these Google API scopes:

- `https://www.googleapis.com/auth/documents`
- `https://www.googleapis.com/auth/drive.file`

Do not submit full Drive, Drive readonly, writable Drive metadata, or Docs
readonly scopes for this connector.

The verification demo should show the OAuth consent screen with all requested
permissions readable, then demonstrate the code-backed user workflows: mounting
and enumerating the Drive workspace folder, reading a Google Doc body, editing
an existing Doc body, creating a new Google Doc, renaming and moving a Doc if
those workflows are exposed, and archiving or trashing a Doc if that workflow is
enabled. For every write shown in Locality, show the resulting change in the
user's Google Drive or Google Docs account.

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
- Drive `files.update` for title, parent move, and trash operations
- Drive `files.create` for new Google Docs and folders

Body updates use `writeControl.requiredRevisionId` when a synced Docs revision
is available. After apply, Locality re-fetches accepted remote state and
reconciles local Markdown and shadows.

Page-directory renames and parent moves plan as `move_entity`. The Google Docs
connector applies them with Drive metadata updates, replacing the old parent
with the new folder parent and updating the document name when needed.

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

## Live E2E

`tests/live_google_docs_vfs_roundtrip.sh` exercises the live Google Docs API,
CLI mount/pull/diff/push paths, `localityd`, and the Linux FUSE projection. It
uses isolated Locality state and a temporary shared root, creates one generated
Google Docs page through the mounted filesystem, verifies the create marker
survives a pull after push, edits the created document through mounted
`page.md`, verifies the edit marker survives another pull, and trashes the
created Drive file during cleanup.

Set the required environment from a stored `connection:google-docs-live`
credential and choose a scratch workspace folder:

```bash
secret_ref='connection:google-docs-live'
secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
export LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$(cat "$HOME/.loc/credentials/$secret_hex")"
export LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_FOLDER='Locality Live E2E'
```

Use the full stored credential JSON. The live harness requires
`access_token`, `oauth_broker_url`, `refresh_token_handle`, and numeric
`expires_at` so it can exercise broker refresh when the token expires.

Run the gated test explicitly:

```bash
LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 tests/live_google_docs_vfs_roundtrip.sh
```

For deeper local connector quality work, run the mutation scenario:

```bash
LOCALITY_LIVE_GOOGLE_DOCS_SCENARIO=1 tests/live_google_docs_mutation_scenario.sh
```

The scenario uses the same isolated state and scratch workspace folder as the
roundtrip test, then exercises the broader edit surface:

- creates a scratch Google Doc from a mounted `page.md`
- pulls it back as an existing one-line document
- edits that existing body into one line, one blank line, then another text line
- updates `title` frontmatter and verifies the Drive file name
- renames the page directory and verifies the Drive file name
- creates a scratch Drive folder through the Drive API, pulls it into the mount,
  moves the document under it through the mounted filesystem, and verifies the
  Drive parent
- deletes the mounted page directory through the filesystem and verifies the
  Drive file is trashed

The scenario also trashes its scratch Drive folder during cleanup. Set
`LOCALITY_GOOGLE_DOCS_SCENARIO_KEEP_TMP=1` to keep the temporary Locality state
and mount root after a failure for inspection.

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
