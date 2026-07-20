# Agent Conversation Profile Script

`experiment/agent-conversation-profile.mjs` compares two Claude or Codex
conversation exports and writes Perfetto-readable traces plus summary reports.
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
- `summary.json`: machine-readable totals by kind, tool, timing quality, and
  longest events.
- `summary.md`: human-readable comparison tables.

Open any `*.perfetto.json` file in Perfetto or another Chrome trace viewer.
If two labels sanitize to the same filename, the split trace filenames are
deconflicted with their input side and the exact paths are listed in
`summary.json`.

## Timing Model

Timestamps are read from `timestamp`, `created_at`, `time`, or `ts`. Numeric
timestamps below `1e12` are treated as seconds; larger numeric timestamps are
treated as milliseconds; ISO strings are parsed directly.

Durations are exact only when the source record includes an explicit duration
field such as `duration_ms`, `durationMs`, `elapsed_ms`, or `latency_ms`. When a
record has a start timestamp but no duration, the script infers the end from
the next timestamp in the same conversation. If there is no next timestamp, it
uses `--default-duration-ms` or `1000ms`.

Every trace slice and summary bucket carries `timing_quality` as `measured` or
`inferred`. Treat inferred reasoning time as a useful approximation, not ground
truth.
