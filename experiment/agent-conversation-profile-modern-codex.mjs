#!/usr/bin/env node

import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { basename, extname, join, resolve } from "node:path";

const DEFAULT_DURATION_MS = 1000;
const TIMESTAMP_KEYS = [
  "timestamp",
  "created_at",
  "time",
  "ts",
  "observed_at_ms",
  "started_at_ms",
];
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

  const splitOutputBases = splitOutputBasenames([
    { side: "left", label: left.label },
    { side: "right", label: right.label },
  ]);
  const outputFiles = {
    combined: join(outDir, "combined.perfetto.json"),
    split: splitOutputFiles(outDir, splitOutputBases, ".perfetto.json"),
    snakeviz: {
      combined: join(outDir, "combined.snakeviz.prof"),
      split: splitOutputFiles(outDir, splitOutputBases, ".snakeviz.prof"),
    },
    snakevizStats: {
      combined: join(outDir, "combined.snakeviz.stats.md"),
      split: splitOutputFiles(outDir, splitOutputBases, ".snakeviz.stats.md"),
    },
    flamegraph: {
      combined: join(outDir, "combined.folded"),
      split: splitOutputFiles(outDir, splitOutputBases, ".folded"),
    },
    speedscope: {
      combined: join(outDir, "combined.speedscope.json"),
      split: splitOutputFiles(outDir, splitOutputBases, ".speedscope.json"),
    },
    summaryJson: join(outDir, "summary.json"),
    summaryMarkdown: join(outDir, "summary.md"),
  };

  writeJson(outputFiles.combined, buildCombinedTrace(conversations));
  writeJson(outputFiles.split[0].path, buildSplitTrace(left));
  writeJson(outputFiles.split[1].path, buildSplitTrace(right));
  writeFolded(outputFiles.flamegraph.combined, buildCombinedFolded(conversations));
  writeFolded(outputFiles.flamegraph.split[0].path, buildSplitFolded(left));
  writeFolded(outputFiles.flamegraph.split[1].path, buildSplitFolded(right));
  writeSnakevizProfile(outputFiles.snakeviz.combined, conversations);
  writeSnakevizProfile(outputFiles.snakeviz.split[0].path, [left]);
  writeSnakevizProfile(outputFiles.snakeviz.split[1].path, [right]);
  writeSnakevizStatsTable(outputFiles.snakevizStats.combined, conversations);
  writeSnakevizStatsTable(outputFiles.snakevizStats.split[0].path, [left]);
  writeSnakevizStatsTable(outputFiles.snakevizStats.split[1].path, [right]);
  writeJson(outputFiles.speedscope.combined, buildCombinedSpeedscope(conversations));
  writeJson(outputFiles.speedscope.split[0].path, buildSplitSpeedscope(left));
  writeJson(outputFiles.speedscope.split[1].path, buildSplitSpeedscope(right));

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
  node experiment/agent-conversation-profile.mjs \\
    --left claude.jsonl --left-label claude \\
    --right codex.jsonl --right-label codex \\
    --out target/agent-profiles/run-1

Options:
  --default-duration-ms <ms>  Duration used for terminal inferred events. Default: ${DEFAULT_DURATION_MS}

Viewer files:
  speedscope <file>.speedscope.json
  snakeviz <file>.snakeviz.prof
  flamegraph.pl --countname=us <file>.folded > <file>.svg
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
        tool_call_id: partial.toolCallId,
        tool_command: partial.toolCommand,
        harness_source: partial.harnessSource,
        harness_phase: partial.harnessPhase,
        record_type: partial.recordType,
        record_subtype: partial.recordSubtype,
        attachment_type: partial.attachmentType,
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
  enrichToolEvents(events);

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
  if (isPlainObject(record.event)) {
    return mergeEventContext(record.event, record);
  }
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
    observed_at_ms: target.observed_at_ms ?? parent.observed_at_ms,
    started_at_ms: target.started_at_ms ?? parent.started_at_ms,
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
  const toolLike = kind.startsWith("tool") || kind.startsWith("file_change");
  const toolName = toolLike
    ? toolNameFor(object, parent)
    : null;
  const toolCallId = toolLike
    ? toolCallIdFor(object, parent)
    : null;
  const toolCommand = kind === "tool_call"
    ? toolCommandFor(object, parent)
    : null;
  const recordType = stringOrNull(object.type ?? parent?.type);
  const recordSubtype = stringOrNull(object.subtype ?? parent?.subtype);
  const attachmentType = stringOrNull(
    object.attachment?.type ?? parent?.attachment?.type,
  );

  return {
    role,
    kind,
    toolName,
    toolCallId,
    toolCommand,
    harnessSource: stringOrNull(object.harness_source ?? parent?.harness_source),
    harnessPhase: stringOrNull(object.phase ?? parent?.phase),
    recordType,
    recordSubtype,
    attachmentType,
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

function stringOrNull(value) {
  if (typeof value === "string" && value.trim() !== "") {
    return value.trim();
  }
  return null;
}

function classifyKind(object, parent) {
  const rawType = String(object.type ?? object.kind ?? parent?.type ?? "")
    .toLowerCase()
    .replace(/\s+/g, "_");
  const role = roleFor(object, parent);
  const status = String(object.status ?? parent?.status ?? "").toLowerCase();

  if (rawType === "harness.phase") {
    const phase = String(object.phase ?? object.activity ?? "")
      .toLowerCase()
      .replace(/\s+/g, "_");
    if (["tool", "tool_call", "file_change"].includes(phase)) {
      return "tool_call";
    }
    if (["thinking", "reasoning"].includes(phase)) {
      return "reasoning";
    }
    if (["agent_response", "assistant_message", "output_response"].includes(phase)) {
      return "assistant_message";
    }
    if (["input_query", "user_query", "user"].includes(phase)) {
      return "user";
    }
    if (phase === "system") {
      return "system";
    }
    return "unknown";
  }

  if (rawType === "agent_message") {
    return "assistant_message";
  }

  if (
    rawType === "command_execution" ||
    rawType === "mcp_tool_call" ||
    rawType === "custom_tool_call"
  ) {
    return codexCompletedStatus(status) ? "tool_result" : "tool_call";
  }

  if (rawType === "file_change") {
    return codexCompletedStatus(status) ? "file_change_result" : "file_change";
  }

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
  const rawType = String(object.type ?? object.kind ?? parent?.type ?? "")
    .toLowerCase()
    .replace(/\s+/g, "_");
  if (rawType === "agent_message") {
    return "assistant";
  }
  const role = String(object.role ?? parent?.role ?? "").toLowerCase();
  if (["user", "assistant", "system", "tool"].includes(role)) {
    return role;
  }
  return null;
}

function codexCompletedStatus(status) {
  return ["completed", "failed", "cancelled", "canceled"].includes(status);
}

function toolNameFor(object, parent) {
  const rawType = String(object.type ?? object.kind ?? parent?.type ?? "")
    .toLowerCase()
    .replace(/\s+/g, "_");
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

  if (rawType === "command_execution") {
    return "command_execution";
  }
  if (rawType === "file_change") {
    return "file_change";
  }

  return "unknown_tool";
}

function toolCallIdFor(object, parent) {
  const candidate =
    object.call_id ??
    object.callId ??
    object.tool_call_id ??
    object.tool_use_id ??
    object.toolUseId ??
    object.id ??
    parent?.call_id ??
    parent?.callId ??
    parent?.tool_call_id ??
    parent?.tool_use_id ??
    parent?.toolUseId ??
    parent?.id;
  if (typeof candidate === "string" && candidate.trim() !== "") {
    return candidate.trim();
  }
  return null;
}

function toolCommandFor(object, parent) {
  const direct =
    object.command ??
    object.tool_command ??
    object.input?.command ??
    object.action?.command ??
    parent?.command ??
    parent?.tool_command ??
    parent?.input?.command ??
    parent?.action?.command;
  if (typeof direct === "string" && direct.trim() !== "") {
    return direct.trim();
  }

  const parsedArguments =
    parseJsonObject(object.arguments) ??
    parseJsonObject(parent?.arguments);
  const command = parsedArguments?.command;
  if (typeof command === "string" && command.trim() !== "") {
    return command.trim();
  }

  return null;
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

function enrichToolEvents(events) {
  const callsById = new Map();
  const unmatchedCalls = [];

  for (const event of events) {
    if (event.kind !== "tool_call") {
      continue;
    }
    const toolProfile = toolProfileFor(event);
    event.tool_group = toolProfile.toolGroup;
    event.tool_command_group = toolProfile.toolCommandGroup;
    if (event.tool_call_id) {
      callsById.set(event.tool_call_id, event);
    } else {
      unmatchedCalls.push(event);
    }
  }

  for (const event of events) {
    if (event.kind !== "tool_result") {
      continue;
    }
    const matched =
      (event.tool_call_id ? callsById.get(event.tool_call_id) : null) ??
      unmatchedCalls.find((call) => call.start_ms <= event.start_ms);
    if (matched) {
      event.tool_name = matched.tool_name;
      event.tool_command = matched.tool_command;
      event.tool_group = matched.tool_group;
      event.tool_command_group = matched.tool_command_group;
    } else {
      const toolProfile = toolProfileFor(event);
      event.tool_group = toolProfile.toolGroup;
      event.tool_command_group = toolProfile.toolCommandGroup;
    }
  }
}

function toolProfileFor(event) {
  return {
    toolGroup: toolGroupFor(event),
    toolCommandGroup: toolCommandGroupFor(event),
  };
}

function toolGroupFor(event) {
  if (event.kind === "file_change") {
    return "non_loc";
  }
  if (locCommandGroupFor(event.tool_command)) {
    return "loc";
  }
  return "non_loc";
}

function toolCommandGroupFor(event) {
  const toolName = event.tool_name ?? "unknown_tool";
  if (event.kind === "file_change") {
    return "file_change";
  }
  if (toolName.toLowerCase() === "bash") {
    return locCommandGroupFor(event.tool_command) ??
      shellCommandGroupFor(event.tool_command) ??
      "bash";
  }
  if (
    ["command_execution", "local_shell"].includes(toolName.toLowerCase())
  ) {
    return locCommandGroupFor(event.tool_command) ??
      shellCommandGroupFor(event.tool_command) ??
      "unknown_command";
  }
  return sanitizeCommandGroupPart(toolName);
}

function locCommandGroupFor(command) {
  const subcommands = locSubcommandsFor(command);
  if (subcommands.length === 0) {
    return null;
  }
  if (subcommands.length > 1) {
    return subcommands.join("+");
  }
  return subcommands[0];
}

function locSubcommandsFor(command) {
  if (typeof command !== "string" || command.trim() === "") {
    return [];
  }

  const commandText = nestedShellCommand(command) ?? command;
  const subcommands = new Set();
  for (const segment of shellCommandSegments(commandText)) {
    const subcommand = locSubcommandForSegment(segment);
    if (subcommand) {
      subcommands.add(subcommand);
    }
  }
  return [...subcommands].sort();
}

function nestedShellCommand(command) {
  if (typeof command !== "string" || command.trim() === "") {
    return null;
  }
  const match = command.match(
    /(?:^|\s)(?:\/[^\s]+\/)?(?:ba)?sh|(?:^|\s)(?:\/[^\s]+\/)?zsh/,
  );
  if (!match) {
    return null;
  }
  const afterShell = command.slice(match.index + match[0].length).trimStart();
  const loginCommand = afterShell.match(/^-l?c\s+(.+)$/s);
  if (!loginCommand) {
    return null;
  }
  return stripShellTokenQuotes(loginCommand[1].trim());
}

function shellCommandSegments(command) {
  const segments = [];
  let current = "";
  let quote = null;
  let escaped = false;

  for (let index = 0; index < command.length; index += 1) {
    const char = command[index];
    const next = command[index + 1];

    if (escaped) {
      current += char;
      escaped = false;
      continue;
    }
    if (char === "\\") {
      current += char;
      escaped = true;
      continue;
    }
    if (quote) {
      current += char;
      if (char === quote) {
        quote = null;
      }
      continue;
    }
    if (char === "'" || char === '"') {
      current += char;
      quote = char;
      continue;
    }
    if (
      char === "\n" ||
      char === ";" ||
      char === "|" ||
      (char === "&" && next === "&")
    ) {
      segments.push(current);
      current = "";
      if ((char === "|" && next === "|") || (char === "&" && next === "&")) {
        index += 1;
      }
      continue;
    }
    current += char;
  }

  segments.push(current);
  return segments;
}

function shellCommandGroupFor(command) {
  const executables = shellExecutablesFor(command).filter(
    (executable) => executable !== "loc",
  );
  if (executables.length === 0) {
    return null;
  }
  if (executables.length > 1) {
    return executables.join("+");
  }
  return executables[0];
}

function shellExecutablesFor(command) {
  if (typeof command !== "string" || command.trim() === "") {
    return [];
  }

  const commandText = nestedShellCommand(command) ?? command;
  const executables = new Set();
  for (const segment of shellCommandSegments(commandText)) {
    const executable = shellExecutableForSegment(segment);
    if (executable) {
      executables.add(executable);
    }
  }
  for (const executable of knownShellExecutablesFor(commandText)) {
    executables.add(executable);
  }
  return [...executables].sort();
}

function shellExecutableForSegment(segment) {
  const tokens = shellTokens(segment);
  const executableIndex = shellExecutableTokenIndex(tokens);
  if (executableIndex === null) {
    return null;
  }
  const executable = executableNameForToken(tokens[executableIndex]);
  if (!executable || SHELL_CONTROL_KEYWORDS.has(executable)) {
    return null;
  }
  return executable;
}

function executableNameForToken(value) {
  const executable = basename(value);
  if (!/^[A-Za-z_][A-Za-z0-9_.+-]*$/.test(executable)) {
    return null;
  }
  return executable;
}

function knownShellExecutablesFor(command) {
  const known = new Set();
  const pattern =
    /(?:^|[^A-Za-z0-9_.+-])(cat|curl|date|find|gh|git|grep|head|jq|loc|ls|mkdir|node|printf|pwd|python3|rg|sed|sort|true|uniq|xargs)(?=$|[^A-Za-z0-9_.+-])/g;
  for (const match of command.matchAll(pattern)) {
    known.add(match[1]);
  }
  return [...known];
}

function locSubcommandForSegment(segment) {
  const tokens = shellTokens(segment);
  const executableIndex = shellExecutableTokenIndex(tokens);
  if (executableIndex === null) {
    return null;
  }
  if (basename(tokens[executableIndex]) !== "loc") {
    return null;
  }

  const subcommandParts = [];
  for (let index = executableIndex + 1; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token === "--help" || token === "-h") {
      return "help";
    }
    if (token.startsWith("-")) {
      continue;
    }

    subcommandParts.push(sanitizeLocSubcommandPart(token));
    if (token !== "create" || subcommandParts.length === 2) {
      break;
    }
  }

  return subcommandParts.length > 0 ? subcommandParts.join("-") : "unknown";
}

function shellTokens(value) {
  const tokens = [];
  let remaining = value.trim();
  while (remaining !== "") {
    const token = firstShellToken(remaining);
    if (!token) {
      break;
    }
    tokens.push(stripShellTokenQuotes(token.value));
    remaining = remaining.slice(token.end).trimStart();
  }
  return tokens;
}

function shellExecutableTokenIndex(tokens) {
  for (let index = 0; index < tokens.length; index += 1) {
    const value = tokens[index];
    if (/^[A-Za-z_][A-Za-z0-9_]*=.*/.test(value)) {
      continue;
    }
    if (["command", "env", "nice", "nohup", "sudo", "time"].includes(value)) {
      continue;
    }
    return index;
  }
  return null;
}

const SHELL_CONTROL_KEYWORDS = new Set([
  "!",
  "[",
  "[[",
  "]",
  "]]",
  "case",
  "do",
  "done",
  "elif",
  "else",
  "esac",
  "fi",
  "for",
  "function",
  "if",
  "in",
  "select",
  "then",
  "until",
  "while",
  "{",
  "}",
]);

function sanitizeLocSubcommandPart(value) {
  return sanitizeCommandGroupPart(value);
}

function sanitizeCommandGroupPart(value) {
  return basename(value).replace(/[^A-Za-z0-9_.+-]+/g, "-") || "unknown";
}

function firstShellToken(value) {
  const match = value.match(/^(?:"(?:\\"|[^"])*"|'[^']*'|\\\s|\S)+/);
  if (!match) {
    return null;
  }
  return {
    value: match[0],
    end: match[0].length,
  };
}

function stripShellTokenQuotes(value) {
  return value
    .replace(/^"(.*)"$/s, "$1")
    .replace(/^'(.*)'$/s, "$1")
    .replace(/\\\s/g, " ");
}

function buildProfileEntries(conversation) {
  const measuredHookActivities = hookMeasuredActivities(conversation);
  const entries = conversation.events
    .map((event, index) =>
      profileEntryForEvent(conversation, event, index, {
        measuredHookActivities,
      }),
    )
    .filter(Boolean);
  if (!measuredHookActivities.has("reasoning")) {
    entries.push(...inferredInitialReasoningEntries(conversation));
  }
  return entries;
}

function profileEntryForEvent(conversation, event, index, options = {}) {
  if (isMetadataEvent(event)) {
    return null;
  }

  const activity = profileActivityForEvent(event);
  if (
    event.record_type !== "harness.phase" &&
    options.measuredHookActivities?.has(activity)
  ) {
    return null;
  }
  const durationMs = profileDurationMs(event, conversation.events, index);

  return {
    conversation_label: conversation.label,
    source_index: event.source_index,
    activity,
    kind: event.kind,
    tool_name: event.tool_name,
    tool_group: profileToolGroupFor(event),
    tool_command_group: profileToolCommandGroupFor(event),
    tool_command: event.tool_command,
    harness_source: event.harness_source,
    harness_phase: event.harness_phase,
    start_ms: event.start_ms,
    end_ms: event.start_ms + durationMs,
    duration_ms: durationMs,
    timing_quality: event.timing_quality,
    excerpt: event.excerpt,
  };
}

function profileActivityForEvent(event) {
  if (event.kind === "tool_result" || event.kind === "file_change_result") {
    return "reasoning";
  }
  return activityForEvent(event);
}

function inferredInitialReasoningEntries(conversation) {
  const entries = [];
  for (const event of conversation.events) {
    if (event.record_type !== "turn.started") {
      continue;
    }
    const next = conversation.events.find(
      (candidate) =>
        candidate.start_ms > event.start_ms && !isMetadataEvent(candidate),
    );
    if (!next) {
      continue;
    }
    const durationMs = next.start_ms - event.start_ms;
    if (durationMs <= 0) {
      continue;
    }
    entries.push({
      conversation_label: conversation.label,
      source_index: event.source_index,
      activity: "reasoning",
      kind: "reasoning",
      tool_name: null,
      tool_group: null,
      tool_command_group: null,
      tool_command: null,
      harness_source: null,
      harness_phase: null,
      start_ms: event.start_ms,
      end_ms: next.start_ms,
      duration_ms: durationMs,
      timing_quality: "inferred",
      excerpt: "inferred initial model work from turn start to first Codex item",
    });
  }
  return entries;
}

function hookMeasuredActivities(conversation) {
  const activities = new Set();
  for (const event of conversation.events) {
    if (
      event.record_type === "harness.phase" &&
      event.harness_source === "codex_hook"
    ) {
      activities.add(activityForEvent(event));
    }
  }
  return activities;
}

function profileDurationMs(event, events, index) {
  if (event.kind === "tool_call") {
    return waitDurationMs(event, events, index, "tool_result");
  }
  if (event.kind === "file_change") {
    return waitDurationMs(event, events, index, "file_change_result");
  }
  return event.duration_ms;
}

function profileToolGroupFor(event) {
  if (event.kind === "tool_call" || event.kind === "file_change") {
    return toolGroupFor(event);
  }
  return null;
}

function profileToolCommandGroupFor(event) {
  if (event.kind === "tool_call" || event.kind === "file_change") {
    return toolCommandGroupFor(event);
  }
  return null;
}

function activityForEvent(event) {
  if (isMetadataEvent(event)) {
    return "metadata";
  }
  if (event.kind === "tool_call") {
    return "tool";
  }
  if (event.kind === "file_change") {
    return "tool";
  }
  if (event.kind === "reasoning") {
    return "reasoning";
  }
  if (event.kind === "user") {
    return "user_query";
  }
  if (event.kind === "assistant_message") {
    return "agent_response";
  }
  if (event.kind === "system") {
    return "system";
  }
  return "other";
}

function isMetadataEvent(event) {
  if (event.kind !== "unknown") {
    return false;
  }
  if (event.record_type === "system" || event.record_type === "attachment") {
    return true;
  }
  if (event.attachment_type || event.record_subtype) {
    return true;
  }
  return [
    "ai-title",
    "file-history-delta",
    "file-history-snapshot",
    "harness.hook",
    "harness.hook_error",
    "last-prompt",
    "mode",
    "permission-mode",
    "thread.started",
    "turn.started",
    "turn.completed",
    "turn.failed",
  ].includes(event.record_type);
}

function metadataCategoryFor(event) {
  if (event.record_type === "attachment") {
    return event.attachment_type
      ? `attachment:${event.attachment_type}`
      : "attachment";
  }
  if (event.record_type === "system") {
    return event.record_subtype
      ? `system:${event.record_subtype}`
      : "system";
  }
  if (event.record_subtype) {
    return `${event.record_type ?? "unknown"}:${event.record_subtype}`;
  }
  return event.record_type ?? event.raw_type ?? "unknown";
}

function toolWaitDurationMs(event, events, index) {
  return waitDurationMs(event, events, index, "tool_result");
}

function waitDurationMs(event, events, index, resultKind) {
  if (event.timing_quality === "measured") {
    return event.duration_ms;
  }
  const result = matchingCompletionEvent(event, events, index, resultKind);
  if (!result) {
    return event.duration_ms;
  }
  return Math.max(1, result.start_ms - event.start_ms);
}

function matchingToolResult(event, events, index) {
  return matchingCompletionEvent(event, events, index, "tool_result");
}

function matchingCompletionEvent(event, events, index, resultKind) {
  if (event.tool_call_id) {
    const byId = events.find(
      (candidate, candidateIndex) =>
        candidateIndex > index &&
        candidate.kind === resultKind &&
        candidate.tool_call_id === event.tool_call_id,
    );
    if (byId) {
      return byId;
    }
  }

  return events.find(
    (candidate, candidateIndex) =>
      candidateIndex > index &&
      candidate.kind === resultKind &&
      candidate.start_ms >= event.start_ms,
  );
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
  const baseMs = earliestTraceStart(conversations);
  const traceEvents = [
    {
      ph: "M",
      pid: 1,
      name: "process_name",
      args: { name: "agent conversation comparison" },
    },
  ];
  const tracks = new Map();

  for (const { conversation, entry } of sortedProfileEntries(conversations)) {
    const trackName = profileTraceTrackName(conversation, entry, {
      includeConversation: true,
    });
    const tid = traceTidForTrack(trackName, tracks, traceEvents);
    traceEvents.push(profileTraceEventFor(entry, 1, tid, baseMs));
  }

  return { traceEvents };
}

function buildSplitTrace(conversation) {
  const baseMs = earliestTraceStart([conversation]);
  const traceEvents = [
    {
      ph: "M",
      pid: 1,
      name: "process_name",
      args: { name: conversation.label },
    },
  ];
  const tracks = new Map();

  for (const entry of buildProfileEntries(conversation).sort(profileEntrySort)) {
    const trackName = profileTraceTrackName(conversation, entry, {
      includeConversation: false,
    });
    const tid = traceTidForTrack(trackName, tracks, traceEvents);
    traceEvents.push(profileTraceEventFor(entry, 1, tid, baseMs));
  }

  return { traceEvents };
}

function profileEntrySort(left, right) {
  return left.start_ms - right.start_ms || left.source_index - right.source_index;
}

function profileTraceTrackName(conversation, entry, { includeConversation }) {
  const frames = [];
  if (includeConversation) {
    frames.push(`conversation:${conversation.label}`);
  }
  frames.push(`activity:${entry.activity}`);
  if (entry.activity === "tool") {
    frames.push(`tool:${entry.tool_group ?? "unknown_tool"}`);
    frames.push(`command:${toolCommandFrameName(entry)}`);
  }
  return frames.join(" / ");
}

function traceTidForTrack(trackName, tracks, traceEvents) {
  const existing = tracks.get(trackName);
  if (existing !== undefined) {
    return existing;
  }
  const tid = tracks.size + 1;
  tracks.set(trackName, tid);
  traceEvents.push({
    ph: "M",
    pid: 1,
    tid,
    name: "thread_name",
    args: { name: trackName },
  });
  return tid;
}

function buildCombinedFolded(conversations) {
  return buildFoldedStacks(conversations, { includeRoot: true });
}

function buildSplitFolded(conversation) {
  return buildFoldedStacks([conversation], { includeRoot: false });
}

function buildCombinedSpeedscope(conversations) {
  return buildSpeedscopeFile(conversations, {
    includeRoot: true,
    name: "agent conversation comparison",
  });
}

function buildSplitSpeedscope(conversation) {
  return buildSpeedscopeFile([conversation], {
    includeRoot: false,
    name: conversation.label,
  });
}

function buildSpeedscopeFile(conversations, { includeRoot, name }) {
  const frameTable = new SpeedscopeFrameTable();
  const samples = [];
  const weights = [];
  const entries = sortedProfileEntries(conversations);

  for (const { conversation, entry } of entries) {
    samples.push(
      speedscopeStackFor(conversation, entry, includeRoot).map((frameName) =>
        frameTable.index(frameName),
      ),
    );
    weights.push(entry.duration_ms);
  }

  return {
    $schema: "https://www.speedscope.app/file-format-schema.json",
    exporter: "agent-conversation-profile",
    name,
    activeProfileIndex: 0,
    shared: {
      frames: frameTable.frames(),
    },
    profiles: [
      {
        type: "sampled",
        name,
        unit: "milliseconds",
        startValue: 0,
        endValue: weights.reduce((total, weight) => total + weight, 0),
        samples,
        weights,
      },
    ],
  };
}

function sortedProfileEntries(conversations) {
  return conversations
    .flatMap((conversation, conversationIndex) =>
      buildProfileEntries(conversation).map((entry) => ({
        conversation,
        conversationIndex,
        entry,
      })),
    )
    .sort(
      (left, right) =>
        left.entry.start_ms - right.entry.start_ms ||
        left.conversationIndex - right.conversationIndex ||
        left.entry.source_index - right.entry.source_index,
    );
}

function speedscopeStackFor(conversation, entry, includeRoot) {
  const frames = [];
  if (includeRoot) {
    frames.push("agent-conversation-profile");
  }
  frames.push(
    speedscopeFrameName("conversation", conversation.label),
    speedscopeFrameName("activity", entry.activity),
  );
  if (entry.activity === "tool") {
    frames.push(speedscopeFrameName("tool", entry.tool_group ?? "unknown_tool"));
    frames.push(speedscopeFrameName("command", toolCommandFrameName(entry)));
  }
  return frames;
}

function toolCommandFrameName(entry) {
  const toolGroup = entry.tool_group ?? "unknown_tool";
  const command = entry.tool_command_group ?? "unknown_command";
  return `${toolGroup}:${command}`;
}

function speedscopeFrameName(prefix, value) {
  return `${prefix}:${String(value).replace(/\r?\n/g, " ")}`;
}

class SpeedscopeFrameTable {
  constructor() {
    this.frameIndexByName = new Map();
    this.frameList = [];
  }

  index(name) {
    const existing = this.frameIndexByName.get(name);
    if (existing !== undefined) {
      return existing;
    }

    const index = this.frameList.length;
    this.frameIndexByName.set(name, index);
    this.frameList.push({ name });
    return index;
  }

  frames() {
    return this.frameList;
  }
}

function buildFoldedStacks(conversations, { includeRoot }) {
  const stacks = new Map();

  for (const conversation of conversations) {
    for (const entry of buildProfileEntries(conversation)) {
      const frames = [];
      if (includeRoot) {
        frames.push("agent-conversation-profile");
      }
      frames.push(
        `conversation:${conversation.label}`,
        `activity:${entry.activity}`,
      );
      if (entry.activity === "tool") {
        frames.push(`tool:${entry.tool_group ?? "unknown_tool"}`);
        frames.push(`command:${toolCommandFrameName(entry)}`);
      }

      const stack = frames.map(sanitizeFoldedFrame).join(";");
      stacks.set(stack, (stacks.get(stack) ?? 0) + durationUs(entry));
    }
  }

  return [...stacks.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([stack, duration]) => `${stack} ${duration}`);
}

function sanitizeFoldedFrame(value) {
  return String(value).replace(/[;\r\n]/g, "_");
}

function durationUs(event) {
  return Math.round(event.duration_ms * 1000);
}

function writeFolded(path, lines) {
  writeFileSync(path, lines.length > 0 ? `${lines.join("\n")}\n` : "");
}

function writeSnakevizProfile(path, conversations) {
  const payload = {
    path,
    frames: buildSnakevizProfileFrames(conversations),
  };
  const result = spawnSync("python3", ["-c", PYTHON_PSTATS_WRITER], {
    input: JSON.stringify(payload),
    encoding: "utf8",
    maxBuffer: 10 * 1024 * 1024,
  });

  if (result.error) {
    throw new UsageError(
      `failed to generate SnakeViz profile ${path}: ${result.error.message}. SnakeViz output requires python3.`,
    );
  }

  if (result.status !== 0) {
    const detail =
      result.stderr.trim() ||
      result.stdout.trim() ||
      `python3 exited with status ${result.status}`;
    throw new UsageError(
      `failed to generate SnakeViz profile ${path}: ${detail}. SnakeViz output requires python3.`,
    );
  }
}

function writeSnakevizStatsTable(path, conversations) {
  const rows = buildSnakevizStatsRows(conversations);
  const lines = [
    "# SnakeViz Stats",
    "",
    "| Rank | ncalls | tottime | percall | cumtime | percall | Frame | Callers |",
    "| ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |",
  ];

  rows.forEach((row, index) => {
    lines.push(
      `| ${index + 1} | ${row.ncalls} | ${formatSeconds(
        row.total_time,
      )} | ${formatSeconds(row.total_percall)} | ${formatSeconds(
        row.cumulative_time,
      )} | ${formatSeconds(row.cumulative_percall)} | ${markdownTableCell(
        row.frame,
      )} | ${markdownTableCell(
        row.callers.length > 0 ? row.callers.join(", ") : "",
      )} |`,
    );
  });

  const toolRows = buildSnakevizToolCommandRows(conversations);
  lines.push("", "## Tool Command Breakdown", "");
  if (toolRows.length === 0) {
    lines.push("No tool calls.", "");
  } else {
    lines.push(
      "| Conversation | Tool group | Command | ncalls | tottime | percall | cumtime | percall |",
    );
    lines.push("| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |");
    for (const row of toolRows) {
      lines.push(
        `| ${markdownTableCell(row.conversation)} | ${markdownTableCell(
          row.tool_group,
        )} | ${markdownTableCell(row.command)} | ${row.ncalls} | ${formatSeconds(
          row.total_time,
        )} | ${formatSeconds(row.total_percall)} | ${formatSeconds(
          row.cumulative_time,
        )} | ${formatSeconds(row.cumulative_percall)} |`,
      );
    }
    lines.push("");
  }

  writeFileSync(path, `${lines.join("\n")}\n`);
}

function buildSnakevizStatsRows(conversations) {
  return buildSnakevizProfileFrames(conversations)
    .filter(
      (frame) =>
        frame.name !== "agent-conversation-profile" ||
        frame.total_time > 0 ||
        frame.cumulative_time > 0,
    )
    .map((frame) => {
      const ncalls = snakevizCallCount(frame);
      return {
        ncalls,
        total_time: frame.total_time,
        total_percall: ncalls === 0 ? 0 : frame.total_time / ncalls,
        cumulative_time: frame.cumulative_time,
        cumulative_percall: ncalls === 0 ? 0 : frame.cumulative_time / ncalls,
        frame: `${frame.filename}:${frame.line}(${frame.name})`,
        callers: frame.callers.map((caller) => caller.name).sort(),
      };
    })
    .sort(
      (left, right) =>
        right.cumulative_time - left.cumulative_time ||
        right.total_time - left.total_time ||
        left.frame.localeCompare(right.frame),
    );
}

function buildSnakevizToolCommandRows(conversations) {
  const totals = new Map();
  for (const conversation of conversations) {
    for (const entry of buildProfileEntries(conversation)) {
      if (entry.activity !== "tool") {
        continue;
      }

      const conversationLabel = conversation.label;
      const toolGroup = entry.tool_group ?? "unknown_tool";
      const command = entry.tool_command_group ?? "unknown_command";
      const key = `${conversationLabel}\0${toolGroup}\0${command}`;
      const previous = totals.get(key) ?? {
        conversation: conversationLabel,
        tool_group: toolGroup,
        command,
        ncalls: 0,
        total_time: 0,
        cumulative_time: 0,
      };
      previous.ncalls += 1;
      previous.total_time += entry.duration_ms / 1000;
      previous.cumulative_time += entry.duration_ms / 1000;
      totals.set(key, previous);
    }
  }

  return [...totals.values()]
    .map((row) => ({
      ...row,
      total_percall: row.ncalls === 0 ? 0 : row.total_time / row.ncalls,
      cumulative_percall:
        row.ncalls === 0 ? 0 : row.cumulative_time / row.ncalls,
    }))
    .sort(
      (left, right) =>
        left.conversation.localeCompare(right.conversation) ||
        left.tool_group.localeCompare(right.tool_group) ||
        right.cumulative_time - left.cumulative_time ||
        left.command.localeCompare(right.command),
    );
}

function snakevizCallCount(frame) {
  return frame.total_calls > 0 ? frame.total_calls : frame.primitive_calls;
}

const PYTHON_PSTATS_WRITER = `
import json
import marshal
import sys

payload = json.load(sys.stdin)
stats = {}

for frame in payload["frames"]:
    key = (str(frame["filename"]), int(frame["line"]), str(frame["name"]))
    callers = {}
    for caller in frame["callers"]:
        caller_key = (
            str(caller["filename"]),
            int(caller["line"]),
            str(caller["name"]),
        )
        callers[caller_key] = (
            int(caller["primitive_calls"]),
            int(caller["total_calls"]),
            float(caller["total_time"]),
            float(caller["cumulative_time"]),
        )
    stats[key] = (
        int(frame["primitive_calls"]),
        int(frame["total_calls"]),
        float(frame["total_time"]),
        float(frame["cumulative_time"]),
        callers,
    )

with open(payload["path"], "wb") as handle:
    marshal.dump(stats, handle)
`;

function buildSnakevizProfileFrames(conversations) {
  const builder = new ProfileFrameBuilder();
  const root = builder.frame("agent-conversation-profile");
  root.primitiveCalls = 1;
  root.totalCalls = 1;

  for (const conversation of conversations) {
    const conversationFrame = builder.frame(`conversation:${conversation.label}`);
    for (const entry of buildProfileEntries(conversation)) {
      const durationSeconds = entry.duration_ms / 1000;
      const activityFrame = builder.frame(`activity:${entry.activity}`);
      const stack = [root, conversationFrame, activityFrame];

      if (entry.activity === "tool") {
        stack.push(builder.frame(`tool:${entry.tool_group ?? "unknown_tool"}`));
        stack.push(builder.frame(`command:${toolCommandFrameName(entry)}`));
      }

      builder.addStack(stack, durationSeconds);
    }
  }

  return builder.frames();
}

class ProfileFrameBuilder {
  constructor() {
    this.nextLine = 1;
    this.frameByName = new Map();
  }

  frame(name) {
    const existing = this.frameByName.get(name);
    if (existing) {
      return existing;
    }

    const frame = {
      filename: "agent-conversation-profile.synthetic",
      line: this.nextLine,
      name,
      primitiveCalls: 0,
      totalCalls: 0,
      totalTime: 0,
      cumulativeTime: 0,
      callers: new Map(),
    };
    this.nextLine += 1;
    this.frameByName.set(name, frame);
    return frame;
  }

  addStack(stack, durationSeconds) {
    if (stack.length === 0) {
      return;
    }

    for (const frame of stack) {
      frame.cumulativeTime += durationSeconds;
    }

    const leaf = stack[stack.length - 1];
    leaf.totalTime += durationSeconds;

    for (let index = 1; index < stack.length; index += 1) {
      const caller = stack[index - 1];
      const callee = stack[index];
      const totalTime = callee === leaf ? durationSeconds : 0;

      callee.primitiveCalls += 1;
      callee.totalCalls += 1;
      const callerStats = callee.callers.get(caller.name) ?? {
        frame: caller,
        primitiveCalls: 0,
        totalCalls: 0,
        totalTime: 0,
        cumulativeTime: 0,
      };
      callerStats.primitiveCalls += 1;
      callerStats.totalCalls += 1;
      callerStats.totalTime += totalTime;
      callerStats.cumulativeTime += durationSeconds;
      callee.callers.set(caller.name, callerStats);
    }
  }

  frames() {
    return [...this.frameByName.values()].map((frame) => ({
      filename: frame.filename,
      line: frame.line,
      name: frame.name,
      primitive_calls: frame.primitiveCalls,
      total_calls: frame.totalCalls,
      total_time: frame.totalTime,
      cumulative_time: frame.cumulativeTime,
      callers: [...frame.callers.values()].map((caller) => ({
        filename: caller.frame.filename,
        line: caller.frame.line,
        name: caller.frame.name,
        primitive_calls: caller.primitiveCalls,
        total_calls: caller.totalCalls,
        total_time: caller.totalTime,
        cumulative_time: caller.cumulativeTime,
      })),
    }));
  }
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
      activity: activityForEvent(event),
      tool_name: event.tool_name,
      tool_group: event.tool_group,
      tool_command_group: event.tool_command_group,
      tool_command: event.tool_command,
      harness_source: event.harness_source,
      harness_phase: event.harness_phase,
      timing_quality: event.timing_quality,
      raw_type: event.raw_type,
      start_ms: event.start_ms,
      end_ms: event.end_ms,
      duration_ms: event.duration_ms,
      excerpt: event.excerpt,
    },
  };
}

function profileTraceEventFor(entry, pid, tid, baseMs) {
  return {
    ph: "X",
    pid,
    tid,
    ts: Math.max(0, Math.round((entry.start_ms - baseMs) * 1000)),
    dur: Math.max(1, Math.round(entry.duration_ms * 1000)),
    cat: entry.activity,
    name: profileTraceEventName(entry),
    args: {
      conversation_label: entry.conversation_label,
      source_index: entry.source_index,
      kind: entry.kind,
      activity: entry.activity,
      tool_name: entry.tool_name,
      tool_group: entry.tool_group,
      tool_command_group: entry.tool_command_group,
      tool_command: entry.tool_command,
      harness_source: entry.harness_source,
      harness_phase: entry.harness_phase,
      timing_quality: entry.timing_quality,
      start_ms: entry.start_ms,
      end_ms: entry.end_ms,
      duration_ms: entry.duration_ms,
      excerpt: entry.excerpt,
    },
  };
}

function profileTraceEventName(entry) {
  if (entry.activity === "tool") {
    return `command:${toolCommandFrameName(entry)}`;
  }
  return `activity:${entry.activity}`;
}

function eventName(event) {
  if (event.kind === "tool_call") {
    return `tool ${event.tool_name ?? "unknown_tool"}`;
  }
  if (event.kind === "tool_result") {
    return `tool result ${event.tool_name ?? "unknown_tool"}`;
  }
  if (event.kind === "file_change") {
    return "file change";
  }
  if (event.kind === "file_change_result") {
    return "file change result";
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
  if (
    kind === "tool_call" ||
    kind === "tool_result" ||
    kind === "file_change" ||
    kind === "file_change_result"
  ) {
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

function earliestTraceStart(conversations) {
  const starts = conversations.flatMap((conversation) =>
    traceableEvents(conversation).map((event) => event.start_ms),
  );
  return starts.length > 0 ? Math.min(...starts) : earliestStart(conversations);
}

function traceableEvents(conversation) {
  return conversation.events.filter((event) => !isMetadataEvent(event));
}

function buildSummary(conversations, outputFiles) {
  return {
    ok: true,
    outputs: {
      combined: outputFiles.combined,
      split: outputFiles.split,
      snakeviz: outputFiles.snakeviz,
      snakeviz_stats: outputFiles.snakevizStats,
      flamegraph: outputFiles.flamegraph,
      speedscope: outputFiles.speedscope,
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
  const totalsByActivity = {};
  const percentByActivity = {};
  const toolTotals = new Map();
  const toolGroupTotals = new Map();
  const toolCommandTotals = new Map();
  const metadataTotals = new Map();
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

    if (isMetadataEvent(event)) {
      const category = metadataCategoryFor(event);
      const previous = metadataTotals.get(category) ?? {
        category,
        count: 0,
        duration_ms: 0,
        measured_duration_ms: 0,
        inferred_duration_ms: 0,
      };
      previous.count += 1;
      previous.duration_ms += event.duration_ms;
      if (event.timing_quality === "measured") {
        previous.measured_duration_ms += event.duration_ms;
      } else {
        previous.inferred_duration_ms += event.duration_ms;
      }
      metadataTotals.set(category, previous);
    }
  }

  const profileEntries = buildProfileEntries(conversation);
  for (const entry of profileEntries) {
    totalsByActivity[entry.activity] =
      (totalsByActivity[entry.activity] ?? 0) + entry.duration_ms;
    if (entry.activity === "tool") {
      const name = entry.tool_name ?? "unknown_tool";
      const previousTool = toolTotals.get(name) ?? {
        tool_name: name,
        count: 0,
        duration_ms: 0,
      };
      previousTool.count += 1;
      previousTool.duration_ms += entry.duration_ms;
      toolTotals.set(name, previousTool);

      const group = entry.tool_group ?? "unknown_tool";
      const previous = toolGroupTotals.get(group) ?? {
        tool_group: group,
        count: 0,
        duration_ms: 0,
      };
      previous.count += 1;
      previous.duration_ms += entry.duration_ms;
      toolGroupTotals.set(group, previous);

      const command = entry.tool_command_group ?? "unknown_command";
      const commandKey = `${group}\0${command}`;
      const previousCommand = toolCommandTotals.get(commandKey) ?? {
        tool_group: group,
        command,
        count: 0,
        duration_ms: 0,
      };
      previousCommand.count += 1;
      previousCommand.duration_ms += entry.duration_ms;
      toolCommandTotals.set(commandKey, previousCommand);
    }
  }

  const wallTimeMs = wallTime(conversation.events);
  for (const [kind, duration] of Object.entries(totalsByKind)) {
    percentByKind[kind] =
      wallTimeMs === 0 ? 0 : Number(((duration / wallTimeMs) * 100).toFixed(2));
  }
  for (const [activity, duration] of Object.entries(totalsByActivity)) {
    percentByActivity[activity] =
      wallTimeMs === 0 ? 0 : Number(((duration / wallTimeMs) * 100).toFixed(2));
  }

  return {
    label: conversation.label,
    source_path: conversation.source_path,
    event_count: conversation.events.length,
    wall_time_ms: wallTimeMs,
    measured_duration_ms: measuredDurationMs,
    inferred_duration_ms: inferredDurationMs,
    metadata_duration_ms: [...metadataTotals.values()].reduce(
      (total, item) => total + item.duration_ms,
      0,
    ),
    totals_by_activity: sortObject(totalsByActivity),
    percent_by_activity: sortObject(percentByActivity),
    totals_by_kind: sortObject(totalsByKind),
    percent_by_kind: sortObject(percentByKind),
    tools: [...toolTotals.values()].sort(
      (left, right) =>
        right.duration_ms - left.duration_ms ||
        left.tool_name.localeCompare(right.tool_name),
    ),
    tool_groups: [...toolGroupTotals.values()].sort(
      (left, right) =>
        right.duration_ms - left.duration_ms ||
        left.tool_group.localeCompare(right.tool_group),
    ),
    tool_commands: [...toolCommandTotals.values()].sort(
      (left, right) =>
        right.duration_ms - left.duration_ms ||
        left.tool_group.localeCompare(right.tool_group) ||
        left.command.localeCompare(right.command),
    ),
    metadata: [...metadataTotals.values()].sort(
      (left, right) =>
        right.duration_ms - left.duration_ms ||
        left.category.localeCompare(right.category),
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
    longest_profile_entries: [...profileEntries]
      .sort((left, right) => right.duration_ms - left.duration_ms)
      .slice(0, 5)
      .map((entry) => ({
        activity: entry.activity,
        kind: entry.kind,
        tool_group: entry.tool_group,
        tool_command_group: entry.tool_command_group,
        harness_source: entry.harness_source,
        harness_phase: entry.harness_phase,
        duration_ms: entry.duration_ms,
        timing_quality: entry.timing_quality,
        source_index: entry.source_index,
        excerpt: entry.excerpt,
      })),
    warnings: conversation.warnings,
  };
}

function wallTime(events) {
  if (events.length === 0) {
    return 0;
  }
  const nonMetadataEvents = events.filter((event) => !isMetadataEvent(event));
  if (nonMetadataEvents.length === 0) {
    return Math.max(...events.map((event) => event.end_ms)) -
      Math.min(...events.map((event) => event.start_ms));
  }

  const startMs = Math.min(...nonMetadataEvents.map((event) => event.start_ms));
  const endMs = Math.max(
    ...nonMetadataEvents.map((event) => event.end_ms),
    ...events.filter(isMetadataEvent).map((event) => event.start_ms),
  );
  return endMs - startMs;
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

  lines.push("", "## Viewer Files", "");
  lines.push("Open `*.perfetto.json` files in Perfetto or another Chrome trace viewer.");
  lines.push("Run `speedscope <file>.speedscope.json` for Speedscope profiles.");
  lines.push("Run `snakeviz <file>.snakeviz.prof` for SnakeViz profiles.");
  lines.push(
    "Run `flamegraph.pl --countname=us <file>.folded > <file>.svg` for folded stacks.",
  );
  lines.push("");
  lines.push("| Viewer | File |");
  lines.push("| --- | --- |");
  for (const file of [
    summary.outputs.combined,
    ...summary.outputs.split.map((item) => item.path),
  ]) {
    lines.push(`| Perfetto | ${markdownTableCell(file)} |`);
  }
  for (const file of viewerFiles(summary.outputs.speedscope)) {
    lines.push(`| Speedscope | ${markdownTableCell(file)} |`);
  }
  for (const file of viewerFiles(summary.outputs.snakeviz)) {
    lines.push(`| SnakeViz | ${markdownTableCell(file)} |`);
  }
  for (const file of viewerFiles(summary.outputs.snakeviz_stats)) {
    lines.push(`| SnakeViz stats table | ${markdownTableCell(file)} |`);
  }
  for (const file of viewerFiles(summary.outputs.flamegraph)) {
    lines.push(`| FlameGraph folded stack | ${markdownTableCell(file)} |`);
  }

  lines.push("", "## Time By Activity", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    lines.push("| Activity | Duration | Percent of wall time |");
    lines.push("| --- | ---: | ---: |");
    for (const [activity, duration] of Object.entries(
      conversation.totals_by_activity,
    )) {
      lines.push(
        `| ${markdownTableCell(activity)} | ${formatMs(duration)} | ${
          conversation.percent_by_activity[activity] ?? 0
        }% |`,
      );
    }
    lines.push("");
  }

  lines.push("## Tool Time By Group", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.tool_groups.length === 0) {
      lines.push("No tool calls.", "");
      continue;
    }
    lines.push("| Tool group | Count | Duration |");
    lines.push("| --- | ---: | ---: |");
    for (const tool of conversation.tool_groups) {
      lines.push(
        `| ${markdownTableCell(tool.tool_group)} | ${tool.count} | ${formatMs(
          tool.duration_ms,
        )} |`,
      );
    }
    lines.push("");
  }

  lines.push("## Tool Time By Command", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.tool_commands.length === 0) {
      lines.push("No tool calls.", "");
      continue;
    }
    lines.push("| Tool group | Command | Count | Duration |");
    lines.push("| --- | --- | ---: | ---: |");
    for (const command of conversation.tool_commands) {
      lines.push(
        `| ${markdownTableCell(command.tool_group)} | ${markdownTableCell(
          command.command,
        )} | ${command.count} | ${formatMs(command.duration_ms)} |`,
      );
    }
    lines.push("");
  }

  lines.push("## Excluded Metadata", "");
  lines.push(
    "These records are kept in raw kind totals but excluded from viewer profiles and Time By Activity.",
  );
  lines.push("");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.metadata.length === 0) {
      lines.push("No metadata records excluded.", "");
      continue;
    }
    lines.push("| Category | Count | Duration | Measured | Inferred |");
    lines.push("| --- | ---: | ---: | ---: | ---: |");
    for (const metadata of conversation.metadata) {
      lines.push(
        `| ${markdownTableCell(metadata.category)} | ${
          metadata.count
        } | ${formatMs(metadata.duration_ms)} | ${formatMs(
          metadata.measured_duration_ms,
        )} | ${formatMs(metadata.inferred_duration_ms)} |`,
      );
    }
    lines.push("");
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

  lines.push("## Longest Profile Entries", "");
  for (const conversation of summary.conversations) {
    lines.push(`### ${markdownHeadingText(conversation.label)}`, "");
    if (conversation.longest_profile_entries.length === 0) {
      lines.push("No profile entries.", "");
      continue;
    }

    lines.push("| Activity | Kind | Tool group | Command | Duration | Timing | Source index | Excerpt |");
    lines.push("| --- | --- | --- | --- | ---: | --- | ---: | --- |");
    for (const entry of conversation.longest_profile_entries) {
      lines.push(
        `| ${markdownTableCell(entry.activity)} | ${markdownTableCell(
          entry.kind,
        )} | ${markdownTableCell(entry.tool_group ?? "")} | ${markdownTableCell(
          entry.tool_command_group ?? "",
        )} | ${formatMs(
          entry.duration_ms,
        )} | ${markdownTableCell(entry.timing_quality)} | ${
          entry.source_index
        } | ${markdownTableCell(entry.excerpt)} |`,
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

function viewerFiles(outputGroup) {
  return [
    outputGroup.combined,
    ...outputGroup.split.map((output) => output.path),
  ];
}

function formatMs(value) {
  if (value < 1000) {
    return `${value}ms`;
  }
  return `${(value / 1000).toFixed(2)}s`;
}

function formatSeconds(value) {
  if (value === 0) {
    return "0.000";
  }
  return value.toFixed(6);
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

function splitOutputBasenames(conversations) {
  const used = new Set();
  return conversations.map(({ side, label }) => {
    const base = safeFileName(label);
    const basename = uniqueSplitOutputBasename(side, base, used);
    used.add(basename);
    return { side, label, basename };
  });
}

function splitOutputFiles(outDir, splitOutputs, extension) {
  return splitOutputs.map(({ side, label, basename: outputBasename }) => ({
    side,
    label,
    path: join(outDir, `${outputBasename}${extension}`),
  }));
}

function uniqueSplitOutputBasename(side, base, used) {
  const candidates = [base, `${side}-${base}`];
  for (const candidate of candidates) {
    if (!used.has(candidate)) {
      return candidate;
    }
  }

  for (let suffix = 2; ; suffix += 1) {
    const candidate = `${side}-${base}-${suffix}`;
    if (!used.has(candidate)) {
      return candidate;
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
