# Linux FUSE Projection

Linux online-only mounts are implemented as an `locality-fuse` projection over the
daemon-owned virtual filesystem boundary. The daemon remains the authority for
SQLite state, hydration, network work, dirty-state transitions, pushes, and
reconciliation. FUSE is only the Linux presentation layer.

Mounts use projection mode `linux_fuse`, which shares the same no-placeholder
behavior as `macos_file_provider`.

## Responsibilities

- `localityd` owns durable mount/entity/shadow/journal state under `~/.loc/`.
- `localityd` serializes hydration, write, push, scheduled pull, and reconciliation
  jobs through the runtime queue.
- `locality-fuse` mounts a virtual tree and translates kernel filesystem callbacks
  into daemon IPC.
- `loc mount ... --projection linux-fuse` records a mount-point folder under the
  shared FUSE root; `loc file-provider register <mount>` can repair or restart
  the shared-root registration. The mount-point directory is virtual and appears
  from daemon state; `loc mount` does not pre-create it in the mounted root.
- `loc daemon start` prunes stale pre-shared-root systemd user units for the
  selected state directory so removed mounts do not remount independently of
  SQLite state.
- Hydrated and edited contents for virtual projections live under
  `~/.loc/content/<mount-id>/files/`; the mounted root remains virtual.
- Plain-directory projection remains the fallback for tests, unsupported systems,
  and recovery.

## FUSE Operations

- `lookup`/`getattr`: return metadata for online-only entities from
  `virtual_fs_item` without creating placeholder files.
- `readdir`: list child entries from `virtual_fs_children`.
- `open`/`read`: call `virtual_fs_materialize`, block until hydration completes,
  then read bytes from the daemon-materialized canonical Markdown path.
- `write`/`flush`: stage local contents under `~/.loc/fuse-staging/` and submit
  the final bytes through `virtual_fs_commit_write`. The daemon writes the
  content cache and marks dirty state; the FUSE process does not mutate SQLite or
  connector state directly.
- `create`/`rename`/`unlink`: submit daemon-owned virtual mutations. New
  Markdown files are kept in the content cache until `loc push` creates the
  remote page or database row; local deletes become pending remote archives.
- Database directories may expose a cached `_schema.yaml` file from scheduled
  pull so row property validation does not need to read through the FUSE mount.

## Smoke Test

CI runs a credentialless real-mount smoke test on GitHub-hosted Ubuntu. The
script creates a temporary Locality state directory, records a `linux_fuse` mount,
seeds hydrated fixture pages in the daemon content cache, starts `localityd`, starts
`locality-fuse`, and then verifies real filesystem operations through the mounted
directory:

```bash
LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 tests/linux_fuse_smoke.sh
```

The hosted test intentionally does not use Notion credentials. Live-provider
e2e should use a disposable test workspace/account with narrowly scoped
credentials. macOS File Provider e2e is not part of hosted CI; developers should
run it on local Macs where the signed app/extension and user approval are
available. The fixture-backed smoke starts `localityd` with
`LOCALITY_DAEMON_BACKGROUND_CONNECTOR_SYNC=disabled` so directory listings,
materialization, and local mutations exercise only the seeded daemon state
instead of queueing connector-backed refresh work with the dummy token.

## Live Notion FUSE E2E

The opt-in live FUSE script uses the real `loc`, `localityd`, and
`locality-fuse` binaries against a real Notion scratch page. It reads
`NOTION_TOKEN`/`NOTION_AT` when set, otherwise it reads the installed Locality
credential store under `~/.loc/credentials` for `connection:notion-default`.
Set `LOCALITY_NOTION_LIVE_CONNECTION_ID` to use another stored Notion
connection.

```bash
export LOCALITY_NOTION_LIVE_PARENT_PAGE=<writable-notion-parent-page>
LOCALITY_LIVE_NOTION_VFS_PUSH_PULL=1 tests/live_notion_vfs_push_pull.sh
```

When `LOCALITY_NOTION_PAGE_ID` or `NOTION_PAGE_ID` is not set, the script creates
and archives its own scratch page under `LOCALITY_NOTION_LIVE_PARENT_PAGE`. It
seeds an isolated test connection through `loc connect notion --token-stdin`,
then runs the FUSE mount, pull, edit, push, child create, child rename, and child
delete/archive path with token environment variables removed from `loc` and
`localityd`.

The `notion-live-e2e` GitHub Actions workflow runs this script on
`ubuntu-latest` when the live Notion environment is configured. That job seeds
`~/.loc/credentials` from the `NOTION_TOKEN` secret before invoking the script
with `NOTION_TOKEN` removed, so the CI path exercises the stored-credential
product path rather than relying on direct token environment variables.

### Existing Mount

Build the daemon, CLI, and FUSE helper:

```bash
cargo build -p localityd -p loc-cli -p locality-fuse
```

Create or reuse a Linux FUSE mount:

```bash
./target/debug/loc daemon start --session --localityd-bin "$PWD/target/debug/localityd"
./target/debug/loc mount notion "$HOME/Locality/notion-main" --root-page <notion-page-id> --mount-id notion-main --projection linux-fuse
./target/debug/loc pull "$HOME/Locality/notion-main"
```

Verify the daemon health, shared FUSE root, and mount-point directory listing:

```bash
./target/debug/loc doctor
findmnt -R "$HOME/Locality"
findmnt -T "$HOME/Locality/notion-main"
ls -la "$HOME/Locality"
ls -la "$HOME/Locality/notion-main"
```

For workspace Notion mounts, the FUSE virtual listing shows the synthetic
`Private/` and `Workspace/` roots under the mount point. These roots are folders
only and do not contain `page.md`. When the connection exposes a user owner,
accessible top-level workspace pages created by that owner or by that user's
bot appear under `Private/`; this is derived from the direct workspace parent
plus those creator IDs. Create private top-level pages under `Private/`; create
children inside existing page directories rather than directly under
`Workspace/`.

Read a projected Markdown file to force hydration:

```bash
head -n 40 "$HOME/Locality/notion-main/<projected-page>/page.md"
./target/debug/loc status "$HOME/Locality/notion-main" --json
```

Exercise local writes without pushing to Notion by saving the current content,
appending a smoke-test line, then writing the original bytes back:

```bash
file="$HOME/Locality/notion-main/<projected-page>/page.md"
backup="$(mktemp)"
cat "$file" > "$backup"
printf '\nFUSE smoke edit %s\n' "$(date -Is)" >> "$file"
./target/debug/loc status "$file"
cat "$backup" > "$file"
./target/debug/loc status "$file"
rm -f "$backup"
```

Exercise pending create, rename, and delete without touching remote state by
creating a draft inside a page child directory or database directory, renaming
it, and removing it before pushing:

```bash
parent="$HOME/Locality/notion-main/<page-or-database-directory>"
draft="$parent/locality-fuse-smoke.md"
renamed="$parent/locality-fuse-smoke-renamed.md"
printf '# FUSE smoke\n' > "$draft"
mv "$draft" "$renamed"
./target/debug/loc status "$renamed"
rm "$renamed"
./target/debug/loc status "$parent"
```

The same flow is available as an opt-in script for manual or CI-hosted FUSE
hosts. By default the script creates its own seeded mount. To keep the temp
state for debugging:

```bash
LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_KEEP_TMP=1 tests/linux_fuse_smoke.sh
```

If `ls` reports `Function not implemented`, rebuild the helper and restart the
shared-root registration so the running process has the latest FUSE operation
support:

```bash
cargo build -p loc-cli -p locality-fuse
./target/debug/loc file-provider register "$HOME/Locality/notion-main"
```

## Why Not Watchers

inotify is after-the-fact for this use case: by the time an access event arrives,
the caller may already have received placeholder bytes. fanotify permission
events can block an open, but they still cannot supply file contents; Locality
would need to create a real backing file before allowing the open. FUSE avoids
that mismatch because the Locality process serves directory entries, metadata, and
read bytes directly.

## Platform Boundary

The shared daemon API is `virtual_fs`, not `file_provider`. macOS File Provider,
Linux FUSE, and a future Windows Cloud Files projection should be separate
adapters over that API so platform-specific lifecycle and kernel integration do
not leak into daemon sync semantics.
