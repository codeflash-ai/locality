#!/usr/bin/env node

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

function main(argv) {
  const batchDir = resolve(argv[0] ?? "");
  if (!argv[0] || !existsSync(join(batchDir, "batch-summary.json"))) {
    console.error("Usage: node summarize-natural-batch.mjs <batch-dir>");
    process.exit(2);
  }

  const batch = JSON.parse(readFileSync(join(batchDir, "batch-summary.json"), "utf8"));
  const summary = buildNormalizedSummary(batch);
  writeFileSync(
    join(batchDir, "normalized-summary.json"),
    JSON.stringify(summary, null, 2) + "\n",
  );
  writeFileSync(join(batchDir, "normalized-summary.md"), renderMarkdown(summary));
  console.log(`Wrote normalized natural retrieval summary to ${batchDir}`);
}

function buildNormalizedSummary(batch) {
  const runs = flattenRuns(batch);
  const strategies = groupBy(runs, (run) => run.strategy_id);
  const scenarios = groupBy(runs, (run) => `${run.scenario_id}/${run.strategy_id}`);
  const pairs = pairRuns(runs);
  const pairRatios = pairs
    .map((pair) => ratioForPair(pair))
    .filter(Boolean);

  return {
    batch_id: batch.batch_id,
    model: batch.model,
    reasoning_effort: batch.reasoning_effort,
    generated_at: new Date().toISOString(),
    run_count: runs.length,
    strategy_aggregates: Object.fromEntries(
      [...strategies.entries()].map(([strategy, rows]) => [
        strategy,
        aggregateRuns(rows),
      ]),
    ),
    scenario_strategy_aggregates: Object.fromEntries(
      [...scenarios.entries()].map(([key, rows]) => [
        key,
        aggregateRuns(rows),
      ]),
    ),
    pairwise_mcp_over_locality: aggregateRatios(pairRatios),
    pairwise_samples: pairRatios,
    slowest_runs: [...runs]
      .sort((left, right) => right.duration_ms - left.duration_ms)
      .slice(0, 8),
  };
}

function flattenRuns(batch) {
  return batch.pairs.flatMap((pair) =>
    pair.runs.map((run) => ({
      scenario_id: pair.scenario_id,
      variant_id: pair.variant_id,
      repeat: pair.repeat,
      strategy_id: run.strategy_id,
      status: run.status,
      duration_ms: numberOrZero(run.duration_ms),
      input_tokens: numberOrZero(run.usage?.input_tokens),
      cached_input_tokens: numberOrZero(run.usage?.cached_input_tokens),
      output_tokens: numberOrZero(run.usage?.output_tokens),
      mcp_tool_calls: numberOrZero(run.tool_counts?.mcp_tool_call),
      report_path: run.report_path,
    })),
  );
}

function aggregateRuns(rows) {
  return {
    count: rows.length,
    ok: rows.filter((run) => run.status === "ok").length,
    failed: rows.filter((run) => run.status !== "ok").length,
    duration_ms: stats(rows.map((run) => run.duration_ms)),
    input_tokens: stats(rows.map((run) => run.input_tokens)),
    cached_input_tokens: stats(rows.map((run) => run.cached_input_tokens)),
    output_tokens: stats(rows.map((run) => run.output_tokens)),
    mcp_tool_calls: stats(rows.map((run) => run.mcp_tool_calls)),
  };
}

function pairRuns(runs) {
  const grouped = groupBy(
    runs,
    (run) => `${run.scenario_id}/${run.variant_id}/${run.repeat}`,
  );
  return [...grouped.values()]
    .map((rows) => ({
      locality: rows.find((run) => run.strategy_id === "locality-natural"),
      mcp: rows.find((run) => run.strategy_id === "notion-mcp-natural"),
    }))
    .filter((pair) => pair.locality && pair.mcp);
}

function ratioForPair(pair) {
  const { locality, mcp } = pair;
  if (
    locality.status !== "ok" ||
    mcp.status !== "ok" ||
    locality.duration_ms <= 0 ||
    locality.input_tokens <= 0
  ) {
    return null;
  }
  return {
    scenario_id: locality.scenario_id,
    variant_id: locality.variant_id,
    repeat: locality.repeat,
    duration_ratio: mcp.duration_ms / locality.duration_ms,
    input_token_ratio: mcp.input_tokens / locality.input_tokens,
    output_token_ratio:
      locality.output_tokens > 0 ? mcp.output_tokens / locality.output_tokens : null,
    mcp_tool_calls: mcp.mcp_tool_calls,
    locality_duration_ms: locality.duration_ms,
    mcp_duration_ms: mcp.duration_ms,
    locality_input_tokens: locality.input_tokens,
    mcp_input_tokens: mcp.input_tokens,
  };
}

function aggregateRatios(rows) {
  return {
    count: rows.length,
    duration_ratio: stats(rows.map((row) => row.duration_ratio)),
    input_token_ratio: stats(rows.map((row) => row.input_token_ratio)),
    output_token_ratio: stats(
      rows.map((row) => row.output_token_ratio).filter((value) => value !== null),
    ),
    mcp_tool_calls: stats(rows.map((row) => row.mcp_tool_calls)),
  };
}

function renderMarkdown(summary) {
  const lines = [
    "# Normalized Natural Retrieval Summary",
    "",
    `Batch: \`${summary.batch_id}\``,
    `Model: \`${summary.model}\``,
    `Reasoning effort: \`${summary.reasoning_effort}\``,
    "",
    "## Strategy Aggregates",
    "",
    "| Strategy | Runs | OK | Mean wall | Median wall | Min wall | Max wall | Mean input | Median input | Mean output | Mean MCP calls |",
    "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];

  for (const [strategy, aggregate] of Object.entries(summary.strategy_aggregates)) {
    lines.push(
      `| ${strategy} | ${aggregate.count} | ${aggregate.ok} | ${formatMs(aggregate.duration_ms.mean)} | ${formatMs(aggregate.duration_ms.median)} | ${formatMs(aggregate.duration_ms.min)} | ${formatMs(aggregate.duration_ms.max)} | ${formatInteger(aggregate.input_tokens.mean)} | ${formatInteger(aggregate.input_tokens.median)} | ${formatInteger(aggregate.output_tokens.mean)} | ${formatNumber(aggregate.mcp_tool_calls.mean)} |`,
    );
  }

  lines.push(
    "",
    "## Pairwise MCP Over Locality",
    "",
    "Ratios compare the matched MCP run against the Locality run for the same scenario, prompt variant, and repeat. A value above `1.00x` means the MCP run used more of that resource.",
    "",
    "| Metric | Count | Mean | Median | Min | Max |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
    ratioRow("Wall time", summary.pairwise_mcp_over_locality.duration_ratio),
    ratioRow("Input tokens", summary.pairwise_mcp_over_locality.input_token_ratio),
    ratioRow("Output tokens", summary.pairwise_mcp_over_locality.output_token_ratio),
    ratioValueRow("MCP tool calls", summary.pairwise_mcp_over_locality.mcp_tool_calls),
    "",
    "## Scenario Aggregates",
    "",
    "| Scenario / Strategy | Runs | Mean wall | Median wall | Mean input | Mean MCP calls |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
  );

  for (const [key, aggregate] of Object.entries(summary.scenario_strategy_aggregates)) {
    lines.push(
      `| ${key} | ${aggregate.count} | ${formatMs(aggregate.duration_ms.mean)} | ${formatMs(aggregate.duration_ms.median)} | ${formatInteger(aggregate.input_tokens.mean)} | ${formatNumber(aggregate.mcp_tool_calls.mean)} |`,
    );
  }

  lines.push(
    "",
    "## Slowest Runs",
    "",
    "| Scenario | Variant | Repeat | Strategy | Wall time | Input tokens | MCP calls | Report |",
    "| --- | --- | ---: | --- | ---: | ---: | ---: | --- |",
  );
  for (const run of summary.slowest_runs) {
    lines.push(
      `| ${run.scenario_id} | ${run.variant_id} | ${run.repeat} | ${run.strategy_id} | ${formatMs(run.duration_ms)} | ${formatInteger(run.input_tokens)} | ${formatNumber(run.mcp_tool_calls)} | ${run.report_path} |`,
    );
  }

  return `${lines.join("\n")}\n`;
}

function ratioRow(label, metric) {
  return `| ${label} | ${metric.count} | ${formatRatio(metric.mean)} | ${formatRatio(metric.median)} | ${formatRatio(metric.min)} | ${formatRatio(metric.max)} |`;
}

function ratioValueRow(label, metric) {
  return `| ${label} | ${metric.count} | ${formatNumber(metric.mean)} | ${formatNumber(metric.median)} | ${formatNumber(metric.min)} | ${formatNumber(metric.max)} |`;
}

function groupBy(values, keyFn) {
  const groups = new Map();
  for (const value of values) {
    const key = keyFn(value);
    const group = groups.get(key) ?? [];
    group.push(value);
    groups.set(key, group);
  }
  return groups;
}

function stats(values) {
  const cleaned = values.filter((value) => Number.isFinite(value));
  if (cleaned.length === 0) {
    return { count: 0, mean: 0, median: 0, min: 0, max: 0, stddev: 0 };
  }
  const sorted = [...cleaned].sort((left, right) => left - right);
  const mean = sorted.reduce((sum, value) => sum + value, 0) / sorted.length;
  const variance =
    sorted.reduce((sum, value) => sum + (value - mean) ** 2, 0) / sorted.length;
  return {
    count: sorted.length,
    mean,
    median:
      sorted.length % 2 === 0
        ? (sorted[sorted.length / 2 - 1] + sorted[sorted.length / 2]) / 2
        : sorted[Math.floor(sorted.length / 2)],
    min: sorted[0],
    max: sorted[sorted.length - 1],
    stddev: Math.sqrt(variance),
  };
}

function numberOrZero(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}

function formatMs(ms) {
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatInteger(value) {
  return Math.round(value).toLocaleString("en-US");
}

function formatNumber(value) {
  return Number(value).toFixed(2);
}

function formatRatio(value) {
  return `${Number(value).toFixed(2)}x`;
}

main(process.argv.slice(2));
