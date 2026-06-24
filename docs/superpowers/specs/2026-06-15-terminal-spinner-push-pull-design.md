# Terminal Spinner for Push and Pull

## Context

`loc pull` and `loc push` can spend noticeable time waiting on the daemon,
falling back to direct connector execution, hydrating content, or applying remote
changes. Today the CLI is silent until the command completes, which makes a
human terminal session look stalled.

The CLI already supports `--json` for machine-readable output. That output must
remain clean and must not include transient loading frames.

## Goals

- Show a lightweight loading state while `loc pull` or `loc push` work is in
  progress.
- Suppress the loading state for `--json`.
- Keep stdout stable for normal reports and JSON output.
- Keep the change scoped to the CLI command surface.

## Non-Goals

- Do not add daemon-side progress events.
- Do not expose detailed per-stage progress.
- Do not change push or pull execution semantics.
- Do not merge this worktree into the main worktree.

## Design

Add a small internal spinner helper in the CLI crate. The helper starts a
background thread that periodically writes a spinner frame and label to stderr,
then clears the line when dropped. The command code owns the spinner for the
duration of each blocking push or pull operation.

The spinner is enabled only when both conditions are true:

- the command is not running with `--json`;
- stderr is an interactive terminal.

This preserves JSON output and avoids control characters in redirected logs.
Spinner frames go to stderr so stdout remains reserved for command reports.

`loc pull <path>` will show a single spinner around daemon execution and, when
needed, direct fallback execution. The label will be generic, for example
`pulling <path>`.

`loc push <path>` will show a spinner around each target push execution. For a
single file, that means one spinner. For a scoped directory push, each selected
target gets its own spinner using a label like `pushing <path>`. Existing
confirmation prompts still happen without a spinner active.

## Error Handling

The spinner helper should be best-effort. If writing to stderr fails, command
execution should continue. Dropping the spinner must stop the background thread
before the command prints its final report or error.

## Testing

Add unit coverage for the spinner enablement policy:

- enabled for non-JSON interactive stderr;
- disabled for `--json`;
- disabled for non-interactive stderr.

Add a focused behavior test for command wiring where practical, using a
test-friendly spinner configuration so tests do not depend on wall-clock timing
or real terminal state.

Run the relevant CLI tests first, then the full workspace test suite.
