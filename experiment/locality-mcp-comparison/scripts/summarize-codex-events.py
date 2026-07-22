#!/usr/bin/env python3
import csv
import json
import sys
from collections import Counter
from pathlib import Path

if len(sys.argv) != 4:
    raise SystemExit("usage: summarize-codex-events.py <timed-jsonl> <summary-json> <events-tsv>")

events_path = Path(sys.argv[1])
summary_path = Path(sys.argv[2])
events_tsv_path = Path(sys.argv[3])

event_counts = Counter()
item_counts = Counter()
tool_counts = Counter()
errors = []
usage = {}
rows = []
first_ms = None
last_ms = None

for line_no, line in enumerate(events_path.read_text().splitlines(), 1):
    if not line.strip():
        continue
    record = json.loads(line)
    observed_at_ms = int(record.get("observed_at_ms", 0))
    event = record.get("event", {})
    event_type = event.get("type", "unknown")
    item = event.get("item") or {}
    item_type = item.get("type", "")
    message = event.get("message") or item.get("message") or ""

    first_ms = observed_at_ms if first_ms is None else min(first_ms, observed_at_ms)
    last_ms = observed_at_ms if last_ms is None else max(last_ms, observed_at_ms)
    event_counts[event_type] += 1
    if item_type:
        item_counts[item_type] += 1
    if "tool" in item_type or item_type in {"function_call", "local_shell_call", "mcp_tool_call"}:
        tool_counts[item_type] += 1
    if event_type == "error" or item_type == "error":
        errors.append(message)
    if "usage" in event:
        usage = event["usage"]

    rows.append(
        {
            "line": line_no,
            "observed_at_ms": observed_at_ms,
            "event_type": event_type,
            "item_type": item_type,
            "message": message.replace("\n", " ")[:500],
        }
    )

with events_tsv_path.open("w", newline="") as f:
    writer = csv.DictWriter(
        f,
        fieldnames=["line", "observed_at_ms", "event_type", "item_type", "message"],
        delimiter="\t",
    )
    writer.writeheader()
    writer.writerows(rows)

summary = {
    "events_path": str(events_path),
    "event_count": len(rows),
    "observed_started_at_ms": first_ms,
    "observed_ended_at_ms": last_ms,
    "observed_duration_ms": (last_ms - first_ms) if first_ms is not None and last_ms is not None else 0,
    "event_counts": dict(event_counts),
    "item_counts": dict(item_counts),
    "tool_counts": dict(tool_counts),
    "usage": usage,
    "errors": errors,
}
summary_path.write_text(json.dumps(summary, indent=2) + "\n")
