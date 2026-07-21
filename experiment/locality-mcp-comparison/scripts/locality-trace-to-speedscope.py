#!/usr/bin/env python3
import csv
import json
import re
import sys
from collections import Counter
from pathlib import Path


def usage() -> None:
    raise SystemExit(
        "usage: locality-trace-to-speedscope.py <trace-jsonl> <out-prefix>\n"
        "writes <out-prefix>-spans.tsv, <out-prefix>-summary.json, and <out-prefix>-speedscope.json"
    )


if len(sys.argv) != 3:
    usage()

trace_path = Path(sys.argv[1])
out_prefix = Path(sys.argv[2])


def shorten(value, limit=120):
    if value is None:
        return ""
    if not isinstance(value, str):
        value = json.dumps(value, sort_keys=True)
    value = re.sub(r"\s+", " ", value).strip()
    return value if len(value) <= limit else value[: limit - 1] + "..."


def label_for(record):
    attrs = record.get("attrs") or {}
    details = []
    for key in (
        "branch",
        "reason",
        "outcome",
        "connector",
        "mount_id",
        "entity_kind",
        "selected_kind",
    ):
        value = attrs.get(key)
        if value not in (None, ""):
            details.append(f"{key}={shorten(value, 40)}")
    return record["span"] if not details else f"{record['span']} ({', '.join(details[:3])})"


records = []
for line_no, line in enumerate(trace_path.read_text().splitlines(), 1):
    if not line.strip():
        continue
    raw = json.loads(line)
    try:
        start_ms = int(raw["ts_start_ms"])
        end_ms = int(raw["ts_end_ms"])
    except KeyError as error:
        raise SystemExit(f"{trace_path}:{line_no} is missing {error}") from error
    if end_ms < start_ms:
        end_ms = start_ms
    attrs = raw.get("attrs") or {}
    records.append(
        {
            "line": line_no,
            "span": raw.get("span", "unknown"),
            "status": raw.get("status", "ok"),
            "run_id": raw.get("run_id", ""),
            "start_ms": start_ms,
            "end_ms": end_ms,
            "duration_ms": int(raw.get("duration_ms") or (end_ms - start_ms)),
            "attrs": attrs,
            "label": label_for(raw),
        }
    )

if not records:
    raise SystemExit(f"no trace records in {trace_path}")

base_ms = min(record["start_ms"] for record in records)
for record in records:
    record["relative_start_ms"] = record["start_ms"] - base_ms
    record["relative_end_ms"] = max(record["end_ms"] - base_ms, record["start_ms"] - base_ms + 1)

spans_path = out_prefix.with_name(out_prefix.name + "-spans.tsv")
summary_path = out_prefix.with_name(out_prefix.name + "-summary.json")
speedscope_path = out_prefix.with_name(out_prefix.name + "-speedscope.json")

fieldnames = [
    "line",
    "span",
    "status",
    "run_id",
    "relative_start_ms",
    "relative_end_ms",
    "duration_ms",
    "attrs",
]
with spans_path.open("w", newline="") as f:
    writer = csv.DictWriter(f, fieldnames=fieldnames, delimiter="\t")
    writer.writeheader()
    for record in sorted(records, key=lambda row: (row["relative_start_ms"], row["relative_end_ms"], row["line"])):
        writer.writerow(
            {
                "line": record["line"],
                "span": record["span"],
                "status": record["status"],
                "run_id": record["run_id"],
                "relative_start_ms": record["relative_start_ms"],
                "relative_end_ms": record["relative_end_ms"],
                "duration_ms": record["duration_ms"],
                "attrs": json.dumps(record["attrs"], sort_keys=True),
            }
        )

span_counts = Counter(record["span"] for record in records)
status_counts = Counter(record["status"] for record in records)
duration_by_span = Counter()
for record in records:
    duration_by_span[record["span"]] += record["duration_ms"]

summary = {
    "source": str(trace_path),
    "record_count": len(records),
    "started_at_ms": min(record["start_ms"] for record in records),
    "ended_at_ms": max(record["end_ms"] for record in records),
    "duration_ms": max(record["end_ms"] for record in records) - base_ms,
    "span_counts": dict(span_counts),
    "status_counts": dict(status_counts),
    "duration_by_span_ms": dict(duration_by_span.most_common()),
    "top_spans": [
        {
            "span": record["span"],
            "duration_ms": record["duration_ms"],
            "status": record["status"],
            "attrs": record["attrs"],
        }
        for record in sorted(records, key=lambda row: row["duration_ms"], reverse=True)[:20]
    ],
}
summary_path.write_text(json.dumps(summary, indent=2) + "\n")

frame_index = {}
frames = []


def frame_for(name):
    if name not in frame_index:
        frame_index[name] = len(frames)
        frames.append({"name": name})
    return frame_index[name]


events = []
for record in sorted(records, key=lambda row: (row["relative_start_ms"], -row["relative_end_ms"])):
    frame = frame_for(record["label"])
    events.append({"type": "O", "at": record["relative_start_ms"], "frame": frame})
    events.append({"type": "C", "at": record["relative_end_ms"], "frame": frame})
events.sort(key=lambda event: (event["at"], 0 if event["type"] == "C" else 1))

speedscope = {
    "$schema": "https://www.speedscope.app/file-format-schema.json",
    "shared": {"frames": frames},
    "profiles": [
        {
            "type": "evented",
            "name": trace_path.name,
            "unit": "milliseconds",
            "startValue": 0,
            "endValue": max(record["relative_end_ms"] for record in records),
            "events": events,
        }
    ],
    "metadata": {
        "source": str(trace_path),
        "format": "Locality JSONL spans",
    },
    "activeProfileIndex": 0,
}
speedscope_path.write_text(json.dumps(speedscope, indent=2) + "\n")

print(spans_path)
print(summary_path)
print(speedscope_path)
