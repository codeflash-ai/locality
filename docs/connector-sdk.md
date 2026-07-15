# Connector SDK

A connector implements seven responsibilities and leaves caching, journaling,
validation, conflict detection, and network admission mechanics to the host:

1. Enumerate remote tree metadata.
2. Fetch full native content for one entity.
3. Render native content to canonical Markdown plus frontmatter.
4. Parse edited canonical content back to a connector-owned model.
5. Check remote concurrency immediately before mutation.
6. Apply a validated push plan as remote API operations.
7. Apply a complete undo plan as remote reverse API operations.

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
tracks block updates, databases, OAuth, cheap remote observation, lazy child
enumeration, media download, entity moves, undo, and future batch observation.
Hosts should use capabilities for scheduling and preflight decisions, not for
bypassing authoritative push concurrency checks.

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

Undo requests include the target push ID, mount ID, and a connector-neutral complete undo plan. Connectors should fail the request rather than partially applying a plan they cannot support.

## v1 connector

`locality-notion` is the first connector. It owns Notion-specific block mapping, database schema translation, OAuth/API behavior, and conversion between Notion payloads and the canonical Locality document model.

The current Notion slice is live-capable for reads and narrow writes: it retrieves page metadata, recursively fetches paginated block children, enumerates root-page descendants and database rows into stable projected paths, stores native JSON bundles, renders canonical Markdown plus shadow snapshots, writes `_schema.yaml` for databases, applies simple block update/append/archive plans, moves pages between supported parents, updates supported page properties, and creates new database rows from new Markdown files. Reverse apply is available for the supported block/entity effects recorded in the journal.

Page-directory renames and parent changes are represented as the connector-neutral
`move_entity` push operation. Connectors that support it should update the
remote parent and title as one logical operation and return a moved-entity
journal effect so reconcile can fetch the entity at its final projected path.
