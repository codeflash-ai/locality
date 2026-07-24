#!/usr/bin/env python3
import json
import subprocess
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("experiment/runs")
summaries = sorted(root.glob("**/summary.json"))
STRATEGY_DIRS = {"locality", "notion-mcp"}


def run_id_for_summary(path):
    parent = path.parent
    try:
        rel_parts = parent.relative_to(root).parts
    except ValueError:
        rel_parts = parent.parts

    if not rel_parts:
        return root.name
    if len(rel_parts) >= 2 and rel_parts[-2] == "artifacts" and rel_parts[-1] in STRATEGY_DIRS:
        trial_parts = rel_parts[:-2]
        return "/".join(trial_parts) if trial_parts else root.parent.name
    if len(rel_parts) >= 1 and rel_parts[-1] in STRATEGY_DIRS:
        trial_parts = rel_parts[:-1]
        return "/".join(trial_parts) if trial_parts else root.name
    return "/".join(rel_parts)


rows = []
for path in summaries:
    data = json.loads(path.read_text())
    metrics = data.get("metrics", [])
    if not metrics:
        continue
    run_id = run_id_for_summary(path)
    for metric in metrics:
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
lines.append(f"Runs discovered: {len(set(row['run_id'] for row in rows))}")
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

chart_script = Path(__file__).with_name("token-usage-charts.py")
if rows and chart_script.exists():
    subprocess.run(
        [sys.executable, str(chart_script), str(root), str(root / "token-usage")],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    print(root / "token-usage")
