# Agent Conversation Profile Script

`experiment/agent-conversation-profile.mjs` compares two Claude or Codex
conversation exports and writes Perfetto-readable traces, SnakeViz profiles,
Speedscope profiles, folded-stack files, and summary reports.
It is intended for local investigation of where an agent run spent time:
reasoning, assistant text, tool calls, tool results, and unsupported or unknown
records.

`experiment/agent-conversation-profile-modern-codex.mjs` is a compatibility
variant for newer Codex JSONL streams. It keeps the same CLI and output shape,
but additionally recognizes Codex `agent_message`, `command_execution`,
`mcp_tool_call`, `file_change`, live harness `harness.phase` records, and
turn/thread lifecycle records. Use it for recent `codex exec --json` captures
when the original profiler reports most of the run as `unknown`, double-counts
started/completed MCP calls, or needs to consume live Codex hook timings.

## Usage

```bash
node experiment/agent-conversation-profile.mjs \
  --left claude.jsonl --left-label claude \
  --right codex.jsonl --right-label codex \
  --out target/agent-profiles/run-1
```

For newer Codex event streams:

```bash
node experiment/agent-conversation-profile-modern-codex.mjs \
  --left locality-codex-events.jsonl --left-label locality \
  --right notion-mcp-codex-events.jsonl --right-label notion-mcp \
  --out target/agent-profiles/modern-codex-run-1
```

The script accepts JSONL, JSON arrays, or JSON objects containing nested event
arrays. It does not discover local Claude or Codex history files; pass explicit
export paths so the run is deterministic and auditable.

## Outputs

The output directory contains:

- `combined.perfetto.json`: one combined Chrome trace JSON file with one track
  per profile row, including conversation, activity, tool group, and command
  group.
- `<left-label>.perfetto.json`: split trace for the left conversation, with one
  track per activity or tool command group.
- `<right-label>.perfetto.json`: split trace for the right conversation, with
  one track per activity or tool command group.
- `combined.snakeviz.prof`: combined synthetic Python pstats profile for
  SnakeViz.
- `<left-label>.snakeviz.prof`: SnakeViz profile for the left conversation.
- `<right-label>.snakeviz.prof`: SnakeViz profile for the right conversation.
- `combined.snakeviz.stats.md`: SnakeViz-style stats table for the combined
  synthetic profile.
- `<left-label>.snakeviz.stats.md`: SnakeViz-style stats table for the left
  conversation.
- `<right-label>.snakeviz.stats.md`: SnakeViz-style stats table for the right
  conversation.
- `combined.speedscope.json`: combined duration-weighted Speedscope profile.
- `<left-label>.speedscope.json`: Speedscope profile for the left conversation.
- `<right-label>.speedscope.json`: Speedscope profile for the right
  conversation.
- `combined.folded`: combined folded-stack file for FlameGraph.
- `<left-label>.folded`: folded-stack file for the left conversation.
- `<right-label>.folded`: folded-stack file for the right conversation.
- `summary.json`: machine-readable totals by kind, tool, timing quality, and
  longest events. It also includes high-level `totals_by_activity`,
  `tool_groups`, `tool_commands`, and excluded `metadata`.
- `summary.md`: human-readable comparison tables.

Open any `*.perfetto.json` file in Perfetto or another Chrome trace viewer.
Open Speedscope profiles with:

```bash
speedscope <file>.speedscope.json
```

Open SnakeViz profiles with:

```bash
snakeviz <file>.snakeviz.prof
```

Open the adjacent `*.snakeviz.stats.md` file for a sortable-source text view of
the same synthetic pstats frames using cProfile-style columns: `ncalls`,
`tottime`, `percall`, `cumtime`, `percall`, frame, and callers. The table also
includes a `Tool Command Breakdown` section so loc commands appear as
`tool_group=loc` with subcommands such as `push`, `status`, and `create-page`.

Render folded stacks with Brendan Gregg's FlameGraph tooling:

```bash
flamegraph.pl --countname=us <file>.folded > <file>.svg
```

If two labels sanitize to the same filename, the split trace, SnakeViz,
Speedscope, and folded-stack filenames are deconflicted with their input side.
The exact paths are listed in `summary.json` under `outputs.split`,
`outputs.snakeviz`, `outputs.snakeviz_stats`, `outputs.speedscope`, and
`outputs.flamegraph`.

## Activity Model

Viewer profiles and the `Time By Activity` summary use derived activity buckets
that are meant for high-level agent timing:

- `tool`: time spent running an agent action until it completes. This is
  derived from live hook phases when present, otherwise from tool-call and
  file-change records.
- `reasoning`: reasoning or thinking records. In hook-instrumented traces, this
  is the measured model span from prompt/result handoff until the next tool
  call. In raw Codex traces without hook phases, this is inferred from
  turn-start-to-first-item gaps and tool-result or file-change-result gaps until
  the next Codex item.
- `user_query`: user-message records. In hook-instrumented traces, this comes
  from `UserPromptSubmit` and the session-start boundary.
- `agent_response`: assistant text response records. In hook-instrumented
  traces, this comes from the final model span ending at `Stop`.
- `system`: system-message records.
- `other`: non-metadata records that do not fit the activity model.

Tool activity uses a two-level stack hierarchy. The first frame is the broad
tool group: `loc` for shell calls that invoke the Locality CLI, or `non_loc` for
everything else. The second frame is the command detail. For `loc`, that detail
is the loc subcommand, for example `status`, `diff`, `push`, or `create-page`.
Shell commands that run more than one loc subcommand join the subcommands with
`+`, for example `diff+status`. For non-loc shell calls, the detail is the
executable name such as `git`, `gh`, `sed`, or `find`; for non-shell tools, it is
the tool name such as `list_issues` or `API-post-search`. Flattened viewer rows
include the tool group in the command frame, for example
`command:loc:diff+status` or `command:non_loc:git`.

Harness metadata records, such as Claude terminal `result:success` records,
`system:turn_duration`, `system:local_command`, attachments, file-history
deltas, and raw `harness.hook` summaries, are excluded from Perfetto traces,
viewer profiles, and `totals_by_activity` because they can duplicate elapsed
time that is already represented by user, assistant, reasoning, and tool spans.
They are reported separately under `metadata` and in the Markdown
`Excluded Metadata` section.

## Timing Model

Timestamps are read from `timestamp`, `created_at`, `time`, or `ts`. Numeric
timestamps below `1e12` are treated as seconds; larger numeric timestamps are
treated as milliseconds; ISO strings are parsed directly.

Durations are exact only when the source record includes an explicit duration
field such as `duration_ms`, `durationMs`, `elapsed_ms`, or `latency_ms`. When a
record has a start timestamp but no duration, the script infers the end from
the next timestamp in the same conversation. If there is no next timestamp, it
uses `--default-duration-ms` or `1000ms`.

For the Codex launch-readiness harness, live Codex hooks write `harness.phase`
records during the running session. The modern profiler prefers those measured
phase records for any activity bucket they cover and leaves the underlying raw
Codex stream records in the audit-oriented kind and metadata summaries.

Top-level wall time is computed from non-metadata event spans when any are
present. Metadata timestamps can still act as run-boundary markers, but metadata
durations do not extend wall time; this keeps terminal aggregate records from
doubling the apparent runtime.

Every trace slice and summary entry carries `timing_quality` as `measured` or
`inferred`, but timing quality is not included as a Speedscope, SnakeViz, or
folded-stack frame. Treat inferred reasoning time as a useful approximation, not
ground truth. Raw `Time By Kind` totals still include metadata durations for
auditability and therefore may not sum to wall time.
