#!/usr/bin/env node

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";

const DEFAULT_DURATION_MS = 1000;
const TIMESTAMP_KEYS = ["timestamp", "created_at", "time", "ts"];
const DURATION_KEYS = [
  "duration_ms",
  "durationMs",
  "elapsed_ms",
  "elapsedMs",
  "latency_ms",
  "latencyMs",
];

class UsageError extends Error {
  constructor(message) {
    super(message);
    this.name = "UsageError";
    this.exitCode = 2;
  }
}

function main(argv) {
  const options = parseArgs(argv);
  const input = resolve(options.input);
  const outDir = resolve(options.out);
  const label = options.label ?? labelFromPath(input);
  const baseName = sanitizeFilename(label);
  const defaultDurationMs = options.defaultDurationMs ?? DEFAULT_DURATION_MS;

  const records = readJsonl(input);
  const profile = buildProfile(records, label, defaultDurationMs);

  mkdirSync(outDir, { recursive: true });
  const outputs = {
    folded: join(outDir, `${baseName}.folded`),
    speedscope: join(outDir, `${baseName}.speedscope.json`),
    svg: join(outDir, `${baseName}.svg`),
    summaryJson: join(outDir, `${baseName}.summary.json`),
    summaryMarkdown: join(outDir, `${baseName}.summary.md`),
    timeline: join(outDir, `${baseName}.timeline.tsv`),
  };

  writeFolded(outputs.folded, profile.folded);
  writeJson(outputs.speedscope, buildSpeedscope(profile));
  writeFileSync(outputs.svg, renderSvg(profile));
  writeJson(outputs.summaryJson, summaryFor(profile, input, outputs));
  writeFileSync(outputs.summaryMarkdown, renderSummaryMarkdown(profile, input, outputs));
  writeTimeline(outputs.timeline, profile.profileEntries);

  console.log(`Wrote Codex session flamegraph artifacts to ${outDir}`);
  for (const output of Object.values(outputs)) {
    console.log(output);
  }
}

function parseArgs(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--help":
      case "-h":
        printUsage();
        process.exit(0);
        break;
      case "--input":
      case "-i":
        options.input = readFlagValue(argv, ++index, arg);
        break;
      case "--label":
        options.label = readFlagValue(argv, ++index, arg);
        break;
      case "--out":
      case "-o":
        options.out = readFlagValue(argv, ++index, arg);
        break;
      case "--default-duration-ms": {
        const value = Number(readFlagValue(argv, ++index, arg));
        if (!Number.isFinite(value) || value <= 0) {
          throw new UsageError("--default-duration-ms must be a positive number");
        }
        options.defaultDurationMs = Math.round(value);
        break;
      }
      default:
        if (!options.input && !arg.startsWith("-")) {
          options.input = arg;
        } else {
          throw new UsageError(`unknown argument: ${arg}`);
        }
    }
  }

  if (!options.input || !options.out) {
    throw new UsageError("missing required --input or --out argument");
  }
  return options;
}

function readFlagValue(argv, index, flag) {
  const value = argv[index];
  if (!value || value.startsWith("--")) {
    throw new UsageError(`${flag} requires a value`);
  }
  return value;
}

function printUsage() {
  console.error(`Usage:
  node experiment/codex-session-flamegraph.mjs \\
    --input ~/.codex/sessions/.../rollout.jsonl \\
    --label grocery-sf-codex \\
    --out target/codex-session-flamegraphs/grocery-sf

Outputs:
  <label>.folded           Folded stack file compatible with FlameGraph tooling.
  <label>.speedscope.json  Speedscope sampled flamegraph profile.
  <label>.svg              Self-contained SVG flamegraph.
  <label>.timeline.tsv     Profile events used to build the stacks.
  <label>.summary.{json,md}
`);
}

function readJsonl(path) {
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    throw new UsageError(`failed to read ${path}: ${error.message}`);
  }
  const records = [];
  text.split(/\r?\n/).forEach((line, index) => {
    if (line.trim() === "") {
      return;
    }
    try {
      records.push({ sourceIndex: index, raw: JSON.parse(line) });
    } catch (error) {
      throw new UsageError(
        `malformed input ${path}: invalid JSON on line ${index + 1}: ${error.message}`,
      );
    }
  });
  if (records.length === 0) {
    throw new UsageError(`malformed input ${path}: file is empty`);
  }
  return records;
}

function buildProfile(records, label, defaultDurationMs) {
  const normalized = records
    .map(({ raw, sourceIndex }) => normalizeRecord(raw, sourceIndex))
    .filter((event) => event && event.startMs !== null)
    .sort(
      (left, right) =>
        left.startMs - right.startMs || left.sourceIndex - right.sourceIndex,
    );

  const turnLabels = turnLabelsFor(normalized);
  const toolCallsById = new Map();
  for (const event of normalized) {
    if (event.kind === "tool_call" && event.toolCallId) {
      toolCallsById.set(event.toolCallId, event);
    }
  }

  for (const event of normalized) {
    if (event.kind !== "tool_result") {
      continue;
    }
    const call = event.toolCallId ? toolCallsById.get(event.toolCallId) : null;
    if (!call) {
      continue;
    }
    call.endMs = Math.max(event.startMs, call.startMs + 1);
    call.durationMs = call.endMs - call.startMs;
    call.timingQuality = "measured";
    call.status = event.status;
    call.exitCode = event.exitCode;
    call.outputExcerpt = event.excerpt;
  }

  const profileEntries = [];
  for (let index = 0; index < normalized.length; index += 1) {
    const event = normalized[index];
    if (!isProfileEvent(event)) {
      continue;
    }

    if (event.kind !== "tool_call") {
      const explicitDuration = event.explicitDurationMs;
      const durationMs =
        explicitDuration ??
        Math.max(
          1,
          (nextTimestampAfter(normalized, index) ?? event.startMs + defaultDurationMs) -
            event.startMs,
        );
      event.durationMs = durationMs;
      event.endMs = event.startMs + durationMs;
      event.timingQuality = explicitDuration === null ? "inferred" : "measured";
    } else if (event.durationMs === null) {
      const fallbackEnd = nextTimestampAfter(normalized, index) ?? event.startMs + defaultDurationMs;
      event.durationMs = Math.max(1, fallbackEnd - event.startMs);
      event.endMs = event.startMs + event.durationMs;
      event.timingQuality = "inferred";
    }

    profileEntries.push({
      ...event,
      activity: activityForKind(event.kind),
      turnLabel: turnLabels.get(event.turnId) ?? "session",
    });
  }

  const folded = foldedStacksFor(label, profileEntries);
  const firstMs = normalized[0]?.startMs ?? 0;
  const lastMs = normalized[normalized.length - 1]?.startMs ?? firstMs;
  return {
    label,
    records: normalized,
    profileEntries,
    folded,
    firstMs,
    lastMs,
    wallTimeMs: Math.max(0, lastMs - firstMs),
  };
}

function normalizeRecord(record, sourceIndex) {
  const payload = isPlainObject(record.payload) ? record.payload : record;
  const startMs = readTimestamp(record) ?? readTimestamp(payload);
  const payloadType = stringOrNull(payload.type) ?? stringOrNull(record.type) ?? "unknown";
  const turnId = turnIdFor(record, payload);
  const explicitDurationMs = readDuration(payload) ?? readDuration(record);

  if (record.type === "session_meta" || record.type === "world_state" || record.type === "turn_context") {
    return metadataEvent(record, payload, sourceIndex, startMs, payloadType, turnId);
  }

  switch (payload.type) {
    case "message": {
      const role = stringOrNull(payload.role);
      const text = contentText(payload.content);
      if (role === "assistant") {
        return baseEvent({
          kind: "assistant_message",
          rawType: "message",
          recordType: "message",
          sourceIndex,
          startMs,
          turnId,
          explicitDurationMs,
          excerpt: text,
        });
      }
      if (role === "user" && !isHarnessUserMessage(text)) {
        return baseEvent({
          kind: "user",
          rawType: "message",
          recordType: "message",
          sourceIndex,
          startMs,
          turnId,
          explicitDurationMs,
          excerpt: text,
        });
      }
      return metadataEvent(record, payload, sourceIndex, startMs, payloadType, turnId);
    }
    case "reasoning":
      return baseEvent({
        kind: "reasoning",
        rawType: "reasoning",
        recordType: "reasoning",
        sourceIndex,
        startMs,
        turnId,
        explicitDurationMs,
        excerpt: reasoningText(payload),
      });
    case "function_call": {
      const parsedArguments = parseJsonObject(payload.arguments) ?? {};
      const command = commandFromArguments(parsedArguments);
      const toolName = stringOrNull(payload.name) ?? "unknown_tool";
      return baseEvent({
        kind: "tool_call",
        rawType: "function_call",
        recordType: "function_call",
        sourceIndex,
        startMs,
        turnId,
        explicitDurationMs,
        excerpt: command || payload.arguments || toolName,
        toolName,
        toolGroup: toolGroupFor(toolName, command, parsedArguments),
        toolCallId: stringOrNull(payload.call_id),
        toolCommand: command,
      });
    }
    case "function_call_output": {
      const output = String(payload.output ?? "");
      const parsedStatus = outputStatus(output);
      return baseEvent({
        kind: "tool_result",
        rawType: "function_call_output",
        recordType: "function_call_output",
        sourceIndex,
        startMs,
        turnId,
        explicitDurationMs,
        excerpt: outputExcerpt(output),
        toolCallId: stringOrNull(payload.call_id),
        status: parsedStatus.status,
        exitCode: parsedStatus.exitCode,
      });
    }
    default:
      return metadataEvent(record, payload, sourceIndex, startMs, payloadType, turnId);
  }
}

function baseEvent({
  kind,
  rawType,
  recordType,
  sourceIndex,
  startMs,
  turnId,
  explicitDurationMs = null,
  excerpt = "",
  toolName = null,
  toolGroup = null,
  toolCallId = null,
  toolCommand = null,
  status = null,
  exitCode = null,
}) {
  return {
    kind,
    rawType,
    recordType,
    sourceIndex,
    startMs,
    endMs: null,
    durationMs: null,
    explicitDurationMs,
    timingQuality: null,
    turnId,
    excerpt: shorten(excerpt, 400),
    toolName,
    toolGroup,
    toolCallId,
    toolCommand,
    status,
    exitCode,
  };
}

function metadataEvent(record, payload, sourceIndex, startMs, rawType, turnId) {
  return baseEvent({
    kind: "metadata",
    rawType,
    recordType: stringOrNull(payload.type) ?? stringOrNull(record.type),
    sourceIndex,
    startMs,
    turnId,
    excerpt: payload.message ?? payload.last_agent_message ?? record.type ?? rawType,
  });
}

function isProfileEvent(event) {
  return ["assistant_message", "reasoning", "tool_call", "user"].includes(event.kind);
}

function activityForKind(kind) {
  switch (kind) {
    case "assistant_message":
      return "agent_response";
    case "reasoning":
      return "reasoning";
    case "tool_call":
      return "tool";
    case "user":
      return "user_query";
    default:
      return "other";
  }
}

function turnLabelsFor(events) {
  const labels = new Map();
  let fallbackIndex = 0;
  for (const event of events) {
    if (event.kind !== "user" || !event.turnId || labels.has(event.turnId)) {
      continue;
    }
    fallbackIndex += 1;
    labels.set(
      event.turnId,
      `${fallbackIndex} ${shorten(event.excerpt, 70) || event.turnId.slice(0, 8)}`,
    );
  }
  return labels;
}

function nextTimestampAfter(events, index) {
  const startMs = events[index].startMs;
  for (let cursor = index + 1; cursor < events.length; cursor += 1) {
    if (events[cursor].startMs > startMs) {
      return events[cursor].startMs;
    }
  }
  return null;
}

function foldedStacksFor(label, profileEntries) {
  const stacks = new Map();
  for (const event of profileEntries) {
    const frames = [
      `conversation:${label}`,
      `turn:${event.turnLabel}`,
      `activity:${event.activity}`,
    ];
    if (event.activity === "tool") {
      frames.push(`tool:${event.toolGroup ?? event.toolName ?? "unknown_tool"}`);
      frames.push(`status:${event.status ?? "unknown"}`);
    }
    frames.push(`timing:${event.timingQuality}`);

    const key = frames.map(sanitizeFrame).join(";");
    const weightUs = Math.max(1, Math.round(event.durationMs * 1000));
    stacks.set(key, (stacks.get(key) ?? 0) + weightUs);
  }
  return [...stacks.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([stack, weightUs]) => ({ stack, weightUs }));
}

function buildSpeedscope(profile) {
  const frameIndex = new Map();
  const frames = [];
  const samples = [];
  const weights = [];

  function frameFor(name) {
    if (!frameIndex.has(name)) {
      frameIndex.set(name, frames.length);
      frames.push({ name });
    }
    return frameIndex.get(name);
  }

  for (const entry of profile.folded) {
    samples.push(entry.stack.split(";").map(frameFor));
    weights.push(entry.weightUs / 1000);
  }

  return {
    $schema: "https://www.speedscope.app/file-format-schema.json",
    exporter: "codex-session-flamegraph",
    name: profile.label,
    activeProfileIndex: 0,
    shared: { frames },
    profiles: [
      {
        type: "sampled",
        name: `${profile.label} folded stack`,
        unit: "milliseconds",
        startValue: 0,
        endValue: weights.reduce((sum, value) => sum + value, 0),
        samples,
        weights,
      },
    ],
    metadata: {
      wall_time_ms: profile.wallTimeMs,
      timing_note:
        "Tool calls are measured from function_call to matching function_call_output. Non-tool events infer duration from the next timestamp.",
    },
  };
}

function renderSvg(profile) {
  const tree = buildTree(profile.folded);
  const width = 1400;
  const frameHeight = 19;
  const top = 54;
  const bottom = 32;
  const side = 12;
  const maxDepth = treeDepth(tree);
  const height = top + bottom + maxDepth * frameHeight;
  const contentWidth = width - side * 2;
  const rects = [];

  function drawChildren(node, x, depth, scale) {
    let cursor = x;
    const children = [...node.children.values()].sort(
      (left, right) => right.value - left.value || left.name.localeCompare(right.name),
    );
    for (const child of children) {
      const childWidth = child.value * scale;
      drawNode(child, cursor, depth, childWidth);
      drawChildren(child, cursor, depth + 1, scale);
      cursor += childWidth;
    }
  }

  function drawNode(node, x, depth, nodeWidth) {
    if (nodeWidth < 0.5) {
      return;
    }
    const y = top + depth * frameHeight;
    const label = `${node.name} (${formatDurationMs(node.value / 1000)})`;
    const fill = colorFor(node.name);
    const id = `clip-${rects.length}`;
    rects.push({ id, x, y, width: nodeWidth, label, fill });
  }

  const scale = tree.value > 0 ? contentWidth / tree.value : 1;
  drawChildren(tree, side, 0, scale);

  const defs = rects
    .map(
      (rect) =>
        `<clipPath id="${rect.id}"><rect x="${rect.x.toFixed(3)}" y="${rect.y}" width="${Math.max(0, rect.width - 1).toFixed(3)}" height="${frameHeight - 2}"/></clipPath>`,
    )
    .join("\n");
  const body = rects
    .map((rect) => {
      const text = rect.width > 42
        ? `<text x="${(rect.x + 4).toFixed(3)}" y="${rect.y + 13}" clip-path="url(#${rect.id})">${escapeXml(rect.label)}</text>`
        : "";
      return `<g>
  <title>${escapeXml(rect.label)}</title>
  <rect x="${rect.x.toFixed(3)}" y="${rect.y}" width="${Math.max(0, rect.width - 1).toFixed(3)}" height="${frameHeight - 2}" fill="${rect.fill}" rx="2" ry="2"/>
  ${text}
</g>`;
    })
    .join("\n");

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">
<style>
  text { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; font-size: 12px; fill: #111827; }
  .title { font-size: 18px; font-weight: 700; }
  .subtitle { font-size: 12px; fill: #4b5563; }
  rect { stroke: rgba(17, 24, 39, 0.35); stroke-width: 0.5; }
</style>
<rect width="100%" height="100%" fill="#fff"/>
<text class="title" x="${side}" y="24">${escapeXml(profile.label)} Codex session flamegraph</text>
<text class="subtitle" x="${side}" y="42">Wall time ${formatDurationMs(profile.wallTimeMs)}; profiled ${formatDurationMs(tree.value / 1000)}. Width is proportional to duration.</text>
<defs>
${defs}
</defs>
${body}
</svg>
`;
}

function buildTree(folded) {
  const root = { name: "root", value: 0, children: new Map() };
  for (const { stack, weightUs } of folded) {
    const frames = stack.split(";");
    root.value += weightUs;
    let node = root;
    for (const frame of frames) {
      if (!node.children.has(frame)) {
        node.children.set(frame, { name: frame, value: 0, children: new Map() });
      }
      node = node.children.get(frame);
      node.value += weightUs;
    }
  }
  return root;
}

function treeDepth(node) {
  if (node.children.size === 0) {
    return 0;
  }
  return 1 + Math.max(...[...node.children.values()].map(treeDepth));
}

function writeFolded(path, folded) {
  writeFileSync(
    path,
    folded.map(({ stack, weightUs }) => `${stack} ${weightUs}`).join("\n") + "\n",
  );
}

function writeTimeline(path, entries) {
  const header = [
    "source_index",
    "start_iso",
    "duration_ms",
    "activity",
    "kind",
    "tool_group",
    "status",
    "timing",
    "turn",
    "excerpt",
  ];
  const lines = [header.join("\t")];
  for (const entry of entries) {
    lines.push(
      [
        entry.sourceIndex + 1,
        new Date(entry.startMs).toISOString(),
        entry.durationMs,
        entry.activity,
        entry.kind,
        entry.toolGroup ?? "",
        entry.status ?? "",
        entry.timingQuality,
        entry.turnLabel,
        entry.excerpt.replace(/\t|\r?\n/g, " "),
      ].join("\t"),
    );
  }
  writeFileSync(path, lines.join("\n") + "\n");
}

function summaryFor(profile, input, outputs) {
  return {
    input,
    label: profile.label,
    outputs,
    record_count: profile.records.length,
    profile_entry_count: profile.profileEntries.length,
    wall_time_ms: profile.wallTimeMs,
    profiled_duration_ms: profile.folded.reduce(
      (sum, entry) => sum + entry.weightUs / 1000,
      0,
    ),
    activity_totals_ms: totalsBy(profile.profileEntries, (entry) => entry.activity),
    tool_totals_ms: totalsBy(
      profile.profileEntries.filter((entry) => entry.activity === "tool"),
      (entry) => entry.toolGroup ?? "unknown_tool",
    ),
    status_totals_ms: totalsBy(
      profile.profileEntries.filter((entry) => entry.activity === "tool"),
      (entry) => entry.status ?? "unknown",
    ),
    longest_events: [...profile.profileEntries]
      .sort((left, right) => right.durationMs - left.durationMs)
      .slice(0, 10)
      .map((entry) => ({
        source_index: entry.sourceIndex,
        activity: entry.activity,
        duration_ms: entry.durationMs,
        tool_group: entry.toolGroup,
        status: entry.status,
        timing: entry.timingQuality,
        turn: entry.turnLabel,
        excerpt: entry.excerpt,
      })),
  };
}

function renderSummaryMarkdown(profile, input, outputs) {
  const summary = summaryFor(profile, input, outputs);
  const lines = [
    `# Codex Session Flamegraph`,
    "",
    `Source: \`${input}\``,
    "",
    `Wall time: ${formatDurationMs(summary.wall_time_ms)}`,
    `Profiled duration: ${formatDurationMs(summary.profiled_duration_ms)}`,
    "",
    "## Outputs",
    "",
    `- Folded stack: \`${outputs.folded}\``,
    `- Speedscope: \`${outputs.speedscope}\``,
    `- SVG: \`${outputs.svg}\``,
    `- Timeline: \`${outputs.timeline}\``,
    "",
    "## Activity",
    "",
    "| Activity | Duration |",
    "| --- | ---: |",
  ];
  for (const [activity, durationMs] of Object.entries(summary.activity_totals_ms)) {
    lines.push(`| ${escapeMarkdown(activity)} | ${formatDurationMs(durationMs)} |`);
  }
  lines.push("", "## Tools", "", "| Tool | Duration |", "| --- | ---: |");
  for (const [tool, durationMs] of Object.entries(summary.tool_totals_ms)) {
    lines.push(`| ${escapeMarkdown(tool)} | ${formatDurationMs(durationMs)} |`);
  }
  lines.push("", "## Longest Events", "", "| Activity | Tool | Duration | Timing | Excerpt |", "| --- | --- | ---: | --- | --- |");
  for (const event of summary.longest_events) {
    lines.push(
      `| ${escapeMarkdown(event.activity)} | ${escapeMarkdown(event.tool_group ?? "")} | ${formatDurationMs(event.duration_ms)} | ${escapeMarkdown(event.timing)} | ${escapeMarkdown(shorten(event.excerpt, 120))} |`,
    );
  }
  return lines.join("\n") + "\n";
}

function totalsBy(entries, keyFor) {
  const totals = new Map();
  for (const entry of entries) {
    const key = keyFor(entry);
    totals.set(key, (totals.get(key) ?? 0) + entry.durationMs);
  }
  return Object.fromEntries(
    [...totals.entries()].sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0])),
  );
}

function readTimestamp(record) {
  if (!isPlainObject(record)) {
    return null;
  }
  for (const key of TIMESTAMP_KEYS) {
    if (Object.hasOwn(record, key)) {
      const parsed = parseTimestamp(record[key]);
      if (parsed !== null) {
        return parsed;
      }
    }
  }
  return null;
}

function parseTimestamp(value) {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value < 1e12 ? Math.round(value * 1000) : Math.round(value);
  }
  if (typeof value === "string") {
    const numeric = Number(value);
    if (Number.isFinite(numeric) && value.trim() !== "") {
      return parseTimestamp(numeric);
    }
    const parsed = Date.parse(value);
    if (Number.isFinite(parsed)) {
      return parsed;
    }
  }
  return null;
}

function readDuration(record) {
  if (!isPlainObject(record)) {
    return null;
  }
  for (const key of DURATION_KEYS) {
    if (!Object.hasOwn(record, key)) {
      continue;
    }
    const value = Number(record[key]);
    if (Number.isFinite(value) && value >= 0) {
      return Math.round(value);
    }
  }
  return null;
}

function turnIdFor(record, payload) {
  return stringOrNull(payload.internal_chat_message_metadata_passthrough?.turn_id)
    ?? stringOrNull(record.internal_chat_message_metadata_passthrough?.turn_id)
    ?? stringOrNull(payload.turn_id)
    ?? stringOrNull(record.turn_id);
}

function contentText(content) {
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((block) => {
      if (typeof block === "string") {
        return block;
      }
      if (!isPlainObject(block)) {
        return "";
      }
      return block.text ?? block.input_text ?? block.output_text ?? "";
    })
    .filter(Boolean)
    .join("\n");
}

function reasoningText(payload) {
  if (typeof payload.text === "string") {
    return payload.text;
  }
  if (Array.isArray(payload.summary)) {
    return payload.summary
      .map((part) => (typeof part === "string" ? part : part?.text ?? ""))
      .filter(Boolean)
      .join("\n");
  }
  return payload.encrypted_content ? "[encrypted reasoning]" : "";
}

function isHarnessUserMessage(text) {
  const trimmed = text.trim();
  return (
    trimmed.startsWith("# AGENTS.md instructions") ||
    trimmed.startsWith("<environment_context>") ||
    trimmed.startsWith("<INSTRUCTIONS>")
  );
}

function parseJsonObject(value) {
  if (isPlainObject(value)) {
    return value;
  }
  if (typeof value !== "string" || value.trim() === "") {
    return null;
  }
  try {
    const parsed = JSON.parse(value);
    return isPlainObject(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function commandFromArguments(args) {
  const cmd = args.cmd ?? args.command;
  if (typeof cmd === "string" && cmd.trim() !== "") {
    return cmd.trim();
  }
  if (Array.isArray(args.argv) && args.argv.length > 0) {
    return args.argv.map((part) => String(part)).join(" ");
  }
  return null;
}

function toolGroupFor(toolName, command, args) {
  if (toolName === "write_stdin") {
    return "write_stdin";
  }
  if (toolName !== "exec_command" || !command) {
    return toolName;
  }
  const executables = shellExecutables(command);
  if (executables.includes("loc")) {
    return "loc";
  }
  if (executables.includes("sqlite3")) {
    return "sqlite3";
  }
  if (executables.includes("security")) {
    return "security";
  }
  if (executables.includes("curl")) {
    return "curl";
  }
  if (executables.some((executable) => ["rg", "find", "sed", "ls", "cat", "awk", "wc", "grep", "test", "tail"].includes(executable))) {
    return "fs_read";
  }
  if (executables.some((executable) => ["mkdir", "cp", "mv", "rm", "touch"].includes(executable))) {
    return "fs_mutate";
  }
  if (executables.some((executable) => ["node", "python", "python3", "ruby", "perl"].includes(executable))) {
    return "script";
  }
  if (executables.includes("git")) {
    return "git";
  }
  return executables[0] ?? args.tool ?? "exec_command";
}

function shellExecutables(command) {
  return command
    .split(/(?:&&|\|\||[;|\n])/)
    .map((segment) => shellSegmentExecutable(segment))
    .filter(Boolean);
}

function shellSegmentExecutable(segment) {
  let remaining = segment.trim();
  while (remaining !== "") {
    const token = firstShellToken(remaining);
    if (!token) {
      return null;
    }
    const value = basename(stripShellTokenQuotes(token.value));
    remaining = remaining.slice(token.end).trimStart();
    if (/^[A-Za-z_][A-Za-z0-9_]*=.*/.test(value)) {
      continue;
    }
    if (["command", "env", "nice", "nohup", "sudo", "time"].includes(value)) {
      continue;
    }
    return value;
  }
  return null;
}

function firstShellToken(value) {
  const match = value.match(/^(?:"(?:\\"|[^"])*"|'[^']*'|\\\s|\S)+/);
  if (!match) {
    return null;
  }
  return { value: match[0], end: match[0].length };
}

function stripShellTokenQuotes(value) {
  return value
    .replace(/^"(.*)"$/s, "$1")
    .replace(/^'(.*)'$/s, "$1")
    .replace(/\\\s/g, " ");
}

function outputStatus(output) {
  const code = output.match(/Process exited with code ([^\n]+)/)?.[1]?.trim();
  if (code) {
    return { status: code === "0" ? "ok" : "error", exitCode: code };
  }
  if (/Process running with session ID/.test(output)) {
    return { status: "running", exitCode: null };
  }
  return { status: "unknown", exitCode: null };
}

function outputExcerpt(output) {
  const afterOutput = output.split(/\nOutput:\n/)[1] ?? output;
  return shorten(afterOutput, 400);
}

function sanitizeFrame(value) {
  return String(value)
    .replace(/[;\r\n]/g, "|")
    .replace(/\s+/g, " ")
    .trim();
}

function sanitizeFilename(value) {
  const sanitized = String(value)
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return sanitized || "codex-session";
}

function labelFromPath(path) {
  return basename(path).replace(/\.(jsonl|json)$/i, "") || "codex-session";
}

function colorFor(name) {
  const hue = hashString(name) % 360;
  return `hsl(${hue}, 74%, 72%)`;
}

function hashString(value) {
  let hash = 0;
  for (const char of value) {
    hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  }
  return hash;
}

function formatDurationMs(ms) {
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(2)}s`;
  }
  return `${Math.round(ms)}ms`;
}

function shorten(value, limit = 220) {
  if (value === null || value === undefined) {
    return "";
  }
  const text = String(value).replace(/\s+/g, " ").trim();
  return text.length <= limit ? text : `${text.slice(0, limit - 1)}…`;
}

function stringOrNull(value) {
  if (typeof value === "string" && value.trim() !== "") {
    return value.trim();
  }
  return null;
}

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function escapeXml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function escapeMarkdown(value) {
  return String(value ?? "")
    .replace(/\|/g, "\\|")
    .replace(/\r?\n/g, " ");
}

try {
  main(process.argv.slice(2));
} catch (error) {
  if (error instanceof UsageError) {
    console.error(error.message);
    process.exit(error.exitCode);
  }
  throw error;
}
