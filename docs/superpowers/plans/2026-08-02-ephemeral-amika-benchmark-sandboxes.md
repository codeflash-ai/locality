# Ephemeral Amika Benchmark Sandboxes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make each Amika launch-readiness comparison fork fresh Locality and MCP sandboxes from `locality-snapshot` and `mcp-snapshot`, collect artifacts, and delete only those run-owned sandboxes.

**Architecture:** Keep orchestration in the existing Bash wrapper. Replace fixed-sandbox startup with an ownership-checked create/delete lifecycle tracked in an in-memory array, install cleanup traps before creation, and preserve pipeline failures while still attempting artifact sync. Leave the generic SSH provider untouched.

**Tech Stack:** Bash 3-compatible shell patterns, Python 3 for JSON parsing already used by the wrapper, fake-command shell integration tests, Amika CLI.

## Global Constraints

- Default names are exactly `launch-readiness-$RUN_ID-locality` and `launch-readiness-$RUN_ID-mcp`.
- Default snapshot slugs are exactly `locality-snapshot` and `mcp-snapshot`.
- Existing target names must cause failure before either sandbox is created.
- Cleanup may delete only sandbox names whose create commands succeeded in the current invocation.
- Amika sandboxes are deleted after artifact collection, including when setup or execution fails.
- `SYNC_ARTIFACTS=0` still deletes ephemeral sandboxes and emits a data-loss warning.
- The `REMOTE_PROVIDER=ssh` path must not create or delete instances.
- Preserve the original operation failure if cleanup also fails; surface cleanup failure when all earlier work succeeded.

---

### Task 1: Fresh sandbox happy-path lifecycle

**Files:**
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:12-166`
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:12-108,367-407,969-1027`

**Interfaces:**
- Consumes: Amika CLI commands `sandbox list`, `sandbox create`, `sandbox ssh`, and `sandbox delete`.
- Produces: `create_amika_sandboxes()` and `cleanup_amika_sandboxes()` Bash functions; `CREATED_AMIKA_SANDBOXES` contains only successfully created names.

- [ ] **Step 1: Change the fake Amika boundary and happy-path assertions**

Teach the fake `amika` command to return `FAKE_AMIKA_LIST_JSON` for `sandbox list`, accept `sandbox create` and `sandbox delete`, and retain `sandbox ssh`. Replace the old fixed-sandbox expectations with literal behavior expectations:

```bash
assert_contains "$run_default_out/run.env" "locality_sandbox=launch-readiness-testrun-locality"
assert_contains "$run_default_out/run.env" "mcp_sandbox=launch-readiness-testrun-mcp"
assert_contains "$run_default_out/run.env" "locality_snapshot=locality-snapshot"
assert_contains "$run_default_out/run.env" "mcp_snapshot=mcp-snapshot"
assert_contains "$fake_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-testrun-locality"
assert_contains "$fake_log" "amika sandbox create --remote --no-git --snapshot mcp-snapshot --name launch-readiness-testrun-mcp"
assert_contains "$fake_log" "amika sandbox delete --remote --force launch-readiness-testrun-locality launch-readiness-testrun-mcp"
assert_not_contains "$fake_log" "amika sandbox start"
```

Add an `assert_line_before FILE FIRST SECOND` helper using literal `grep -nF` matches, then verify both create lines precede the first strategy SSH operation and the final strategy SSH operation precedes delete. Keep the existing overlap assertions so the test still protects parallel execution.

- [ ] **Step 2: Run the wrapper test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL because `run.env` still records `aseem-locality`/`aseem-mcp` and the wrapper issues `sandbox start` instead of snapshot-backed `sandbox create` and `sandbox delete`.

- [ ] **Step 3: Implement owned sandbox creation and normal cleanup**

Set the new defaults after `RUN_ID` is known:

```bash
LOCALITY_SANDBOX="${LOCALITY_SANDBOX:-launch-readiness-$RUN_ID-locality}"
MCP_SANDBOX="${MCP_SANDBOX:-launch-readiness-$RUN_ID-mcp}"
LOCALITY_SNAPSHOT="${LOCALITY_SNAPSHOT:-locality-snapshot}"
MCP_SNAPSHOT="${MCP_SNAPSHOT:-mcp-snapshot}"
declare -a CREATED_AMIKA_SANDBOXES=()
```

Replace `ensure_amika_sandboxes_started()` with `create_amika_sandboxes()`. It must call `amika sandbox list --remote -o json`, parse the complete JSON once with Python, fail if either requested name is present, and then run these commands sequentially:

```bash
amika sandbox create --remote --no-git --snapshot "$LOCALITY_SNAPSHOT" --name "$LOCALITY_SANDBOX"
CREATED_AMIKA_SANDBOXES+=("$LOCALITY_SANDBOX")
amika sandbox create --remote --no-git --snapshot "$MCP_SNAPSHOT" --name "$MCP_SANDBOX"
CREATED_AMIKA_SANDBOXES+=("$MCP_SANDBOX")
```

Implement `cleanup_amika_sandboxes()` to no-op outside Amika mode or for an empty array, otherwise invoke one exact deletion command and clear the array only on success:

```bash
amika sandbox delete --remote --force "${CREATED_AMIKA_SANDBOXES[@]}"
CREATED_AMIKA_SANDBOXES=()
```

Install an EXIT handler before calling `create_amika_sandboxes()`. The handler captures `$?`, disables its own traps, calls cleanup with `set +e`, and exits with the original nonzero status or, when the original status is zero, the cleanup status. Let normal script completion trigger this handler after report generation.

- [ ] **Step 4: Record snapshot metadata and verify GREEN**

In the Amika branch of `run.env`, write:

```bash
printf 'locality_snapshot=%s\n' "$LOCALITY_SNAPSHOT"
printf 'mcp_snapshot=%s\n' "$MCP_SNAPSHOT"
```

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: PASS, including parallel-pipeline and create-before-delete assertions.

- [ ] **Step 5: Commit the happy path**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh
git commit -m "Spawn ephemeral Amika benchmark sandboxes"
```

---

### Task 2: Failure-safe collection, collision protection, and cleanup

**Files:**
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:36-211`
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:367-407,943-980,1021-1044`

**Interfaces:**
- Consumes: `create_amika_sandboxes()` and `cleanup_amika_sandboxes()` from Task 1.
- Produces: `run_strategy_pipeline()` that returns the benchmark/setup failure preferentially but calls `sync_artifacts()` after any reached benchmark invocation; signal handlers exit through the shared EXIT cleanup path.

- [ ] **Step 1: Add collision, partial-create, no-sync, and cleanup-failure cases**

Extend the fake Amika script with explicit environment-controlled failures:

```bash
if [ "${2:-}" = "create" ] && [[ " $* " == *" --snapshot ${FAKE_AMIKA_FAIL_CREATE_SNAPSHOT:-__never__} "* ]]; then
  exit "${FAKE_AMIKA_FAIL_CREATE_RC:-23}"
fi
if [ "${2:-}" = "delete" ] && [ "${FAKE_AMIKA_FAIL_DELETE:-0}" = "1" ]; then
  exit 29
fi
```

Add separate invocations and literal assertions for these breaks:

1. `FAKE_AMIKA_LIST_JSON` contains `launch-readiness-collision-locality`: wrapper exits nonzero and the log has neither `sandbox create` nor `sandbox delete`.
2. `FAKE_AMIKA_FAIL_CREATE_SNAPSHOT=mcp-snapshot`: wrapper returns 23 and deletes only `launch-readiness-partial-locality`.
3. `SYNC_ARTIFACTS=0`: stderr contains `SYNC_ARTIFACTS=0; ephemeral Amika sandboxes will be deleted without retaining remote artifacts` and both owned sandboxes are deleted.
4. `FAKE_AMIKA_FAIL_DELETE=1` after successful pipelines: wrapper returns 29.
5. Custom names and `LOCALITY_SNAPSHOT=custom-locality-snapshot`, `MCP_SNAPSHOT=custom-mcp-snapshot`: create and delete commands use all four custom literals.

- [ ] **Step 2: Run the wrapper test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL first on the unimplemented collision/partial-create or cleanup exit-status behavior.

- [ ] **Step 3: Preserve pipeline status while attempting artifact sync**

Change `run_strategy_pipeline()` so setup failures return immediately, while a reached benchmark always proceeds to sync:

```bash
prepare_worktree "$sandbox" || return $?
sync_local_experiment "$sandbox" || return $?
local run_rc=0
local sync_rc=0
run_launch_strategy_with_args "$sandbox" "$strategy" "$remote_out_dir" || run_rc=$?
sync_artifacts "$sandbox" "$strategy" "$remote_out_dir" || sync_rc=$?
if [ "$run_rc" -ne 0 ]; then
  return "$run_rc"
fi
return "$sync_rc"
```

Before sandbox creation, print the exact no-sync warning when `SYNC_ARTIFACTS=0`. Ensure collision detection happens before the first create command. Keep each successful name append immediately after its create command so the EXIT trap handles partial creation.

- [ ] **Step 4: Route signals and all exits through cleanup**

Replace the shared INT/TERM handler with a status-aware handler:

```bash
stop_strategy_pipelines() {
  local signal_rc="$1"
  local pid
  trap - INT TERM
  for pid in "${locality_pipeline_pid:-}" "${mcp_pipeline_pid:-}"; do
    if [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1; then
      kill "$pid" >/dev/null 2>&1 || true
    fi
  done
  exit "$signal_rc"
}
trap 'stop_strategy_pipelines 130' INT
trap 'stop_strategy_pipelines 143' TERM
```

The subsequent EXIT trap performs owned-sandbox deletion. Confirm its status rule is: original nonzero status wins over delete failure; otherwise delete failure becomes the wrapper status.

- [ ] **Step 5: Run focused and surrounding tests**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
```

Expected: all three print their `tests passed` line and exit 0.

- [ ] **Step 6: Commit failure handling**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh
git commit -m "Clean up failed Amika benchmark runs"
```

---

### Task 3: User-facing contract and final verification

**Files:**
- Modify: `experiment/locality-mcp-comparison/run-agent-comparison.sh:4-70`
- Modify: `experiment/locality-mcp-comparison/README.md:19-28,49-78,149-166,249-263`
- Modify: `tests/launch_readiness_amika_split_wrapper.sh:12-211`

**Interfaces:**
- Consumes: environment variables and behavior implemented in Tasks 1-2.
- Produces: accurate `--help` output and operator documentation for ephemeral Amika runs.

- [ ] **Step 1: Add a failing help-contract assertion**

In `tests/launch_readiness_amika_split_wrapper.sh`, capture `"$WRAPPER" --help` and assert it describes both default snapshots and automatic deletion:

```bash
help_out="${tmp_root}/help.out"
"$WRAPPER" --help > "$help_out"
assert_contains "$help_out" "LOCALITY_SNAPSHOT=locality-snapshot"
assert_contains "$help_out" "MCP_SNAPSHOT=mcp-snapshot"
assert_contains "$help_out" "Created Amika sandboxes are deleted automatically after artifact collection."
```

- [ ] **Step 2: Run the help test and verify RED**

Run: `bash tests/launch_readiness_amika_split_wrapper.sh`

Expected: FAIL because the old usage text still advertises `aseem-locality` and `aseem-mcp` and omits snapshot/cleanup behavior.

- [ ] **Step 3: Update usage and README**

Change wrapper usage to show the exact per-run name defaults, both snapshot variables, automatic cleanup, collision behavior, and the `SYNC_ARTIFACTS=0` warning. In the README:

- replace instructions to prepare/reuse `aseem-locality` and `aseem-mcp` with snapshot prerequisites;
- document the two default snapshot slugs and run-specific sandbox names;
- explain that overrides name newly created sandboxes rather than selecting reusable ones;
- state that cleanup occurs after artifact collection and on failures/signals;
- state that `SYNC_ARTIFACTS=0` intentionally discards remote-only outputs because sandboxes remain ephemeral.

- [ ] **Step 4: Run all launch-readiness shell tests and static checks**

Run:

```bash
bash tests/launch_readiness_amika_split_wrapper.sh
bash tests/launch_readiness_mcp_config.sh
bash tests/launch_readiness_prompt_paths.sh
bash -n experiment/locality-mcp-comparison/run-agent-comparison.sh
bash -n tests/launch_readiness_amika_split_wrapper.sh
git diff --check
```

Expected: all tests exit 0, both syntax checks exit 0, and `git diff --check` emits no output.

- [ ] **Step 5: Review the requirement diff**

Run:

```bash
git diff -- experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh experiment/locality-mcp-comparison/README.md
```

Confirm the diff contains exactly: snapshot-backed create, collision refusal, owned cleanup, pipeline sync-on-failure, metadata/help/docs, and tests; it must contain no fixed `aseem-locality`/`aseem-mcp` defaults and no Amika `sandbox start` call.

- [ ] **Step 6: Commit documentation and final test contract**

```bash
git add experiment/locality-mcp-comparison/run-agent-comparison.sh tests/launch_readiness_amika_split_wrapper.sh experiment/locality-mcp-comparison/README.md
git commit -m "Document ephemeral benchmark environments"
```
