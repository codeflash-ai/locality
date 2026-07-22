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

The host must validate and reconcile the entire result before persisting
`next_checkpoint`. If validation, store mutation, or projection reconciliation
fails, the previous checkpoint remains current so the connector can safely
replay the batch.

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
