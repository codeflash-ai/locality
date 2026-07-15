# Locality macOS File Provider

This package contains the macOS online-only projection:

- a `NSFileProviderReplicatedExtension` implementation;
- a named File Provider service source for containing-app warm-up;
- `locality-file-providerctl`, a small domain registration helper;
- a minimal containing `Locality.app` bundle template; and
- Command Line Tools scripts for a local ad-hoc development bundle.

The extension delegates all durable state and network work to `localityd`. These
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
identifier: loc
display:    Locality
```

Each mount is exposed as a top-level mount-point folder inside that domain, for
example `Locality/notion-main`. The extension namespaces File Provider item
identifiers with the internal Locality `mount_id`, then sends the unwrapped mount
id and item identifier to `localityd`. This keeps Finder paths stable as multiple
mounts and connectors are added under one shared Locality root.

The extension talks to `localityd` over `127.0.0.1:38567` by default because
sandboxed app extensions should not depend on a Unix socket in `~/.loc`.

The desktop app registers the shared `loc` domain from the foreground app
process during onboarding. After registration it resolves the extension's named
File Provider service and opens a short-lived XPC connection, matching the
standard containing-app to provider-extension pattern. Finder's `Enable` button
is recovery behavior, not the normal activation path.

## Development Build

```sh
platform/macos/LocalityFileProvider/scripts/install-dev-bundle.sh
```

The script builds `Locality.app`, embeds `LocalityFileProvider.appex`, signs both
ad-hoc, installs the app to `~/Applications/Locality.app`, registers it with
LaunchServices, and starts the tiny background containing app.

After creating a mount with `--projection macos-file-provider`, register it:

```sh
loc file-provider register <mount-id-or-path>
loc file-provider open <mount-id-or-path>
loc file-provider list
loc file-provider unregister <mount-id-or-path>
```

`register` is idempotent: every macOS File Provider mount registers the shared
`loc` domain. Existing legacy per-mount domains can be removed with
`loc file-provider reset` after local edits are backed up or reconciled.

`open` asks macOS for the domain's user-visible File Provider URL and opens it
in Finder. Opening the raw mount root is not enough to test lazy enumeration:
Finder must enter the File Provider domain so directory listings call
`file_provider_children` on `localityd`.

Mount activation signals the shared domain root after adding a source. Because
macOS creates that source folder asynchronously, Locality briefly waits for the
new mount point before inspecting it. If the folder remains absent or File
Provider reports an unhealthy replica, Locality resets and re-registers the
shared domain, refreshes it from durable mount state, and waits for the source
folder to become healthy. Reconnecting an existing source retries this
activation path instead of only reloading daemon mounts.

Delete support still returns unsupported. Creates and renames are represented as
daemon virtual mutations and stay pending until the normal review and push flow
applies them to the remote source.
