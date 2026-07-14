# Live Mode Mechanics

This document is derived from the current implementation. If this file and the
code disagree, the code wins. Existing docs may describe intent, but the source
of truth for this reference is the code paths named inline.

## Scope

Live Mode is the desktop background sync loop for a mounted workspace. When it
is enabled, Locality can:

- push safe local edits without a manual `loc push`
- pull or fast-forward clean remote updates without a manual `loc pull`
- observe recently active remote pages so remote changes can be discovered
- pause when review, conflict resolution, destructive changes, remote deletes,
  unsupported plans, or connector failures require a human decision

Live Mode is not a single daemon feature. It is an orchestration layer across:

- desktop Tauri commands and the desktop runner in
  `apps/desktop/src-tauri/src/main.rs`
- durable mount and per-file state in `crates/locality-store`
- daemon freshness, hydration, file-event, and auto-push queues in
  `crates/localityd`
- status and pending-change classification in `crates/loc-cli/src/status.rs`

## Terminology

- **Mount Live Mode** is a per-mount setting stored as
  `MountLiveModeRecord`. It controls the desktop Live Mode runner for the
  selected mount.
- **File Live Mode** is implemented as an `AutoSaveEnrollmentRecord`. The UI
  labels it as Live Mode for a file, but the durable state and daemon policy are
  named auto-save.
- **Freshness** records local activity and remote hints. It schedules cheap
  observations and helps decide which pages deserve active checks.
- **Observation** uses connector `observe` to fetch remote metadata/version
  only. It does not fetch page body content.
- **Remote fast-forward** is hydration of a changed remote page into local
  content when the local file still matches the shadow and no active lease says
  the user is probably working in it.
- **Auto-push** is daemon-triggered execution of an auto-save push job for one
  path.

## Durable State

### Mount Live Mode

`MountLiveModeRecord` and `MountLiveModeState` live in
`crates/locality-store/src/records.rs`.

States:

- `Off`: disabled.
- `Active`: enabled and not currently marked syncing.
- `Syncing`: the desktop runner has started a tick for the mount.
- `Error`: disabled because Live Mode paused for a review-grade failure.

`MountLiveModeRecord` stores:

- `mount_id`
- `enabled`
- `state`
- `last_reason`
- `last_run_at`
- `created_at`
- `updated_at`

Repository boundary:

- `MountLiveModeRepository` in `crates/locality-store/src/repository.rs`
- SQLite implementation in `crates/locality-store/src/sqlite.rs`
- table `mount_live_modes`

State changes that should wake desktop/daemon surfaces go through
`save_mount_live_mode_and_publish_signal` in
`crates/locality-store/src/live_mode.rs`. That writes the durable record and
publishes `live-mode.changed` under the state root. The desktop state watcher
uses that file as an explicit wake signal instead of treating SQLite WAL churn
as a wake source.

### Per-File Auto-Save State

`AutoSaveEnrollmentRecord`, `AutoSaveOrigin`, and `AutoSaveState` live in
`crates/locality-store/src/records.rs`.

Origins:

- `LocalityCreated`: the file was created locally or is represented by a local
  virtual create.
- `UserEnabled`: the user explicitly enabled file Live Mode for an existing
  file.

States:

- `Active`: enabled and allowed to auto-save.
- `Blocked`: the latest auto-save plan needed review. This state is still
  enabled; later writes can retry.
- `PausedRemoteChanged`: remote drift was detected, so auto-save is suppressed
  until user action.
- `PausedFailure`: an execution failure paused auto-save until user action.

`AutoSaveEnrollmentRecord` stores:

- `mount_id`
- `path`
- `remote_id`
- `enabled`
- `origin`
- `state`
- `last_reason`
- `last_push_id`
- `created_at`
- `updated_at`

Repository boundary:

- `AutoSaveRepository` in `crates/locality-store/src/repository.rs`
- SQLite implementation in `crates/locality-store/src/sqlite.rs`
- table `auto_save_enrollments`

The UI command `set_live_mode_for_file` writes this record. The older command
name `set_auto_save_for_file` is still registered and delegates to
`set_live_mode_for_file`.

## UI And Command Entry Points

React UI types and controls live in `apps/desktop/src/App.tsx`.

- `MountLiveMode` mirrors the mount-level summary surfaced from the backend.
- `PendingChange.liveMode` mirrors per-file auto-save state.
- `useMountLiveModeController` calls Tauri command `set_mount_live_mode`.
- File-row toggles call Tauri command `set_live_mode_for_file`.

Tauri command registration is in `apps/desktop/src-tauri/src/main.rs`.
Relevant commands:

- `live_mode_tick`
- `set_mount_live_mode`
- `set_live_mode_for_file`
- `set_auto_save_for_file`

`set_mount_live_mode_blocking` chooses the current mount, creates or updates a
`MountLiveModeRecord`, publishes the state-change signal, wakes the runner, and
returns "on/off for this folder".

`set_live_mode_for_file_blocking` resolves the absolute path to a mount-relative
path, loads any existing enrollment, records the current remote id when known,
sets the enrollment `enabled` flag, resets state to `Active`, clears
`last_reason`, and saves it. When enabling a file, it immediately calls
`auto_save_target_direct`; that first push attempt is best-effort from the UI
command and its result is not used as the command response.

## Desktop Runner Lifecycle

The desktop app starts two relevant background threads during Tauri setup in
`apps/desktop/src-tauri/src/main.rs`:

- `start_state_change_watcher`
- `start_live_mode_runner`

`start_state_change_watcher` watches the state root and virtual content roots.
It debounces events, refreshes desktop surfaces for relevant state/content
changes, and calls `wake_live_mode_runner` when:

- `live-mode.changed` is written
- virtual content under the state root/content roots changes

`start_live_mode_runner` is the background loop:

- if no mount has Live Mode enabled, it waits up to
  `LIVE_MODE_RUNNER_IDLE_RECHECK` (5 minutes) or until woken
- if a mount has Live Mode enabled, it runs `live_mode_tick_blocking`
- after an active tick, it waits up to `LIVE_MODE_RUNNER_ACTIVE_INTERVAL`
  (500 ms) or until woken
- it refreshes desktop surfaces when the tick reports sync, pull, pause, off,
  or failure states

`live_mode_tick_blocking` has an atomic `LIVE_MODE_TICK_IN_PROGRESS` guard. If
another tick is already running, it returns successfully with "already syncing"
instead of starting overlapping work.

`live_mode_tick_blocking_inner`:

1. Opens the default state root.
2. Finds the currently selected enabled mount with `live_mode_enabled_mount`.
3. Marks the mount `Syncing` with `mark_mount_live_mode_syncing`.
4. Runs `live_mode_tick_for_enabled_mount`.
5. Records the result with `record_mount_live_mode_tick_result`.

`record_mount_live_mode_tick_result` keeps Live Mode enabled after transient
failures unless `live_mode_failure_should_pause` classifies the message as
review-grade. Review-grade failures currently include:

- messages starting with `Live Mode paused for`
- messages containing `Review required before pushing`
- messages that cannot identify the remote page

`loc status` and desktop summaries also hide stale disabled error records when
there are no current pending changes requiring attention.

## Tick Algorithm

The central algorithm is `live_mode_tick_for_enabled_mount` and
`live_mode_tick_from_snapshot` in `apps/desktop/src-tauri/src/main.rs`.

Each tick is intentionally bounded. It handles at most the first pending change
from the desktop snapshot, or a bounded batch of remote-check targets when no
local pending changes exist.

### Step 1: Reconcile Recent Local Provider Targets

Before inspecting pending changes, the runner calls
`live_mode_reconcile_recent_local_targets`.

This selects recently active hydrated pages from local state and calls
`reconcile_newer_macos_file_provider_projection` for each selected target. The
selection is bounded by `live_mode_remote_check_page_budget`, sorted by recent
local/open activity, and throttled per path by
`LIVE_MODE_LOCAL_RECONCILE_INTERVAL` (5 seconds).

This matters for visible File Provider projections, where the CloudStorage
visible file, daemon content root, and durable state can diverge.

### Step 2: Load Desktop Snapshot

The runner loads `DesktopSnapshot` through `load_desktop_snapshot`.

Pending changes are not computed by a special Live Mode diff. They come from
`run_status` in `crates/loc-cli/src/status.rs`, then desktop code maps status
entries into `PendingChange` values.

Desktop pending states:

- `safe`: a local pending change that status says can be pushed without review.
- `needs_review`: remote update available, review needed, or large-change issue.
- `blocked`: missing/error state or failed journal that is not just stale.
- `conflict`: conflicted sync state.

Status sync states come from `StatusSyncState` in
`crates/loc-cli/src/status.rs`:

- `all_synced`
- `checking_freshness`
- `remote_update_available`
- `pending_local_changes`
- `review_needed`
- `conflicted`

### Step 3: If No Pending Changes, Queue Remote Checks

When the snapshot has no pending changes and
`live_mode_remote_pull_scan_is_due` says the mount is due, the runner selects
remote-check targets with `live_mode_next_remote_pull_targets_for_state_root`.

Candidate selection in `live_mode_remote_pull_candidates`:

- only page entities
- only `Hydrated` entities
- only recently active pages, where recent means:
  - `remote_hint_pending`, or
  - opened within `LIVE_MODE_ACTIVE_TARGET_WINDOW` (5 minutes), or
  - locally changed within that same window
- only when the remote check is due, based on `last_checked_at` and
  `LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL` (5 seconds)
- remote-hint-pending pages are not reselected for this scan because they
  already need follow-up

Selection uses a rotating cursor so repeated scans do not always start at the
same page. Batch size comes from `live_mode_remote_check_page_budget_for_rate`,
which uses the configured Notion request rate, gives Live Mode one third of the
interval budget, assumes one request per page, and caps the batch at
`LIVE_MODE_REMOTE_CHECK_MAX_BATCH_PAGES` (5).

If targets are selected and the snapshot still has no pending changes after
local reconciliation, `live_mode_tick_from_snapshot` queues one
`DaemonRequest::ObserveEntity` for each target. The daemon queues these as
immediate `RemoteMaybeChanged` freshness jobs, subject to daemon-side queue
budgeting.

### Step 4: If There Is A Pending Change, Handle The First One

`live_mode_tick_from_snapshot` only handles `snapshot.pending_changes.first()`.

For a `safe` change:

- it calls `push_target_direct(target, false)`
- the direct push uses `assume_yes = true` and `confirm_dangerous = false`
- if push succeeds, the tick reports one synced pending change
- if push fails, the failure is recorded and may pause Live Mode depending on
  the failure message

For a `needs_review` change with issue code `remote_changed`:

- the runner treats it as a remote-only update
- it queues `DaemonRequest::RemoteFastForward`
- the daemon converts that into `HydrationReason::LiveModeRemoteFastForward`

For a `needs_review` change that may be local edits plus remote drift:

- `live_mode_change_may_merge_remote_drift` checks for issue code
  `remote_changed_with_local_pending` or equivalent summary text
- the runner calls `live_mode_merge_remote_drift_target`
- that function reconciles visible projection state, loads the entity and
  shadow, fetches/renders the current remote body with `fetch_render`, and
  performs a three-way Markdown merge using the previous shadow as base
- if the merge has no conflict markers, it writes merged Markdown/assets,
  updates shadow/entity/remote observation/freshness hint state, and then
  pushes the local pending change
- if the merge needs conflict markers, it writes those markers, marks the entity
  `Conflicted`, clears the remote hint, and stops for review

For any other non-safe change:

- the tick returns a pause message for the file
- `record_mount_live_mode_tick_result` disables mount Live Mode with an `Error`
  record when the message is review-grade

## Local Edit And Auto-Push Path

There are two related but distinct local push paths.

### Desktop Runner Push

The desktop runner pushes the first `safe` pending change in its snapshot with
`push_target_direct`. This path:

- reconciles visible provider changes for the target
- resolves the connector for the path
- runs push with `assume_yes = true`
- refuses dangerous plans because `confirm_dangerous = false`

This path is controlled by mount Live Mode and status classification. It does
not require a per-file auto-save enrollment.

### Daemon Auto-Push

Daemon auto-push is a lower-level shortcut for write events and some virtual
write responses.

Plain-file mounts:

- the daemon watches only `ProjectionMode::PlainFiles` roots
- file events are classified in `crates/localityd/src/watcher.rs`
- write events call `handle_write_event` in `crates/localityd/src/runtime.rs`
- hydrated pages whose file no longer matches the shadow become `Dirty`
- dirty/conflicted writes record local-change freshness and queue an observe
  job with `ChangeHintKind::LocalEdited`
- if `auto_save_target_for_write` returns a path, runtime queues auto-push

`auto_save_target_for_write` in `crates/localityd/src/autosave.rs` returns a
target when either:

- an enabled per-file auto-save enrollment exists and is not paused for remote
  change/failure, or
- mount Live Mode is enabled for that mount

Virtual/provider writes:

- virtual write responses are handled in `run_job` for
  `VirtualFsCommitWrite`
- `response_auto_push_targets` queues daemon auto-push only when the response
  reports dirty hydration and an enabled per-file enrollment exists for the
  remote id
- otherwise the desktop runner can still pick up the pending change from status
  on the next tick when mount Live Mode is enabled

Queued daemon auto-push:

- `queue_auto_push` pushes an `AutoPush` mutating request to the front of the
  runtime pending-request queue
- `auto_push_job` uses `assume_yes = true` and `confirm_dangerous = false`
- `run_auto_push` calls `execute_auto_save_push_job_with_content_root`

## Auto-Save Safety Policy

The policy lives in `crates/localityd/src/autosave.rs` and
`execute_auto_save_push_job_with_content_root` in `crates/localityd/src/push.rs`.

Auto-save first prepares and preflights a push. `auto_save_block_reason` blocks
the job before apply when:

- local Markdown validation is not clean
- the pipeline asks for explicit plan confirmation
- the pipeline asks for dangerous/destructive confirmation
- validation must be fixed
- the mount is read-only
- operations are unsupported
- guardrails require review
- the push plan is missing
- the plan has degradations

Allowed plan operations:

- `CreateEntity`
- `UpdateBlock`
- `AppendBlock`
- `UpdateProperties`

Blocked plan operations:

- `ReplaceBlock`
- `MoveBlock`
- `UpdateMedia`
- `ArchiveBlock`
- `ArchiveEntity`

Auto-save state updates:

- `Reconciled` marks enrollment `Active`, records remote id and push id.
- `NotReady` with a no-op plan marks enrollment `Active`.
- Other `NotReady` outcomes mark enrollment `Blocked`.
- Failed guardrail errors about remote changes mark enrollment
  `PausedRemoteChanged`.
- Other failures mark enrollment `PausedFailure`.

`PausedRemoteChanged` and `PausedFailure` suppress future auto-save attempts
from `auto_save_enabled_for_path`. `Blocked` does not suppress retry; a later
edit can reattempt auto-save and either become active or remain blocked.

## Remote Observation And Fast-Forward Path

### Queueing Remote Observation

The desktop runner queues remote checks with `DaemonRequest::ObserveEntity`.
Daemon request handling in `crates/localityd/src/runtime.rs` turns this into a
`SyncJob`:

- kind `ObserveEntity`
- reason `RemoteMaybeChanged`
- tier `Immediate`

The job is queued by `queue_bounded_live_mode_remote_observe`. It allows an
existing duplicate, otherwise caps queued Live Mode remote-observe jobs by
`live_mode_remote_observe_queue_budget(active_interval)`. That budget uses the
configured Notion request rate, one third of the active interval, and caps at
`LIVE_MODE_REMOTE_OBSERVE_MAX_QUEUE_JOBS`, which equals the daemon freshness
budget units.

Freshness jobs are accepted only when daemon `background_connector_sync` is
enabled. That setting defaults on and can be disabled with
`LOCALITY_DAEMON_BACKGROUND_CONNECTOR_SYNC=0|off|none|disabled`.

### Applying Remote Observation

`execute_observe_entity_job` calls connector `observe`. Then
`apply_remote_observation`:

- validates the observation matches the job
- handles remote deletes according to remote-delete policy
- compares the observed remote version with the known entity version
- if the remote version changed, pauses any per-file auto-save enrollment for
  the remote id with `PausedRemoteChanged`
- saves the `RemoteObservationRecord`
- updates `FreshnessStateRecord.last_checked_at`
- updates `remote_hint_pending`
- may create a remote fast-forward hydration request

`auto_fast_forward_requests_from_observation` only creates a hydration request
when:

- a matching entity exists
- remote hint is pending
- the observation is not deleted
- the observation kind is page
- the entity is already `Hydrated`

If mount Live Mode is enabled, the hydration reason is
`LiveModeRemoteFastForward`; otherwise it is `RemoteFastForward`.

### Queueing Remote Fast-Forward Hydration

Hydration requests with remote-fast-forward reasons go through
`auto_fast_forward_queue_decision` before being queued.

The queue decision skips unless:

- entity exists
- entity is a page
- entity is `Hydrated`
- freshness exists
- `remote_hint_pending` is true
- mount exists
- projected local content still matches the shadow

The decision delays when the page has an active lease. The lease is based on
`last_opened_at` or `last_local_change_at`, lasts
`AUTO_FAST_FORWARD_ACTIVE_LEASE_MS` (30 seconds), and requeues an observe job
for when the page is next eligible.

Only when all safety checks pass does the daemon queue hydration. The hydration
executor then fetches/renders remote content, validates shadow identity, writes
assets/Markdown, saves shadow/entity state, clears the remote hint when current,
and refreshes visible projection after remote fast-forward.

## Scheduled Pull And Workspace Freshness

Live Mode also interacts with daemon scheduled sync.

Daemon defaults:

- `DaemonConfig.runtime_tick_interval`: 1 second
- `PullSchedulerConfig.active_interval`: 15 seconds
- `PullSchedulerConfig.cold_interval`: 300 seconds
- `background_connector_sync`: enabled by default
- `LOCALITY_DAEMON_PULL_MODE=relay|off|disabled` switches scheduled polling to
  relay mode, where scheduler ticks are idle

The scheduler lives in `crates/localityd/src/scheduler.rs`. The daemon runtime
drains work in this priority order:

1. pending foreground/mutating requests
2. hydration queue
3. freshness queue
4. scheduled pull

For workspace-level virtual mounts, scheduled full enumeration is avoided.
Instead, `workspace_virtual_freshness_jobs` in
`crates/localityd/src/runtime.rs` selects bounded observe jobs for already-known
pages. When mount Live Mode is enabled and an active-tick candidate was selected
because it was opened, the tier is promoted to `Immediate`.

Workspace freshness selection includes pages that are `Hydrated`, `Dirty`, or
`Conflicted`, and skips `Virtual`/`Stub` pages. It sorts by tier, last checked
time, path, mount id, and remote id, caps active Live Mode candidates by remote
observe budget, and truncates to `MAX_WORKSPACE_FRESHNESS_JOBS_PER_TICK`.

## File Open Signals

File open/read signals feed freshness and hydration, which Live Mode later uses
for active remote checks.

Plain-file mounts:

- `NotifyFileWatcher` maps open/access/read/atime events to `FileEventKind::Read`
- `PollingStubReadWatcher` also polls stub access time
- `handle_read_event` records file-open freshness and queues an observe job with
  `ChangeHintKind::FileOpened`
- if the entity is a page in `Virtual` or `Stub` state, it also queues
  `HydrationReason::StubRead`

Virtual/File Provider reads:

- daemon materialize/read jobs produce observe jobs with
  `ChangeHintKind::FileOpened`
- these update freshness so the desktop runner can consider the page recently
  active

Freshness tiers come from `crates/locality-core/src/freshness.rs`.
`FileOpened`, `LocalEdited`, and `RemoteMaybeChanged` recommend hot freshness;
the desktop runner can promote selected active Live Mode checks to immediate.

## Conservative Boundaries

Live Mode is intentionally conservative.

It pauses or refuses to act for:

- conflicts
- missing/error states
- unresolved conflict markers
- remote deletes that require review
- local dirty state plus remote drift that cannot be cleanly merged
- large or destructive plans
- unsupported operations
- read-only mounts
- validation failures
- push guardrails that require confirmation
- remote drift detected immediately before push
- failures that imply the remote page cannot be identified

It does not:

- make read-only mounts writable
- force dangerous push plans
- resolve conflicts without writing reviewable conflict markers
- overwrite recently opened/edited local files with remote fast-forward while
  the active lease is in effect
- fetch full remote bodies during observation
- full-enumerate workspace virtual mounts just because Live Mode is enabled
- run the desktop Live Mode tick when the desktop app is not running

The daemon can still run background scheduled sync/freshness work while the
daemon is running, but the mount-level Live Mode loop described in this file is
started by the desktop app.

## Status And Debug Surfaces

`loc status` includes mount Live Mode in `StatusMountReport.live_mode`.
`StatusLiveMode::from_record` maps durable mount state to:

- `off`
- `active`
- `syncing`
- `error`

Desktop snapshot summaries add:

- total pending count
- review count
- covered/safe pending count

File rows use `LiveModeFileStatus`, derived from
`AutoSaveEnrollmentRecord`:

- no enrollment or disabled enrollment: `off`
- `Active`: `active`
- `Blocked`: `blocked`
- `PausedRemoteChanged`: `paused_remote_changed`
- `PausedFailure`: `paused_failure`

The UI does not decide sync behavior from these labels. The labels are derived
from store/status state after the backend has classified the file.

## Code Map

Desktop runner and UI commands:

- `apps/desktop/src-tauri/src/main.rs`
- `live_mode_tick_blocking`
- `live_mode_tick_for_enabled_mount`
- `live_mode_tick_from_snapshot`
- `start_live_mode_runner`
- `start_state_change_watcher`
- `set_mount_live_mode_blocking`
- `set_live_mode_for_file_blocking`
- `live_mode_merge_remote_drift_target`

Desktop React surface:

- `apps/desktop/src/App.tsx`
- `useMountLiveModeController`
- file-row `toggleFileLiveMode`

Durable state:

- `crates/locality-store/src/records.rs`
- `crates/locality-store/src/repository.rs`
- `crates/locality-store/src/sqlite.rs`
- `crates/locality-store/src/live_mode.rs`

Daemon runtime:

- `crates/localityd/src/runtime.rs`
- request handlers for `ObserveEntity` and `RemoteFastForward`
- file-event handling
- auto-push queueing
- freshness execution
- fast-forward queue decision
- workspace virtual freshness selection

Auto-save policy:

- `crates/localityd/src/autosave.rs`
- `crates/localityd/src/push.rs`

Freshness:

- `crates/locality-core/src/freshness.rs`
- `crates/localityd/src/freshness.rs`

Watchers:

- `crates/localityd/src/server.rs`
- `crates/localityd/src/watcher.rs`

Status:

- `crates/loc-cli/src/status.rs`

## Test-Audit Checklist

When changing Live Mode, audit and update tests around:

- mount Live Mode enable/disable and stale error hiding
- file Live Mode enrollment and status mapping
- desktop tick with no pending changes
- desktop tick selecting bounded remote candidates
- desktop tick pushing a safe pending change
- desktop tick queueing remote-only fast-forward
- desktop tick merging remote drift with local pending edits
- conflict marker materialization for overlapping remote/local edits
- auto-save policy blocking destructive or ambiguous plans
- auto-save state transitions for reconciled, blocked, remote-changed, and
  failed pushes
- daemon file-event write path marking dirty and queueing auto-push
- freshness observation setting `remote_hint_pending`
- remote observation pausing file auto-save
- remote fast-forward queue decision for dirty local files and active leases
- workspace virtual freshness budgets with Live Mode enabled
- state-change signal waking the desktop runner without reacting to SQLite WAL
  writes

Relevant test files currently include:

- `apps/desktop/src-tauri/src/main.rs` unit tests
- `crates/localityd/tests/runtime.rs`
- `crates/localityd/tests/push_execution.rs`
- `crates/localityd/tests/scheduled_pull.rs`
- `crates/localityd/tests/hydration_executor.rs`
- `crates/loc-cli/tests/status.rs`
- `crates/loc-cli/tests/e2e_push_workflow.rs`
