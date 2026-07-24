#!/usr/bin/env python3
import csv
import json
import sys
from collections import defaultdict
from pathlib import Path


def usage() -> None:
    raise SystemExit("usage: deep-dive-report.py <run-root> [out-md]")


if len(sys.argv) not in {2, 3}:
    usage()

run_root = Path(sys.argv[1])
out_path = Path(sys.argv[2]) if len(sys.argv) == 3 else run_root / "deep-dive.md"


def load_json(path: Path) -> dict:
    if not path.exists() or path.stat().st_size == 0:
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


def read_tsv(path: Path) -> list[dict]:
    if not path.exists() or path.stat().st_size == 0:
        return []
    with path.open(newline="", encoding="utf-8") as handle:
        return list(csv.DictReader(handle, delimiter="\t"))


def rel(path: Path) -> str:
    try:
        return str(path.relative_to(run_root))
    except ValueError:
        return str(path)


def cell(value) -> str:
    text = "" if value is None else str(value)
    return text.replace("|", "\\|").replace("\n", " ")


def shorten(value, limit: int = 120) -> str:
    text = "" if value is None else str(value)
    text = " ".join(text.split())
    if len(text) <= limit:
        return text
    return text[: limit - 1] + "..."


def int_field(row: dict, key: str) -> int:
    try:
        return int(float(row.get(key) or 0))
    except (TypeError, ValueError):
        return 0


def fmt_ms(value) -> str:
    try:
        ms = int(float(value or 0))
    except (TypeError, ValueError):
        ms = 0
    if abs(ms) >= 1000:
        return f"{ms / 1000:.3f}s"
    return f"{ms}ms"


def usage_totals(summary: dict) -> dict:
    usage = summary.get("usage") or {}
    buckets = {
        "input": 0,
        "cached_input": 0,
        "output": 0,
        "reasoning_output": 0,
        "total": 0,
    }
    for key, value in usage.items():
        if not isinstance(value, int):
            continue
        lowered = key.lower()
        if "cached" in lowered and "input" in lowered:
            buckets["cached_input"] += value
        elif "reasoning" in lowered and "output" in lowered:
            buckets["reasoning_output"] += value
        elif "input" in lowered:
            buckets["input"] += value
        elif "output" in lowered:
            buckets["output"] += value
        elif lowered == "total_tokens":
            buckets["total"] += value
    if buckets["total"] == 0:
        buckets["total"] = buckets["input"] + buckets["cached_input"] + buckets["output"] + buckets["reasoning_output"]
    return buckets


def non_warning_errors(summary: dict) -> list[str]:
    errors = summary.get("errors") or []
    return [
        error
        for error in errors
        if "--dangerously-bypass-hook-trust" not in str(error)
    ]


def warning_count(summary: dict) -> int:
    errors = summary.get("errors") or []
    return len(errors) - len(non_warning_errors(summary))


def strategy_roots(root: Path) -> dict[str, Path]:
    candidates = {}
    artifacts = root / "artifacts"
    for name in ("locality", "notion-mcp"):
        if (artifacts / name / "summary.json").exists():
            candidates[name] = artifacts / name
        elif (root / name / "summary.json").exists():
            candidates[name] = root / name
        elif root.name == name and (root / "summary.json").exists():
            candidates[name] = root
    return candidates


def scenario_dirs(strategy_root: Path) -> list[Path]:
    scenarios = strategy_root / "scenarios"
    if not scenarios.exists():
        return []
    return sorted(path for path in scenarios.iterdir() if path.is_dir())


def span_activity(row: dict) -> str:
    return row.get("activity") or row.get("phase") or row.get("span_kind") or "unknown"


def tool_key(row: dict) -> str:
    group = row.get("tool_group") or row.get("tool_name") or "unknown"
    command = row.get("tool_command_group") or shorten(row.get("tool_command") or row.get("name") or "unknown", 80)
    return f"{group}:{command}"


def tool_rows(spans: list[dict]) -> list[dict]:
    return [row for row in spans if is_tool_span(row)]


def is_tool_span(row: dict) -> bool:
    activity = span_activity(row)
    return activity == "tool" or row.get("phase") == "tool_call" or bool(row.get("tool_name"))


def summarize_spans(spans: list[dict]) -> tuple[dict[str, int], dict[str, int]]:
    activity_totals: dict[str, int] = defaultdict(int)
    tool_totals: dict[str, int] = defaultdict(int)
    for row in spans:
        duration = int_field(row, "duration_ms")
        activity_totals[span_activity(row)] += duration
        if is_tool_span(row):
            tool_totals[tool_key(row)] += duration
    return dict(activity_totals), dict(tool_totals)


def top_items(items: dict[str, int], limit: int = 8) -> list[tuple[str, int]]:
    return sorted(items.items(), key=lambda item: (-item[1], item[0]))[:limit]


def render_metric_table(lines: list[str], rows: list[dict], strategy: str) -> None:
    if not rows:
        lines.extend(["No phase metrics were recorded.", ""])
        return
    lines.extend(
        [
            "| Phase | Status | Duration | Detail |",
            "| --- | --- | ---: | --- |",
        ]
    )
    for row in rows:
        lines.append(
            f"| `{cell(row.get('phase'))}` | {cell(row.get('status'))} | {fmt_ms(row.get('duration_ms'))} | {cell(shorten(row.get('detail'), 180))} |"
        )
    lines.append("")


def render_bucket_table(lines: list[str], title: str, buckets: dict[str, int]) -> None:
    lines.extend([f"#### {title}", ""])
    if not buckets:
        lines.extend(["No bucketed spans were available.", ""])
        return
    lines.extend(["| Bucket | Duration |", "| --- | ---: |"])
    for name, duration in top_items(buckets, 12):
        lines.append(f"| `{cell(name)}` | {fmt_ms(duration)} |")
    lines.append("")


def render_tool_timeline(lines: list[str], spans: list[dict]) -> None:
    rows = sorted(tool_rows(spans), key=lambda row: (int_field(row, "start_ms"), int_field(row, "end_ms")))
    lines.extend(["#### Tool Timeline", ""])
    if not rows:
        lines.extend(["No tool-call spans were available. Check the transcript for observed-gap-only runs.", ""])
        return
    lines.extend(["| Start | Duration | Tool | Command / Call |", "| ---: | ---: | --- | --- |"])
    for row in rows[:80]:
        lines.append(
            "| {start} | {duration} | `{tool}` | {command} |".format(
                start=fmt_ms(row.get("start_ms")),
                duration=fmt_ms(row.get("duration_ms")),
                tool=cell(row.get("tool_group") or row.get("tool_name") or "unknown"),
                command=cell(shorten(row.get("tool_command") or row.get("tool_command_group") or row.get("name"), 180)),
            )
        )
    if len(rows) > 80:
        lines.append(f"|  |  |  | {len(rows) - 80} additional tool spans omitted from this markdown view; see the spans TSV. |")
    lines.append("")


def render_top_spans(lines: list[str], spans: list[dict]) -> None:
    rows = sorted(spans, key=lambda row: (-int_field(row, "duration_ms"), int_field(row, "start_ms")))[:10]
    lines.extend(["#### Longest Spans", ""])
    if not rows:
        lines.extend(["No spans were available.", ""])
        return
    lines.extend(["| Duration | Activity | Span |", "| ---: | --- | --- |"])
    for row in rows:
        lines.append(
            f"| {fmt_ms(row.get('duration_ms'))} | `{cell(span_activity(row))}` | {cell(shorten(row.get('name'), 180))} |"
        )
    lines.append("")


def render_locality_trace_summary(lines: list[str], trace_summary: Path) -> None:
    data = load_json(trace_summary)
    if not data:
        return
    lines.extend([f"#### Locality Trace: `{cell(rel(trace_summary))}`", ""])
    lines.append(
        f"Duration: **{fmt_ms(data.get('duration_ms'))}**. Records: **{data.get('record_count', 0)}**."
    )
    lines.append("")
    top_spans = data.get("top_spans") or []
    if not top_spans:
        return
    lines.extend(["| Span | Duration | Status | Important attrs |", "| --- | ---: | --- | --- |"])
    for span in top_spans[:8]:
        attrs = span.get("attrs") or {}
        useful = []
        for key in ("source", "connector", "operation", "query", "path", "selected_path", "result_count", "request_count", "page_count"):
            if key in attrs:
                useful.append(f"{key}={shorten(attrs[key], 60)}")
        lines.append(
            f"| `{cell(span.get('span'))}` | {fmt_ms(span.get('duration_ms'))} | {cell(span.get('status'))} | {cell('; '.join(useful))} |"
        )
    lines.append("")


roots = strategy_roots(run_root)
lines: list[str] = [
    "# Multi-Source Experiment Deep Dive",
    "",
    f"Run root: `{run_root}`",
    "",
]

run_env = run_root / "run.env"
if run_env.exists():
    lines.extend(["## Run Configuration", "", "```text", run_env.read_text(encoding="utf-8").strip(), "```", ""])

overview_rows = []
per_strategy_data = []

for strategy, strategy_root in roots.items():
    summary = load_json(strategy_root / "summary.json")
    metrics = summary.get("metrics") or []
    prefix = "locality" if strategy == "locality" else "notion-mcp"
    for scenario_dir in scenario_dirs(strategy_root):
        scenario = scenario_dir.name
        codex_summary = load_json(scenario_dir / f"{prefix}-codex-summary.json")
        spans_path = scenario_dir / f"{prefix}-spans.tsv"
        spans = read_tsv(spans_path)
        activity_buckets, tool_buckets = summarize_spans(spans)
        scenario_metrics = [row for row in metrics if row.get("scenario") == scenario]
        tokens = usage_totals(codex_summary)
        overview_rows.append(
            {
                "scenario": scenario,
                "strategy": strategy,
                "status": "errors"
                if non_warning_errors(codex_summary)
                else ("ok_with_warnings" if warning_count(codex_summary) else "ok"),
                "duration_ms": codex_summary.get("observed_duration_ms", 0),
                "events": codex_summary.get("event_count", 0),
                "tokens": tokens,
                "tool_count": sum(int(value) for value in (codex_summary.get("tool_counts") or {}).values()),
                "report": scenario_dir / ("report-body.md" if strategy == "locality" else "notion-mcp-report-body.md"),
                "trace": scenario_dir / ("locality-agent-trace.md" if strategy == "locality" else "notion-mcp-agent-trace.md"),
                "spans": spans_path,
                "speedscope": scenario_dir / f"{prefix}-speedscope.json",
                "perfetto": scenario_dir / f"{prefix}.perfetto.json",
            }
        )
        per_strategy_data.append(
            {
                "strategy": strategy,
                "strategy_root": strategy_root,
                "scenario": scenario,
                "scenario_dir": scenario_dir,
                "prefix": prefix,
                "codex_summary": codex_summary,
                "spans": spans,
                "activity_buckets": activity_buckets,
                "tool_buckets": tool_buckets,
                "metrics": scenario_metrics,
            }
        )

lines.extend(["## Overview", ""])
if not overview_rows:
    lines.extend(["No scenario artifacts were found.", ""])
else:
    lines.extend(
        [
            "| Scenario | Strategy | Status | Codex observed | Events | Tool events | Tokens | Report | Timeline |",
            "| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |",
        ]
    )
    for row in sorted(overview_rows, key=lambda item: (item["scenario"], item["strategy"])):
        lines.append(
            "| {scenario} | {strategy} | {status} | {duration} | {events} | {tools} | {tokens} | `{report}` | `{perfetto}` |".format(
                scenario=cell(row["scenario"]),
                strategy=cell(row["strategy"]),
                status=cell(row["status"]),
                duration=fmt_ms(row["duration_ms"]),
                events=row["events"],
                tools=row["tool_count"],
                tokens=row["tokens"]["total"],
                report=cell(rel(row["report"])),
                perfetto=cell(rel(row["perfetto"])),
            )
        )
    lines.append("")

lines.extend(["## Pair Comparison", ""])
by_scenario: dict[str, dict[str, dict]] = defaultdict(dict)
for row in overview_rows:
    by_scenario[row["scenario"]][row["strategy"]] = row
if by_scenario:
    lines.extend(["| Scenario | Locality observed | MCP observed | Delta | Locality tokens | MCP tokens |", "| --- | ---: | ---: | ---: | ---: | ---: |"])
    for scenario, values in sorted(by_scenario.items()):
        locality = values.get("locality", {})
        mcp = values.get("notion-mcp", {})
        locality_ms = int(locality.get("duration_ms") or 0)
        mcp_ms = int(mcp.get("duration_ms") or 0)
        delta = mcp_ms - locality_ms if locality_ms and mcp_ms else 0
        lines.append(
            f"| {cell(scenario)} | {fmt_ms(locality_ms)} | {fmt_ms(mcp_ms)} | {fmt_ms(delta)} | "
            f"{(locality.get('tokens') or {}).get('total', 0)} | {(mcp.get('tokens') or {}).get('total', 0)} |"
        )
    lines.append("")
else:
    lines.extend(["No paired strategy data was available.", ""])

for item in sorted(per_strategy_data, key=lambda row: (row["scenario"], row["strategy"])):
    strategy = item["strategy"]
    scenario = item["scenario"]
    scenario_dir = item["scenario_dir"]
    prefix = item["prefix"]
    codex_summary = item["codex_summary"]
    spans = item["spans"]
    lines.extend([f"## {scenario} / {strategy}", ""])
    lines.append(f"Scenario artifact directory: `{rel(scenario_dir)}`")
    lines.append("")
    tokens = usage_totals(codex_summary)
    lines.extend(
        [
            "| Codex observed | Events | Input | Cached input | Output | Reasoning output | Total tokens | Errors | Warnings |",
            "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
            f"| {fmt_ms(codex_summary.get('observed_duration_ms'))} | {codex_summary.get('event_count', 0)} | "
            f"{tokens['input']} | {tokens['cached_input']} | {tokens['output']} | {tokens['reasoning_output']} | "
            f"{tokens['total']} | {len(non_warning_errors(codex_summary))} | {warning_count(codex_summary)} |",
            "",
        ]
    )

    if codex_summary.get("tool_counts"):
        lines.extend(["### Event Tool Counts", "", "| Tool / item | Count |", "| --- | ---: |"])
        for name, count in sorted(codex_summary["tool_counts"].items(), key=lambda item: (-int(item[1]), item[0])):
            lines.append(f"| `{cell(name)}` | {count} |")
        lines.append("")

    lines.extend(["### Phase Metrics", ""])
    render_metric_table(lines, item["metrics"], strategy)
    lines.extend(["### Span Buckets", ""])
    render_bucket_table(lines, "By Activity", item["activity_buckets"])
    render_bucket_table(lines, "By Tool Command", item["tool_buckets"])
    render_tool_timeline(lines, spans)
    render_top_spans(lines, spans)

    artifacts = {
        "report": scenario_dir / ("report-body.md" if strategy == "locality" else "notion-mcp-report-body.md"),
        "agent_trace": scenario_dir / ("locality-agent-trace.md" if strategy == "locality" else "notion-mcp-agent-trace.md"),
        "transcript": scenario_dir / f"{prefix}-transcript.md",
        "spans": scenario_dir / f"{prefix}-spans.tsv",
        "speedscope": scenario_dir / f"{prefix}-speedscope.json",
        "perfetto": scenario_dir / f"{prefix}.perfetto.json",
        "snakeviz": scenario_dir / f"{prefix}.snakeviz.prof",
        "snakeviz_stats": scenario_dir / f"{prefix}.snakeviz.stats.md",
        "command": scenario_dir / f"{prefix}-codex-command.txt",
    }
    lines.extend(["### Trace Index", "", "| Artifact | Path |", "| --- | --- |"])
    for label, path in artifacts.items():
        if path.exists():
            lines.append(f"| {label} | `{cell(rel(path))}` |")
    lines.append("")

    if non_warning_errors(codex_summary):
        lines.extend(["### Errors", ""])
        for error in non_warning_errors(codex_summary)[:10]:
            lines.append(f"- {shorten(error, 240)}")
        lines.append("")

    if warning_count(codex_summary):
        lines.extend(["### Warnings", ""])
        for error in (codex_summary.get("errors") or [])[:10]:
            if "--dangerously-bypass-hook-trust" in str(error):
                lines.append(f"- {shorten(error, 240)}")
        lines.append("")

    if strategy == "locality":
        scenario_trace_summary = scenario_dir / "locality-agent-locality-trace-summary.json"
        render_locality_trace_summary(lines, scenario_trace_summary)

for strategy, strategy_root in roots.items():
    if strategy != "locality":
        continue
    trace_root = strategy_root / "locality-traces"
    if not trace_root.exists():
        continue
    lines.extend(["## Locality Setup Trace Summaries", ""])
    for summary_path in sorted(trace_root.glob("*-summary.json")):
        render_locality_trace_summary(lines, summary_path)

out_path.parent.mkdir(parents=True, exist_ok=True)
out_path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")
print(out_path)
