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
`AuthorizedGenerationDeliveryPoll` response carries a separate server
selection, which may select only a subset with limits no larger than the offer;
it must not replace or narrow a later client offer. The three V1 capabilities
are bounded content body windows, idempotent terminal receipt acknowledgments,
and device-scoped generation pin leases. Local sync validates the selected set
immediately after the poll returns and before the returned delivery can cause
journal, filesystem, or observed-generation mutations. Startup recovery and
reconciliation still run before polling.

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
The compatibility response selects the legacy transport. The default client
offer is legacy, and the existing `open_content` stream remains the fallback.
Selecting a capability and then omitting its response is a contract error;
selection never silently falls back.

## Body windows

A body-window request repeats the delta ID, terminal receipt digest, complete
`GenerationFileIdentity`, offset, and requested maximum bytes. Authenticated
response metadata repeats those bindings and supplies the exact range, terminal
flag, and per-window SHA-256. Raw body bytes are transported separately from
the JSON metadata.

The local daemon accepts a window only when its metadata matches the request,
its range is contiguous and bounded, the terminal flag exactly matches the
declared content end, the streamed window length and SHA-256 match, and the
assembled file matches `GenerationFileIdentity`.

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
| Body window | 4 MiB |
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
