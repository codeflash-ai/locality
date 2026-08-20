# Connector SDK

A connector implements eight responsibilities and leaves caching, journaling,
validation, conflict detection, and network admission mechanics to the host:

1. Enumerate remote tree metadata.
2. Observe mount-wide metadata changes in checkpointed batches.
3. Fetch full native content for one entity.
4. Render native content to canonical Markdown plus frontmatter.
5. Parse edited canonical content back to a connector-owned model.
6. Check remote concurrency immediately before mutation.
7. Apply a validated push plan as remote API operations.
8. Apply a complete undo plan as remote reverse API operations.

First-party connectors compile in as Rust crates. A future third-party connector ABI should be possible if this trait remains narrow, explicit, and host-mediated.

The versioned public connector catalog lives in `connectors/registry.json` and
is validated by `locality-connector`. It is descriptive metadata, not executable
authority: resolver, credential, path-write, validation, and apply behavior
remain in trusted code. See [Connector Development](connector-development.md)
for the exact registration and fixture contract.

## Network Policy

Each connector supplies a `ConnectorNetworkConfig` for the quota scope enforced
by its upstream API: requests per second, token-bucket burst, per-scope
in-flight limit, request timeout, and retry backoff parameters. The shared
network gate implements those mechanics. The connector still owns semantic
decisions such as which methods and status codes are safe to retry, how to
decode `Retry-After`, and how authentication failures are reported.

A process-wide orchestrator sits above the connector scopes. It applies a high
global in-flight ceiling as resource backpressure and admits waiting scopes in
round-robin order. It is not a global requests-per-second limit: one provider's
empty bucket or cooldown does not consume another provider's quota. Clients
using the same quota scope share its bucket and cooldown.

The global ceiling is 32 in-flight requests. It is internal product policy, not
a user-facing environment setting. The limit is process-local; provider limits
remain authoritative when CLI and daemon processes run at the same time, so
connector defaults should remain conservative.

Notion uses the same 3 requests/second, burst 3, four retries, and exponential
backoff behavior as its previous production-tested limiter. Granola uses a
separate 5 requests/second, burst 3, maximum 8 in-flight scope. Adding a
connector should add a new policy and reuse the gate rather than creating a new
scheduler or request-throttling implementation.

First-party connectors are exposed through the daemon source registry. The
registry is the single runtime list of connectors supported by the current
build, and it owns the descriptor metadata consumed by CLI flows such as mount
guidance, default mount IDs, auth hints, and display names. Descriptor lookup
may return generic guidance for an unknown connector string, but remote I/O must
still fail unless that connector has a registered runtime resolver.

Connectors are resolved through a profile/account boundary before any API call.
A connector profile is the local auth-config record: auth kind, scopes, enabled
action classes, connector version, status, and capabilities. A connected account
references one profile and owns provider/workspace metadata plus a `secret_ref`;
the secret itself lives in the credential store. Implementations should treat a
missing or inactive profile as an auth/setup problem before attempting remote
I/O.

Capabilities are explicit connector-neutral booleans. The current contract
tracks block updates, whole-entity body updates, databases, OAuth, cheap remote
observation, lazy child enumeration, media download, entity moves, undo, and
batch observation.
Hosts should use capabilities for scheduling and preflight decisions, not for
bypassing authoritative push concurrency checks.

## Initial-Hydration Budgets

`locality_connector::hydration_budget` defines an additive, opt-in execution
budget for initial hydration. A host constructs one `InitialHydrationBudget`
per job and passes clones of that handle through every stage. A connector must
not store it on a connector instance or in a process-global cache: clones share
only the counters and deadline for that one job.

The validated contract independently limits aggregate provider response bytes,
provider calls and elapsed deadline, inventory items and encoded bytes,
traversal nodes and depth, connector-native bytes, media assets and decoded
bytes, rendered content bytes, projection and change counts, and shared
retained bytes. Multi-dimensional reservations are atomic and use checked
arithmetic. `Content-Length` is a preflight only; streamed chunks are still
charged before extending a body buffer. Native JSON is charged from Serde's
writer before extending its destination, so transformations such as media
base64 expansion count their actual encoded size. Retained bytes describe live
logical representations: temporary inventory and cursor encodings are reserved
while live and released when dropped; a returned native entity retains its raw
bytes plus identity and kind; and a render retains the canonical JSON encoding
of the complete returned document, shadow, media metadata, or portable
projection result. Rendered body bytes remain a separate content dimension and
are not added to retained bytes a second time. Outputs whose exact size is not
knowable before provider decoding or rendering are charged before return.

Budget errors contain only a typed resource dimension. Provider response bodies,
URLs, credentials, and transport messages are not retained. A rate limit keeps
the provider and `Retry-After` duration as structured fields so the scheduler
can park the job; the response message remains redacted. Limit and validation
failures are permanent for that attempt, while rate limits and provider
unavailability are retryable.

Invalid trusted scope is also reported separately from an invalid provider
response. Its reason is a stable, redaction-safe code such as
`overlapping_roots`; root identities, titles, paths, and provider payloads are
not retained in the error. Notion partitions ordinary parent/child root overlap
at the selected child boundary; the error remains fail-closed for provider
shapes that still project one object ambiguously across unrelated roots. Hosts
can use that distinction to offer scope reselection instead of telling an
administrator that provider data was bad.

`locality_notion::hydration` provides the first bounded primitives: page and
database native fetch, recursive block traversal, hosted-media fetch, native
JSON encoding, ordinary and portable render accounting, and change accounting.
The production Notion HTTP client rejects oversized declared bodies before
reading and charges chunked bodies as they stream. Its quota-gate admission,
request send, and every response read share the same absolute job deadline; an
expired gate waiter is removed rather than issuing a late request. A huge
structured `Retry-After` remains intact for scheduling, while internal cooldown
deadline arithmetic saturates instead of overflowing.

The bounded hosted-media path is deliberately one GET with no redirect and no
retry. It waits for the reusable transport mutex only within the absolute job
deadline. Before provider admission, it atomically reserves one exclusive body
allowance across decoded-media and retained-byte dimensions. Concurrent budget
clones therefore cannot each allocate against the same remaining bytes. The
reservation releases in full on error or unwind; success atomically keeps the
actual body plus returned media-type bytes and releases unused capacity. The
path counts the provider attempt and every consumed success-body chunk, does
not consume failure bodies, and rechecks the deadline after transport and read
waits. Redirects fail closed. Custom media fetchers must explicitly implement
the bounded single-attempt hook, respect its pre-reserved maximum, and account
consumed response chunks without charging media bytes again; its default fails
closed. The public boundary rechecks the deadline and returned body length, so
a custom fetcher cannot return a late or per-asset-oversized capture as success.

Traversal depth and node capacity are preflighted before each child, data-source,
or paginated row discovery request. Pagination cursors are retained before they
enter cycle-detection storage and are released at the end of the operation.
Existing `NotionApi`,
`Connector::fetch_portable`, and render behavior remains unchanged unless a
caller explicitly selects these bounded APIs.
Custom `NotionApi` implementations fail closed on the bounded methods unless
they explicitly implement the same accounting; there is no fallback to an
unbounded provider call.

These primitives deliberately do not claim snapshot authority, persist a
checkpoint, or create a job session. The follow-up session workflow must own one
budget from the first provider call through the final unpersisted projection,
and must commit a checkpoint only after the complete bounded result validates.

## Batch Observation

`Connector::observe_batch` is the mount-wide metadata discovery contract. A
request carries the mount ID and an optional `ConnectorCheckpoint`; a result
contains upserts or explicit tombstones, a completeness declaration, and the
next checkpoint. The checkpoint JSON is opaque and connector-owned. Its
`state_version` and `min_reader_version` let a connector reject state written by
a newer incompatible implementation with a structured `UpdateRequired` error.

Batch observation does not hydrate entity bodies. An upsert is a `TreeEntry`
metadata record suitable for reconciliation and later lazy hydration. A
`Complete` result makes omission authoritative only within that mount's
configured remote scope. An `Incremental` result never turns omission into a
deletion; only an explicit tombstone authorizes deletion handling.

The original `PortableSyncRequest`, `PortableSyncHint`, `PortableChangeBatch`,
and `Connector::sync_portable` contract remains unchanged and carries no
omission authority. The additive v2 contract makes the safety boundary
explicit. `PortableSyncMode::HintsOnly` asks for differential work from the
opaque checkpoint and supplied object hints; omission from its result never
means deletion. `PortableSyncMode::ReconcileScope` asks the connector to inspect
the whole requested scope, but omission is authoritative only when the engine's
single `authorizes_omission` predicate sees the three semantic conditions—the
request mode is `ReconcileScope`, terminal connector coverage is complete, and
the batch declares `PortableBatchAuthority::CompleteScopeSnapshot`—plus the
scope-validation gates below. Non-terminal or incremental batches can delete
only through an explicit tombstone. Missing v2 mode or authority fields default
to `HintsOnly` and `Incremental`; unknown enum values are rejected. Each v2
batch explicitly lists
`covered_root_remote_ids`; omission requires that validated, unique set to equal
the exact requested root set. The engine preserves that requested scope,
rejects every returned change or tombstone with missing or foreign owning-root
provenance, and derives the omission flag only after bounded response, coverage,
and projection validation. A batch covering only A for a request of A+B is
non-authoritative.
An empty change result can authorize omission only when it explicitly covers
every requested root; missing coverage never can. The v2 result's scope, mode,
connector authority, completeness, and derived flag are private and available
only through read-only accessors.

Hosts that need to consume a checkpointed v2 result can use
`synchronize_and_project_portable_v2_to_completion` with explicit aggregate
checkpoint, change, and content-byte limits. The engine preserves the original
connection, scope, mode, hints, and per-response `max_changes` across every
validated dispatcher call; only a continuation request substitutes the next
opaque checkpoint. Continuation checkpoints must be nonempty, advancing, and
acyclic, while a terminal connector may legitimately return an empty or
unchanged stateless checkpoint. Cross-page source, artifact, and path identity
collisions fail closed; repeated covered roots are idempotent because terminal
snapshot coverage repeats roots reported by intermediate pages. Continuation
is pagination control flow, while every other connector, fetch, and render
incompleteness remains in the aggregate. The aggregate change limit is checked
after response validation but before fetch/render; content bytes are bounded
after rendering. For requested roots A+B, an intermediate page may cover A and
the terminal `CompleteScopeSnapshot` page must itself cover A+B. Omission is
derived only after that terminal response, and requires both accumulated and
terminal coverage to equal the requested roots; authority from an intermediate
page is never carried forward.

`dispatch_portable_sync_v2` is the validated trust boundary. It validates the
request and then calls the connector's `sync_portable_v2_impl` hook. The default
hook forwards legacy remote-ID hints to `sync_portable` and converts every
legacy result to `Incremental`. The convenience trait method
`Connector::sync_portable_v2` uses the dispatcher by default, but trait methods
are overrideable, so hosts must not treat a direct override call as validated;
the engine calls the free dispatcher. SDK implementers can migrate without a
crate major-version bump: keep the legacy method for old hosts, override only
the v2 implementation hook when the connector can honor prior metadata and
full-scope reconciliation, and return `CompleteScopeSnapshot` only for a
terminal complete inventory of the requested scope. Current Notion uses the
compatibility adapter; it ignores v2 prior metadata and full-inventory intent
and returns no covered roots, so it cannot authorize omission until its
dedicated implementation PR. A legacy result without explicit owning-root
provenance also fails the v2 engine workflow before projection.

Portable v2 sync hints may include the host's prior provider version, validated
logical path, and source kind; every hint must include an owning root from the
request scope. These values support differential provider decisions but are not
identity or deletion authority. Before connector dispatch, v2 requires 1..=256
explicit scope roots, accepts at most 4,096 hints, and bounds `max_changes` to
1..=10,000. Source, remote, scope-root, and owning-root IDs must contain
1..=1,024 UTF-8 bytes; provider versions are at most 1,024 UTF-8 bytes;
connector-defined source kinds are at most 128 UTF-8 bytes; and opaque
checkpoints are at most 65,536 UTF-8 bytes. Logical paths retain the portable
core's 1,024-byte ceiling. Duplicate scope-root or hint remote IDs and missing
or out-of-scope hint owning roots are rejected. Response coverage uses the same
256-root and 1,024-byte ID ceilings; duplicate or foreign covered roots are
rejected. A response may contain no more changes than the original request's
`max_changes` and its next opaque checkpoint has the same 65,536-byte ceiling;
both are checked before fetch, render, or any host persistence. Checkpoint bytes
remain connector-owned and opaque: hosts bound, persist, and return them
without parsing their contents.

The host must validate and reconcile the entire result before persisting
`next_checkpoint`. If validation, store mutation, or projection reconciliation
fails, the previous checkpoint remains current so the connector can safely
replay the batch.

Notion initial hydration has an opt-in `NotionInitialHydrationSession` wrapper
for hosts that need to drain a large explicit-root bootstrap without repeating
provider inventory work. The session is created from a configured
`NotionConnector`, a trusted SHA-256 source-connection identity, a page size,
and `InitialHydrationLimits`. It owns one shared budget across inventory,
fetch/media, render, and projection and implements `Connector` for use with the
ordinary engine pipeline. A fresh session accepts only a bootstrap request with
no checkpoint. Nonterminal checkpoints are random-nonce, connection, canonical
root-set, inventory, and next-index bound; they are valid only on that live
session and are rejected by the base connector. Only the terminal page returns
the normal durable Notion checkpoint that later synchronization accepts. Hosts
must publish that terminal checkpoint only with the completely validated
aggregate. Dropping or failing a session requires a new session, nonce, and
provider inventory. Every emitted fetch is also bound to the exact source kind
and provider version observed by that inventory; a rename or edit that changes
the provider version invalidates the session instead of mixing snapshots.

Apply requests include the core `push_id`, mount ID, approved push plan, and deterministic operation IDs aligned with `plan.operations`. Connectors should use those operation IDs as source-side idempotency keys for block-level API calls when the source supports idempotent writes.

Apply results include changed remote IDs plus operation-level journal effects. Created-block and created-entity effects must include the remote IDs assigned by the source, because reconcile and undo use those IDs to read back, materialize, and reverse appends and creates safely.

Connectors may lower an approved `PushPlan` into connector-specific execution
steps before making remote API calls. Those steps may batch multiple compatible
plan operations into fewer remote requests, but the connector-neutral `PushPlan`
remains operation-granular. When batching, apply must still return one
`JournalApplyEffect` for each original plan operation, with the correct
`operation_id`, `operation_index`, and remote ID. The Notion connector uses this
rule to lower contiguous compatible `append_block` operations into
`append block children` calls of up to 100 children while preserving per-block
journal and undo semantics.

Undo requests include the target push ID, mount ID, and a connector-neutral
complete undo plan. Connectors should fail the request rather than partially
applying a plan they cannot support. Expected-current fields make drift guards
available to connectors, but existing Notion block and entity undo lowering does
not yet enforce those remote guards. Connectors that require guarded undo, such
as whole-entity sources, must validate the complete plan before the first write.

After reversing a move or restoring an archived entity, connectors must return a
fresh non-deleted `RemoteObservation` for the changed entity. Archiving an entity
created by the target push must return an observation that reports that created
entity as deleted. The host validates mount, entity, parent, path, deletion state,
and path ownership before reconciling local files.

## Search Metadata

Connectors may add connector-neutral search hints under the reserved
`loc_search` object inside `RemoteObservation.raw_metadata_json`. The host treats
this payload as rebuildable index input only. It does not infer identity,
parentage, projection paths, or push behavior from it.

The supported shape is:

```json
{
  "loc_search": {
    "metadata_text": ["customer escalation", "Engineering", "Todo"],
    "aliases": ["ENG-1"],
    "source_url": "https://linear.app/acme/issue/ENG-1/improve-sync"
  }
}
```

Use `metadata_text` for concise provider-specific fields users naturally search
for, such as issue identifiers, team/project/status names, labels, assignee
names, and due dates. Use `aliases` for stable short handles or alternate IDs.
Use `source_url` for the canonical provider URL. Do not include secrets,
credentials, opaque auth state, or large raw provider payloads solely for search.

## v1 connector

`locality-notion` is the first connector. It owns Notion-specific block mapping, database schema translation, OAuth/API behavior, and conversion between Notion payloads and the canonical Locality document model.

The current Notion slice is live-capable for reads and narrow writes: it retrieves page metadata, recursively fetches paginated block children, enumerates root-page descendants and database rows into stable projected paths, stores native JSON bundles, renders canonical Markdown plus shadow snapshots, writes `_schema.yaml` for databases, applies simple block update/append/archive plans, moves pages between supported parents, updates supported page properties, and creates new database rows from new Markdown files. Reverse apply is available for the supported block/entity effects recorded in the journal.

Page-directory renames and parent changes are represented as the connector-neutral
`move_entity` push operation. Connectors that support it should update the
remote parent and title as one logical operation and return a moved-entity
journal effect so reconcile can fetch the entity at its final projected path.
Each source descriptor also declares whether virtual renames derive the remote
title from the destination filename or preserve the canonical title. Sources
such as Linear preserve canonical titles: a filesystem rename relocates cached
bytes unchanged, and an explicit title edit inside those bytes is lowered into
the single `move_entity` title field during push planning. Pending moves with
cached content run the same parsing, identity, source-schema, conflict, body
diff, semantic, media, and guardrail pipeline as ordinary existing documents.
When no bytes exist, a complete shadow permits a structural-only move; without
either bytes or a shadow, planning fails and requires materialization. During
virtual filesystem move publication, `VirtualMutationRecord.content_path` can
temporarily point at the source cache while `projected_path` already names the
destination. Push planners must prefer `content_path` when present so an
interrupted cache publication remains retryable without losing local edits.

Apply results for every planned `move_entity` must include both the entity in
`changed_remote_ids` and a matching moved-entity effect at the planned operation
index. Reconciliation stages the destination path/title, fetches and accepts the
remote result, and only then removes durable `move:`/`rename:` intent. Missing
effects, missing changed IDs, or failed readback leave that intent available for
recovery.
