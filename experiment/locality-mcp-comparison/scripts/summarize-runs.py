#!/usr/bin/env python3
import json
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("experiment/runs")
summaries = sorted(root.glob("*/summary.json"))
rows = []
for path in summaries:
    data = json.loads(path.read_text())
    run_id = path.parent.name
    for metric in data.get("metrics", []):
        rows.append(
            {
                "run_id": run_id,
                "model": data.get("model"),
                "effort": data.get("reasoning_effort"),
                "scenario": metric.get("scenario", "default"),
                **metric,
            }
        )

latest = rows[-200:]
by_phase = {}
for row in latest:
    key = (row["model"], row["effort"], row.get("scenario", "default"), row["strategy"], row["phase"])
    by_phase.setdefault(key, []).append(row["duration_ms"])

lines = []
lines.append("# Repeated Benchmark Summary")
lines.append("")
lines.append(f"Runs discovered: {len(summaries)}")
lines.append("")
lines.append("| Model | Effort | Scenario | Strategy | Phase | Runs | Mean | Median | Min | Max |")
lines.append("| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |")
for (model, effort, scenario, strategy, phase), values in sorted(by_phase.items()):
    values_s = sorted(values)
    lines.append(
        f"| {model} | {effort} | {scenario} | {strategy} | `{phase}` | {len(values)} | "
        f"{statistics.mean(values_s)/1000:.2f}s | {statistics.median(values_s)/1000:.2f}s | "
        f"{min(values_s)/1000:.2f}s | {max(values_s)/1000:.2f}s |"
    )

out = root / "repeated-summary.md"
out.write_text("\n".join(lines) + "\n")
print(out)
