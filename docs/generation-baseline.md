# Generation-baseline sidecar

Generation-baseline V1 is a portable, metadata-only sidecar for a successfully
completed generation-2 full export. It lets an authenticated client seed the
exact initial observed generation and target inventory without adding fields to
the frozen export-v2 archive or terminal-control records.

The intended private hosted route is:

```text
GET /v2/sessions/{session}/export-attempts/{attempt}/generation-baseline
```

The hosted service must authenticate the request, authorize both path IDs, and
return only the baseline sealed for that exact completed attempt. This public
crate defines no HTTP handler, credential flow, database query, or provider
cursor.

The public blocking client is `GenerationBaselineHttpClient` in `localityd`;
`loc_cli::generation_http` re-exports it source-compatibly for existing callers.
Construction takes a verified `WorkspaceProfileSessionV2`, so its opaque
capability cannot be separated from the session route identity. Fetching takes
that same session plus the exact offer and namespaced inventory. The client
derives the attempt route identity from the sealed offer, encodes both opaque
IDs as individual URL path segments, sends the capability only in the
`Authorization: Bearer` header, and never places it in a URL or diagnostic.
The HTTP adapter rejects the exact opaque IDs `.` and `..` before network
access because URL-standard dot-segment normalization cannot preserve them as
exact route identities; they remain valid opaque values outside this route.

The request is a replay-safe GET with `Accept: application/json` and
`Cache-Control: no-store`. Every accepted response, including a structured
error response, must have exact `Content-Type: application/json`, exact
`Cache-Control: no-store`, a single valid `Content-Length`, and no transfer
encoding. The success body is bounded before allocation by
`maximum_encoded_bytes_for_export` for the exact verified export context.
Only transient connection/timeout/body-framing failures and HTTP 502, 503, or
504 are retried, with the identical method, route, authorization boundary, and
headers. Authorization, missing/stale/expired attempt, update-required,
malformed, oversized, and context/integrity failures are terminal typed errors
whose diagnostics do not include response text or credentials.

## Required export authority

Network callers must retain all three verified generation-2 values:

- `WorkspaceProfileSessionV2`, including its sealed scope-to-mount layout;
- `WorkspaceExportOfferV2`, including the exact attempt and source generations;
- `WorkspaceNamespacedInventoryV2`, decoded or recomputed through export-v2.

They pass those values to `decode_json_against_export`. There is intentionally
no unbound network decoder, and `GenerationBaselineResponseV1` intentionally
does not implement `Deserialize`. The private wire representation is decoded
only inside the context-bound decoder, so generic Serde decoding cannot skip
the session/attempt checks. Before accepting a sidecar, the decoder recomputes
the canonical export inventory against the session layout and offer, then
compares every baseline file to its authoritative export record. Mount ID,
source connection ID, projection ID, logical path, content SHA-256, and byte
length must all match file-for-file; missing and extra files fail closed.

The generation-2 tar adapter exposes that third value through the opt-in
`validate_workspace_tar_with_inventory_v2` API and its
`ValidatedWorkspaceArchiveWithInventoryV2` result. The existing
`validate_workspace_tar` function and `ValidatedWorkspaceArchive` fields are
unchanged, so exhaustive patterns and struct literals remain source compatible.
Callers migrating to baseline seeding call the opt-in function, use
`validated()` for the prior validation result, and retain `inventory()` as
baseline authority. `into_parts()` transfers ownership of both values when
they must outlive the wrapper.

The returned inventory is the canonical `WorkspaceNamespacedInventoryV2`
planned once from the exact bounded archive members, terminal-control scope
authority, sealed session layout, and offer that also produce the returned
materialization plan. The public protocol equivalent is
`WorkspaceMaterializationPlanWithInventoryV2`; its `plan` method returns both
values without cloning authorized entries into an intermediate collection.
Callers must carry this inventory forward; they must not reconstruct baseline
authority from the materialization plan, filesystem paths, or terminal-control
counts. The inventory remains host-neutral and excludes writable provider
preconditions and remote IDs. No inventory is returned when archive, content,
control, context, or negotiated local-limit validation fails.

`content_version_id` is not present in frozen export-v2 records. The
authenticated endpoint supplies that authoritative immutable ID. Each
per-source `target_inventory_sha256` commits the complete ordered tuple
`[projection_id, logical_path, content_version_id, content_sha256, byte_length]`
using the existing `locality.generation-target-inventory.v1` preimage. The
whole-response `baseline_sha256` commits those target digests and every file
tuple again together with the exact profile, session, attempt, layout, export
inventory, and source-generation vector. Changing a content-version ID changes
both seals.

## Shared mounts and canonical order

A mount contains an ordered `sources` collection. This represents profiles
where scopes from multiple source connections share one mount. The expected
mount/source pairs are derived by joining session-layout scope ordinals with
the verified inventory's scope-source authority. Empty source inventories are
retained so every authorized mount/source generation can be seeded exactly.

Source generations use contiguous sealed-offer ordinals. Mounts use exact
bytewise mount-ID order. Source states within a mount use source-generation
order, and files within each source state use exact bytewise projection-ID
order. Projection IDs are unique across the response. Portable logical paths
use `LogicalPath`, including traversal, absolute-path, reserved-name, Unicode,
and cross-platform path validation.

## Negotiated limits and fallback

The sidecar does not impose the generation-delta V1 limits of 100,000 entries,
512 MiB changed content, or 64 MiB per file on a full export that was valid
under its negotiated `ExportAttemptLimits`. File count and content-byte truth
come from the exact offer and recomputed inventory. The raw JSON ceiling is
derived from that selected inventory, the sealed layout, every serialized
source-state occurrence of its escaped observed generation ID, and a bounded
4 KiB content-version ID capability instead of a fixed 64 MiB response ceiling.

Each source state declares a deterministic `refresh_mode`:

- `generation_delta_v1` when its file sizes and opaque IDs fit the existing
  generation-delta V1 reader; or
- `full_export_only` when the negotiated full export is valid but delta V1
  cannot represent that source state.

Clients must use another full export for `full_export_only` states. They must
not attempt to feed those identities to the delta-V1 reader. The mode is
recomputed during validation and included in the canonical baseline digest.
If an authoritative content-version ID exceeds the sidecar's explicit 4 KiB
scalar capability, the endpoint cannot emit V1 at all; the client keeps the
already verified full-export tree but does not seed generation-delta state, and
its next refresh remains a full export. The decoder never truncates or aliases
that identity.

All response, source-generation, mount, source-state, and file wire objects
reject unknown fields. The contract contains no absolute host path,
credential, provider cursor, mutable title, target label, or display name.
