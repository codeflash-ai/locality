# Agent Conversation Profile Script

`experiment/agent-conversation-profile.mjs` compares two Claude or Codex
conversation exports and writes Perfetto-readable traces, SnakeViz profiles,
Speedscope profiles, folded-stack files, and summary reports.
It is intended for local investigation of where an agent run spent time:
reasoning, assistant text, tool calls, tool results, and unsupported or unknown
records.

## Usage

```bash
node experiment/agent-conversation-profile.mjs \
  --left claude.jsonl --left-label claude \
  --right codex.jsonl --right-label codex \
  --out target/agent-profiles/run-1
```

The script accepts JSONL, JSON arrays, or JSON objects containing nested event
arrays. It does not discover local Claude or Codex history files; pass explicit
export paths so the run is deterministic and auditable.

## Outputs

The output directory contains:

- `combined.perfetto.json`: one combined Chrome trace JSON file with one track
  per conversation.
- `<left-label>.perfetto.json`: split trace for the left conversation.
- `<right-label>.perfetto.json`: split trace for the right conversation.
- `combined.snakeviz.prof`: combined synthetic Python pstats profile for
  SnakeViz.
- `<left-label>.snakeviz.prof`: SnakeViz profile for the left conversation.
- `<right-label>.snakeviz.prof`: SnakeViz profile for the right conversation.
- `combined.speedscope.json`: combined duration-weighted Speedscope profile.
- `<left-label>.speedscope.json`: Speedscope profile for the left conversation.
- `<right-label>.speedscope.json`: Speedscope profile for the right
  conversation.
- `combined.folded`: combined folded-stack file for FlameGraph.
- `<left-label>.folded`: folded-stack file for the left conversation.
- `<right-label>.folded`: folded-stack file for the right conversation.
- `summary.json`: machine-readable totals by kind, tool, timing quality, and
  longest events. It also includes high-level `totals_by_activity`,
  `tool_groups`, and excluded `metadata`.
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

Render folded stacks with Brendan Gregg's FlameGraph tooling:

```bash
flamegraph.pl --countname=us <file>.folded > <file>.svg
```

If two labels sanitize to the same filename, the split trace, SnakeViz,
Speedscope, and folded-stack filenames are deconflicted with their input side.
The exact paths are listed in `summary.json` under `outputs.split`,
`outputs.snakeviz`, `outputs.speedscope`, and `outputs.flamegraph`.

## Activity Model

Viewer profiles and the `Time By Activity` summary use derived activity buckets
that are meant for high-level agent timing:

- `tool`: time spent calling and waiting for a tool call to finish. This is
  derived from tool-call records and, when a matching result exists, spans from
  the call timestamp to the result timestamp. Tool-result records remain in the
  raw kind summary but are not counted as separate tool activity.
- `reasoning`: reasoning or thinking records.
- `user_query`: user-message records.
- `agent_response`: assistant text response records.
- `system`: system-message records.
- `other`: non-metadata records that do not fit the activity model.

Tool activity is additionally grouped in `tool_groups`. Bash calls whose shell
segment invokes a `loc` executable are reported as `bash_loc`; other Bash calls
are reported as `bash_other`; non-Bash tools keep their tool name.

Harness metadata records, such as Claude terminal `result:success` records,
`system:turn_duration`, `system:local_command`, attachments, file-history
deltas, and hook summaries, are excluded from Perfetto traces, viewer profiles,
and `totals_by_activity` because they can duplicate elapsed time that is already
represented by user, assistant, reasoning, and tool spans. They are reported
separately under `metadata` and in the Markdown `Excluded Metadata` section.

## Timing Model

Timestamps are read from `timestamp`, `created_at`, `time`, or `ts`. Numeric
timestamps below `1e12` are treated as seconds; larger numeric timestamps are
treated as milliseconds; ISO strings are parsed directly.

Durations are exact only when the source record includes an explicit duration
field such as `duration_ms`, `durationMs`, `elapsed_ms`, or `latency_ms`. When a
record has a start timestamp but no duration, the script infers the end from
the next timestamp in the same conversation. If there is no next timestamp, it
uses `--default-duration-ms` or `1000ms`.

Top-level wall time is computed from non-metadata event spans when any are
present. Metadata timestamps can still act as run-boundary markers, but metadata
durations do not extend wall time; this keeps terminal aggregate records from
doubling the apparent runtime.

Every trace slice and summary bucket carries `timing_quality` as `measured` or
`inferred`. Treat inferred reasoning time as a useful approximation, not ground
truth. Raw `Time By Kind` totals still include metadata durations for auditability
and therefore may not sum to wall time.
