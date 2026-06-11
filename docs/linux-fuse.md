# Linux FUSE Projection

Linux online-only mounts are implemented as an `afs-fuse` projection over the
daemon-owned virtual filesystem boundary. The daemon remains the authority for
SQLite state, hydration, network work, dirty-state transitions, pushes, and
reconciliation. FUSE is only the Linux presentation layer.

Mounts use projection mode `linux_fuse`, which shares the same no-placeholder
behavior as `macos_file_provider`.

## Responsibilities

- `afsd` owns durable mount/entity/shadow/journal state under `~/.afs/`.
- `afsd` serializes hydration, write, push, scheduled pull, and reconciliation
  jobs through the runtime queue.
- `afs-fuse` mounts a virtual tree and translates kernel filesystem callbacks
  into daemon IPC.
- `afs file-provider register <mount>` creates and starts a per-mount systemd
  user service for `afs-fuse`.
- Hydrated and edited contents for virtual projections live under
  `~/.afs/content/<mount-id>/files/`; the mounted root remains virtual.
- Plain-directory projection remains the fallback for tests, unsupported systems,
  and recovery.

## FUSE Operations

- `lookup`/`getattr`: return metadata for online-only entities from
  `virtual_fs_item` without creating placeholder files.
- `readdir`: list child entries from `virtual_fs_children`.
- `open`/`read`: call `virtual_fs_materialize`, block until hydration completes,
  then read bytes from the daemon-materialized canonical Markdown path.
- `write`/`flush`: stage local contents under `~/.afs/fuse-staging/` and submit
  the final bytes through `virtual_fs_commit_write`. The daemon writes the
  content cache and marks dirty state; the FUSE process does not mutate SQLite or
  connector state directly.

## Smoke Test

Build the daemon, CLI, and FUSE helper:

```bash
cargo build -p afsd -p afs-cli -p afs-fuse
```

Create or reuse a Linux FUSE mount:

```bash
./target/debug/afs daemon start --session --afsd-bin "$PWD/target/debug/afsd"
./target/debug/afs mount notion /path/to/mount --root-page <notion-page-id> --mount-id notion-test --projection linux-fuse
./target/debug/afs pull /path/to/mount
./target/debug/afs file-provider register /path/to/mount
```

Verify the mount and directory listing:

```bash
systemctl --user status ai.codeflash.afs.fuse.notion-test.service --no-pager
findmnt -R /path/to/mount
ls -la /path/to/mount
```

Read a projected Markdown file to force hydration:

```bash
head -n 40 "/path/to/mount/<projected-page>.md"
./target/debug/afs status /path/to/mount --json
```

Exercise local writes without pushing to Notion by saving the current content,
appending a smoke-test line, then writing the original bytes back:

```bash
file="/path/to/mount/<projected-page>.md"
backup="$(mktemp)"
cat "$file" > "$backup"
printf '\nFUSE smoke edit %s\n' "$(date -Is)" >> "$file"
./target/debug/afs status "$file"
cat "$backup" > "$file"
./target/debug/afs status "$file"
rm -f "$backup"
```

If `ls` reports `Function not implemented`, rebuild the helper and restart the
per-mount service so the running process has the latest FUSE operation support:

```bash
cargo build -p afs-cli -p afs-fuse
./target/debug/afs file-provider register /path/to/mount
systemctl --user restart ai.codeflash.afs.fuse.notion-test.service
```

## Why Not Watchers

inotify is after-the-fact for this use case: by the time an access event arrives,
the caller may already have received placeholder bytes. fanotify permission
events can block an open, but they still cannot supply file contents; AgentFS
would need to create a real backing file before allowing the open. FUSE avoids
that mismatch because the AgentFS process serves directory entries, metadata, and
read bytes directly.

## Platform Boundary

The shared daemon API is `virtual_fs`, not `file_provider`. macOS File Provider,
Linux FUSE, and a future Windows Cloud Files projection should be separate
adapters over that API so platform-specific lifecycle and kernel integration do
not leak into daemon sync semantics.
