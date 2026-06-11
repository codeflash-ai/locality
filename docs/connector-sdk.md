# Connector SDK

A connector implements seven responsibilities and leaves caching, journaling, validation, conflict detection, and rate limiting to the host:

1. Enumerate remote tree metadata.
2. Fetch full native content for one entity.
3. Render native content to canonical Markdown plus frontmatter.
4. Parse edited canonical content back to a connector-owned model.
5. Check remote concurrency immediately before mutation.
6. Apply a validated push plan as remote API operations.
7. Apply a complete undo plan as remote reverse API operations.

First-party connectors compile in as Rust crates. A future third-party connector ABI should be possible if this trait remains narrow, explicit, and host-mediated.

Connectors are resolved through a profile/account boundary before any API call.
A connector profile is the local auth-config record: auth kind, scopes, enabled
action classes, connector version, status, and capabilities. A connected account
references one profile and owns provider/workspace metadata plus a `secret_ref`;
the secret itself lives in the credential store. Implementations should treat a
missing or inactive profile as an auth/setup problem before attempting remote
I/O.

Apply requests include the core `push_id`, mount ID, approved push plan, and deterministic operation IDs aligned with `plan.operations`. Connectors should use those operation IDs as source-side idempotency keys for block-level API calls when the source supports idempotent writes.

Apply results include changed remote IDs plus operation-level journal effects. Created-block and created-entity effects must include the remote IDs assigned by the source, because reconcile and undo use those IDs to read back, materialize, and reverse appends and creates safely.

Undo requests include the target push ID, mount ID, and a connector-neutral complete undo plan. Connectors should fail the request rather than partially applying a plan they cannot support.

## v1 connector

`afs-notion` is the first connector. It owns Notion-specific block mapping, database schema translation, OAuth/API behavior, and conversion between Notion payloads and the canonical AgentFS document model.

The current Notion slice is live-capable for reads and narrow writes: it retrieves page metadata, recursively fetches paginated block children, enumerates root-page descendants and database rows into stable projected paths, stores native JSON bundles, renders canonical Markdown plus shadow snapshots, writes `_schema.yaml` for databases, applies simple block update/append/archive plans, updates supported page properties, and creates new database rows from new Markdown files. Reverse apply is available for the supported block/entity effects recorded in the journal.
