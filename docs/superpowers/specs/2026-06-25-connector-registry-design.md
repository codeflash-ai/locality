# Connector Registry Design

## Goal

Prepare AFS for additional first-party connectors without changing current Notion behavior or adding any new connector implementation.

## Scope

This change introduces a registry-backed source boundary used by both the daemon and CLI. The registry initially contains exactly one runtime-supported connector, `notion`. Unknown connector IDs may still receive generic descriptor and mount guidance where existing code already supports that, but source resolution must continue to fail with `unsupported_connector`.

## Architecture

`afs-connector` remains the connector trait boundary. `afsd` owns the first-party runtime registry because connector resolution needs daemon-owned profile, connection, credential, hydration, and scheduled-pull integration. CLI code consumes descriptor APIs from the same registry instead of constructing Notion metadata independently.

Notion-specific code stays in Notion-named modules or Notion-named registry entries. The registry entry for Notion owns its descriptor metadata and resolver function. Notion database schema validation, Notion URL search, Notion OAuth, Notion media planning, and Notion block semantics remain explicitly Notion-only.

## Components

- `crates/afsd/src/source.rs`
  - Exposes registry-backed descriptor lookup.
  - Exposes supported runtime connector IDs.
  - Dispatches source resolution through registered first-party connector entries.
  - Keeps `ResolvedSource` as the daemon-facing erased connector enum for now.

- `crates/afsd/src/notion.rs`
  - Continues to own Notion auth, OAuth refresh, connection fallback, hydration, and media handling.
  - May expose constants or helper functions needed by the registry.

- `crates/afs-cli`
  - Uses `source_descriptor(connector)` for mount/guidance metadata.
  - Keeps Notion-only commands and URL handling clearly scoped to Notion.

- `docs/connector-sdk.md`
  - Documents that first-party connectors are registered in the daemon registry and consumed by CLI descriptor APIs.

## Error Handling

Unsupported runtime connectors continue to fail with `ConnectorResolveError::UnsupportedConnector`. Missing Notion auth/profile/connection errors remain unchanged. Generic descriptors must not imply that the current build can perform remote I/O for that connector.

## Testing

Focused tests should verify:

- `notion` is the only runtime-supported connector.
- Notion descriptor metadata stays unchanged.
- Generic descriptors still produce fallback guidance for unknown connector IDs.
- Unsupported connector resolution still returns `unsupported_connector`.
- CLI-visible guidance remains registry-backed and behavior-compatible.

