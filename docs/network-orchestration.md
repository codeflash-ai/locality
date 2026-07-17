# Network Orchestration

Locality separates provider policy from global resource coordination. This
keeps the mature Notion behavior intact while allowing many independent data
sources to make progress concurrently.

## Connector layer

Every HTTP connector configures a quota scope with:

- sustained requests per second;
- token-bucket burst;
- maximum requests in flight for that scope;
- request timeout;
- retry count and exponential backoff bounds.

The shared `ConnectorNetworkGate` owns token accounting, cooldown timing, and
permits. A connector owns request semantics: safe retry methods, retryable HTTP
statuses, authentication, `Retry-After` parsing, and response decoding. A
provider response can cool down only its own scope.

Connector operations receive a host-selected execution policy. Foreground work
can use bounded inline retries, while daemon-owned discovery asks connectors to
return provider cooldowns instead of sleeping through them. The transport
returns a structured provider, delay, and error after the first rate-limit
response; the daemon parks the operation until that delay plus bounded jitter
has elapsed. Later scheduler ticks merge into the deferred job, so only one
retry exists and unrelated local work can continue. Notion and Granola both
implement this connector-neutral policy using their existing internal network
configuration.

Current defaults are:

| Scope | Requests/second | Burst | Scope in flight | Retries | Timeout |
| --- | ---: | ---: | ---: | ---: | ---: |
| Notion | 3 | 3 | 32 | 4 | 30 s |
| Granola | 5 | 3 | 8 | 4 | 30 s |
| Slack metadata | 1 | 2 | 2 | 4 | 30 s |
| Slack history | 1/min | 1 | 1 | 4 | 30 s |

Notion's rate, burst, retry classification, exponential delay, and cooldown
refill behavior match the previous Notion-specific limiter. This is a refactor
of its admission mechanics, not a new Notion scheduling policy.

Slack uses two connector-owned quota scopes. Metadata calls cover conversation
and user listings. History calls cover `conversations.history` and default to a
1 request/minute gate with a 15-message request limit to satisfy Slack's
strictest documented history policy for new non-Marketplace commercial apps.
Internal customer-built and Marketplace apps may have higher provider limits,
but Locality keeps this default conservative.

## Global layer

The process-wide `NetworkOrchestrator` defaults to 32 total in-flight requests.
When capacity is full it rotates among waiting quota scopes, so one busy source
cannot monopolize every newly available permit. The global layer does not
impose a requests-per-second rate and does not combine provider cooldowns.

This gives 30 connectors independent quota buckets while retaining bounded CPU,
socket, and memory pressure. Multiple clients for the same provider quota scope
share a bucket. The initial implementation uses connector-level scopes;
providers that publish per-account or per-tenant limits can derive a stable,
non-secret account scope in their connector configuration later.

## Discovery scheduling

The daemon continues to use the existing child-refresh queue for virtual
filesystem discovery. Its global worker limits are separate from API quotas:
32 total child refreshes and 16 background refreshes by default. A connector
descriptor may opt into periodic root discovery without changing Notion's
recursive discovery behavior.

Background admission is additionally capped per connector. Notion and Granola
each admit at most three background discovery workers, matching their initial
token-bucket burst. Interactive discovery can still use the remaining global
capacity. A workspace walk therefore cannot turn all 16 background slots into
threads waiting inside one connector's rate limiter.

Failed durable child refreshes remain queued with exponential retry delay from
one second up to five minutes. Delayed work from one mount does not block ready
work from another mount. An interactive directory request promotes its delayed
refresh immediately, preserving on-demand behavior while preventing a broken
credential or offline provider from creating a hot global retry loop.

Granola opts in at five minutes. After its first complete enumeration, it saves
a durable versioned checkpoint and requests only notes updated since the last
successful discovery, with a two-day overlap. Periodic requests refresh the
Granola mount root only and are not persisted as retry jobs. The scheduler
checks for due connector work every 15 seconds even when scheduled pull is in
relay mode.

## Configuration ownership

Global and Granola quota, concurrency, retry, and scheduling values are typed
internal policy in code. They are deliberately not exposed as environment
variables: changing them affects reliability and provider compliance and
should go through review and tests.

Notion quota, retry, and timeout values use the same typed internal policy. Its
previous environment overrides were removed without changing the established
values or request behavior. New connectors should follow the same pattern.

## Failure and state boundaries

Network permits are RAII guards and are released on every response or error.
Provider cooldowns are isolated by scope. Granola discovery state is scoped to
the mount and is cleared with other source-scoped state if the mount changes
connection or remote root. A state record requiring a newer reader fails with
an update-required error instead of silently discarding the checkpoint.

Scheduled pull resolves mounts independently. A missing credential or broken
profile skips that mount for the tick while healthy mounts continue through
reconciliation and freshness scheduling.
