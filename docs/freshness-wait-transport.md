# Freshness Wait Transport Contract V1

`locality_protocol::freshness_wait` defines the portable durable wait-attempt
values used by generation-2 workspace clients that advertise
`freshness_wait: 1`. It is separate from generation-delta delivery and does not
define a hosted route, database schema, refresh scheduler, worker lease, or
generation-pin lifecycle.

## Identity and replay

A `FreshnessWaitAttemptRequest` carries the workspace session, bounded caller
idempotency key, and the client's complete capability offer. It does not carry
a client-authored deadline. An authenticated server response selects
`freshness_wait: 1` and seals the trusted `created_at` plus the session's
`FreshnessRequirement`. The only legal durable deadline is:

`original_deadline_at = created_at + wait_timeout_seconds`

The sealed wait timeout must be positive, must use `on_stale: wait_then_fail`,
and cannot exceed five minutes. A waiting snapshot is rejected at or after that
deadline. Creation and update timestamps more than five seconds ahead of
authenticated server time are also rejected, making clock-skew and
already-expired responses explicit failures instead of implicit deadline
extension.

The returned `FreshnessWaitAttempt` adds a bounded opaque wait-attempt ID and a
positive sequence. Sequence is the replay cursor, not a delivery count or an
acknowledgment: the server may return its latest durable snapshot without
retaining every intermediate response. An exact snapshot replay is valid. A
changed successor must have a strictly greater sequence, while gaps are valid
after a lost poll or concurrent progress. `updated_at` cannot regress but may
remain equal across revisions because V1 timestamps have one-second precision.
Attempt ID, session, idempotency key, selected capability, sealed requirement,
creation time, derived deadline, source order, source-and-scope identities, and
target epochs are immutable. Satisfied or failed sources cannot reopen, and an
aggregate terminal snapshot absorbs every changed successor. Clients retain
only the last accepted snapshot as their cursor; no separate client ack is
needed for correctness.

The contract is available only after `freshness_wait: 1` negotiation. The
request capability set is always a client offer. `selected_capability` in the
authenticated response is the immutable server selection and must remain in
every later offer, but it never replaces or narrows the client's future offers.
Clients without that capability retain the existing immediate stale-result
behavior and never enter this polling protocol. The request and response also
carry a format/minimum-reader envelope; version 1 requires workspace HTTP API
generation 2.

## Multi-source progress

Each attempt contains one to 64 source targets in the session's canonical scope
ordinal order. A target carries the source connection ID, immutable source
scope ID, captured target epoch, current applied epoch, typed source state,
stable freshness reason, and optional shared retry classification. One source
connection may therefore contribute several independently tracked scopes. The
connection-and-scope pair is unique within an attempt and remains at the same
ordinal for every successor. Epochs use the public `FreshnessEpoch` canonical
quoted-decimal encoding, preserving the ADR 0003 `BIGINT` range without
JavaScript precision loss.

Source facts are fail-closed:

- `waiting` requires `applied_epoch < target_epoch` and a reason;
- `satisfied` requires `applied_epoch >= target_epoch` and no reason or retry;
- `failed` requires an unapplied target and a reason.

Duplicate connection-and-scope pairs, noncanonical ordinals, unknown labels,
contradictory state, and source arrays beyond the fixed bound are rejected.

## Polling and terminal outcomes

An aggregate `waiting` attempt has poll metadata and no terminal result. The
attempt carries the positive sequence; poll metadata carries the snapshot
observation time and `after_delay` retry advice capped at one hour. Validation
rejects advice that would schedule the next useful poll after the immutable
deadline.

An aggregate `terminal` attempt has no poll metadata and exactly one terminal
outcome:

- `satisfied`: every source reached its target;
- `deadline_exceeded`: at least one source remains waiting at the original
  deadline; or
- `failed`: at least one source reached a typed terminal failure.

Aggregate terminal reason and retry advice are intentionally absent. Ordered
per-source reason/retry tuples are authoritative, avoiding a lossy or
nondeterministic derived aggregate choice when several sources fail.
Cancellation or disconnect is intentionally absent: it does not cancel the
durable refresh.

## Bounds and strict decoding

| Value | V1 maximum |
| --- | ---: |
| Start/resume request JSON | 4 KiB |
| Attempt status JSON | 64 KiB |
| Source targets | 64 |
| Session, source connection, source scope, and wait-attempt IDs | 128 bytes |
| Idempotency key | 128 bytes |
| Durable wait window | 5 minutes |
| Poll delay | 1 hour |

All object levels deny unknown fields, including nested retry metadata. Opaque
values reject empty, whitespace-padded, control-bearing, and oversized inputs.
Timestamps are canonical UTC seconds and all state/terminal combinations are
validated after decoding. A persisted attempt's creation-to-deadline window is
capped at five minutes, matching the workspace freshness policy ceiling. Debug
output redacts the idempotency key and wait ID.
