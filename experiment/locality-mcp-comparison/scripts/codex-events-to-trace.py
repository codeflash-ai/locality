#!/usr/bin/env python3
import csv
import json
import marshal
import re
import shlex
import sys
from collections import defaultdict
from pathlib import Path


def usage() -> None:
    raise SystemExit(
        "usage: codex-events-to-trace.py <timed-jsonl> <out-prefix>\n"
        "writes transcript, spans TSV, folded stack, SnakeViz pstats, SnakeViz stats, Speedscope, and Perfetto files"
    )


if len(sys.argv) != 3:
    usage()

events_path = Path(sys.argv[1])
out_prefix = Path(sys.argv[2])


def shorten(value, limit=220):
    if value is None:
        return ""
    if not isinstance(value, str):
        value = json.dumps(value, sort_keys=True)
    value = re.sub(r"\s+", " ", value).strip()
    return value if len(value) <= limit else value[: limit - 1] + "..."


def shell_words(command):
    if not command:
        return []
    try:
        return shlex.split(command)
    except ValueError:
        return command.split()


def command_breakdown(command):
    words = shell_words(command)
    executables = []
    loc_subcommands = []
    separators = {"&&", "||", ";", "|"}
    for index, word in enumerate(words):
        base = Path(word).name
        if base in separators or not base:
            continue
        if base == "loc":
            for next_word in words[index + 1 :]:
                next_base = Path(next_word).name
                if next_base in separators:
                    break
                if next_word.startswith("-"):
                    continue
                loc_subcommands.append(next_base)
                break
        if base not in executables and re.match(r"^[A-Za-z0-9_.+-]+$", base):
            executables.append(base)

    if loc_subcommands:
        return "loc", "+".join(dict.fromkeys(loc_subcommands)) or "loc"
    if executables:
        return "non_loc", "+".join(executables[:6])
    return "non_loc", "unknown_command"


def tool_breakdown(tool_name, command):
    tool_name = tool_name or ""
    if tool_name == "Bash":
        return command_breakdown(command)
    if tool_name.startswith("mcp__"):
        mcp_name = re.sub(r"^mcp__", "", tool_name).replace("__", ":")
        return "mcp", mcp_name or "mcp_tool"
    if tool_name:
        return "non_loc", tool_name
    return command_breakdown(command)


def span_activity(span):
    if span.get("activity"):
        return span["activity"]
    if span.get("phase"):
        return span["phase"]
    if span.get("span_kind"):
        return span["span_kind"]
    return "unknown"


def folded_frame(value):
    return str(value).replace(";", "_").replace("\n", " ").replace("\r", " ")


def span_stack(span, *, include_root=True):
    frames = []
    if include_root:
        frames.append("codex-readiness")
    frames.append(f"activity:{span_activity(span)}")
    if span_activity(span) == "tool":
        tool_group = span.get("tool_group") or "unknown_tool"
        command = span.get("tool_command_group") or "unknown_command"
        frames.append(f"tool:{tool_group}")
        frames.append(f"command:{tool_group}:{command}")
    else:
        frames.append(f"span:{span.get('name') or 'unknown_span'}")
    return [folded_frame(frame) for frame in frames]


def duration_us(span):
    return max(1, int(round(float(span.get("duration_ms") or 0) * 1000)))


def duration_seconds(span):
    return duration_us(span) / 1_000_000


def perfetto_track_name(span):
    activity = span_activity(span)
    if activity == "tool":
        tool_group = span.get("tool_group") or "unknown_tool"
        command = span.get("tool_command_group") or "unknown_command"
        return f"tool:{tool_group}:{command}"
    return f"{activity}:{span.get('name') or 'unknown_span'}"


def perfetto_args(span):
    fields = [
        "duration_ms",
        "start_line",
        "end_line",
        "event_type",
        "item_type",
        "span_kind",
        "phase",
        "activity",
        "tool_name",
        "tool_command",
        "tool_group",
        "tool_command_group",
    ]
    return {field: span.get(field, "") for field in fields if span.get(field, "") != ""}


def read_records(path):
    records = []
    for line_no, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        raw = json.loads(line)
        event = raw.get("event", raw)
        item = event.get("item") or {}
        observed_at_ms = int(raw.get("observed_at_ms") or event.get("observed_at_ms") or 0)
        records.append(
            {
                "line": line_no,
                "observed_at_ms": observed_at_ms,
                "event": event,
                "item": item,
                "event_type": event.get("type", "unknown"),
                "item_type": item.get("type", ""),
            }
        )
    return records


def item_id(record):
    event = record["event"]
    item = record["item"]
    return (
        item.get("id")
        or item.get("call_id")
        or event.get("item_id")
        or event.get("id")
        or event.get("call_id")
    )


def is_start(record):
    event_type = record["event_type"]
    return event_type.endswith(".started") or event_type in {
        "item.started",
        "response.output_item.added",
        "response.function_call_arguments.delta",
    }


def is_end(record):
    event_type = record["event_type"]
    return event_type.endswith(".completed") or event_type.endswith(".done") or event_type in {
        "item.completed",
        "response.output_item.done",
    }


def item_label(record):
    event = record["event"]
    item = record["item"]
    item_type = record["item_type"] or "event"

    if item_type in {"local_shell_call", "command_execution"}:
        command = item.get("command") or item.get("cmd") or item.get("argv") or event.get("command")
        return "shell: " + shorten(command, 120) if command else "shell"

    if item_type == "mcp_tool_call":
        server = item.get("server") or item.get("server_name") or event.get("server") or "mcp"
        tool = item.get("tool") or item.get("name") or event.get("tool") or event.get("name") or "tool"
        return f"mcp: {server}.{tool}"

    if item_type in {"function_call", "tool_call"}:
        name = item.get("name") or item.get("tool") or event.get("name") or "tool"
        return f"tool: {name}"

    if item_type == "message":
        role = item.get("role") or event.get("role") or "message"
        return f"message: {role}"

    message = event.get("message") or item.get("message") or item.get("text") or event.get("text")
    if message:
        return f"{item_type or record['event_type']}: {shorten(message, 120)}"
    return item_type or record["event_type"]


def span_label(left, right):
    left_type = left["event_type"]
    left_item = left["item_type"]

    if left_type == "turn.started":
        return "model: first response"

    if left_item in {"command_execution", "local_shell_call"}:
        if left_type.endswith(".started"):
            return item_label(left)
        return "model: process shell result"

    if left_item == "mcp_tool_call":
        if left_type.endswith(".started"):
            return item_label(left)
        return "model: process MCP result"

    if left_item in {"function_call", "tool_call"}:
        if left_type.endswith(".started"):
            return item_label(left)
        return "model: process tool result"

    if left_item == "agent_message":
        return "model: plan after message"

    if left_item == "file_change":
        return "model: continue after file change"

    if left_type == "turn.completed":
        return "turn complete"

    return "codex: " + item_label(left)


def build_observed_spans(records, base_ms):
    spans = []
    for left, right in zip(records, records[1:]):
        start_ms = left["observed_at_ms"]
        end_ms = right["observed_at_ms"]
        if end_ms <= start_ms:
            continue
        spans.append(
            {
                "name": span_label(left, right),
                "start_ms": start_ms - base_ms,
                "end_ms": end_ms - base_ms,
                "duration_ms": end_ms - start_ms,
                "start_line": left["line"],
                "end_line": right["line"],
                "event_type": left["event_type"],
                "item_type": left["item_type"],
                "span_kind": "observed_gap",
                "phase": "",
                "activity": "observed_gap",
                "tool_name": "",
                "tool_command": "",
                "tool_group": "",
                "tool_command_group": "",
            }
        )
    return spans


def hook_phase_label(record):
    event = record["event"]
    phase = event.get("phase") or "phase"
    activity = event.get("activity") or phase
    if phase == "tool_call":
        tool = event.get("tool_name") or "tool"
        command = event.get("tool_command") or event.get("command")
        if command:
            return f"hook: {activity}: {tool}: {shorten(command, 120)}"
        return f"hook: {activity}: {tool}"
    return f"hook: {activity}: {phase}"


def build_hook_phase_spans(records, base_ms):
    spans = []
    for record in records:
        event = record["event"]
        if event.get("type") != "harness.phase":
            continue
        start_ms = int(event.get("started_at_ms") or record["observed_at_ms"])
        duration_ms = int(event.get("duration_ms") or 0)
        end_ms = int(event.get("ended_at_ms") or (start_ms + duration_ms))
        if end_ms <= start_ms:
            end_ms = start_ms + max(1, duration_ms)
        phase = event.get("phase") or ""
        activity = event.get("activity") or phase
        tool_name = event.get("tool_name") or ""
        tool_command = event.get("tool_command") or event.get("command") or ""
        tool_group, tool_command_group = tool_breakdown(tool_name, tool_command)
        if phase != "tool_call":
            tool_group = ""
            tool_command_group = ""
        spans.append(
            {
                "name": hook_phase_label(record),
                "start_ms": start_ms - base_ms,
                "end_ms": end_ms - base_ms,
                "duration_ms": end_ms - start_ms,
                "start_line": record["line"],
                "end_line": record["line"],
                "event_type": record["event_type"],
                "item_type": record["item_type"],
                "span_kind": "codex_hook_phase",
                "phase": phase,
                "activity": activity,
                "tool_name": tool_name,
                "tool_command": tool_command,
                "tool_group": tool_group,
                "tool_command_group": tool_command_group,
            }
        )
    return spans


def extract_transcript_text(record):
    event = record["event"]
    item = record["item"]
    parts = []
    for key in ("message", "text", "output", "content"):
        if key in event:
            parts.append(shorten(event.get(key), 600))
        if key in item:
            parts.append(shorten(item.get(key), 600))
    if "arguments" in item:
        parts.append("arguments=" + shorten(item["arguments"], 600))
    if "command" in item:
        parts.append("command=" + shorten(item["command"], 600))
    if "cmd" in item:
        parts.append("cmd=" + shorten(item["cmd"], 600))
    if event.get("phase"):
        parts.append("phase=" + shorten(event.get("phase"), 120))
    if event.get("tool_command"):
        parts.append("tool_command=" + shorten(event.get("tool_command"), 600))
    return " | ".join(part for part in parts if part)


records = read_records(events_path)
if not records:
    raise SystemExit(f"no events in {events_path}")

base_ms = min(r["observed_at_ms"] for r in records if r["observed_at_ms"])

open_spans = {}
paired_spans = []

for record in records:
    ident = item_id(record)
    if ident and is_start(record):
        open_spans[ident] = record
        continue
    if ident and is_end(record) and ident in open_spans:
        start = open_spans.pop(ident)
        start_ms = start["observed_at_ms"]
        end_ms = max(record["observed_at_ms"], start_ms)
        paired_spans.append(
            {
                "name": item_label(start),
                "start_ms": start_ms - base_ms,
                "end_ms": end_ms - base_ms,
                "duration_ms": end_ms - start_ms,
                "start_line": start["line"],
                "end_line": record["line"],
                "event_type": start["event_type"],
                "item_type": start["item_type"],
                "span_kind": "item_pair",
                "phase": "",
                "activity": "item_pair",
                "tool_name": "",
                "tool_command": "",
                "tool_group": "",
                "tool_command_group": "",
            }
        )

for ident, start in open_spans.items():
    end_ms = records[-1]["observed_at_ms"]
    paired_spans.append(
        {
            "name": item_label(start),
            "start_ms": start["observed_at_ms"] - base_ms,
            "end_ms": end_ms - base_ms,
            "duration_ms": end_ms - start["observed_at_ms"],
            "start_line": start["line"],
            "end_line": records[-1]["line"],
            "event_type": start["event_type"],
            "item_type": start["item_type"],
            "span_kind": "open_item",
            "phase": "",
            "activity": "open_item",
            "tool_name": "",
            "tool_command": "",
            "tool_group": "",
            "tool_command_group": "",
        }
    )

observed_spans = build_observed_spans(records, base_ms)
hook_phase_spans = build_hook_phase_spans(records, base_ms)
spans = hook_phase_spans or observed_spans

paired_positive_ms = sum(span["duration_ms"] for span in paired_spans if span["duration_ms"] > 0)
observed_positive_ms = sum(span["duration_ms"] for span in observed_spans if span["duration_ms"] > 0)
hook_phase_positive_ms = sum(span["duration_ms"] for span in hook_phase_spans if span["duration_ms"] > 0)
if not spans and paired_spans:
    spans = paired_spans

transcript_path = out_prefix.with_name(out_prefix.name + "-transcript.md")
spans_path = out_prefix.with_name(out_prefix.name + "-spans.tsv")
speedscope_path = out_prefix.with_name(out_prefix.name + "-speedscope.json")
perfetto_path = out_prefix.with_name(out_prefix.name + ".perfetto.json")
folded_path = out_prefix.with_name(out_prefix.name + ".folded")
snakeviz_path = out_prefix.with_name(out_prefix.name + ".snakeviz.prof")
snakeviz_stats_path = out_prefix.with_name(out_prefix.name + ".snakeviz.stats.md")

with transcript_path.open("w") as f:
    f.write(f"# Codex Event Transcript\n\nSource: `{events_path}`\n\n")
    f.write(
        "Timing note: when `harness.phase` hook records are present, spans use those live Codex hook measurements. "
        "Otherwise spans are inferred from observed gaps between consecutive Codex JSON events.\n\n"
    )
    for record in records:
        rel = record["observed_at_ms"] - base_ms
        f.write(
            f"## +{rel}ms line {record['line']}: `{record['event_type']}`"
            f" / `{record['item_type'] or '-'}`\n\n"
        )
        text = extract_transcript_text(record)
        if text:
            f.write(text + "\n\n")

with spans_path.open("w", newline="") as f:
    writer = csv.DictWriter(
        f,
        fieldnames=[
            "name",
            "start_ms",
            "end_ms",
            "duration_ms",
            "start_line",
            "end_line",
            "event_type",
            "item_type",
            "span_kind",
            "phase",
            "activity",
            "tool_name",
            "tool_command",
            "tool_group",
            "tool_command_group",
        ],
        delimiter="\t",
    )
    writer.writeheader()
    writer.writerows(sorted(spans, key=lambda s: (s["start_ms"], s["end_ms"])))


def build_folded_lines(spans):
    totals = defaultdict(int)
    for span in spans:
        stack = ";".join(span_stack(span, include_root=True))
        totals[stack] += duration_us(span)
    return [f"{stack} {value}" for stack, value in sorted(totals.items())]


class PstatsFrame:
    def __init__(self, name, line):
        self.name = name
        self.line = line
        self.primitive_calls = 0
        self.total_calls = 0
        self.total_time = 0.0
        self.cumulative_time = 0.0
        self.callers = defaultdict(lambda: [0, 0, 0.0, 0.0])

    @property
    def key(self):
        return ("codex-readiness.synthetic", self.line, self.name)


def build_pstats_frames(spans):
    frames_by_name = {}

    def frame(name):
        if name not in frames_by_name:
            frames_by_name[name] = PstatsFrame(name, len(frames_by_name) + 1)
        return frames_by_name[name]

    for span in spans:
        duration = duration_seconds(span)
        stack = [frame(name) for name in span_stack(span, include_root=True)]
        if not stack:
            continue
        for stack_frame in stack:
            stack_frame.cumulative_time += duration
        leaf = stack[-1]
        leaf.total_time += duration
        leaf.primitive_calls += 1
        leaf.total_calls += 1
        for parent, child in zip(stack, stack[1:]):
            caller = child.callers[parent.key]
            caller[0] += 1
            caller[1] += 1
            caller[2] += duration
            caller[3] += duration

    return sorted(frames_by_name.values(), key=lambda item: item.line)


def write_snakeviz_profile(path, spans):
    stats = {}
    for frame in build_pstats_frames(spans):
        stats[frame.key] = (
            frame.primitive_calls,
            frame.total_calls,
            frame.total_time,
            frame.cumulative_time,
            {key: tuple(value) for key, value in frame.callers.items()},
        )
    with path.open("wb") as handle:
        marshal.dump(stats, handle)


def write_snakeviz_stats(path, spans):
    rows = []
    for frame in build_pstats_frames(spans):
        ncalls = frame.total_calls or frame.primitive_calls
        rows.append(
            {
                "ncalls": ncalls,
                "tottime": frame.total_time,
                "cumtime": frame.cumulative_time,
                "frame": f"{frame.key[0]}:{frame.key[1]}({frame.key[2]})",
                "callers": ", ".join(sorted(key[2] for key in frame.callers)),
            }
        )
    rows.sort(key=lambda row: (-row["cumtime"], -row["tottime"], row["frame"]))

    def seconds(value):
        return f"{value:.6f}s"

    def md_cell(value):
        return str(value).replace("|", "\\|").replace("\n", " ")

    lines = [
        "# SnakeViz Stats",
        "",
        "| Rank | ncalls | tottime | percall | cumtime | percall | Frame | Callers |",
        "| ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |",
    ]
    for index, row in enumerate(rows, 1):
        ncalls = row["ncalls"] or 1
        lines.append(
            f"| {index} | {row['ncalls']} | {seconds(row['tottime'])} | "
            f"{seconds(row['tottime'] / ncalls)} | {seconds(row['cumtime'])} | "
            f"{seconds(row['cumtime'] / ncalls)} | {md_cell(row['frame'])} | {md_cell(row['callers'])} |"
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


folded_path.write_text("\n".join(build_folded_lines(spans)) + ("\n" if spans else ""), encoding="utf-8")
write_snakeviz_profile(snakeviz_path, spans)
write_snakeviz_stats(snakeviz_stats_path, spans)

frame_index = {}
frames = []


def frame_for(name):
    if name not in frame_index:
        frame_index[name] = len(frames)
        frames.append({"name": name})
    return frame_index[name]


events = []
for span in sorted(spans, key=lambda s: (s["start_ms"], -s["end_ms"])):
    frame = frame_for(span["name"])
    events.append({"type": "O", "at": span["start_ms"], "frame": frame})
    events.append({"type": "C", "at": max(span["end_ms"], span["start_ms"] + 0.001), "frame": frame})
events.sort(key=lambda e: (e["at"], 0 if e["type"] == "C" else 1))

speedscope = {
    "$schema": "https://www.speedscope.app/file-format-schema.json",
    "shared": {"frames": frames},
    "profiles": [
        {
            "type": "evented",
            "name": events_path.name + (" hook phases" if hook_phase_spans else " observed gaps"),
            "unit": "milliseconds",
            "startValue": 0,
            "endValue": max((s["end_ms"] for s in spans), default=0),
            "events": events,
        }
    ],
    "metadata": {
        "source": str(events_path),
        "span_strategy": "live Codex hook phases" if hook_phase_spans else "observed gaps between consecutive Codex JSON events",
        "paired_item_positive_ms": paired_positive_ms,
        "observed_gap_positive_ms": observed_positive_ms,
        "hook_phase_positive_ms": hook_phase_positive_ms,
    },
    "activeProfileIndex": 0,
}
speedscope_path.write_text(json.dumps(speedscope, indent=2) + "\n")

perfetto_events = [
    {
        "ph": "M",
        "pid": 1,
        "name": "process_name",
        "args": {"name": events_path.name},
    }
]
perfetto_tracks = {}
for span in sorted(spans, key=lambda s: (s["start_ms"], s["end_ms"], s["name"])):
    track = perfetto_track_name(span)
    if track not in perfetto_tracks:
        perfetto_tracks[track] = len(perfetto_tracks) + 1
        perfetto_events.append(
            {
                "ph": "M",
                "pid": 1,
                "tid": perfetto_tracks[track],
                "name": "thread_name",
                "args": {"name": track},
            }
        )
    perfetto_events.append(
        {
            "ph": "X",
            "pid": 1,
            "tid": perfetto_tracks[track],
            "ts": max(0, int(round(float(span.get("start_ms") or 0) * 1000))),
            "dur": duration_us(span),
            "cat": span_activity(span),
            "name": span.get("name") or track,
            "args": perfetto_args(span),
        }
    )

perfetto = {
    "traceEvents": perfetto_events,
    "metadata": {
        "source": str(events_path),
        "span_strategy": "live Codex hook phases" if hook_phase_spans else "observed gaps between consecutive Codex JSON events",
        "paired_item_positive_ms": paired_positive_ms,
        "observed_gap_positive_ms": observed_positive_ms,
        "hook_phase_positive_ms": hook_phase_positive_ms,
    },
}
perfetto_path.write_text(json.dumps(perfetto, indent=2) + "\n")

print(transcript_path)
print(spans_path)
print(folded_path)
print(snakeviz_path)
print(snakeviz_stats_path)
print(speedscope_path)
print(perfetto_path)
