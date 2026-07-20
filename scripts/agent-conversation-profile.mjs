#!/usr/bin/env node

import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { basename, extname, join, resolve } from "node:path";

const DEFAULT_DURATION_MS = 1000;
const TIMESTAMP_KEYS = ["timestamp", "created_at", "time", "ts"];
const CONTAINER_KEYS = [
  "events",
  "messages",
  "items",
  "records",
  "entries",
  "conversation",
  "data",
];
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
  const defaultDurationMs = options.defaultDurationMs ?? DEFAULT_DURATION_MS;
  const left = loadConversation(options.left, options.leftLabel, defaultDurationMs);
  const right = loadConversation(
    options.right,
    options.rightLabel,
    defaultDurationMs,
  );
  const conversations = [left, right];
  const outDir = resolve(options.out);

  mkdirSync(outDir, { recursive: true });

  const splitOutputs = splitOutputFiles(outDir, [
    { side: "left", label: left.label },
    { side: "right", label: right.label },
  ]);
  const outputFiles = {
    combined: join(outDir, "combined.perfetto.json"),
    split: splitOutputs,
    summaryJson: join(outDir, "summary.json"),
    summaryMarkdown: join(outDir, "summary.md"),
  };

  writeJson(outputFiles.combined, buildCombinedTrace(conversations));
  writeJson(outputFiles.split[0].path, buildSplitTrace(left));
  writeJson(outputFiles.split[1].path, buildSplitTrace(right));

  const summary = buildSummary(conversations, outputFiles);
  writeJson(outputFiles.summaryJson, summary);
  writeFileSync(outputFiles.summaryMarkdown, renderSummaryMarkdown(summary));

  console.log(`Wrote agent conversation profile to ${outDir}`);
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
      case "--left":
        options.left = readFlagValue(argv, ++index, arg);
        break;
      case "--right":
        options.right = readFlagValue(argv, ++index, arg);
        break;
      case "--left-label":
        options.leftLabel = readFlagValue(argv, ++index, arg);
        break;
      case "--right-label":
        options.rightLabel = readFlagValue(argv, ++index, arg);
        break;
      case "--out":
        options.out = readFlagValue(argv, ++index, arg);
        break;
      case "--default-duration-ms": {
        const value = Number(readFlagValue(argv, ++index, arg));
        if (!Number.isFinite(value) || value <= 0) {
          throw new UsageError("--default-duration-ms must be a positive number");
        }
        options.defaultDurationMs = value;
        break;
      }
      default:
        throw new UsageError(`unknown argument: ${arg}`);
    }
  }

  if (!options.left || !options.right || !options.out) {
    throw new UsageError("missing required --left, --right, or --out argument");
  }

  options.left = resolve(options.left);
  options.right = resolve(options.right);
  options.leftLabel =
    options.leftLabel || labelFromPath(options.left) || "left";
  options.rightLabel =
    options.rightLabel || labelFromPath(options.right) || "right";

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
  node scripts/agent-conversation-profile.mjs \\
    --left claude.jsonl --left-label claude \\
    --right codex.jsonl --right-label codex \\
    --out target/agent-profiles/run-1

Options:
  --default-duration-ms <ms>  Duration used for terminal inferred events. Default: ${DEFAULT_DURATION_MS}
`);
}

function loadConversation(path, label, defaultDurationMs) {
  const parsed = parseConversationFile(path);
  const records = collectRecords(parsed);
  const warnings = [];
  const events = [];

  records.forEach((record, sourceIndex) => {
    const target = eventTargetForRecord(record);
    const timestamp = readTimestamp(target) ?? readTimestamp(record);
    if (timestamp === null) {
      warnings.push({
        source_index: sourceIndex,
        code: "missing_timestamp",
        message: "record skipped because it does not expose a supported timestamp",
      });
      return;
    }

    const partials = partialEventsForRecord(record, target);
    for (const partial of partials) {
      events.push({
        conversation_label: label,
        source_path: path,
        source_index: sourceIndex,
        role: partial.role,
        kind: partial.kind,
        tool_name: partial.toolName,
        base_start_ms: timestamp,
        start_ms: timestamp + (partial.offsetMs ?? 0),
        end_ms: null,
        duration_ms: null,
        explicit_duration_ms: partial.durationMs,
        duration_share_count: partial.durationShareCount ?? 1,
        duration_share_index: partial.durationShareIndex ?? 0,
        timing_quality: null,
        raw_type: partial.rawType,
        excerpt: partial.excerpt,
      });
    }
  });

  events.sort(
    (left, right) =>
      left.base_start_ms - right.base_start_ms ||
      left.source_index - right.source_index ||
      left.duration_share_index - right.duration_share_index,
  );

  for (let index = 0; index < events.length; index += 1) {
    const event = events[index];
    if (event.explicit_duration_ms !== null) {
      event.duration_ms = event.explicit_duration_ms;
      event.end_ms = event.start_ms + event.duration_ms;
      event.timing_quality = "measured";
    } else {
      const next = events.find(
        (candidate, candidateIndex) =>
          candidateIndex > index && candidate.base_start_ms > event.base_start_ms,
      );
      const baseEndMs = next
        ? next.base_start_ms
        : event.base_start_ms + defaultDurationMs;
      const inferredDurationMs = Math.max(1, baseEndMs - event.base_start_ms);
      const offsetMs = distributedDurationOffset(
        inferredDurationMs,
        event.duration_share_count,
        event.duration_share_index,
      );
      event.duration_ms = distributedDuration(
        inferredDurationMs,
        event.duration_share_count,
        event.duration_share_index,
      );
      event.start_ms = event.base_start_ms + offsetMs;
      event.end_ms = event.start_ms + event.duration_ms;
      event.timing_quality = "inferred";
    }
    delete event.base_start_ms;
    delete event.explicit_duration_ms;
    delete event.duration_share_count;
    delete event.duration_share_index;
  }

  events.sort(
    (left, right) =>
      left.start_ms - right.start_ms || left.source_index - right.source_index,
  );

  return {
    label,
    source_path: path,
    event_count: events.length,
    events,
    warnings,
  };
}

function parseConversationFile(path) {
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    throw new UsageError(`failed to read ${path}: ${error.message}`);
  }

  if (text.trim() === "") {
    throw new UsageError(`malformed input ${path}: file is empty`);
  }

  try {
    return JSON.parse(text);
  } catch (jsonError) {
    const lines = text
      .split(/\r?\n/)
      .map((line, index) => ({ line, number: index + 1 }))
      .filter(({ line }) => line.trim() !== "");
    if (lines.length === 0) {
      throw new UsageError(`malformed input ${path}: file is empty`);
    }

    const records = [];
    for (const { line, number } of lines) {
      try {
        records.push(JSON.parse(line));
      } catch (lineError) {
        throw new UsageError(
          `malformed input ${path}: invalid JSON on line ${number}: ${lineError.message}`,
        );
      }
    }
    if (records.length === 1) {
      throw new UsageError(
        `malformed input ${path}: ${jsonError.message}`,
      );
    }
    return records;
  }
}

function collectRecords(value) {
  const records = [];
  collectRecordsInto(value, records);
  return records;
}

function collectRecordsInto(value, records) {
  if (Array.isArray(value)) {
    for (const item of value) {
      collectRecordsInto(item, records);
    }
    return;
  }

  if (!isPlainObject(value)) {
    return;
  }

  const nestedRecordsStart = records.length;
  for (const key of CONTAINER_KEYS) {
    const child = value[key];
    if (isPlainObject(child) || Array.isArray(child)) {
      collectRecordsInto(child, records);
    }
  }

  if (records.length > nestedRecordsStart) {
    return;
  }

  if (isRecordLike(value)) {
    records.push(value);
    return;
  }

  for (const child of Object.values(value)) {
    if (isPlainObject(child) || Array.isArray(child)) {
      collectRecordsInto(child, records);
    }
  }
}

function isRecordLike(value) {
  if (!isPlainObject(value)) {
    return false;
  }

  return (
    TIMESTAMP_KEYS.some((key) => Object.hasOwn(value, key)) ||
    Object.hasOwn(value, "duration_ms") ||
    typeof value.role === "string" ||
    typeof value.type === "string" ||
    isPlainObject(value.item) ||
    isPlainObject(value.message) ||
    Object.hasOwn(value, "content")
  );
}

function partialEventsForRecord(record, target = eventTargetForRecord(record)) {
  const contentEvents = contentBlockEvents(record, target);
  if (contentEvents.length > 0) {
    return contentEvents;
  }

  return [partialEventFromObject(target, record)];
}

function eventTargetForRecord(record) {
  if (isPlainObject(record.item)) {
    return mergeEventContext(record.item, record);
  }
  if (isPlainObject(record.message)) {
    return mergeEventContext(record.message, record);
  }
  return record;
}

function mergeEventContext(target, parent, options = {}) {
  const inheritDuration = options.inheritDuration !== false;
  return {
    ...target,
    role: target.role ?? parent.role,
    timestamp: target.timestamp ?? parent.timestamp,
    created_at: target.created_at ?? parent.created_at,
    time: target.time ?? parent.time,
    ts: target.ts ?? parent.ts,
    duration_ms: inheritDuration
      ? target.duration_ms ?? parent.duration_ms
      : target.duration_ms,
    durationMs: inheritDuration
      ? target.durationMs ?? parent.durationMs
      : target.durationMs,
    elapsed_ms: inheritDuration
      ? target.elapsed_ms ?? parent.elapsed_ms
      : target.elapsed_ms,
    elapsedMs: inheritDuration
      ? target.elapsedMs ?? parent.elapsedMs
      : target.elapsedMs,
    latency_ms: inheritDuration
      ? target.latency_ms ?? parent.latency_ms
      : target.latency_ms,
    latencyMs: inheritDuration
      ? target.latencyMs ?? parent.latencyMs
      : target.latencyMs,
  };
}

function contentBlockEvents(record, target) {
  if (!Array.isArray(target.content)) {
    return [];
  }

  const events = [];
  for (const block of target.content) {
    if (!isPlainObject(block)) {
      continue;
    }
    const kind = classifyKind(block, target);
    if (kind === "unknown") {
      continue;
    }
    events.push(
      partialEventFromObject(
        mergeEventContext(block, target, { inheritDuration: false }),
        target,
        { allowParentDuration: false },
      ),
    );
  }

  const parentDurationMs = readDuration(target) ?? readDuration(record);
  if (parentDurationMs !== null && events.length > 0) {
    let offsetMs = 0;
    events.forEach((event, index) => {
      event.durationMs = distributedDuration(parentDurationMs, events.length, index);
      event.offsetMs = offsetMs;
      offsetMs += event.durationMs;
    });
  } else if (events.length > 1) {
    events.forEach((event, index) => {
      if (event.durationMs === null) {
        event.durationShareCount = events.length;
        event.durationShareIndex = index;
      }
    });
  }

  return events;
}

function partialEventFromObject(object, parent, options = {}) {
  const kind = classifyKind(object, parent);
  const rawType = String(object.type ?? parent?.type ?? object.kind ?? "");
  const role = roleFor(object, parent);
  const toolName = kind.startsWith("tool")
    ? toolNameFor(object, parent)
    : null;

  return {
    role,
    kind,
    toolName,
    durationMs:
      readDuration(object) ??
      (options.allowParentDuration === false ? null : readDuration(parent)),
    durationShareCount: 1,
    durationShareIndex: 0,
    offsetMs: 0,
    rawType,
    excerpt: excerptFor(object),
  };
}

function classifyKind(object, parent) {
  const rawType = String(object.type ?? object.kind ?? parent?.type ?? "")
    .toLowerCase()
    .replace(/\s+/g, "_");
  const role = roleFor(object, parent);

  if (
    rawType.includes("reasoning") ||
    rawType.includes("thinking") ||
    rawType === "analysis" ||
    rawType === "summary"
  ) {
    return "reasoning";
  }

  if (
    rawType.includes("tool_use") ||
    rawType.includes("tool_call") ||
    rawType.includes("function_call") ||
    rawType.includes("local_shell_call") ||
    rawType === "mcp_call"
  ) {
    if (
      rawType.includes("output") ||
      rawType.includes("result") ||
      rawType.includes("response")
    ) {
      return "tool_result";
    }
    return "tool_call";
  }

  if (
    rawType.includes("tool_result") ||
    rawType.includes("function_call_output") ||
    rawType.includes("call_output") ||
    rawType === "command_output"
  ) {
    return "tool_result";
  }

  if (role === "user") {
    return "user";
  }
  if (role === "assistant") {
    return "assistant_message";
  }
  if (role === "system") {
    return "system";
  }

  return "unknown";
}

function roleFor(object, parent) {
  const role = String(object.role ?? parent?.role ?? "").toLowerCase();
  if (["user", "assistant", "system", "tool"].includes(role)) {
    return role;
  }
  return null;
}

function toolNameFor(object, parent) {
  const candidate =
    object.name ??
    object.tool_name ??
    object.toolName ??
    object.tool ??
    object.function?.name ??
    parent?.name ??
    parent?.tool_name ??
    parent?.toolName ??
    parent?.tool ??
    parent?.function?.name;
  if (typeof candidate === "string" && candidate.trim() !== "") {
    return candidate.trim();
  }

  if (isPlainObject(object.action) && typeof object.action.command === "string") {
    return "local_shell";
  }

  return "unknown_tool";
}

function readTimestamp(record) {
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

function distributedDuration(totalDurationMs, count, index) {
  if (count <= 1) {
    return totalDurationMs;
  }
  const base = Math.floor(totalDurationMs / count);
  const remainder = totalDurationMs % count;
  return Math.max(1, base + (index < remainder ? 1 : 0));
}

function distributedDurationOffset(totalDurationMs, count, index) {
  let offsetMs = 0;
  for (let current = 0; current < index; current += 1) {
    offsetMs += distributedDuration(totalDurationMs, count, current);
  }
  return offsetMs;
}

function excerptFor(value) {
  return truncate(cleanWhitespace(extractText(value)), 160);
}

function extractText(value) {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    return value.map(extractText).filter(Boolean).join(" ");
  }
  if (!isPlainObject(value)) {
    return "";
  }

  for (const key of [
    "text",
    "content",
    "summary",
    "output",
    "result",
    "message",
    "arguments",
    "input",
  ]) {
    if (Object.hasOwn(value, key)) {
      const text = extractText(value[key]);
      if (text) {
        return text;
      }
    }
  }

  try {
    return JSON.stringify(value);
  } catch {
    return "";
  }
}

function cleanWhitespace(value) {
  return value.replace(/\s+/g, " ").trim();
}

function truncate(value, maxLength) {
  if (value.length <= maxLength) {
    return value;
  }
  return `${value.slice(0, maxLength - 3)}...`;
}

function buildCombinedTrace(conversations) {
  const baseMs = earliestStart(conversations);
  const traceEvents = [
    {
      ph: "M",
      pid: 1,
      name: "process_name",
      args: { name: "agent conversation comparison" },
    },
  ];

  conversations.forEach((conversation, index) => {
    const tid = index + 1;
    traceEvents.push({
      ph: "M",
      pid: 1,
      tid,
      name: "thread_name",
      args: { name: conversation.label },
    });
    for (const event of conversation.events) {
      traceEvents.push(traceEventFor(event, 1, tid, baseMs));
    }
  });

  return { traceEvents };
}

function buildSplitTrace(conversation) {
  const baseMs = earliestStart([conversation]);
  const tracks = [
    ["reasoning", 1],
    ["assistant", 2],
    ["tools", 3],
    ["other", 4],
  ];
  const traceEvents = [
    {
      ph: "M",
      pid: 1,
      name: "process_name",
      args: { name: conversation.label },
    },
    ...tracks.map(([name, tid]) => ({
      ph: "M",
      pid: 1,
      tid,
      name: "thread_name",
      args: { name },
    })),
  ];

  for (const event of conversation.events) {
    traceEvents.push(traceEventFor(event, 1, splitTidForKind(event.kind), baseMs));
  }

  return { traceEvents };
}

function traceEventFor(event, pid, tid, baseMs) {
  return {
    ph: "X",
    pid,
    tid,
    ts: Math.max(0, Math.round((event.start_ms - baseMs) * 1000)),
    dur: Math.max(1, Math.round(event.duration_ms * 1000)),
    cat: event.kind,
    name: eventName(event),
    args: {
      conversation_label: event.conversation_label,
      source_index: event.source_index,
      role: event.role,
      kind: event.kind,
      tool_name: event.tool_name,
      timing_quality: event.timing_quality,
      raw_type: event.raw_type,
      start_ms: event.start_ms,
      end_ms: event.end_ms,
      duration_ms: event.duration_ms,
      excerpt: event.excerpt,
    },
  };
}

function eventName(event) {
  if (event.kind === "tool_call") {
    return `tool ${event.tool_name ?? "unknown_tool"}`;
  }
  if (event.kind === "tool_result") {
    return `tool result ${event.tool_name ?? "unknown_tool"}`;
  }
  return event.kind;
}

function splitTidForKind(kind) {
  if (kind === "reasoning") {
    return 1;
  }
  if (kind === "assistant_message" || kind === "user") {
    return 2;
  }
  if (kind === "tool_call" || kind === "tool_result") {
    return 3;
  }
  return 4;
}

function earliestStart(conversations) {
  const starts = conversations.flatMap((conversation) =>
    conversation.events.map((event) => event.start_ms),
  );
  return starts.length > 0 ? Math.min(...starts) : 0;
}

function buildSummary(conversations, outputFiles) {
  return {
    ok: true,
    outputs: {
      combined: outputFiles.combined,
      split: outputFiles.split,
      summary_json: outputFiles.summaryJson,
      summary_md: outputFiles.summaryMarkdown,
    },
    conversations: conversations.map(summarizeConversation),
    warnings: conversations.flatMap((conversation) =>
      conversation.warnings.map((warning) => ({
        conversation_label: conversation.label,
        ...warning,
      })),
    ),
  };
}

function summarizeConversation(conversation) {
  const totalsByKind = {};
  const percentByKind = {};
  const toolTotals = new Map();
  let measuredDurationMs = 0;
  let inferredDurationMs = 0;

  for (const event of conversation.events) {
    totalsByKind[event.kind] =
      (totalsByKind[event.kind] ?? 0) + event.duration_ms;
    if (event.timing_quality === "measured") {
      measuredDurationMs += event.duration_ms;
    } else {
      inferredDurationMs += event.duration_ms;
    }

    if (event.kind === "tool_call") {
      const name = event.tool_name ?? "unknown_tool";
      const previous = toolTotals.get(name) ?? {
        tool_name: name,
        count: 0,
        duration_ms: 0,
      };
      previous.count += 1;
      previous.duration_ms += event.duration_ms;
      toolTotals.set(name, previous);
    }
  }

  const wallTimeMs = wallTime(conversation.events);
  for (const [kind, duration] of Object.entries(totalsByKind)) {
    percentByKind[kind] =
      wallTimeMs === 0 ? 0 : Number(((duration / wallTimeMs) * 100).toFixed(2));
  }

  return {
    label: conversation.label,
    source_path: conversation.source_path,
    event_count: conversation.events.length,
    wall_time_ms: wallTimeMs,
    measured_duration_ms: measuredDurationMs,
    inferred_duration_ms: inferredDurationMs,
    totals_by_kind: sortObject(totalsByKind),
    percent_by_kind: sortObject(percentByKind),
    tools: [...toolTotals.values()].sort(
      (left, right) =>
        right.duration_ms - left.duration_ms ||
        left.tool_name.localeCompare(right.tool_name),
    ),
    longest_events: [...conversation.events]
      .sort((left, right) => right.duration_ms - left.duration_ms)
      .slice(0, 5)
      .map((event) => ({
        kind: event.kind,
        tool_name: event.tool_name,
        duration_ms: event.duration_ms,
        timing_quality: event.timing_quality,
        source_index: event.source_index,
        excerpt: event.excerpt,
      })),
    warnings: conversation.warnings,
  };
}

function wallTime(events) {
  if (events.length === 0) {
    return 0;
  }
  return Math.max(...events.map((event) => event.end_ms)) -
    Math.min(...events.map((event) => event.start_ms));
}

function sortObject(value) {
  return Object.fromEntries(
    Object.entries(value).sort(([left], [right]) => left.localeCompare(right)),
  );
}

function renderSummaryMarkdown(summary) {
  const lines = [
    "# Agent Conversation Profile",
    "",
    "## Wall Time",
    "",
    "| Conversation | Events | Wall time | Measured | Inferred |",
    "| --- | ---: | ---: | ---: | ---: |",
  ];

  for (const conversation of summary.conversations) {
    lines.push(
      `| ${markdownTableCell(conversation.label)} | ${conversation.event_count} | ${formatMs(
        conversation.wall_time_ms,
      )} | ${formatMs(conversation.measured_duration_ms)} | ${formatMs(
        conversation.inferred_duration_ms,
      )} |`,
    );
  }

  lines.push("", "## Time By Kind", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    lines.push("| Kind | Duration | Percent of wall time |");
    lines.push("| --- | ---: | ---: |");
    for (const [kind, duration] of Object.entries(
      conversation.totals_by_kind,
    )) {
      lines.push(
        `| ${markdownTableCell(kind)} | ${formatMs(duration)} | ${
          conversation.percent_by_kind[kind] ?? 0
        }% |`,
      );
    }
    lines.push("");
  }

  lines.push("## Tool Calls", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.tools.length === 0) {
      lines.push("No tool calls.", "");
      continue;
    }
    lines.push("| Tool | Count | Duration |");
    lines.push("| --- | ---: | ---: |");
    for (const tool of conversation.tools) {
      lines.push(
        `| ${markdownTableCell(tool.tool_name)} | ${tool.count} | ${formatMs(
          tool.duration_ms,
        )} |`,
      );
    }
    lines.push("");
  }

  lines.push("## Longest Events", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.longest_events.length === 0) {
      lines.push("No events.", "");
      continue;
    }

    lines.push("| Kind | Tool | Duration | Timing | Source index | Excerpt |");
    lines.push("| --- | --- | ---: | --- | ---: | --- |");
    for (const event of conversation.longest_events) {
      lines.push(
        `| ${markdownTableCell(event.kind)} | ${markdownTableCell(
          event.tool_name ?? "",
        )} | ${formatMs(event.duration_ms)} | ${markdownTableCell(
          event.timing_quality,
        )} | ${event.source_index} | ${markdownTableCell(event.excerpt)} |`,
      );
    }
    lines.push("");
  }

  if (summary.warnings.length > 0) {
    lines.push("## Warnings", "");
    for (const warning of summary.warnings) {
      lines.push(
        `- ${warning.conversation_label} record ${warning.source_index}: ${warning.message}`,
      );
    }
    lines.push("");
  }

  return `${lines.join("\n")}\n`;
}

function formatMs(value) {
  if (value < 1000) {
    return `${value}ms`;
  }
  return `${(value / 1000).toFixed(2)}s`;
}

function markdownTableCell(value) {
  return String(value ?? "")
    .replace(/\\/g, "\\\\")
    .replace(/\|/g, "\\|")
    .replace(/\r?\n/g, " ");
}

function markdownHeadingText(value) {
  return String(value ?? "").replace(/\r?\n/g, " ");
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function splitOutputFiles(outDir, conversations) {
  const used = new Set();
  return conversations.map(({ side, label }) => {
    const base = safeFileName(label);
    const path = uniqueSplitOutputPath(outDir, side, base, used);
    used.add(path);
    return { side, label, path };
  });
}

function uniqueSplitOutputPath(outDir, side, base, used) {
  const candidates = [`${base}.perfetto.json`, `${side}-${base}.perfetto.json`];
  for (const candidate of candidates) {
    const path = join(outDir, candidate);
    if (!used.has(path)) {
      return path;
    }
  }

  for (let suffix = 2; ; suffix += 1) {
    const path = join(outDir, `${side}-${base}-${suffix}.perfetto.json`);
    if (!used.has(path)) {
      return path;
    }
  }
}

function safeFileName(label) {
  const normalized = label.trim().replace(/[^A-Za-z0-9._-]+/g, "_");
  return normalized || "conversation";
}

function labelFromPath(path) {
  const name = basename(path);
  const extension = extname(name);
  return extension ? name.slice(0, -extension.length) : name;
}

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

try {
  main(process.argv.slice(2));
} catch (error) {
  const exitCode = error.exitCode ?? 1;
  console.error(error.message);
  if (exitCode === 2) {
    printUsage();
  }
  process.exit(exitCode);
}
