# Ephemeral Amika Benchmark Sandboxes

## Goal

The launch-readiness comparison wrapper must run each Amika benchmark in two
fresh sandboxes forked from known snapshots, then delete those sandboxes after
artifact collection. This prevents one benchmark run from inheriting mutable
state from an earlier run.

## Scope

This change applies to
`experiment/locality-mcp-comparison/run-agent-comparison.sh` when
`REMOTE_PROVIDER=amika`. The existing generic SSH-provider path remains
unchanged.

## Sandbox ownership and naming

The wrapper assigns deterministic, run-specific names:

- Locality: `launch-readiness-$RUN_ID-locality`
- MCP: `launch-readiness-$RUN_ID-mcp`

`LOCALITY_SANDBOX` and `MCP_SANDBOX` may override those names. Snapshot slugs
default to `locality-snapshot` and `mcp-snapshot` and may be overridden with
`LOCALITY_SNAPSHOT` and `MCP_SNAPSHOT`.

Before creation, the wrapper lists existing Amika sandboxes and fails if either
target name already exists. It never adopts, starts, deletes, or otherwise
mutates an existing sandbox. This ownership check makes later automatic
deletion safe.

## Creation and execution flow

For Amika runs, the wrapper creates each sandbox with the equivalent of:

```text
amika sandbox create --remote --no-git --snapshot <snapshot> --name <name>
```

The wrapper records successful creation of each sandbox independently. This
allows cleanup of the first sandbox if creation of the second fails. Once both
exist, the existing Locality and MCP pipelines prepare detached worktrees and
run concurrently as before.

The snapshot supplies the baseline machine state. `--no-git` prevents Amika
from implicitly mounting or initializing the caller's local repository; the
benchmark continues to prepare its own remote worktree.

## Artifact collection and cleanup

Each strategy pipeline preserves the status of its benchmark work while still
attempting its configured artifact sync. After both pipelines finish, the
wrapper generates local reports when possible and then deletes the sandboxes it
created:

```text
amika sandbox delete --remote --force <locality-name> <mcp-name>
```

Cleanup preserves the benchmark's original exit status. An EXIT and signal
cleanup path deletes any sandbox created by the current invocation if setup,
execution, artifact sync, or report generation fails. Cleanup is idempotent and
only includes names whose create commands succeeded.

`SYNC_ARTIFACTS=0` remains supported as an explicit opt-out. The ephemeral
sandboxes are still deleted, and the wrapper warns that remote benchmark
artifacts will not be retained.

The generic SSH-provider path neither creates nor deletes remote instances.

## Metadata and documentation

The wrapper usage and experiment README document the ephemeral lifecycle,
default snapshot slugs, run-specific names, overrides, and the consequence of
`SYNC_ARTIFACTS=0`. `run.env` records both snapshot slugs in Amika mode so a
completed run can be traced to its source environments.

## Error handling

- Existing target name: fail before creating either sandbox.
- First create fails: return that failure; there is nothing to delete.
- Second create fails: delete the successfully created first sandbox and return
  the second create failure.
- Benchmark or artifact sync fails: attempt the other pipeline's completion,
  delete both created sandboxes, and return a pipeline failure.
- Deletion fails after otherwise successful work: return a cleanup failure so
  leaked infrastructure is visible.
- Deletion fails while another operation is already failing: preserve the
  original failure and print the cleanup failure.
- INT or TERM: stop active child pipelines, delete created sandboxes, and return
  the signal-related failure.

## Tests

The split-wrapper shell test will use its fake `amika` executable to verify:

- default names are run-specific;
- the Locality and MCP create commands use `locality-snapshot` and
  `mcp-snapshot`, respectively;
- custom names and snapshot slugs are honored;
- neither pre-existing names nor the old fixed sandboxes are reused;
- strategy pipelines still overlap;
- artifact activity precedes deletion;
- both owned sandboxes are deleted after success;
- a partially created pair is cleaned up when the second create fails;
- existing-name collisions fail without creation or deletion; and
- the explicit no-sync mode warns before deleting ephemeral sandboxes.

Existing benchmark runner tests remain unchanged except where their wrapper
expectations refer to the old fixed sandboxes.
