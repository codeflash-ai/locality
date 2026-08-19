# Locality macOS File Provider

This package contains the macOS online-only projection:

- a `NSFileProviderReplicatedExtension` implementation;
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

Mount activation signals the working-set enumerator after adding a source
because macOS can ignore a root-container signal when no root enumerator is
active. The working set recursively reads every already-known item from local
daemon state, without calling connector APIs, so macOS can ingest cached nested
directories before Finder opens them. Compact sync anchors reference
rebuildable item-version snapshots in the File Provider app-group cache;
subsequent change enumerations report only new, changed, or deleted items while
anchors stay within macOS's 500-byte limit. A missing or incompatible snapshot
expires its anchor and falls back to a clean enumeration. Adding a source can
therefore insert its mount point and cached descendants without updating an
unchanged sibling subtree. Reimport and readiness repair stay scoped to the new
mount-point identifier.
Because macOS creates a source folder asynchronously, Locality waits for it
before inspecting it and retries the scoped refresh once. Automatic activation
never resets or re-registers the shared domain. Reconnecting an existing source
retries this activation path instead of only reloading daemon mounts.

Creates, renames, and supported page/draft deletes are represented as daemon
virtual mutations and stay pending until the normal review and push flow applies
them to the remote source. Mount points and remote-only items remain protected
from unsupported deletion.

## Live Kernel E2E

`tests/live_macos_file_provider.sh` exercises the installed extension through
the real user-visible CloudStorage directory. It creates an isolated Locality
state directory and a scratch Notion page, then verifies File Provider
enumeration, hydration, an atomic `page.md.tmp.*` replacement, push, child-page
create, rename, and delete. Cleanup archives scratch Notion content and refreshes
the shared domain against an empty temporary state so the test mount disappears.
The harness creates its isolated tree directly under short `/tmp` instead of
Darwin's long per-user `TMPDIR`, and validates that `localityd.sock` fits the
Darwin Unix-domain socket limit before registering a mount. Both primary and
cleanup-daemon readiness failures emit redacted start, status, and log tails.

The test deliberately does not register, unregister, or reset the shared `loc`
domain. Run it only in a dedicated macOS user session with a signed test app
whose stable bundle identity has already been enabled in Finder or System
Settings:

```sh
export LOCALITY_MACOS_FILE_PROVIDER_LIVE=1
export LOCALITY_MACOS_FILE_PROVIDER_DEDICATED_HOST=1
export LOCALITY_MACOS_FILE_PROVIDER_APP='/Applications/Locality File Provider Test.app'
export LOCALITY_MACOS_FILE_PROVIDER_EXPECTED_BUNDLE_ID='ai.codeflash.locality.fileprovidertest'
export NOTION_TOKEN=...
export LOCALITY_NOTION_LIVE_PARENT_PAGE=...
make test-live-macos-file-provider
```

The manual `macos-file-provider-live-e2e` workflow builds the current checkout,
reinstalls the stable test identity without resetting its approved domain, and
runs this test on a runner labeled `self-hosted`, `macOS`, and
`locality-file-provider`. A first-time runner must approve that stable extension
identity before the workflow can pass.
