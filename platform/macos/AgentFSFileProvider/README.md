# AgentFS macOS File Provider

This package is the first macOS online-only projection slice. It is not yet a
signed app extension bundle; it is a buildable Swift package that contains the
extension implementation we will move into an Xcode target with the File
Provider entitlement.

The extension delegates all durable state and network work to `afsd`:

- `item(for:)` calls `file_provider_item` for store-only metadata.
- `enumerator(for:)` calls `file_provider_children` for dataless directory
  listings.
- `fetchContents(for:)` calls `file_provider_materialize`, which blocks until
  the daemon hydrates the page and returns a local Markdown file URL.

The File Provider domain identifier must be the AgentFS `mount_id`; the daemon
uses that to resolve the mounted Notion tree. The current write callbacks return
unsupported because the next slice should route File Provider edits through the
same daemon push/reconciler path as explicit `afs push`.
