# AgentFS macOS File Provider

This package contains the macOS online-only projection:

- a `NSFileProviderReplicatedExtension` implementation;
- `agentfs-file-providerctl`, a small domain registration helper;
- a minimal containing `AgentFS.app` bundle template; and
- Command Line Tools scripts for a local ad-hoc development bundle.

The extension delegates all durable state and network work to `afsd`. These
macOS IPC commands are compatibility aliases over the daemon's platform-neutral
`virtual_fs` boundary:

- `item(for:)` calls `file_provider_item` for store-only metadata.
- `enumerator(for:)` calls `file_provider_children` for dataless directory
  listings.
- `fetchContents(for:)` calls `file_provider_materialize`, which blocks until
  the daemon hydrates the page, then copies the materialized Markdown into File
  Provider's transfer directory before returning it to the system.
- `modifyItem(_:contents:)` accepts edits to existing `page.md` files and calls
  `virtual_fs_commit_write`. The daemon writes the replacement bytes to the
  virtual content cache and marks the page dirty so the normal review and push
  flow can decide when to update Notion.
- `createItem(basedOn:contents:)` accepts new Markdown files and new page
  directories. A new directory is recorded as a pending page create whose
  writable body is the synthesized `page.md` file inside that directory.

macOS uses one shared File Provider domain:

```text
identifier: afs
display:    AFS
```

Each connected source is exposed as a top-level folder inside that domain, for
example `AFS/notion`. The extension namespaces File Provider item identifiers
with the internal AgentFS `mount_id`, then sends the unwrapped mount id and item
identifier to `afsd`. This keeps Finder paths stable as more connectors are
added without exposing mount ids in user-visible folder names.

The extension talks to `afsd` over `127.0.0.1:38567` by default because
sandboxed app extensions should not depend on a Unix socket in `~/.afs`.

## Development Build

```sh
platform/macos/AgentFSFileProvider/scripts/install-dev-bundle.sh
```

The script builds `AgentFS.app`, embeds `AgentFSFileProvider.appex`, signs both
ad-hoc, installs the app to `~/Applications/AgentFS.app`, registers it with
LaunchServices, and starts the tiny background containing app.

After creating a mount with `--projection macos-file-provider`, register it:

```sh
afs file-provider register <mount-id-or-path>
afs file-provider open <mount-id-or-path>
afs file-provider list
afs file-provider unregister <mount-id-or-path>
```

`register` is idempotent: every macOS File Provider mount registers the shared
`afs` domain. Existing legacy per-mount domains can be removed with
`afs file-provider reset` after local edits are backed up or reconciled.

`open` asks macOS for the domain's user-visible File Provider URL and opens it
in Finder. Opening the raw mount root is not enough to test lazy enumeration:
Finder must enter the File Provider domain so directory listings call
`file_provider_children` on `afsd`.

Delete support still returns unsupported. Creates and renames are represented as
daemon virtual mutations and stay pending until the normal review and push flow
applies them to the remote source.
