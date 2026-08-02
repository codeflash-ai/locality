# Amika Benchmark Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the launch-readiness wrapper reliably provision the exact Amika snapshots, wait for usable sandboxes, retain recovery environments after artifact-transfer failure, and pass a real paired Amika scenario.

**Architecture:** Replace the unsupported JSON-list assumption with a small Amika v0.12.1 table adapter, then drive creation through an owned retry/readiness state machine. Persist phase-specific pipeline outcomes so cleanup can distinguish benchmark failure from artifact-sync failure. Refresh `locality-snapshot` as an explicit rollout operation after unit behavior is green, then perform a real paired acceptance run.

**Tech Stack:** Bash, Python 3 only where already used by the runner, Amika CLI v0.12.1, fake-command shell integration tests, rsync/scp.

## Global Constraints

- Always use the exact default snapshots `locality-snapshot` and `mcp-snapshot`; never select a fallback.
- Support Amika CLI v0.12.1, whose list commands return tables and do not support `-o json`.
- Default to `AMIKA_CREATE_ATTEMPTS=3`, `AMIKA_READINESS_TIMEOUT_SECONDS=180`, and `AMIKA_READINESS_POLL_SECONDS=3`.
- A target name absent before create and present afterward is owned even if create exits nonzero.
- Delete an owned failed attempt and wait for name absence before retrying.
- Never automatically retry a Codex benchmark.
- With `SYNC_ARTIFACTS=1`, delete the pair only when both artifact syncs succeed; retain both after either sync fails.
- With `SYNC_ARTIFACTS=0`, keep the existing explicit warning and delete behavior.
- Preserve the existing generic SSH-provider behavior.
- Never log credential values.
- Snapshot refresh uses `full` mode and restores `aseem-locality` to its initial started/stopped state.

---

### Task 1: Amika v0.12.1 preflight and table adapter

**Files:**
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:87-224,387-417,1120-1163`
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:50-190,323-440`

**Interfaces:**
- Consumes: `amika sandbox list --remote` and `amika snapshot list` table output.
- Produces: `amika_table_state NAME TABLE`, `load_amika_sandbox_table`, `load_amika_snapshot_table`, `validate_positive_integer NAME VALUE`, and `preflight_amika_environment`.

- [ ] **Step 1: Add real-table fake responses and failing preflight cases**

Change the fake boundary to accept both `sandbox list --remote` and `snapshot list` and emit complete literal tables:

```text
NAME STATE LOCATION BRANCH REPO CREATOR CREATED
existing-box stopped remote - - Test 2026-08-02T00:00:00Z
```

```text
NAME STATE PROVIDER SOURCE CREATED
locality-snapshot active daytona aseem-locality 2026-08-02T00:00:00Z
mcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z
```

Add separate wrapper invocations that prove:

- a remote sandbox-list exit 41 fails before `sandbox create`;
- a missing `locality-snapshot` fails before create with `required Amika snapshot not found: locality-snapshot`;
- `locality-snapshot failed ...` fails with `required Amika snapshot is not active: locality-snapshot (state=failed)`;
- `AMIKA_CREATE_ATTEMPTS=0` fails with `AMIKA_CREATE_ATTEMPTS must be a positive integer`;
- no command log contains `-o json`.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL because the production wrapper still calls `amika sandbox list --remote -o json` and never validates snapshot state or retry configuration.

- [ ] **Step 3: Implement the table adapter and preflight**

Add exact defaults:

```bash
AMIKA_CREATE_ATTEMPTS="${AMIKA_CREATE_ATTEMPTS:-3}"
AMIKA_READINESS_TIMEOUT_SECONDS="${AMIKA_READINESS_TIMEOUT_SECONDS:-180}"
AMIKA_READINESS_POLL_SECONDS="${AMIKA_READINESS_POLL_SECONDS:-3}"
AMIKA_LIFECYCLE_LOG="$LOCAL_OUT_DIR/amika-lifecycle.log"
```

`amika_table_state NAME TABLE` must use `awk` to match the exact first field after the header and print the second field. `load_amika_sandbox_table` runs `amika sandbox list --remote`; `load_amika_snapshot_table` runs `amika snapshot list`. Preserve command stderr and wrap failures with phase-specific context without replacing the original diagnostic.

`preflight_amika_environment` must:

1. no-op unless `REMOTE_PROVIDER=amika`;
2. validate all three numeric settings with `^[1-9][0-9]*$`;
3. load the remote sandbox table, making this the real auth/control-plane check;
4. reject either target-name collision before creation;
5. load the snapshot table;
6. require both configured snapshot names to exist with state `active`.

Call this function after traps and credential loading but before `create_amika_sandboxes`.

- [ ] **Step 4: Verify GREEN and surrounding compatibility**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
bash -n experiment/locality-mcp-comparison/run-agent-comparison.sh
```

Expected: all tests print their pass line; the syntax check exits 0.

- [ ] **Step 5: Commit the CLI compatibility boundary**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh
git commit -m "Support Amika table preflight"
```

---

### Task 2: Owned provisioning retries and readiness

**Files:**
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:318-438`
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:50-190,323-622`

**Interfaces:**
- Consumes: table/preflight helpers from Task 1 and existing `run_managed_command`/cleanup traps.
- Produces: `add_owned_amika_sandbox NAME`, `remove_owned_amika_sandbox NAME`, `delete_owned_amika_sandbox NAME`, `wait_for_amika_sandbox_absent NAME`, `wait_for_amika_sandbox_ready NAME`, `provision_amika_sandbox NAME SNAPSHOT STRATEGY`, and `verify_amika_prerequisites`.

- [ ] **Step 1: Make the fake stateful and add failing lifecycle cases**

Use a per-invocation `FAKE_AMIKA_STATE_DIR` so create/delete/list commands observe the same sandbox states. Add literal cases for:

1. create exits 23 but registers the target as `failed`; the target is deleted, absence is observed, attempt 2 succeeds, SSH fails once, then readiness succeeds;
2. all three creates register `failed`; exactly three create commands and three deletes occur, the wrapper returns nonzero, and stderr names the exact snapshot;
3. create returns zero with `initializing`, list advances to `started`, SSH fails twice, then succeeds before benchmark commands;
4. a ready Locality sandbox fails its `loc`/repository prerequisite and both owned sandboxes are deleted before any benchmark launch.

Set test-only timing to `AMIKA_READINESS_TIMEOUT_SECONDS=3` and `AMIKA_READINESS_POLL_SECONDS=1` so failures remain bounded.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL because create failure is not re-listed as owned, readiness is not polled, and no retry occurs.

- [ ] **Step 3: Implement ownership reconciliation**

`add_owned_amika_sandbox` must avoid duplicates. `delete_owned_amika_sandbox` deletes only a name already in `CREATED_AMIKA_SANDBOXES`; it removes the name from the array only after delete succeeds and absence is confirmed.

After every create attempt, reload the sandbox table even when create failed. If the target now exists and preflight proved it was absent, add it to the owned array before deciding whether to retry. A delete failure stops retrying and leaves ownership recorded for EXIT cleanup/diagnostics.

- [ ] **Step 4: Implement readiness and bounded retry**

`wait_for_amika_sandbox_ready` polls the table until timeout:

- `failed` or missing after registration returns failure immediately;
- `started` triggers `amika sandbox ssh NAME -- true`;
- SSH failure continues polling;
- readiness success records elapsed seconds in `AMIKA_LIFECYCLE_LOG`.

`provision_amika_sandbox` loops from 1 through `AMIKA_CREATE_ATTEMPTS`. Failed attempts delete and wait for absence before the next exact-snapshot create. Exhaustion prints strategy, snapshot, attempt count, and last state; it must not mention a fallback.

Provision Locality and MCP sequentially. Then run `verify_amika_prerequisites`, using remote boolean checks that print no secret values. Preserve the current SSH-provider path.

- [ ] **Step 5: Verify GREEN and lifecycle cleanup**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
bash -n experiment/locality-mcp-comparison/run-agent-comparison.sh
git diff --check
```

Expected: all commands exit 0 and no diff-whitespace output appears.

- [ ] **Step 6: Commit provisioning reliability**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh
git commit -m "Retry Amika provisioning until ready"
```

---

### Task 3: Phase status, artifact retention, recovery, and documentation

**Files:**
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:911-1009,1091-1197`
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:192-668`
- Modify: `experiment/locality-mcp-comparison/README.md:49-91,199-274`

**Interfaces:**
- Consumes: owned sandbox array and lifecycle log from Tasks 1-2.
- Produces: `<local-dir>/<sandbox>/<strategy>-pipeline-status.env`, `read_pipeline_status FILE PREFIX`, `RETAIN_AMIKA_SANDBOXES`, and `print_amika_recovery_instructions`.

- [ ] **Step 1: Add failing artifact-outcome tests**

Extend fake rsync/scp with strategy-specific failure controls. Add cases that prove:

- benchmark rc 47 plus both sync rc 0 returns 47 and deletes both sandboxes;
- Locality sync rc 31 plus MCP sync rc 0 returns 31, does not call `sandbox delete`, and prints `Retaining Amika sandboxes because artifact sync failed` plus both sandbox names and remote output directories;
- both syncs fail, both sandboxes remain owned, and the first sync failure is returned after any earlier setup/benchmark success;
- `SYNC_ARTIFACTS=0` still deletes both with the existing warning;
- each atomic status file contains literal `setup_rc`, `benchmark_rc`, `sync_attempted`, and `sync_rc` values.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL because a pipeline exposes only one combined status and the EXIT trap always deletes sandboxes.

- [ ] **Step 3: Persist phase-specific pipeline status**

Refactor `run_strategy_pipeline` so it always atomically writes:

```text
setup_rc=<integer>
benchmark_rc=<integer>
sync_attempted=0|1
sync_rc=<integer>
```

Use a temporary file in the same directory followed by `mv`. Setup failure skips benchmark and sync. A reached benchmark always proceeds to sync. Function return precedence remains setup, benchmark, then sync.

After both `wait` calls, parse the trusted numeric records. If `SYNC_ARTIFACTS=1`, `sync_attempted=1`, and either `sync_rc` is nonzero, set `RETAIN_AMIKA_SANDBOXES=1` before returning from main flow.

- [ ] **Step 4: Implement retained recovery output**

The EXIT cleanup handler must skip deletion only when `RETAIN_AMIKA_SANDBOXES=1`. `print_amika_recovery_instructions` prints shell-quoted commands for:

```text
amika sandbox ssh <name>
rsync -az --delete "$(amika sandbox ssh --print <name>):<remote-out-dir>/" <local-artifact-dir>/
amika sandbox delete --remote --force <locality-name> <mcp-name>
```

Record retention, phase statuses, and commands in `run.env` or the lifecycle log. Never print credential values.

- [ ] **Step 5: Update operator documentation**

Document the reliability environment variables and defaults, exact-snapshot/no-fallback behavior, table-based v0.12.1 preflight, create/readiness retry semantics, successful-sync deletion versus failed-sync retention, recovery commands, and the explicit `full`-mode snapshot refresh sequence.

- [ ] **Step 6: Verify GREEN**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
bash -n experiment/locality-mcp-comparison/run-agent-comparison.sh
bash -n tests/launch_readiness_amika_split_wrapper.sh
git diff --check
```

Expected: all tests and syntax checks exit 0; `git diff --check` has no output.

- [ ] **Step 7: Commit recovery behavior and docs**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh experiment/locality-mcp-comparison/README.md
git commit -m "Retain Amika sandboxes after sync failure"
```

---

### Task 4: Refresh the exact Locality snapshot and run live acceptance

**Files:**
- Verify: `experiment/locality-mcp-comparison/run-agent-comparison.sh`
- Verify: `experiment/locality-mcp-comparison/README.md`
- Artifacts: `target/launch-readiness-amika/<run-id>/` (git-ignored)

**Interfaces:**
- Consumes: tested wrapper from Tasks 1-3 and Amika source sandbox `aseem-locality`.
- Produces: a newly validated exact `locality-snapshot`, a deleted candidate snapshot, and one locally synced paired benchmark run.

- [ ] **Step 1: Record source state and verify source health**

Capture the source state from `amika sandbox list --remote`. If stopped, run `amika sandbox start --remote aseem-locality`, poll for `started`, and then poll `amika sandbox ssh aseem-locality -- true`.

Run this read-only health check:

```bash
amika sandbox ssh aseem-locality -- bash -lc '
  set -e
  test -d "$HOME/workspace/locality"
  command -v loc >/dev/null
  command -v codex >/dev/null
  find "$HOME/Locality" "$HOME/notion" "$HOME/slack" "$HOME/linear" -maxdepth 2 -type f -print -quit 2>/dev/null | grep -q .
'
```

Expected: exit 0 before any snapshot mutation.

- [ ] **Step 2: Capture and fork-test a candidate snapshot**

Define concrete unique names from one refresh id:

```bash
refresh_id="$(date -u +%Y%m%dT%H%M%SZ)"
candidate="locality-snapshot-candidate-$refresh_id"
candidate_probe="locality-snapshot-candidate-probe-$refresh_id"
final_probe="locality-snapshot-final-probe-$refresh_id"
```

Capture and create the probe:

```bash
amika snapshot create --no-interactive --mode full --name "$candidate" --sandbox aseem-locality --description "Validated launch-readiness candidate $refresh_id"
amika sandbox create --remote --no-git --snapshot "$candidate" --name "$candidate_probe"
```

Use the wrapper's readiness/prerequisite logic manually against the candidate probe, then delete the probe. If this fails, retain the candidate for diagnosis and do not touch `locality-snapshot`.

- [ ] **Step 3: Replace and validate the exact snapshot**

Only after candidate success:

```bash
amika snapshot delete --force locality-snapshot
amika snapshot create --no-interactive --mode full --name locality-snapshot --sandbox aseem-locality --description "Validated launch-readiness snapshot $refresh_id"
amika sandbox create --remote --no-git --snapshot locality-snapshot --name "$final_probe"
```

Wait for the final probe and repeat the exact source health checks. Delete the final probe. Confirm `amika snapshot list` shows `locality-snapshot active`. Then delete the candidate:

```bash
amika snapshot delete --force "$candidate"
```

If final capture or fork validation fails, keep the verified candidate and report the exact-snapshot outage; do not configure a fallback.

- [ ] **Step 4: Restore source state**

If `aseem-locality` was initially stopped, run:

```bash
amika sandbox stop --remote aseem-locality
```

Confirm its final state matches the recorded initial state.

- [ ] **Step 5: Run one real paired benchmark scenario**

Use the local harness in the remote worktree:

```bash
live_run_id="reliability-$(date -u +%Y%m%dT%H%M%SZ)"
RUN_ID="$live_run_id" \
SYNC_LOCAL_EXPERIMENT=1 \
SYNC_ARTIFACTS=1 \
CODEX_EXEC_TIMEOUT_SECONDS=900 \
experiment/locality-mcp-comparison/run-agent-comparison.sh --scenario scenario1
```

Expected: both exact snapshots provision, reach SSH readiness, run concurrently, and sync artifacts.

- [ ] **Step 6: Verify live artifacts and cleanup**

For `target/launch-readiness-amika/$live_run_id`, verify both nonempty reports, both status files with `sync_attempted=1` and `sync_rc=0`, lifecycle log with only the exact snapshots, absence of both run-specific sandboxes, no candidate/probe remnants, and restored source state.

- [ ] **Step 7: Run final local regression verification**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
bash -n experiment/locality-mcp-comparison/run-agent-comparison.sh
bash -n tests/launch_readiness_amika_split_wrapper.sh
git diff --check
git status --short
```

Expected: all commands exit 0; only intentional committed changes exist.
