# Freshness Delivery Transport Contract V1

This document defines the public, portable boundary used by persistent
generation delivery. The Rust values live in
`locality_protocol::freshness_delivery_transport`; the local adapter trait is
`localityd::generation_sync::GenerationTransport`.

The public repository does not define a hosted route, base URL, bearer token,
tenant identifier, private lease repository, or cloud resource. A
private authenticated adapter maps its API to these values and must authenticate
and authorize each response before returning it to local delivery code.

## Negotiation and compatibility

Every new top-level value carries `format_version` and
`minimum_reader_version`. V1 readers accept a higher additive format when its
minimum reader remains 1, ignore unknown object fields, and fail with
`UpdateRequired` when a value requires a newer reader. Missing capability and
version fields decode as the legacy V1 whole-body transport.

The client advertises `GenerationTransportCapabilities` with every next-delta
request. That value remains a client offer across polls. The authenticated
wire response is the serde `GenerationDeliveryPollResponse`. It carries a
separate server selection, which may select only a subset with limits no larger
than the offer; it must not replace or narrow a later client offer. A private
adapter maps that portable value to its local daemon result after authentication;
the protocol value contains no reader or stream type.

The poll response uses `Content-Type: application/json`, is capped at 64 MiB,
and reserves 1 MiB for its bounded envelope and terminal receipt. A complete
delta's exact compact serde JSON is capped at 63 MiB, so every delta accepted by
the public contract fits in one V1 poll response without metadata pagination.
The entry-count ceiling remains 100,000. Delta validation counts serializer
output without allocating a second metadata-sized buffer. The response has
exactly one of these status/payload combinations:

- `delivery` has one delta and terminal receipt and no error.
- `no_delivery` has neither a delivery nor an error.
- `error` has no delivery and one bounded machine-readable freshness reason
  with optional bounded retry advice. It never carries provider or human error
  text.

Every status repeats the mount, source connection, observed generation, and
selected capabilities. The client rejects crossed request correlation, a
selection outside its offer, ambiguous or unknown statuses, an invalid delta,
or a receipt that does not bind that exact delta. Unknown additive object fields
remain ignorable when `minimum_reader_version` remains 1; a higher minimum
reader fails with `UpdateRequired`.

The three V1 capabilities are bounded content body windows, idempotent terminal
receipt acknowledgments, and device-scoped generation pin leases. Local sync
validates the selected set immediately after the poll returns and before the
returned delivery can cause journal, filesystem, or observed-generation
mutations. Startup recovery and reconciliation still run before polling.

For a returned delivery, SQLite durably stores the complete authenticated
selection alongside the apply journal, including body-window bounds,
acknowledgment selection, and pin-lease policy. An exact reservation replay must
match that complete selection. Recovery uses the stored selection, so adapter
configuration changes cannot renegotiate an in-flight apply.

Prerelease schema-v25 journals predate that complete binding. Migration fails
atomically when such a journal is still active because its acknowledgment bit
cannot prove whether body windows or pins were selected; the prerelease reader
must finish that apply before migration is retried. A completed pre-binding
journal may migrate because it can no longer fetch content or exercise pin
policy. Its only legal replay is an exact delta/receipt no-op, and only its
recorded acknowledgment bit remains operative. No capability selection is
inferred for that terminal state. Older component-v3 active journals are known
legacy transport and can be bound faithfully. Prerelease component-v5 journals
with persisted body-window or pin data also retain that exact selection.

An existing adapter can continue implementing the original
`GenerationDeliveryTransport` trait and `GenerationDeliveryRequest` without
source changes. `next_delta_versioned` and `next_delta_poll` are additive
default adapter methods, and `GenerationTransport` is a compatibility alias.
The portable poll serde envelope and HTTP body frame are additive; they do not
change any released V1 request or metadata JSON. The compatibility response
selects the legacy transport. The default client offer is legacy, and the
existing `open_content` stream remains the fallback. Selecting a capability and
then omitting its response is a contract error; selection never silently falls
back.

## Target inventory commitment

`target_inventory_sha256` commits to the complete authoritative file inventory
of the resulting target generation, including files unchanged by the delta. The
inventory is ordered by ascending bytewise `projection_id`; duplicate or
portable-colliding logical paths are invalid. Its canonical preimage is:

1. the ASCII domain `locality.generation-target-inventory.v1` followed by NUL;
2. the inventory entry count as a big-endian `u64`;
3. for every file, its `projection_id`, logical path, content version ID, and
   `sha256:` content digest as UTF-8 byte strings, each prefixed by its
   big-endian `u64` byte length, followed by its byte length as a big-endian
   `u64`.

The commitment is lowercase `sha256:` plus SHA-256 of that preimage. The
protocol fixture includes empty, ASCII, Unicode, and framing-boundary vectors
for implementations in other languages. Local sync derives the resulting
inventory from its authoritative generation-path state plus the delta and
compares the commitment before reserving a journal, downloading content,
publishing filesystem changes, or committing the observed generation. A
mismatch fails closed. This adds validation to the existing V1 field without
changing its wire representation.

## Body windows

A body-window request repeats the delta ID, terminal receipt digest, complete
`GenerationFileIdentity`, offset, and requested maximum bytes. Authenticated
response metadata repeats those bindings and supplies the exact range, terminal
flag, and per-window SHA-256.

A successful HTTP response has the exact media type
`application/vnd.locality.generation-body-window.v1` and a required
`Content-Length`. Its entity body has one canonical framing:

1. four-byte unsigned big-endian metadata length;
2. that many bytes of compact UTF-8 JSON `GenerationBodyWindowMetadata`, capped
   at 16 KiB; then
3. exactly `range.length` raw body bytes.

There is no JSON body embedding, metadata header, multipart boundary, trailing
data, or content-type sniffing. `Content-Length` covers the prefix, metadata,
and raw bytes and must equal the bytes received. A non-success HTTP response is
not a body-window frame; the private adapter owns its authenticated error
mapping.

The private adapter must authenticate the complete HTTP response before exposing
the frame. The public decoder then accepts a window only when the metadata's
delta and terminal receipt digest bind it to the authenticated delivery, its
complete content identity and offset match the request, its range is contiguous
and bounded by the requested maximum, the terminal flag exactly matches the
declared content end, and the raw byte length and SHA-256 match. The assembled
file must still match `GenerationFileIdentity`.

Window size is capped at 4 MiB. A generation file remains capped at 64 MiB and
a delta at 512 MiB by the underlying freshness contract. Empty files are
verified locally without issuing a zero-length window.

## Terminal acknowledgment

After local apply reaches a terminal journal state and advances the observed
generation, the client may send an idempotent acknowledgment. It binds the
delta, mount, source connection, target generation, authorization epoch, and
canonical terminal receipt digest. The authenticated response is either
`accepted` or `already_accepted` and repeats the receipt binding.

An acknowledgment transport failure does not roll back the completed local
apply. When acknowledgment was negotiated, the local SQLite apply journal
durably records the completed receipt as pending. Every later mount sync
replays pending acknowledgments before polling for another delta, including
when that poll would return no delivery. Exact receipt identity is checked
before the journal is marked acknowledged. Source reset cannot discard a
pending acknowledgment. This is local delivery state, not a claim about any
private service's persistence.

## Generation pin leases

Pin leases prevent a retained immutable generation from disappearing while a
device delivers it. IDs are opaque and bounded; a device scope is not a tenant
route or credential. Acquire, renew, and release carry bounded opaque operation
IDs echoed by their responses. Acquire results also echo the exact device,
source, requested generation, duration, and fallback policy. Renewal repeats
the source and generation and cannot retarget the immutable lease identity.
The public contracts do not persist leases.

V1 bounds a lease to 60 seconds through 24 hours and a negotiated quota of at
most 32 active leases per device. Responses report the effective duration,
trusted issue/server time, expiry, active count, and maximum count. The expiry
must equal issue time plus the stated duration, and validation at authenticated
server time rejects already-expired or not-yet-issued leases. Retry advice is
capped at one hour. Acquire and renewal validation also requires the exact
selected pin capability and rejects durations, device quotas, or fallback
policies outside that negotiated selection.

Fallback is explicit:

- `require_exact` accepts only a lease for the requested generation.
- `use_latest_retained` may accept a lease for a different retained complete
  generation and marks `fallback_applied: true`.

There is no continue-unpinned fallback. Unsupported pinning, unavailable
generations, quota exhaustion, and temporary failure return an explicit
bounded unavailable result. The daemon trait exposes these operations for the
private adapter, but local sync does not acquire leases until lifecycle policy
is integrated with generation selection.

## Limits and logging

| Value | V1 maximum |
| --- | ---: |
| Capability JSON | 4 KiB |
| Request/metadata JSON | 16 KiB |
| Complete delta metadata JSON | 63 MiB |
| Poll envelope and terminal-receipt headroom | 1 MiB |
| Poll response JSON | 64 MiB |
| Body window | 4 MiB |
| Body-window frame | 4-byte prefix + 16 KiB metadata + 4 MiB body |
| Device scope ID | 128 bytes |
| Pin operation ID | 128 bytes |
| Lease ID | 256 bytes |
| Lease duration | 24 hours |
| Active leases per device | 32 |
| Pin retry delay | 1 hour |

Custom `Debug` implementations redact content identities, logical paths,
receipt digests, device scopes, and lease IDs. Exact JSON fixtures intentionally
contain test-only opaque values and are the wire-format goldens; debug output is
not a wire format.
