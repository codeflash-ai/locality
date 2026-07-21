# Local Diagnostics Log Strategy

This is the working plan for a local-first diagnostics trail that helps users and
support debug Locality without leaking workspace data or slowing normal sync.

## Goals

- Give users one clear timeline for desktop actions, daemon lifecycle, provider
  events, sync jobs, push reviews, auto-save decisions, and connector failures.
- Keep logs local by default. Nothing is uploaded unless the user explicitly
  exports a support bundle.
- Make the bundle useful for future SAO work: support, audit, and observability.
- Preserve the current product feel: normal users see health, pending work, and
  recent activity; raw logs stay behind diagnostics.
- Never store bearer tokens, OAuth secrets, Notion page bodies, Markdown file
  contents, or credential references that can be decoded into secrets.

## Event Sources

Locality already has partial activity and journal data. The next reliable shape is
to normalize these sources into ordered local events:

| Source | Examples | Storage now | Target |
| --- | --- | --- | --- |
| Desktop app | connect, change access, create mount, repair, open folder, user-triggered push | `desktop-activity.json` | rolling JSONL plus UI activity projection |
| Daemon | start, stop, reload mounts, socket ping, hydration queue, scheduled pull | `~/.loc/logs/localityd.log` | structured JSONL with source sequence |
| Push journal | planned ops, confirmation boundary, apply success/failure | SQLite journals | redacted summary events linked by journal id |
| File provider | provider registration, materialization, unavailable states | platform-specific logs | normalized provider lifecycle events |
| Connector | request class, status, retry, rate limit, auth failure | mixed stderr/errors | sanitized connector operation events |
| Auto-save | enrollment, safe push, blocked plan, paused remote changed | SQLite state + activity | explicit decision events with safe reason codes |
| Search/index | index rebuild, locate miss, unsupported query shape | none | low-volume diagnostics events |

## Storage Layout

Use the state root as the only default location:

```text
~/.loc/
  logs/
    desktop.jsonl
    localityd.jsonl
    provider.jsonl
    connector.jsonl
    autosave.jsonl
    diagnostics-index.sqlite   # optional later
```

The first implementation should write append-only JSONL files with daily or size
based rotation. SQLite indexing can come later when the UI needs fast filtering.
The JSONL shape should stay stable enough for support tooling:

```json
{
  "ts": "2026-06-22T10:15:30.123Z",
  "source": "desktop",
  "seq": 42,
  "level": "info",
  "event": "mount.open_folder.failed",
  "mount_id": "notion-main",
  "connector": "notion",
  "path": "/Users/example/Library/CloudStorage/Locality/notion-main",
  "message": "Could not open folder",
  "code": "open_folder_failed"
}
```

### Opt-In Trace Capture

Locality also supports opt-in span tracing for short debugging sessions. Set
`LOCALITY_TRACE_FILE` to an absolute JSONL path before running `loc` or
`localityd`:

```bash
LOCALITY_TRACE_FILE=/tmp/locality-pull-trace.jsonl loc pull ~/Locality/notion/page.md
```

Each line records one bounded operation with `ts_start_ms`, `ts_end_ms`,
`duration_ms`, `span`, `status`, and redacted attributes. Set
`LOCALITY_TRACE_RUN_ID` when grouping several commands into one investigation.
Trace writes are best effort: failure to create or append the trace file must not
fail sync, pull, push, mount, search, or daemon work.

Daemon-side spans are emitted only by the process doing the work. If `loc pull`
is served by an already-running daemon, the daemon must have inherited
`LOCALITY_TRACE_FILE` to emit internal pull, hydration, and connector spans. For
one-off investigations, `LOCALITY_DAEMON_DISABLE=1` can force the CLI direct path
when the target does not require daemon-only virtual projection behavior.

## Redaction Rules

- No token, client secret, refresh token, OAuth code, or keychain secret ref.
- No Markdown body, Notion block text, uploaded file content, or full journal op
  payload by default.
- Paths are allowed locally because the logs stay on the user's machine, but
  support bundle export should offer `--redact-paths`.
- Remote ids may be included because they are needed to correlate journals and
  sync state. Public Notion URLs should be omitted from logs unless redacted to
  host plus id.
- Error messages must pass through a common sanitizer before writing.

## Ordered Support Bundle

Add a future command:

```bash
loc logs collect --since 24h --out ~/Desktop/loc-support.zip
```

The collector should:

1. Read all JSONL log files and journal summaries.
2. Add `loc doctor --json` output.
3. Add mount and connection metadata without secrets.
4. Sort entries by timestamp, then by source sequence.
5. Write both `timeline.jsonl` and a human `timeline.md`.
6. Include a manifest with Locality version, OS, state schema version, and redaction mode.

This gives support a single chronological view like:

```text
10:12:00 desktop mount created
10:12:03 localityd reload_mounts failed unsupported schema version 10
10:12:05 desktop repair requested
10:12:08 localityd listening
10:13:21 autosave blocked remote_changed
```

## UI Surface

Keep the main app simple:

- Top-right health explains current state and points to the next action.
- Activity page shows meaningful actions, not raw logs.
- Settings diagnostics can expose:
  - `Open Logs Folder`
  - `Run Doctor`
  - `Export Support Bundle`
  - `Copy Latest Error`

The pending page should link failed pushes or blocked auto-save entries to the
relevant timeline slice so users do not have to search raw files.

## Implementation Phases

1. Add a small `loc-diagnostics` crate or shared module with `DiagnosticEvent`,
   redaction, rotation, and append-only JSONL writer.
2. Convert desktop activity writes to emit structured events while keeping the
   current activity projection.
3. Add daemon structured logging at lifecycle, reload, hydration, scheduled pull,
   and auto-save boundaries.
4. Add connector event hooks for request class, status code, retry, auth, and
   rate-limit outcomes.
5. Implement `loc logs collect` and wire `Export Support Bundle` in the desktop
   diagnostics panel.
6. Add an indexed diagnostics view only after JSONL volume or filtering needs it.

## Performance Boundaries

- Log writes must be non-blocking or bounded. A failed diagnostics write must not
  fail sync, push, pull, mount, or OAuth.
- Keep payloads small and structured. Large debug details belong in explicit
  opt-in diagnostic captures.
- Rotation should cap disk usage by default, for example 25 MB per source and
  7-14 days of retention.
- The UI reads a summarized projection, not full logs, during normal refresh.
