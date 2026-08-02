# Generation-baseline sidecar

Generation-baseline V1 is a portable, metadata-only sidecar for a successfully
completed generation-2 full export. It lets an authenticated client seed the
exact initial observed generation and target inventory for every mount without
adding fields to the frozen export-v2 archive or terminal-control records.

The intended private hosted route is:

```text
GET /v2/sessions/{session}/export-attempts/{attempt}/generation-baseline
```

The hosted service must authenticate the request, authorize both path IDs, and
return only the baseline sealed for that exact completed attempt. This public
crate defines no HTTP handler, credential flow, database query, or provider
cursor.

Clients should retain the exact `WorkspaceExportOfferV2` used for the full
export and call `decode_json_against_export_offer`. That comparison binds the
sidecar to the workspace profile ID and revision, session ID, export attempt
ID, layout version and digest, export inventory SHA-256, source-generation
vector, selected file count, and selected content-byte total. A response that
is self-consistent but belongs to another attempt is rejected.

Within the response, source generations use contiguous sealed-offer ordinals,
mounts use exact bytewise mount-ID order, and each mount's files use exact
bytewise projection-ID order. V1 permits one source connection per mount. The
mount source must occur in the source-generation vector and its observed
generation must match; the mount-source set must exactly cover the vector.
Projection IDs are unique across the response, and portable logical paths are
validated by `LogicalPath` (including traversal, absolute path, reserved name,
Unicode normalization, and cross-platform path ceilings).

Each mount's `target_inventory_sha256` uses the existing
`locality.generation-target-inventory.v1` digest, so the seeded inventory is
the direct base for later generation deltas. `baseline_sha256` uses a separate
domain-separated, length-framed canonical preimage covering every attempt,
layout, generation, mount, digest, and file field. The private endpoint may
persist or sign that digest without relying on JSON whitespace or object-key
ordering.

The response is bounded to 256 mounts, 100,000 files, 512 MiB of described
content, 64 MiB per file through the shared generation-file validator, and 64
MiB of encoded metadata. All response, mount, source-generation, and file wire
objects reject unknown fields. The contract contains no absolute host path,
credential, provider cursor, mutable title, target label, or display name.
