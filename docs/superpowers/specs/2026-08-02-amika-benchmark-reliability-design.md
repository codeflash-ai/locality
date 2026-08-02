# Amika Benchmark Reliability

## Goal

Make `experiment/locality-mcp-comparison/run-agent-comparison.sh` reliably
provision, use, recover, and clean up ephemeral Amika sandboxes while always
forking the exact snapshots `locality-snapshot` and `mcp-snapshot`.

## Observed failures

Live verification against Amika CLI v0.12.1 found four concrete failures:

1. `amika sandbox list --remote -o json` exits immediately because this CLI
   version has no `-o` or JSON output option.
2. A successful `sandbox create` can return before SSH exec is ready; an
   immediate probe failed with `exec request failed on channel 0`.
3. A failed snapshot fork can register a named sandbox in `failed` state even
   though `sandbox create` exits nonzero. The current wrapper records ownership
   only after a zero exit, so it can leak that failed sandbox.
4. The current `locality-snapshot` failed to fork twice with an Amika internal
   error. In the same session, `mcp-snapshot` and
   `locality-vm-prehydrated-full` both forked successfully, isolating the
   immediate environment problem to the exact Locality snapshot.

An expired remote token also demonstrated that `amika auth status` can report a
logged-in identity while remote API calls fail. The remote list call itself is
therefore the meaningful authentication preflight.

## Runtime architecture

The wrapper will use an explicit lifecycle state machine:

```text
remote preflight
  -> exact snapshot validation
  -> collision validation
  -> Locality provision/retry/readiness
  -> MCP provision/retry/readiness
  -> parallel benchmark pipelines
  -> artifact outcome evaluation
  -> delete or retain with recovery instructions
```

The generic `REMOTE_PROVIDER=ssh` path remains unchanged and bypasses all Amika
control-plane behavior.

### CLI compatibility adapter

The wrapper will use the table output supported by Amika v0.12.1:

- `amika sandbox list --remote`
- `amika snapshot list`

Small parsing helpers will return exact name/state pairs from the first two
columns after the header. Sandbox and snapshot names cannot contain spaces, so
this boundary is stable for the supported CLI contract. Tests will use the
same complete table structure as the real CLI.

The wrapper will not require or attempt `-o json`. A remote sandbox-list call
must succeed before any creation. Its failure is reported as an Amika auth or
control-plane preflight failure, preserving the original diagnostic.

The snapshot preflight requires both configured snapshot names to exist in
`active` state. Defaults remain exactly `locality-snapshot` and
`mcp-snapshot`. No fallback snapshot is permitted.

## Provisioning and readiness

Defaults:

- `AMIKA_CREATE_ATTEMPTS=3`
- `AMIKA_READINESS_TIMEOUT_SECONDS=180`
- `AMIKA_READINESS_POLL_SECONDS=3`

All three values may be overridden with positive integers for diagnostics.

For each strategy, the wrapper:

1. Confirms the target name was absent during the collision preflight.
2. Runs `amika sandbox create --remote --no-git --snapshot ... --name ...`.
3. Re-lists the target name regardless of the create exit status.
4. If a newly registered target now exists, records it as owned even when the
   create command failed.
5. On a failed create or `failed` state, deletes the owned target, waits until
   the name is absent, and retries while attempts remain.
6. On a successful create, polls until the state is `started` and
   `amika sandbox ssh <name> -- true` succeeds.
7. Treats `failed`, disappearance, or readiness timeout as an attempt failure,
   deletes the owned target, and retries.

Retries always use the same exact configured snapshot. They never retry or
re-run a Codex benchmark, because that is costly and not guaranteed to be
idempotent.

After readiness, narrow strategy prerequisites are checked without exposing
secret values: repository presence and Codex availability on both sandboxes,
`loc` and configured Locality roots on the Locality sandbox, and presence of
the MCP credential sources used by the worker on the MCP sandbox. A missing
prerequisite fails before benchmark execution and cleans up the ephemeral pair.

## Artifact collection and retention

Each background pipeline writes an atomic local status record containing its
setup, benchmark, and artifact-sync exit codes. The parent reads both records
after waiting for the pipelines.

Exit precedence remains:

1. setup or benchmark failure;
2. artifact-sync failure;
3. cleanup failure.

Cleanup policy:

- With `SYNC_ARTIFACTS=1`, delete both sandboxes only when both artifact syncs
  succeed.
- If either artifact sync fails, retain both sandboxes as a paired recovery
  environment, preserve the operation exit status, and print exact SSH, retry,
  remote artifact, and delete commands.
- A benchmark failure followed by successful artifact sync still deletes the
  pair and returns the benchmark failure.
- With explicit `SYNC_ARTIFACTS=0`, retain the existing behavior: warn that
  outputs will not be retained and delete the ephemeral pair.
- Provisioning or readiness failure cleans every sandbox registered by the
  current invocation.

`run.env` and a lifecycle log will record snapshot names, attempt numbers,
observed states, readiness duration, pipeline status codes, retained sandbox
names, and recovery commands. Secret values are never logged.

Only idempotent control-plane, readiness, and artifact-transfer operations may
be retried. Remote benchmark execution itself is single-shot.

## Locality snapshot refresh

Snapshot replacement is an explicit rollout operation, not automatic benchmark
behavior.

The refresh procedure will:

1. Record whether `aseem-locality` was initially stopped or started.
2. Start it if necessary and wait for SSH readiness.
3. Verify `~/workspace/locality`, `loc`, Codex, and the expected hydrated
   Locality roots without changing their contents.
4. Capture a uniquely named `full` candidate snapshot. `full` is used because
   `scrub_and_delete` would remove injected state and delete the source
   sandbox.
5. Fork the candidate into a probe sandbox, wait for readiness, repeat the
   prerequisite checks, and delete the probe.
6. Delete the broken `locality-snapshot` only after the candidate passes.
7. Capture a new `full` snapshot under the exact name `locality-snapshot`.
8. Fork and validate the final exact snapshot, then delete its probe.
9. Delete the candidate only after the final exact snapshot passes.
10. Restore `aseem-locality` to its original stopped/started state.

If final replacement fails, the verified candidate remains available for
recovery, but the benchmark still refuses to use it as a fallback.

## Error reporting

Errors identify the lifecycle phase, strategy, attempt, sandbox or snapshot
name, last observed state, command exit status, and path to local logs.

Retry exhaustion for `locality-snapshot` explicitly says that the exact
snapshot could not be forked and must be repaired; it does not recommend or
select another snapshot.

When sandboxes are retained, the final message begins with a clear retention
notice and lists commands to:

- SSH to each sandbox;
- inspect its remote artifact directory;
- rerun artifact transfer into the existing local output directory; and
- delete both sandboxes after recovery.

## Tests and live acceptance

The shell integration test will use a stateful fake Amika boundary to cover:

- v0.12.1 table output and the absence of JSON flags;
- remote auth/control-plane preflight failure;
- missing and non-active exact snapshots;
- collision rejection before creation;
- `initializing` to `started` state transitions;
- a started sandbox whose SSH endpoint is not ready yet;
- nonzero create that leaves a `failed` owned sandbox;
- deletion, absence wait, retry success, and retry exhaustion;
- prerequisite failure before benchmark execution;
- successful artifact sync followed by deletion;
- benchmark failure plus successful sync followed by deletion;
- artifact-sync failure followed by paired retention and recovery commands;
- explicit no-sync behavior; and
- unchanged generic SSH-provider behavior.

The snapshot refresh will be verified through the real Amika product path with
candidate and final fork probes. Final acceptance requires one real paired
benchmark scenario using the exact `locality-snapshot` and `mcp-snapshot`, both
artifact trees synced locally, expected reports present, and both ephemeral
sandboxes absent afterward.
