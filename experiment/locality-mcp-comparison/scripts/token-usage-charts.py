#!/usr/bin/env python3
import csv
import html
import json
import os
import re
import sys
from pathlib import Path


STRATEGIES = ("locality", "notion-mcp")
COMPONENTS = (
    ("fresh_input_tokens", "Fresh input", "#4C78A8"),
    ("cached_input_tokens", "Cached input", "#72B7B2"),
    ("cache_write_input_tokens", "Cache write input", "#F58518"),
    ("visible_output_tokens", "Output", "#54A24B"),
    ("reasoning_output_tokens", "Reasoning output", "#E45756"),
)
COST_COMPONENTS = (
    ("fresh_input_cost_usd", "Fresh input", "#4C78A8"),
    ("cached_input_cost_usd", "Cached input", "#72B7B2"),
    ("cache_write_input_cost_usd", "Cache write input", "#F58518"),
    ("visible_output_cost_usd", "Output", "#54A24B"),
    ("reasoning_output_cost_usd", "Reasoning output", "#E45756"),
)
DEFAULT_PRICING_USD_PER_1M = {
    "fresh_input_tokens": 1.00,
    "cached_input_tokens": 0.10,
    "cache_write_input_tokens": 1.25,
    "visible_output_tokens": 6.00,
    "reasoning_output_tokens": 6.00,
}
PRICING_ENV = {
    "fresh_input_tokens": "CODEX_COST_INPUT_USD_PER_1M",
    "cached_input_tokens": "CODEX_COST_CACHED_INPUT_USD_PER_1M",
    "cache_write_input_tokens": "CODEX_COST_CACHE_WRITE_INPUT_USD_PER_1M",
    "visible_output_tokens": "CODEX_COST_OUTPUT_USD_PER_1M",
    "reasoning_output_tokens": "CODEX_COST_REASONING_OUTPUT_USD_PER_1M",
}


def usage() -> None:
    print(
        "usage: token-usage-charts.py <run-or-runs-root> [out-dir]\n"
        "writes stacked token and cost SVGs, TSV data, token-usage.json, and index.md",
        file=sys.stderr,
    )


if len(sys.argv) not in (2, 3):
    usage()
    raise SystemExit(2)

root = Path(sys.argv[1]).resolve()
out_dir = Path(sys.argv[2]).resolve() if len(sys.argv) == 3 else root / "token-usage"
by_scenario_dir = out_dir / "by-trial-scenario"
cost_dir = out_dir / "cost"
cost_by_scenario_dir = cost_dir / "by-trial-scenario"
by_scenario_dir.mkdir(parents=True, exist_ok=True)
cost_by_scenario_dir.mkdir(parents=True, exist_ok=True)


def read_pricing() -> dict:
    pricing = {}
    for key, default in DEFAULT_PRICING_USD_PER_1M.items():
        env_name = PRICING_ENV[key]
        raw = os.environ.get(env_name)
        if raw is None or raw == "":
            pricing[key] = default
            continue
        try:
            pricing[key] = float(raw)
        except ValueError as exc:
            raise SystemExit(f"{env_name} must be a number, got {raw!r}") from exc
    return pricing


PRICING_USD_PER_1M = read_pricing()


def slug(value: str) -> str:
    value = value.strip().replace("/", "__")
    value = re.sub(r"[^A-Za-z0-9._-]+", "-", value)
    return value.strip("-") or "run"


def is_run_summary(path: Path) -> bool:
    try:
        data = json.loads(path.read_text())
    except Exception:
        return False
    return isinstance(data, dict) and isinstance(data.get("metrics"), list) and isinstance(data.get("scenarios"), dict)


def trial_id_for_summary(summary_path: Path) -> str:
    parent = summary_path.parent
    try:
        rel_parts = parent.relative_to(root).parts
    except ValueError:
        rel_parts = parent.parts

    if not rel_parts:
        return root.name

    if len(rel_parts) >= 2 and rel_parts[-2] == "artifacts" and rel_parts[-1] in STRATEGIES:
        trial_parts = rel_parts[:-2]
        return "/".join(trial_parts) if trial_parts else root.parent.name

    if len(rel_parts) >= 1 and rel_parts[-1] in STRATEGIES:
        trial_parts = rel_parts[:-1]
        return "/".join(trial_parts) if trial_parts else root.name

    return "/".join(rel_parts)


def token_components(raw_usage: dict) -> dict:
    input_tokens = int(raw_usage.get("input_tokens") or 0)
    cached = int(raw_usage.get("cached_input_tokens") or 0)
    cache_write = int(raw_usage.get("cache_write_input_tokens") or 0)
    output_tokens = int(raw_usage.get("output_tokens") or 0)
    reasoning_output = int(raw_usage.get("reasoning_output_tokens") or 0)

    fresh_input = max(input_tokens - cached - cache_write, 0)
    visible_output = max(output_tokens - reasoning_output, 0)
    return {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "fresh_input_tokens": fresh_input,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": cache_write,
        "visible_output_tokens": visible_output,
        "reasoning_output_tokens": reasoning_output,
        "total_tokens": fresh_input + cached + cache_write + visible_output + reasoning_output,
    }


def cost_components(token_row: dict) -> dict:
    cost = {
        "fresh_input_cost_usd": token_row["fresh_input_tokens"]
        * PRICING_USD_PER_1M["fresh_input_tokens"]
        / 1_000_000,
        "cached_input_cost_usd": token_row["cached_input_tokens"]
        * PRICING_USD_PER_1M["cached_input_tokens"]
        / 1_000_000,
        "cache_write_input_cost_usd": token_row["cache_write_input_tokens"]
        * PRICING_USD_PER_1M["cache_write_input_tokens"]
        / 1_000_000,
        "visible_output_cost_usd": token_row["visible_output_tokens"]
        * PRICING_USD_PER_1M["visible_output_tokens"]
        / 1_000_000,
        "reasoning_output_cost_usd": token_row["reasoning_output_tokens"]
        * PRICING_USD_PER_1M["reasoning_output_tokens"]
        / 1_000_000,
    }
    cost["total_cost_usd"] = sum(cost.values())
    return cost


def fmt_tokens(value: float) -> str:
    value = float(value)
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f}M"
    if value >= 10_000:
        return f"{value / 1000:.0f}k"
    if value >= 1000:
        return f"{value / 1000:.1f}k"
    return f"{value:.0f}"


def fmt_usd(value: float) -> str:
    value = float(value)
    if value >= 100:
        return f"${value:,.0f}"
    if value >= 1:
        return f"${value:,.2f}"
    if value >= 0.01:
        return f"${value:.3f}"
    if value > 0:
        return f"${value:.5f}"
    return "$0"


def svg_text(x: float, y: float, text: str, size: int = 13, anchor: str = "start", weight: str = "400") -> str:
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" font-family="Arial, sans-serif" '
        f'font-size="{size}" font-weight="{weight}" text-anchor="{anchor}" fill="#202124">'
        f"{html.escape(text)}</text>"
    )


def write_chart(
    path: Path,
    title: str,
    rows: dict,
    components: tuple,
    total_key: str,
    formatter,
    notes: list[str] | None = None,
) -> None:
    width = 980
    height = 320
    margin_left = 160
    bar_x = margin_left
    bar_width = 640
    bar_height = 38
    row_gap = 72
    first_y = 96
    max_total = max((rows.get(strategy, {}).get(total_key, 0) for strategy in STRATEGIES), default=0) or 1

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#ffffff"/>',
        svg_text(24, 34, title, size=18, weight="700"),
    ]

    legend_x = 24
    legend_y = 62
    for key, label, color in components:
        parts.append(f'<rect x="{legend_x}" y="{legend_y - 11}" width="12" height="12" fill="{color}" rx="2"/>')
        parts.append(svg_text(legend_x + 18, legend_y, label, size=12))
        legend_x += 148

    for index, strategy in enumerate(STRATEGIES):
        row = rows.get(strategy)
        y = first_y + index * row_gap
        label = "MCP" if strategy == "notion-mcp" else "Locality"
        total = row.get(total_key, 0) if row else 0
        parts.append(svg_text(24, y + 25, label, size=14, weight="700"))
        parts.append(f'<rect x="{bar_x}" y="{y}" width="{bar_width}" height="{bar_height}" fill="#f1f3f4" rx="4"/>')
        x = bar_x
        if row:
            for key, component_label, color in components:
                value = row.get(key, 0)
                if value <= 0:
                    continue
                segment_width = bar_width * value / max_total
                parts.append(
                    f'<rect x="{x:.3f}" y="{y}" width="{segment_width:.3f}" '
                    f'height="{bar_height}" fill="{color}" rx="3"/>'
                )
                if segment_width >= 58:
                    parts.append(
                        svg_text(
                            x + segment_width / 2,
                            y + 24,
                            formatter(value),
                            size=11,
                            anchor="middle",
                            weight="700",
                        )
                    )
                x += segment_width
        else:
            parts.append(svg_text(bar_x + 12, y + 24, "missing", size=12))
        parts.append(svg_text(bar_x + bar_width + 18, y + 24, formatter(total), size=13, weight="700"))

    axis_y = first_y + 2 * row_gap - 14
    parts.append(f'<line x1="{bar_x}" y1="{axis_y}" x2="{bar_x + bar_width}" y2="{axis_y}" stroke="#dadce0"/>')
    for tick in range(5):
        value = max_total * tick / 4
        x = bar_x + bar_width * tick / 4
        parts.append(f'<line x1="{x:.1f}" y1="{axis_y}" x2="{x:.1f}" y2="{axis_y + 5}" stroke="#dadce0"/>')
        parts.append(svg_text(x, axis_y + 21, formatter(value), size=11, anchor="middle"))

    notes = notes or []
    note_y = height - 48
    for note in notes[:2]:
        parts.append(svg_text(24, note_y, note, size=12))
        note_y += 18

    parts.append("</svg>")
    path.write_text("\n".join(parts) + "\n")


summary_paths = sorted(path for path in root.glob("**/summary.json") if is_run_summary(path))
groups: dict[tuple[str, str], dict] = {}
for summary_path in summary_paths:
    data = json.loads(summary_path.read_text())
    trial = trial_id_for_summary(summary_path)
    for scenario_name, scenario_data in sorted(data.get("scenarios", {}).items()):
        agent_summaries = scenario_data.get("agent_event_summaries") or {}
        key = (trial, scenario_name)
        group = groups.setdefault(
            key,
            {
                "trial": trial,
                "scenario": scenario_name,
                "source_summaries": [],
                "strategies": {},
            },
        )
        group["source_summaries"].append(str(summary_path))
        for strategy, event_summary in agent_summaries.items():
            if strategy not in STRATEGIES:
                continue
            group["strategies"][strategy] = token_components(event_summary.get("usage") or {})

records = []
cost_records = []
paired_groups = []
for (trial, scenario), group in sorted(groups.items()):
    strategies = group["strategies"]
    chart_name = f"{slug(trial)}__{slug(scenario)}.svg"
    chart_path = by_scenario_dir / chart_name
    cost_chart_path = cost_by_scenario_dir / chart_name
    notes = []
    missing = [strategy for strategy in STRATEGIES if strategy not in strategies]
    if missing:
        notes.append("Missing strategy data: " + ", ".join(missing))
    write_chart(chart_path, f"Token Usage: {trial} / {scenario}", strategies, COMPONENTS, "total_tokens", fmt_tokens, notes)
    cost_strategies = {
        strategy: {**cost_components(row), "total_tokens": row["total_tokens"]}
        for strategy, row in strategies.items()
    }
    write_chart(
        cost_chart_path,
        f"Token Cost: {trial} / {scenario}",
        cost_strategies,
        COST_COMPONENTS,
        "total_cost_usd",
        fmt_usd,
        notes,
    )
    group["chart"] = str(chart_path)
    group["cost_chart"] = str(cost_chart_path)
    if all(strategy in strategies for strategy in STRATEGIES):
        paired_groups.append(group)
    for strategy in STRATEGIES:
        row = strategies.get(strategy)
        if not row:
            continue
        cost_row = cost_components(row)
        records.append(
            {
                "trial": trial,
                "scenario": scenario,
                "strategy": strategy,
                **row,
                "chart": str(chart_path),
            }
        )
        cost_records.append(
            {
                "trial": trial,
                "scenario": scenario,
                "strategy": strategy,
                **cost_row,
                "chart": str(cost_chart_path),
            }
        )

average_source_groups = paired_groups if paired_groups else list(groups.values())
average = {strategy: {key: 0.0 for key, _, _ in COMPONENTS} for strategy in STRATEGIES}
cost_average = {strategy: {key: 0.0 for key, _, _ in COST_COMPONENTS} for strategy in STRATEGIES}
average_counts = {strategy: 0 for strategy in STRATEGIES}
for group in average_source_groups:
    for strategy in STRATEGIES:
        row = group["strategies"].get(strategy)
        if not row:
            continue
        cost_row = cost_components(row)
        average_counts[strategy] += 1
        for key, _, _ in COMPONENTS:
            average[strategy][key] += row.get(key, 0)
        for key, _, _ in COST_COMPONENTS:
            cost_average[strategy][key] += cost_row.get(key, 0)

average_rows = {}
cost_average_rows = {}
for strategy, values in average.items():
    count = average_counts[strategy]
    if count == 0:
        continue
    row = {key: value / count for key, value in values.items()}
    row["total_tokens"] = sum(row[key] for key, _, _ in COMPONENTS)
    average_rows[strategy] = row
    cost_row = {key: value / count for key, value in cost_average[strategy].items()}
    cost_row["total_cost_usd"] = sum(cost_row[key] for key, _, _ in COST_COMPONENTS)
    cost_average_rows[strategy] = cost_row

average_chart = out_dir / "average.svg"
write_chart(
    average_chart,
    "Average Token Usage Across Scenarios And Trials",
    average_rows,
    COMPONENTS,
    "total_tokens",
    fmt_tokens,
    [f"Average uses {len(average_source_groups)} paired scenario/trial group(s)." if paired_groups else "Average uses available unpaired groups."],
)

cost_average_chart = cost_dir / "average.svg"
write_chart(
    cost_average_chart,
    "Average Token Cost Across Scenarios And Trials",
    cost_average_rows,
    COST_COMPONENTS,
    "total_cost_usd",
    fmt_usd,
    [f"Average uses {len(average_source_groups)} paired scenario/trial group(s)." if paired_groups else "Average uses available unpaired groups."],
)

tsv_path = out_dir / "token-usage.tsv"
fieldnames = [
    "trial",
    "scenario",
    "strategy",
    "fresh_input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "visible_output_tokens",
    "reasoning_output_tokens",
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "chart",
]
with tsv_path.open("w", newline="") as f:
    writer = csv.DictWriter(f, fieldnames=fieldnames, delimiter="\t")
    writer.writeheader()
    for record in records:
        writer.writerow({key: record.get(key, "") for key in fieldnames})

cost_tsv_path = out_dir / "cost-usage.tsv"
cost_fieldnames = [
    "trial",
    "scenario",
    "strategy",
    "fresh_input_cost_usd",
    "cached_input_cost_usd",
    "cache_write_input_cost_usd",
    "visible_output_cost_usd",
    "reasoning_output_cost_usd",
    "total_cost_usd",
    "chart",
]
with cost_tsv_path.open("w", newline="") as f:
    writer = csv.DictWriter(f, fieldnames=cost_fieldnames, delimiter="\t")
    writer.writeheader()
    for record in cost_records:
        writer.writerow({key: record.get(key, "") for key in cost_fieldnames})

manifest = {
    "root": str(root),
    "out_dir": str(out_dir),
    "summary_count": len(summary_paths),
    "trial_scenario_count": len(groups),
    "paired_trial_scenario_count": len(paired_groups),
    "average_chart": str(average_chart),
    "cost_average_chart": str(cost_average_chart),
    "records_tsv": str(tsv_path),
    "cost_records_tsv": str(cost_tsv_path),
    "pricing_usd_per_1m_tokens": PRICING_USD_PER_1M,
    "charts": [
        {
            "trial": group["trial"],
            "scenario": group["scenario"],
            "chart": group["chart"],
            "cost_chart": group["cost_chart"],
            "strategies": sorted(group["strategies"].keys()),
            "source_summaries": sorted(set(group["source_summaries"])),
        }
        for group in sorted(groups.values(), key=lambda item: (item["trial"], item["scenario"]))
    ],
}
manifest_path = out_dir / "token-usage.json"
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

index_lines = [
    "# Token Usage Charts",
    "",
    f"Root: `{root}`",
    f"Paired scenario/trial groups: `{len(paired_groups)}`",
    "",
    f"- Average chart: `{average_chart.name}`",
    f"- Average cost chart: `cost/{cost_average_chart.name}`",
    f"- Records TSV: `{tsv_path.name}`",
    f"- Cost TSV: `{cost_tsv_path.name}`",
    f"- Manifest: `{manifest_path.name}`",
    f"- Pricing: `{PRICING_USD_PER_1M}` USD per 1M tokens",
    "",
    "## Trial Scenario Charts",
    "",
]
for chart in manifest["charts"]:
    index_lines.append(
        f"- `{chart['trial']}` / `{chart['scenario']}`: "
        f"`by-trial-scenario/{Path(chart['chart']).name}`, "
        f"`cost/by-trial-scenario/{Path(chart['cost_chart']).name}`"
    )
index_path = out_dir / "index.md"
index_path.write_text("\n".join(index_lines) + "\n")

print(manifest_path)
print(average_chart)
print(cost_average_chart)
