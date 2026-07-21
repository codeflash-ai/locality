#!/usr/bin/env node

import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
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
  const toolCallId = kind.startsWith("tool")
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

function toolCallIdFor(object, parent) {
  const candidate =
    object.call_id ??
    object.callId ??
    object.tool_use_id ??
    object.toolUseId ??
    object.id ??
    parent?.call_id ??
    parent?.callId ??
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
    object.input?.command ??
    object.action?.command ??
    parent?.command ??
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
    event.tool_group = toolGroupFor(event);
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
    } else {
      event.tool_group = toolGroupFor(event);
    }
  }
}

function toolGroupFor(event) {
  const toolName = event.tool_name ?? "unknown_tool";
  if (toolName.toLowerCase() === "bash") {
    return bashCommandCallsLoc(event.tool_command) ? "bash_loc" : "bash_other";
  }
  return toolName;
}

function bashCommandCallsLoc(command) {
  if (typeof command !== "string" || command.trim() === "") {
    return false;
  }
  return command
    .split(/(?:&&|\|\||[;|\n])/)
    .some((segment) => shellSegmentExecutable(segment) === "loc");
}

function shellSegmentExecutable(segment) {
  let remaining = segment.trim();
  while (remaining !== "") {
    const token = firstShellToken(remaining);
    if (!token) {
      return null;
    }
    const value = stripShellTokenQuotes(token.value);
    remaining = remaining.slice(token.end).trimStart();

    if (/^[A-Za-z_][A-Za-z0-9_]*=.*/.test(value)) {
      continue;
    }
    if (["command", "env", "nice", "nohup", "sudo", "time"].includes(value)) {
      continue;
    }
    return basename(value);
  }
  return null;
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
  return conversation.events
    .map((event, index) => profileEntryForEvent(conversation, event, index))
    .filter(Boolean);
}

function profileEntryForEvent(conversation, event, index) {
  if (event.kind === "tool_result" || isMetadataEvent(event)) {
    return null;
  }

  const activity = activityForEvent(event);
  const durationMs = event.kind === "tool_call"
    ? toolWaitDurationMs(event, conversation.events, index)
    : event.duration_ms;

  return {
    conversation_label: conversation.label,
    source_index: event.source_index,
    activity,
    kind: event.kind,
    tool_name: event.tool_name,
    tool_group: event.kind === "tool_call" ? toolGroupFor(event) : null,
    tool_command: event.tool_command,
    start_ms: event.start_ms,
    end_ms: event.start_ms + durationMs,
    duration_ms: durationMs,
    timing_quality: event.timing_quality,
    excerpt: event.excerpt,
  };
}

function activityForEvent(event) {
  if (isMetadataEvent(event)) {
    return "metadata";
  }
  if (event.kind === "tool_call") {
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
    "last-prompt",
    "mode",
    "permission-mode",
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
  if (event.timing_quality === "measured") {
    return event.duration_ms;
  }
  const result = matchingToolResult(event, events, index);
  if (!result) {
    return event.duration_ms;
  }
  return Math.max(1, result.start_ms - event.start_ms);
}

function matchingToolResult(event, events, index) {
  if (event.tool_call_id) {
    const byId = events.find(
      (candidate, candidateIndex) =>
        candidateIndex > index &&
        candidate.kind === "tool_result" &&
        candidate.tool_call_id === event.tool_call_id,
    );
    if (byId) {
      return byId;
    }
  }

  return events.find(
    (candidate, candidateIndex) =>
      candidateIndex > index &&
      candidate.kind === "tool_result" &&
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

  conversations.forEach((conversation, index) => {
    const tid = index + 1;
    traceEvents.push({
      ph: "M",
      pid: 1,
      tid,
      name: "thread_name",
      args: { name: conversation.label },
    });
    for (const event of traceableEvents(conversation)) {
      traceEvents.push(traceEventFor(event, 1, tid, baseMs));
    }
  });

  return { traceEvents };
}

function buildSplitTrace(conversation) {
  const baseMs = earliestTraceStart([conversation]);
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

  for (const event of traceableEvents(conversation)) {
    traceEvents.push(traceEventFor(event, 1, splitTidForKind(event.kind), baseMs));
  }

  return { traceEvents };
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
  }
  frames.push(speedscopeFrameName("timing", entry.timing_quality));
  return frames;
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
      }
      frames.push(`timing:${entry.timing_quality}`);

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
      }
      stack.push(builder.frame(`timing:${entry.timing_quality}`));

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
      tool_command: event.tool_command,
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
      const group = entry.tool_group ?? "unknown_tool";
      const previous = toolGroupTotals.get(group) ?? {
        tool_group: group,
        count: 0,
        duration_ms: 0,
      };
      previous.count += 1;
      previous.duration_ms += entry.duration_ms;
      toolGroupTotals.set(group, previous);
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
  lines.push("Run `speedscope <file>.speedscope.json` for Speedscope profiles.");
  lines.push("Run `snakeviz <file>.snakeviz.prof` for SnakeViz profiles.");
  lines.push(
    "Run `flamegraph.pl --countname=us <file>.folded > <file>.svg` for folded stacks.",
  );
  lines.push("");
  lines.push("| Viewer | File |");
  lines.push("| --- | --- |");
  for (const file of viewerFiles(summary.outputs.speedscope)) {
    lines.push(`| Speedscope | ${markdownTableCell(file)} |`);
  }
  for (const file of viewerFiles(summary.outputs.snakeviz)) {
    lines.push(`| SnakeViz | ${markdownTableCell(file)} |`);
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

  lines.push("## Tool Wait By Group", "");
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

    lines.push("| Activity | Kind | Tool group | Duration | Timing | Source index | Excerpt |");
    lines.push("| --- | --- | --- | ---: | --- | ---: | --- |");
    for (const entry of conversation.longest_profile_entries) {
      lines.push(
        `| ${markdownTableCell(entry.activity)} | ${markdownTableCell(
          entry.kind,
        )} | ${markdownTableCell(entry.tool_group ?? "")} | ${formatMs(
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
