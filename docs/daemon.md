# Daemon

`localityd` is the local supervisor for mounted Locality trees. The daemon is the
stateful execution owner: CLI surfaces and future IPC submit jobs, while the
daemon mutates local files, shadows, hydration state, journals, and remote
sources through one serialized boundary.

## Execution Boundary

`DaemonExecutor` is the daemon-owned job interface. It currently covers file
events, scheduled pull reconciliation, one-off hydration requests, hydration
queue drains, and push jobs. Push apply, journal writes, and post-apply
reconciliation run through one daemon-owned host so remote writes and local
state advancement cannot drift across separate store handles.

The boundary keeps responsibilities sharp:

- `locality-core` decides pure sync state and validates plans;
- connectors enumerate, fetch, render, and apply source-specific mutations;
- `localityd` executes jobs and is the only layer that advances durable sync state or
  mutates the local projection.

## Process Management

`localityd` stays intentionally small: it runs the daemon in the foreground and owns
the runtime, sockets, watchers, scheduler, and job queue. User-facing process
management lives in `loc daemon ...`:

- `loc daemon start` starts a background daemon. On macOS this installs and
  bootstraps `~/Library/LaunchAgents/ai.codeflash.locality.localityd.plist` by default,
  with `RunAtLoad` and `KeepAlive` so it starts at login and restarts after
  crashes.
- `loc daemon start --session` starts a detached child process that inherits the
  current shell environment and writes `~/.loc/localityd.pid`. This is useful for
  development credentials and temporary test state, but it does not survive
  logout.
- On Windows, session mode uses localhost TCP control IPC. `--tcp-addr off` is
  rejected for daemon start because Windows does not have the Unix socket
  fallback used by macOS/Linux CLI control.
- `loc daemon status` pings the daemon control endpoint and reports the state
  root, socket path, TCP address, manager, log path, runtime queue counts,
  scheduler mode, and watched mount roots.
- `loc daemon reload` asks the running daemon to reconcile file watches with the
  current SQLite mount table.
- `loc daemon stop` unloads the LaunchAgent or kills the session pid file when
  the CLI owns the daemon. A manually started foreground `localityd` still needs to be
  stopped directly.

The process manager passes `LOCALITY_STATE_DIR` to the daemon it starts. `--tcp-addr`
persists `LOCALITY_DAEMON_TCP_ADDR` for that managed daemon. `--include-env <KEY>` is
available for short-lived development variables that launchd would not otherwise
inherit; long-lived connector auth should move to `loc connect` and keychain
storage instead of plist environment variables.

## Foreground Daemon

`localityd` now runs a foreground Unix-socket server at `LOCALITY_STATE_DIR/localityd.sock`
or `~/.loc/localityd.sock` on Unix, plus a localhost TCP listener at
`127.0.0.1:38567` by default. On Windows, the TCP listener is the daemon control
endpoint. It also serves a lightweight MCP-over-HTTP endpoint at
`http://127.0.0.1:38568/mcp` by default. On Unix, CLI `pull` and `push` try the
Unix socket first. On Windows, they use the TCP control endpoint. If the daemon
endpoint is unavailable, they fall back to the same in-process executor; if the
daemon accepts a request but does not answer within the CLI timeout, `pull`
falls back to direct execution while `push` fails closed to avoid duplicate
remote writes. The macOS File Provider extension uses the TCP listener because
the extension is sandboxed. Set `LOCALITY_DAEMON_TCP_ADDR=off` to disable daemon TCP
on Unix, or set it to `host:port` to move the listener. Set
`LOCALITY_MCP_ADDR=off` to disable the MCP endpoint, or set it to `host:port` to move
the endpoint. Setting `LOCALITY_DAEMON_DISABLE=1` forces the CLI fallback path, which
is useful for tests and recovery. Set `LOCALITY_DAEMON_REQUEST_TIMEOUT_MS` to tune the
CLI daemon request timeout.

The MCP endpoint exposes one tool named `loc`. The tool accepts CLI-style
arguments as `argv`, plus optional `cwd` and `timeoutMs`. It is a fallback for
agent sandboxes that cannot run the host `loc` binary directly; agents that can
run `loc` should keep using the CLI. URL-based clients use the daemon HTTP
endpoint with a per-install bearer token stored at `LOCALITY_STATE_DIR/mcp-token` or
`~/.loc/mcp-token`; the desktop agent installer writes that token into supported
local agent MCP config files. Claude Desktop is configured differently: it
launches `loc mcp` and communicates over stdio because Claude Desktop's local
MCP config expects a command-shaped local server. The endpoint does not do work
while idle; the listener thread blocks on accept and only spawns the host `loc`
binary for actual MCP tool calls. To avoid exposing this host bridge to
arbitrary browser origins, requests with an `Origin` header are accepted only
from localhost origins.

The socket accept loop does not run connector calls directly. It reads one JSON
request, submits it to `DaemonRuntime`, and waits for the runtime response.
Health checks are answered by the runtime control loop, while mutating jobs are
queued behind a single active worker. A slow Notion enumerate/fetch/apply call
therefore does not block the daemon from accepting other requests or responding
to pings, and two pull/push/hydration mutations cannot advance durable state at
the same time.

For macOS File Provider mounts, the Swift extension normally sends writes to the
daemon through `modifyItem`. Locality also has a narrow reconciliation fallback at
review and push boundaries: explicit-path status, diff, push, auto-save, and
desktop file actions can import a newer visible CloudStorage file into the
daemon content cache before planning. This fallback is intentionally scoped to
explicit targets; startup, tray refreshes, and bare status do not scan the whole
projection.

## Operator Guide

Reset a local macOS test machine to a clean Locality install state:

```bash
make clean-start-plan
make clean-start
```

`make clean-start-plan` is a dry run. `make clean-start` stops the desktop app,
daemon, and File Provider extension; unregisters File Provider domains; removes
safe Locality mount roots such as `~/Documents/Locality`, `~/Library/CloudStorage/Locality`,
and `~/Library/CloudStorage/Locality-*`;
deletes `~/.loc`; removes the installed `/Applications/Locality.app`; and deletes Locality
connection credentials from the keychain. Run
`scripts/clean-start.sh --yes --keep-credentials` when testing app install state
without clearing OAuth/PAT credentials.

Start the daemon in the foreground:

```bash
localityd
```

From a source checkout, use either repo-root command:

```bash
make run-daemon
cargo run -p localityd
```

On startup it prints the socket path, watched mounts, and auth source:

```text
localityd listening on /Users/alice/.loc/localityd.sock
localityd watching 1 mount: /Users/alice/loc/notion
localityd auth: connection notion-work
```

Check health from the CLI:

```bash
loc daemon status
```

Successful output:

```text
daemon running  socket=/Users/alice/.loc/localityd.sock  ping=ok
```

Stopped output:

```text
daemon stopped  socket=/Users/alice/.loc/localityd.sock
  hint: run `localityd` in another terminal
```

`loc pull` and `loc push` try the daemon first. Human success output includes `(via daemon)` or `(via cli)`, and JSON reports include `via`. If the socket is unavailable, the CLI falls back to direct execution and prints:

```text
localityd not running; executing pull directly (start localityd for background hydration)
```

If a `pull` request times out after being submitted, the CLI also falls back and
prints:

```text
localityd did not respond within 5000ms; executing pull directly
```

Timed-out `push` requests do not fall back because the daemon may already own an
in-flight remote mutation. Stop or recover the daemon before retrying the push.
Set `LOCALITY_DAEMON_DISABLE=1` to force direct execution without the fallback warning.

## Runtime Loop

`DaemonRuntime` is the foreground daemon's control plane. It owns the scheduler
clock, the pending IPC job queue, the hydration queue, and the retry parking lot
for failed hydrations. User-submitted pull and push requests outrank background
work. Queued hydrations drain before the next scheduled poll so policy refreshes
turn into actual local files instead of accumulating indefinitely.

The runtime never performs slow connector work on the control thread. It starts
one mutating worker at a time, and the worker opens the durable store for that
transaction, runs the connector call, and reports completion back to the runtime.
That keeps the current SQLite-backed implementation simple while preserving the
important invariant: daemon-managed mutations are serialized through one queue.
Watcher events use the same queue, so local filesystem changes cannot race
remote pull, hydration, or push reconciliation. Runtime status includes the
current active job kind, target, start time, and elapsed time so wedged workers
are visible through `loc daemon status`.

## Virtual Filesystem Projections

Product-grade online-only mounts must use a virtual filesystem projection, not
read-after-the-fact file watching. The daemon owns the durable state and exposes a
platform-neutral `virtual_fs` boundary:

- `virtual_fs_item` returns one projected item from SQLite without reading a
  Markdown body.
- `virtual_fs_children` returns dataless directory contents from SQLite.
- `virtual_fs_materialize` hydrates a page with `HydrationReason::FileOpen` and
  returns the materialized Markdown path once the content exists locally.
- `virtual_fs_commit_write` records full-file writes from virtual filesystem
  adapters into daemon-owned content storage and updates local dirty state.
- `virtual_fs_create_file`, `virtual_fs_rename`, and `virtual_fs_trash` record
  pending virtual mutations that are applied by the normal push pipeline.

macOS File Provider uses this boundary through compatibility IPC names
(`file_provider_item`, `file_provider_children`, and
`file_provider_materialize`). The Swift extension copies the materialized path
into File Provider's transfer directory before completing `fetchContents`, so the
system can take ownership without moving Locality's canonical hydrated copy.

Linux uses a separate FUSE projection adapter over the same daemon boundary.
`readdir` and `getattr` read store metadata, `open`/`read` block on daemon
materialization and then serve real bytes, write/flush paths route local edits
back through daemon-owned dirty/push/reconcile logic, and create/rename/unlink
callbacks become pending virtual mutations. inotify is not sufficient for
online-only reads because it observes filesystem activity after the kernel has
already asked for file contents; fanotify permission events can block opens but
still require a backing file to exist before allowing the open. FUSE is the clean
Linux equivalent because Locality directly serves the read.

Virtual projection contents are materialized under `~/.loc/content/<mount-id>/`
instead of under the mounted root. This avoids recursive FUSE calls when the root
is itself a virtual mount and gives macOS/Linux adapters one stable byte source.

Scheduled reconciliation skips writing placeholder Markdown files for virtual
filesystem projection modes such as `macos_file_provider` and `linux_fuse`; it
updates durable entity state, queues policy hydration, and caches database
`_schema.yaml` files under daemon-owned content storage. Plain-file mounts still
use the fallback watcher path below.

## File Watching

The foreground daemon starts a `notify` watcher for every mount loaded from the
SQLite store at startup, and `reload_mounts` reconciles those watches with the
current mount table without restarting the process. `loc mount` calls this IPC
after saving a mount, so persistent daemons begin watching newly mounted
directories immediately. Create and modify notifications are normalized to
`Write` events, native access/open notifications are normalized to `Read` events
when the platform reports them, and remove/rename notifications are delivered
but ignored until delete/rename planning is wired.

Write events for hydrated pages are resolved back to stored entities inside the
runtime. If the Local Tree body still matches the Synced Tree shadow, the event
is treated as a daemon-authored projection write and ignored. If the body
differs from the shadow, the entity transitions to `dirty`. This suppresses
feedback from hydration, scheduled pull, and explicit pull without relying on
fragile timing windows or global path ignore lists.

The daemon also runs a stub access watcher. It scans only stored `virtual` and
`stub` entity paths under watched mounts and emits a `Read` event when a stub's
access time advances. This covers platforms where the regular watcher does not
surface open/read notifications to user-space. The scan only submits daemon
events; the runtime decides whether to queue hydration.

Read events are resolved inside `DaemonRuntime`. A read on a `virtual` or `stub`
entity creates a high-priority `StubRead` hydration request and returns to the
control loop; connector fetch/render work happens later through the existing
hydration worker path. Reads of hydrated, dirty, or conflicted files are ignored.

## Push Execution

`localityd::push::execute_push_job` prepares an explicit push job from the target
path, asks `locality-core` to plan and gate the mutation, and then executes the
approved plan through a combined journal/check/apply/reconcile host. The host
owns one mutable store reference for the entire transaction:

1. append the journal entry with the shadow preimage;
2. mark the journal `Applying`;
3. perform connector concurrency checks and apply the approved plan;
4. persist connector apply effects and mark the journal `Applied`;
5. re-fetch the changed remote entities through the hydration source;
6. write the canonical local projection, save the new shadow, update entity
   hydration metadata, and mark the journal `Reconciled`.

If connector apply or read-back fails, the daemon marks the journal `Failed` and
returns a structured push report containing the push id, journal status, and
error. Non-approved plans such as validation failures, confirmation gates, noops,
and read-only mounts return `NotReady` without touching the journal or connector.

## Scheduler

`PullScheduler` owns polling cadence only. It does not call connectors or mutate
state. In direct polling mode, the first tick asks for both active and cold polls
so a newly started daemon catches up immediately. Later ticks become due when
their configured intervals elapse. Relay mode returns idle ticks because the
future relay change feed will drive pull work directly. `DaemonRuntime` advances
the scheduler on its control tick and turns due ticks into serialized scheduled
pull workers.

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
5. persist the Synced Tree shadow snapshot;
6. mark the entity `hydrated` and store the rendered body hash.

Dirty local files are not overwritten. If a non-stub Local Tree file no longer
matches the Synced Tree shadow body, the executor skips that request and marks
the entity `dirty` when the hydration ladder allows it. Source or I/O failures
leave the request in the queue so a later daemon tick can retry.

`localityd::notion` wires `NotionConnector` into this source boundary. It uses the
Notion connector's fetch path and path-aware render method so daemon hydration
persists the same shadow snapshot and media projection that CLI pull uses.

For macOS File Provider mounts, the runtime captures the previous Synced Tree
shadow before a background `remote_fast_forward` hydration. After the hydration
succeeds, it best-effort refreshes any already-materialized visible CloudStorage
replica from the daemon content cache, but only when that visible file still
matches the previous shadow. Diverged visible files are left alone so background
remote refresh cannot erase a local edit that missed the normal File Provider
write callback.

Desktop Live Mode uses the same boundary in a bounded loop. The primary
local-write path is File Provider `modifyItem`; the visible CloudStorage
reconciliation fallback is throttled and scoped to the selected
already-hydrated page so the app does not poll the user-visible file every tick.
Remote checks fetch one already-hydrated page into the daemon content cache and
compare the rendered shadow before refreshing the visible projection. This avoids
relying on Notion page metadata that can miss some body edits, while leaving
unchanged CloudStorage replicas untouched.

## Scheduled Pull Reconciliation

`reconcile_scheduled_pull` is the daemon-side counterpart to `loc pull` for
background refresh. It executes a strategy decision rather than owning scheduling
policy itself:

- `ScheduledPullSource` enumerates a mount and supplies connector-specific
  projection data such as database schemas;
- `FetchScheduleStrategy` decides per mount whether a scheduler tick should
  enumerate, and per entity whether the resulting projection should enqueue
  hydration;
- the reconciler upserts entity records, writes page stubs, refreshes database
  schemas, and queues hydration requests, then returns a structured report.

The default strategy is intentionally conservative: due scheduler ticks
enumerate mounts, remote-root pages hydrate so the mounted entry point stays
usable, small eager-sync workspaces can hydrate through `HydrationPolicy`, and
already hydrated pages with changed Remote Tree versions are queued for refresh.
Project- or mount-specific strategies can dispatch on `MountConfig` without
changing the reconciliation mechanics.

For hydrated, dirty, or conflicted entities, enumeration preserves the Synced
Tree remote version until hydration writes a new shadow. That version is the
push precondition for the current Local Tree file, so it must advance with the
shadow, not with a metadata-only poll.

## Supervisor Events

`DaemonSupervisor` implements `DaemonExecutor` and currently handles these
stateful operations:

- startup loads mounts from the store and registers each root with the watcher;
- reading a `virtual` or `stub` entity queues hydration to `hydrated`;
- scheduled pull ticks can enumerate mounts, refresh projections, and queue
  strategy-selected hydration;
- queued hydration can be drained through a source-specific executor;
- push jobs can apply connector mutations, refresh Synced Tree shadows, and
  advance journals;
- writing a `hydrated` entity marks it `dirty` when the Local Tree body differs
  from the Synced Tree shadow;
- remove and rename events are ignored until conflict/delete planning is wired.

Conflict materialization remains a later daemon stage.
