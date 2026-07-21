#!/usr/bin/env python3
import csv
import json
import re
import sys
from pathlib import Path


def usage() -> None:
    raise SystemExit(
        "usage: codex-events-to-trace.py <timed-jsonl> <out-prefix>\n"
        "writes <out-prefix>-transcript.md, <out-prefix>-spans.tsv, and <out-prefix>-speedscope.json"
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
    return " | ".join(part for part in parts if part)


records = read_records(events_path)
if not records:
    raise SystemExit(f"no events in {events_path}")

base_ms = min(r["observed_at_ms"] for r in records if r["observed_at_ms"])

open_spans = {}
spans = []

for record in records:
    ident = item_id(record)
    if ident and is_start(record):
        open_spans[ident] = record
        continue
    if ident and is_end(record) and ident in open_spans:
        start = open_spans.pop(ident)
        start_ms = start["observed_at_ms"]
        end_ms = max(record["observed_at_ms"], start_ms)
        spans.append(
            {
                "name": item_label(start),
                "start_ms": start_ms - base_ms,
                "end_ms": end_ms - base_ms,
                "duration_ms": end_ms - start_ms,
                "start_line": start["line"],
                "end_line": record["line"],
                "event_type": start["event_type"],
                "item_type": start["item_type"],
            }
        )

for ident, start in open_spans.items():
    end_ms = records[-1]["observed_at_ms"]
    spans.append(
        {
            "name": item_label(start),
            "start_ms": start["observed_at_ms"] - base_ms,
            "end_ms": end_ms - base_ms,
            "duration_ms": end_ms - start["observed_at_ms"],
            "start_line": start["line"],
            "end_line": records[-1]["line"],
            "event_type": start["event_type"],
            "item_type": start["item_type"],
        }
    )

if not spans:
    for left, right in zip(records, records[1:]):
        start_ms = left["observed_at_ms"]
        end_ms = max(right["observed_at_ms"], start_ms)
        spans.append(
            {
                "name": item_label(left),
                "start_ms": start_ms - base_ms,
                "end_ms": end_ms - base_ms,
                "duration_ms": end_ms - start_ms,
                "start_line": left["line"],
                "end_line": right["line"],
                "event_type": left["event_type"],
                "item_type": left["item_type"],
            }
        )

transcript_path = out_prefix.with_name(out_prefix.name + "-transcript.md")
spans_path = out_prefix.with_name(out_prefix.name + "-spans.tsv")
speedscope_path = out_prefix.with_name(out_prefix.name + "-speedscope.json")

with transcript_path.open("w") as f:
    f.write(f"# Codex Event Transcript\n\nSource: `{events_path}`\n\n")
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
        ],
        delimiter="\t",
    )
    writer.writeheader()
    writer.writerows(sorted(spans, key=lambda s: (s["start_ms"], s["end_ms"])))

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
            "name": events_path.name,
            "unit": "milliseconds",
            "startValue": 0,
            "endValue": max((s["end_ms"] for s in spans), default=0),
            "events": events,
        }
    ],
    "activeProfileIndex": 0,
}
speedscope_path.write_text(json.dumps(speedscope, indent=2) + "\n")

print(transcript_path)
print(spans_path)
print(speedscope_path)
