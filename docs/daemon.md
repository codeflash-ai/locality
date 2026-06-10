# Daemon

`afsd` is the local supervisor for mounted AgentFS trees. The first implemented
slice is deliberately deterministic and testable: it wires mount roots to a file
watcher, converts file events into sync-core state transitions, and exposes a
pull scheduler that can be advanced by tests without sleeping.

## Scheduler

`PullScheduler` owns polling cadence only. It does not call connectors or mutate
state. In direct polling mode, the first tick asks for both active and cold polls
so a newly started daemon catches up immediately. Later ticks become due when
their configured intervals elapse. Relay mode returns idle ticks because the
future relay change feed will drive pull work directly.

## Hydration Queue

`HydrationQueue` is keyed by `(mount_id, remote_id)` so one daemon can supervise
many mounts without coalescing unrelated entities. Duplicate requests merge into
one pending request. Explicit pulls and stub reads outrank policy hydration,
which outranks prefetch work.

The queue preserves deterministic behavior:

- high-priority work drains before policy and prefetch work;
- duplicate lower-priority requests do not move a higher-priority request down;
- failed drain attempts requeue the failed request instead of dropping it.

## Hydration Execution

`HydrationExecutor` performs the local hydrate transaction for one queued
request:

1. load the mount and entity from the store;
2. verify the local file is safe to replace;
3. fetch and render through a `HydrationSource`;
4. write the rendered Markdown with temp-file-plus-rename;
5. persist the shadow snapshot;
6. mark the entity `hydrated` and store the rendered body hash.

Dirty local files are not overwritten. If a non-stub file no longer matches the
stored shadow body, the executor skips that request and marks the entity `dirty`
when the hydration ladder allows it. Source or I/O failures leave the request in
the queue so a later daemon tick can retry.

`afsd::notion` wires `NotionConnector` into this source boundary. It uses the
Notion connector's fetch path and `render_native_entity` method so daemon
hydration persists the same shadow snapshot that CLI pull uses.

## Supervisor Events

`DaemonSupervisor` currently handles the safe local state transitions that do not
need connector I/O:

- startup loads mounts from the store and registers each root with the watcher;
- reading a `virtual` or `stub` entity queues hydration to `hydrated`;
- queued hydration can be drained through a source-specific executor;
- writing a `hydrated` entity marks it `dirty` in the store;
- remove and rename events are ignored until conflict/delete planning is wired.

Remote polling and conflict materialization remain later daemon stages.
