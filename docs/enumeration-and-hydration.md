# Enumeration And Hydration Mechanics

This document is derived from the implementation, not from earlier docs. If this
file and the code disagree, the code wins. The most important code paths are
listed inline so future audits can re-check behavior from source.

## Scope

This document answers two questions:

- When does Locality enumerate remote files or directories?
- When does Locality hydrate remote file bodies into local Markdown, assets,
  shadows, or provider data?

In this document:

- **Full enumeration** means asking a connector for a mount-wide tree with
  `Connector::enumerate` or `ScheduledPullSource::enumerate_mount`.
- **Child enumeration** means asking a connector for one container's immediate
  children with `Connector::list_children`.
- **Observation** means asking a connector for metadata or freshness for a
  known entity. Observation is not enumeration and is not hydration by itself.
- **Batch observation** means asking a connector for mount-wide metadata
  changes relative to an opaque connector checkpoint. It is discovery, but it
  is neither full-tree enumeration nor body hydration.
- **Hydration** means fetching/rendering a remote entity body through
  `fetch_render` and then using the result for a local file, shadow, assets, or
  comparison.

## Core Contracts

The connector boundary is in `crates/locality-connector/src/lib.rs`.

- `Connector::enumerate(EnumerateRequest)` returns a tree of `TreeEntry`
  metadata for a mount. It is the full-tree path.
- `Connector::observe(ObserveRequest)` returns metadata/freshness for a known
  remote entity. It does not fetch body content.
- `Connector::observe_batch(BatchObserveRequest)` returns metadata upserts and
  explicit tombstones plus a connector-owned next checkpoint. A `Complete`
  result authorizes omission-based removal only inside the configured mount
  scope. An `Incremental` result never treats omission as deletion.
- `Connector::list_children(ListChildrenRequest)` returns immediate child
  metadata for a `ChildContainer`. The trait comments explicitly keep it to
  metadata and require implementations not to fetch page bodies.
- `Connector::fetch(FetchRequest)` returns the connector-native entity body.

Unlike `enumerate`, batch observation returns changes rather than requiring a
full remote tree on every call. Unlike `observe`, it can discover entities that
are not already known locally. All three paths remain metadata-only unless a
separate fetch/render path hydrates an entity. The daemon must persist the
opaque checkpoint JSON only after the complete batch has been validated and
reconciled successfully.

Daemon hydration uses `fetch_render`, not the raw connector `fetch` method
directly. The common daemon-side abstraction is `HydrationSource::fetch_render`
in `crates/localityd/src/hydration.rs`; concrete adapters live in
`crates/localityd/src/notion.rs` and `crates/localityd/src/google_docs.rs`.

The persisted model lives in `crates/locality-core/src/model.rs`.

- `TreeEntry` is the connector metadata shape used by enumeration and child
  listing.
- `HydrationState` separates `Virtual`, `Stub`, `Hydrated`, `Dirty`, and
  `Conflicted`.
- `CanonicalDocument` can carry a stub marker, which is how plain-file stubs
  are recognized without needing body content.

Hydration requests live in `crates/locality-core/src/hydration.rs`.

- `HydrationRequest` identifies the mount, remote entity, target state, and
  reason.
- `HydrationReason` currently includes `ExplicitPull`, `FileOpen`, `Policy`,
  `RemoteFastForward`, `LiveModeRemoteFastForward`, `StubRead`, and `Prefetch`.
- `HydrationReason::is_remote_fast_forward` groups `RemoteFastForward` and
  `LiveModeRemoteFastForward`.

## Full Enumeration Triggers

### Explicit `loc pull` At A Mount Root

CLI entry point:

- `crates/loc-cli/src/commands.rs`
- `DaemonRequest::Pull`
- fallback direct call to `run_pull_with_state_root` when the daemon path cannot
  be used safely

Daemon/direct implementation:

- `crates/localityd/src/pull.rs`
- `run_pull_with_state_root`
- `should_pull_mount_root`
- `pull_mount_root`

`run_pull_with_state_root` decides whether the target path is a mount root, a
virtual directory, or a single entity path. `should_pull_mount_root` treats
plain mount roots, plain directories, and virtual roots with a `remote_root_id`
as mount-root pulls. Workspace-level virtual mounts without `remote_root_id` do
not run full enumeration from this path.

For a mount-root pull, `pull_mount_root` calls the source's full enumeration
path, merges records into the store, writes projection stubs, optionally
hydrates the mount root entry, and repairs missing media for entries whose local
metadata indicates an incomplete asset set.

Important behavior:

- Child pages and rows discovered by full enumeration are usually written as
  metadata/stubs, not hydrated bodies.
- The remote root page may be hydrated as part of mount-root pull when
  `should_hydrate_mount_root_entry` allows it.
- Missing media repair can fetch body/render data for already-known hydrated
  entities when assets are incomplete.

### Scheduled Pull And Live Mode Reconciliation

Runtime scheduler:

- `crates/localityd/src/runtime.rs`
- `run_scheduled_pull`
- scheduler advancement around `pending_scheduled_tick`
- job priority order in `run_next_ready_job`

Reconciliation:

- `crates/localityd/src/reconcile.rs`
- `reconcile_scheduled_pull_with_state_root`
- `ScheduledPullSource::enumerate_mount`
- `record_remote_observation`
- `policy_hydration`
- `remote_fast_forward_hydration`

Scheduled pull enumerates due mounts through `enumerate_mount`, records remote
observations, refreshes projections/stubs/schema, and may enqueue hydration.
The strategy skips workspace-level virtual mounts that do not have a
`remote_root_id`. Idle ticks do not enumerate.

The hydration enqueues are conservative:

- `Policy` hydration is used for default policy-selected entities such as an
  eager root hydration.
- `RemoteFastForward` hydration is queued when an already-hydrated entity has a
  changed remote version and local content can still be safely replaced.

### Notion URL Search Miss Refresh

CLI search path:

- `crates/loc-cli/src/commands.rs`
- search command handling
- `refresh_search_mount_metadata`
- `refresh_search_mount_metadata_direct`

When searching for a Notion URL and the local metadata lookup misses, the CLI
refreshes each relevant Notion mount. It prefers `DaemonRequest::Pull`; direct
fallback calls `run_pull_with_state_root`. Depending on the target mount, this
can become the same full mount-root enumeration described above.

## Child Enumeration Triggers

Child enumeration uses `list_children`, not full `enumerate`. It is the normal
path for browsing virtual mounts, File Provider folders, FUSE directories, and
Windows Cloud Files directories.

### Virtual FS And File Provider Children Requests

IPC requests:

- `crates/localityd/src/ipc.rs`
- `DaemonRequest::VirtualFsChildren`
- `DaemonRequest::FileProviderChildren`

Runtime handling:

- `crates/localityd/src/runtime.rs`
- request handling for `VirtualFsChildren` and `FileProviderChildren`
- `queue_child_refresh`
- `run_virtual_fs_refresh_children`
- child-refresh completion handling
- `queue_child_refresh_descendants`

Local metadata response:

- `crates/localityd/src/virtual_fs.rs`
- `virtual_fs_children`
- `virtual_fs_item`

The daemon answers `VirtualFsChildren` and `FileProviderChildren` from the local
store first. If that local response succeeds, runtime queues an interactive
child refresh for the same container. The queued refresh is the part that may
call the remote connector.

The local response does not hydrate bodies. It only projects the current store
state, local mutations, and virtual entries into a children report.

### Queued Child Refresh

Implementation:

- `crates/localityd/src/virtual_fs.rs`
- `virtual_fs_children_refresh_needed`
- `refresh_virtual_fs_children`
- `child_container_for_identifier`

`virtual_fs_children_refresh_needed` only returns true when the identifier maps
to a source-backed child container. `child_container_for_identifier` maps
virtual identifiers such as root, mount point, page, database, and source-root
containers to `ChildContainer`.

`refresh_virtual_fs_children` calls `connector.list_children`, saves returned
metadata records, refreshes database schema cache when supplied, and handles
source-root rehoming by listing sibling containers. A child result explicitly
declares whether it is a complete snapshot or an incremental subset. Only a
complete snapshot may prune clean old children that disappeared remotely;
incremental results merge returned identities without interpreting omitted
children as deletions.

Runtime child-refresh behavior:

- Background connector sync must be enabled for queued background refreshes.
- Interactive refreshes are queued after foreground children requests.
- Completed refreshes may invalidate platform provider caches.
- Changed child sets can queue descendant refreshes for already-known child
  directories.

### Daemon Startup And Reload Priming

Implementation:

- `crates/localityd/src/server.rs`
- `crates/localityd/src/runtime.rs`
- `queue_initial_virtual_mount_refreshes`

When daemon startup or mount reload sees virtual mounts with background
connector sync enabled, runtime queues background refreshes for the virtual root
and mount-point containers. These are child refreshes, not full enumerations.

### Remote Fast-Forward Child Discovery

Implementation:

- `crates/localityd/src/runtime.rs`
- `remote_fast_forward_discovery_hints`
- child-refresh queuing after hydration/observation results

When remote fast-forward handling detects that child links may have changed, it
can queue background child refreshes. The actual body update remains a hydration
decision; the child discovery part uses `list_children`.

### `loc pull` At A Virtual Directory

Implementation:

- `crates/localityd/src/pull.rs`
- `pull_virtual_directory_path`
- `hydrate_page_descendants`

For a virtual directory path, `loc pull` uses `list_children` for that
container. Database row directories can hydrate rows up to the configured row
limit. Page directories can recursively hydrate page descendants by hydrating
the page and then listing its child containers.

This path is narrower than mount-root full enumeration: it starts from the
selected virtual container and walks according to virtual-directory semantics.

### Search Ancestor Prefetch

Implementation:

- `crates/loc-cli/src/commands.rs`
- Notion URL search handling
- ancestor container requests through `DaemonRequest::FileProviderChildren`

When a Notion URL search result needs ancestors materialized for a provider
view, the CLI asks the daemon for ancestor container children. The immediate
response is local metadata; the daemon then queues normal child refreshes.

### Platform Browsing: macOS File Provider

Implementation:

- `platform/macos/LocalityFileProvider/LocalityEnumerator.swift`
- `platform/macos/LocalityFileProvider/LocalityDaemonClient.swift`
- `platform/macos/LocalityFileProvider/LocalityFileProviderExtension.swift`

macOS `enumerateItems` calls into the daemon children path. The daemon client
maps provider children requests to `DaemonRequest::FileProviderChildren`.

Fetching file contents uses a different path: `fetchContents` calls the daemon
read/materialize path, which can hydrate. Folder enumeration itself is child
metadata listing.

### Platform Browsing: Linux FUSE

Implementation:

- `platform/linux/locality-fuse/src/linux.rs`

FUSE directory and path operations call the daemon children path:

- path resolution walks use `children`
- lookup/getattr may resolve paths through children
- `readdir` and `readdirplus` call children

File `open` and `read` use materialize/read paths and can hydrate page content.
Directory browsing is child enumeration.

### Platform Browsing: Windows Cloud Files

Implementation:

- `platform/windows/locality-cloud-files/src/main.rs`

The Windows provider context maps folder children requests to
`DaemonRequest::FileProviderChildren` and file content requests to
`DaemonRequest::FileProviderRead`.

Startup placeholder seeding calls the daemon children path to populate root and
child directory placeholders. Fetch-placeholders callbacks also call children.
Fetch-data callbacks call read, which can hydrate.

## Connector Enumeration Implementations

### Notion

Implementation:

- `crates/locality-notion/src/lib.rs`
- `crates/locality-notion/src/projection.rs`
- daemon adapter in `crates/localityd/src/notion.rs`

Notion full enumeration:

- `NotionConnector::enumerate`
- `enumerate_root_page_tree`
- `enumerate_shared_pages`

Root-page enumeration starts at the configured root page and recursively
projects page children. Shared-page enumeration builds a workspace snapshot by
searching pages and databases, then projects private and workspace trees.

Notion child enumeration:

- `NotionConnector::list_children`
- `list_container_children`
- root/source listing helpers
- page child listing helpers
- database row listing helpers

The child path maps `ChildContainer` to exactly one immediate remote container
and returns metadata records, not page bodies.

Notion hydration:

- daemon `fetch_render` adapter in `crates/localityd/src/notion.rs`
- connector-native body fetches in `crates/locality-notion/src/fetch.rs`
- rendering/media helpers under `crates/locality-notion/src/`

### Google Docs

Implementation:

- `crates/locality-google-docs/src/connector.rs`
- daemon adapter in `crates/localityd/src/google_docs.rs`

Google Docs full enumeration:

- `GoogleDocsConnector::enumerate`
- `enumerate_drive_tree`
- `list_drive_children`

The connector recursively lists Google Drive children, pages through Drive API
results, filters trashed entries, and projects supported entries.

Google Docs child enumeration:

- `GoogleDocsConnector::list_children`
- Drive child listing helpers

Google Docs hydration:

- daemon `fetch_render` adapter in `crates/localityd/src/google_docs.rs`
- connector `fetch` path in `crates/locality-google-docs/src/connector.rs`

## Hydration Triggers

### Explicit Pull Of A Page Or Hydrating Entity

Implementation:

- `crates/localityd/src/pull.rs`
- `pull_entity_path`
- `hydrate_entity`

When `loc pull` targets an entity path, `pull_entity_path` calls
`hydrate_entity`. `hydrate_entity` calls `source.fetch_render` with
`HydrationReason::ExplicitPull`, writes rendered files/assets/schema data, and
updates the entity and shadow if the local state allows the result to be
accepted.

Mount-root pull may also hydrate the root entity or repair missing media, as
described in the full-enumeration section.

Virtual-directory pull may hydrate rows or page descendants after child listing.

### Direct Daemon Hydrate And Remote Fast-Forward Requests

Implementation:

- `crates/localityd/src/runtime.rs`
- request handling for `DaemonRequest::Hydrate`
- request handling for `DaemonRequest::RemoteFastForward`
- `queue_hydration`

`DaemonRequest::Hydrate` queues a hydration request with
`HydrationReason::FileOpen`. `DaemonRequest::RemoteFastForward` queues
`HydrationReason::LiveModeRemoteFastForward`.

Both go through the runtime hydration queue and executor, so they are subject to
deduplication, priority, and replacement safety checks.

### Virtual FS And File Provider Materialize/Read

IPC requests:

- `DaemonRequest::VirtualFsMaterialize`
- `DaemonRequest::VirtualFsRead`
- `DaemonRequest::FileProviderMaterialize`
- `DaemonRequest::FileProviderRead`

Runtime implementation:

- `crates/localityd/src/runtime.rs`
- `run_virtual_fs_materialize`
- `run_file_provider_read`
- `file_provider_read_materialized`

Materialization implementation:

- `crates/localityd/src/virtual_fs.rs`
- `materialize_virtual_fs_item`
- `materialize_virtual_fs_item_with_content_root`

Materialize/read requests resolve a virtual item and, for page content that is
not already `Hydrated`, `Dirty`, or `Conflicted`, call hydration with
`HydrationReason::FileOpen`.

Special cases that do not fetch a remote page body include guidance files,
locally-created virtual entries, schema files, and non-page virtual entries.

Runtime also records file-open freshness after successful materialize/read so
later freshness policy has a signal that the page was used.

### Plain-File Stub Read

Watch setup:

- `crates/localityd/src/server.rs`
- plain-file mounts only

Watcher implementation:

- `crates/localityd/src/watcher.rs`
- event classification
- polling stub access detection

Runtime handling:

- `crates/localityd/src/runtime.rs`
- file event read handling
- `should_hydrate_on_read`

For plain-file projections, Locality watches file events. Open/access/read/atime
events map to read events. A polling path also scans plain mounts for page
entities in `Virtual` or `Stub` state and detects accessed-time changes.

On a read event, runtime records file-open freshness. If `should_hydrate_on_read`
returns true, it queues hydration with `HydrationReason::StubRead`.

`should_hydrate_on_read` is intentionally narrow: the entity must be a page and
must currently be `Virtual` or `Stub`.

### Scheduled/Freshness Remote Fast-Forward

Implementation:

- `crates/localityd/src/reconcile.rs`
- `crates/localityd/src/runtime.rs`
- `apply_remote_observation`
- `auto_fast_forward_requests_from_observation`
- `auto_fast_forward_queue_decision`

Scheduled pull can queue `RemoteFastForward` after full enumeration notices a
changed remote version for an already hydrated entity.

Freshness observation can also queue remote fast-forward. The flow is:

1. Runtime queues or receives an observe job for a known entity.
2. The connector observes remote metadata.
3. Runtime records the observation and sets remote-hint state when the remote
   version changed.
4. `auto_fast_forward_requests_from_observation` considers a hydration request.
5. `auto_fast_forward_queue_decision` only allows the request when the entity is
   still a page, still hydrated, has remote-hint freshness, has local content
   matching the shadow, and does not have an active lease.

If those checks fail, the fast-forward is skipped or delayed. This prevents live
mode from overwriting dirty local work.

### Workspace Virtual Freshness Jobs

Implementation:

- `crates/localityd/src/runtime.rs`
- `workspace_virtual_freshness_jobs`

Workspace virtual mounts can use bounded freshness jobs for already-known pages
instead of full enumeration. Those jobs are observation jobs, not enumeration.
Their observations may later lead to fast-forward hydration as described above.

### `loc inspect`

Implementation:

- `crates/loc-cli/src/commands.rs`
- `crates/loc-cli/src/inspect.rs`
- `run_inspect`

`loc inspect` is an explicit remote body fetch/render path, but it is not a
persisted hydration path. It resolves local page/shadow/cache state, reads local
contents, calls `source.fetch_render` with `HydrationReason::ExplicitPull`, and
uses the rendered result for comparison and reporting.

It does not save the entity, update the shadow, or write the rendered Markdown
to the mounted projection.

### Push Preflight And Post-Apply Reconciliation

Implementation:

- `crates/localityd/src/push.rs`
- remote tree/content guard paths
- post-apply reconciliation paths
- `accept_post_apply_remote`

Push can fetch/render remote bodies outside the hydration queue.

Before applying a push, the remote guard can call `fetch_render` with
`HydrationReason::ExplicitPull` to compare remote content against expected state.
After applying creates or updates, push reconciliation can fetch/render the
resulting remote entity, write assets/Markdown, save entity and shadow state,
record observation, and clear remote hints.

This is body fetch and local materialization, but it is owned by push execution
rather than the runtime hydration queue.

## Hydration Queue And Executor Mechanics

Queue implementation:

- `crates/localityd/src/hydration.rs`
- `HydrationQueue`

Runtime integration:

- `crates/localityd/src/runtime.rs`
- `queue_hydration`
- persisted hydration-job loading
- hydration completion/retry handling
- job priority order

Executor:

- `crates/localityd/src/hydration.rs`
- `HydrationExecutor`
- `can_replace_file`

The queue is keyed by mount and remote id. Duplicate requests merge, and the
highest priority reason wins.

Current priority groups:

- High: `ExplicitPull`, `FileOpen`, `LiveModeRemoteFastForward`, `StubRead`
- Normal: `Policy`, `RemoteFastForward`
- Low: `Prefetch`

Production code currently has no non-test enqueue of `Prefetch`.

The runtime schedules work in this order:

1. Pending foreground requests.
2. Hydration queue.
3. Freshness queue.
4. Scheduled pull.

The executor only hydrates to a `Hydrated` target state. It loads mount and
entity records, maps virtual projections to the content root when needed, checks
whether the target file can be replaced, calls `fetch_render`, validates shadow
identity, writes assets/media/schema/Markdown, saves the shadow/entity, and
clears remote hints when the rendered remote version is current.

`can_replace_file` allows replacement when:

- the target file is missing
- the target file is still a stub
- parsed local content still matches the stored shadow

Remote fast-forward hydration skips or delays replacement when local content is
dirty, conflicted, leased, or otherwise not safely replaceable.

## Non-Triggers And Boundaries

These paths are intentionally local metadata or local comparison paths unless
they call one of the trigger paths above.

- `virtual_fs_item` in `crates/localityd/src/virtual_fs.rs` returns one local
  virtual item from the store. It does not call a connector.
- `virtual_fs_children` in `crates/localityd/src/virtual_fs.rs` returns local
  children from the store and mutation state. It only schedules refresh in
  runtime after the response succeeds.
- `file_provider_children` in `crates/localityd/src/file_provider.rs` aliases
  the virtual FS children path. The local response is metadata-only.
- `virtual_projection_root_children` in
  `crates/localityd/src/virtual_projection.rs` builds virtual root entries from
  local mount configuration. It does not call a connector.
- `observe` paths are metadata/freshness checks. They can lead to later
  fast-forward hydration, but the observation call itself does not fetch bodies.
- `observe_batch` is mount-wide metadata discovery. Its upserts remain stubs or
  metadata until a separate hydration trigger fetches their bodies.
- `list_children` paths must not fetch page bodies according to the connector
  contract.
- `loc status` and `loc diff` may inspect local projection and shadow state, but
  they are not remote enumeration triggers.
- Background child refreshes do not queue when background connector sync is
  disabled.

## Typical End-To-End Scenarios

### User Browses A Virtual Folder

1. Platform or CLI asks daemon for children.
2. Daemon returns local metadata from `virtual_fs_children` or
   `file_provider_children`.
3. Runtime queues an interactive child refresh.
4. Child refresh maps the folder identifier to `ChildContainer`.
5. Connector `list_children` fetches immediate remote child metadata.
6. Store/projection metadata updates.
7. No page bodies are fetched unless a separate materialize/read/open action
   happens.

### User Opens A Virtual Page File

1. Platform asks daemon to materialize or read the item.
2. Runtime resolves the virtual item.
3. If it is a page and not already hydrated/dirty/conflicted, materialization
   calls hydration with `FileOpen`.
4. Hydration executor calls `fetch_render`.
5. Executor writes Markdown/assets/shadow and returns materialized content.
6. Runtime records file-open freshness.

### User Opens A Plain-File Stub

1. Watcher or polling stub scanner detects read/access.
2. Runtime records file-open freshness.
3. If the entity is a page in `Virtual` or `Stub` state, runtime queues
   `StubRead` hydration.
4. Hydration executor fetches/renders and replaces the stub if replacement is
   safe.

### Live Mode Notices Remote Drift

1. Scheduled pull enumerates due mounts, or freshness observation checks a known
   page.
2. Runtime records remote observation and remote-hint state.
3. If the local page is hydrated and still matches the shadow, fast-forward
   hydration can be queued.
4. Hydration executor fetches/renders and updates the local file.
5. If local content is dirty or leased, fast-forward is skipped or delayed.

### User Runs `loc inspect`

1. CLI resolves the local page and shadow/cache state.
2. CLI calls `fetch_render` with `ExplicitPull`.
3. CLI compares local and remote rendered content.
4. No local entity, shadow, or projection file is updated.

### User Pushes Local Changes

1. Push preflight may fetch/render remote content to guard against drift.
2. Push applies remote changes.
3. Post-apply reconciliation may fetch/render the final remote entity.
4. Push writes accepted local files/assets/shadows for the applied result.
5. This does not go through the runtime hydration queue.

## Change-Audit Checklist

When changing enumeration or hydration behavior, audit these areas together:

- Connector trait contract in `crates/locality-connector/src/lib.rs`.
- Mount-root pull and virtual-directory pull in `crates/localityd/src/pull.rs`.
- Scheduled reconciliation in `crates/localityd/src/reconcile.rs`.
- Runtime request handling, child refresh queues, freshness queues, and
  hydration queue integration in `crates/localityd/src/runtime.rs`.
- Hydration executor and queue behavior in `crates/localityd/src/hydration.rs`.
- Virtual FS local metadata and child refresh mapping in
  `crates/localityd/src/virtual_fs.rs`.
- File Provider aliasing in `crates/localityd/src/file_provider.rs`.
- Plain-file watcher read/write classification in `crates/localityd/src/watcher.rs`.
- Platform provider call sites under `platform/macos`,
  `platform/linux/locality-fuse`, and `platform/windows/locality-cloud-files`.
- Connector implementations in `crates/locality-notion` and
  `crates/locality-google-docs`.
- Push paths in `crates/localityd/src/push.rs`.
- Inspect path in `crates/loc-cli/src/inspect.rs`.

Tests should cover the behavior at the product boundary that triggers it:

- pull tests for explicit mount/entity/virtual-directory pulls
- scheduled-pull tests for full enumeration and policy/fast-forward hydration
- runtime tests for queue ordering, File Provider/FUSE style children and reads,
  child-refresh dedupe, and hydration retries
- virtual FS projection tests for metadata-only folder listing
- connector tests for `enumerate`, `list_children`, and `fetch_render`
- push tests for preflight and post-apply render/fetch behavior
- inspect tests for remote comparison without persistence
