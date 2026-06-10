# Connector SDK

A connector implements six responsibilities and leaves caching, journaling, validation, conflict detection, and rate limiting to the host:

1. Enumerate remote tree metadata.
2. Fetch full native content for one entity.
3. Render native content to canonical Markdown plus frontmatter.
4. Parse edited canonical content back to a connector-owned model.
5. Check remote concurrency immediately before mutation.
6. Apply a validated push plan as remote API operations.

First-party connectors compile in as Rust crates. A future third-party connector ABI should be possible if this trait remains narrow, explicit, and host-mediated.

Apply requests include the core `push_id`, mount ID, and approved push plan. Connectors should use the `push_id` when deriving source-side idempotency keys for block-level API calls.

## v1 connector

`afs-notion` is the first connector. It owns Notion-specific block mapping, database schema translation, OAuth/API behavior, and conversion between Notion payloads and the canonical AgentFS document model.
