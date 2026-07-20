import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scriptPath = join(repoRoot, "experiment", "agent-conversation-profile.mjs");

test("profiles Claude JSONL and Codex JSON object conversations into combined and split traces", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-"));
  const claudePath = join(temp, "claude.jsonl");
  const codexPath = join(temp, "codex.json");
  const outDir = join(temp, "out");

  writeFileSync(
    claudePath,
    [
      JSON.stringify({
        timestamp: "2026-07-20T10:00:00.000Z",
        role: "user",
        content: "Compare these agent runs.",
      }),
      JSON.stringify({
        timestamp: "2026-07-20T10:00:01.000Z",
        type: "thinking",
        text: "Need to inspect the repository.",
        duration_ms: 1200,
      }),
      JSON.stringify({
        timestamp: "2026-07-20T10:00:02.500Z",
        type: "tool_use",
        name: "exec_command",
        input: { cmd: "rg profile" },
        duration_ms: 2000,
      }),
      JSON.stringify({
        timestamp: "2026-07-20T10:00:04.800Z",
        type: "tool_result",
        tool_name: "exec_command",
        content: "profile results",
      }),
      JSON.stringify({
        timestamp: "2026-07-20T10:00:05.000Z",
        role: "assistant",
        content: "Finished.",
      }),
    ].join("\n"),
  );

  writeFileSync(
    codexPath,
    JSON.stringify(
      {
        events: [
          {
            created_at: "2026-07-20T10:00:00.250Z",
            item: {
              type: "reasoning",
              summary: [{ text: "Map normalized events." }],
            },
            duration_ms: 1000,
          },
          {
            created_at: "2026-07-20T10:00:01.800Z",
            item: {
              type: "function_call",
              name: "exec_command",
              arguments: JSON.stringify({ cmd: "cargo test" }),
            },
            duration_ms: 1500,
          },
          {
            created_at: "2026-07-20T10:00:03.400Z",
            item: {
              type: "function_call_output",
              call_id: "call-1",
              output: "ok",
            },
          },
        ],
      },
      null,
      2,
    ),
  );

  const result = runProfiler([
    "--left",
    claudePath,
    "--left-label",
    "claude",
    "--right",
    codexPath,
    "--right-label",
    "codex",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  for (const file of [
    "combined.perfetto.json",
    "claude.perfetto.json",
    "codex.perfetto.json",
    "summary.json",
    "summary.md",
  ]) {
    assert.ok(existsSync(join(outDir, file)), `${file} should be written`);
  }

  const combined = JSON.parse(
    readFileSync(join(outDir, "combined.perfetto.json"), "utf8"),
  );
  assert.ok(Array.isArray(combined.traceEvents));
  assert.ok(
    combined.traceEvents.some(
      (event) => event.ph === "M" && event.name === "thread_name",
    ),
    "trace should include track metadata",
  );

  const slices = combined.traceEvents.filter((event) => event.ph === "X");
  assert.ok(
    slices.some(
      (event) =>
        event.cat === "reasoning" &&
        event.args.conversation_label === "claude",
    ),
    "trace should include Claude reasoning slices",
  );
  assert.ok(
    slices.some(
      (event) =>
        event.cat === "tool_call" &&
        event.name.includes("exec_command") &&
        event.args.conversation_label === "codex",
    ),
    "trace should include Codex tool-call slices",
  );
  assert.ok(
    slices.some((event) => event.args.timing_quality === "inferred"),
    "trace should label inferred timing",
  );

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  const claude = summary.conversations.find(
    (conversation) => conversation.label === "claude",
  );
  const codex = summary.conversations.find(
    (conversation) => conversation.label === "codex",
  );

  assert.equal(claude.totals_by_kind.reasoning, 1200);
  assert.equal(claude.totals_by_kind.tool_call, 2000);
  assert.equal(
    claude.tools.find((tool) => tool.tool_name === "exec_command").count,
    1,
  );
  assert.equal(codex.totals_by_kind.reasoning, 1000);
  assert.equal(codex.totals_by_kind.tool_call, 1500);
  assert.ok(claude.inferred_duration_ms > 0);

  const summaryMarkdown = readFileSync(join(outDir, "summary.md"), "utf8");
  assert.match(summaryMarkdown, /claude/);
  assert.match(summaryMarkdown, /codex/);
  assert.match(summaryMarkdown, /exec_command/);
});

test("accepts JSON array inputs", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-array-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  const records = [
    {
      ts: 1_784_545_200_000,
      role: "assistant",
      content: [{ type: "reasoning", text: "Think." }],
      duration_ms: 250,
    },
  ];
  writeFileSync(leftPath, JSON.stringify(records));
  writeFileSync(rightPath, JSON.stringify(records));

  const result = runProfiler([
    "--left",
    leftPath,
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);
  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.equal(summary.conversations[0].totals_by_kind.reasoning, 250);
  assert.equal(summary.conversations[1].totals_by_kind.reasoning, 250);
});

test("profiles nested events even when the wrapper has metadata timestamps", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-wrapper-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  const wrapped = {
    timestamp: "2026-07-20T10:00:00Z",
    session_id: "session-1",
    events: [
      {
        timestamp: "2026-07-20T10:00:01Z",
        type: "thinking",
        text: "Child reasoning event.",
        duration_ms: 300,
      },
    ],
  };
  writeFileSync(leftPath, JSON.stringify(wrapped));
  writeFileSync(rightPath, JSON.stringify(wrapped));

  const result = runProfiler([
    "--left",
    leftPath,
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);
  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.equal(summary.conversations[0].totals_by_kind.reasoning, 300);
  assert.equal(summary.conversations[0].event_count, 1);
});

test("deconflicts split trace outputs when labels sanitize to the same filename", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-collision-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    JSON.stringify([{ timestamp: "2026-07-20T10:00:00Z", role: "user" }]),
  );
  writeFileSync(
    rightPath,
    JSON.stringify([{ timestamp: "2026-07-20T10:00:00Z", role: "assistant" }]),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "a/b",
    "--right",
    rightPath,
    "--right-label",
    "a b",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.equal(summary.outputs.split.length, 2);
  assert.notEqual(summary.outputs.split[0].path, summary.outputs.split[1].path);
  assert.ok(existsSync(summary.outputs.split[0].path));
  assert.ok(existsSync(summary.outputs.split[1].path));
});

test("uses timestamps from nested item or message wrappers", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-inner-ts-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    JSON.stringify([
      {
        item: {
          created_at: "2026-07-20T10:00:00Z",
          type: "reasoning",
          text: "Inner item timestamp.",
          duration_ms: 400,
        },
      },
    ]),
  );
  writeFileSync(
    rightPath,
    JSON.stringify([
      {
        message: {
          timestamp: "2026-07-20T10:00:01Z",
          role: "assistant",
          content: "Inner message timestamp.",
          duration_ms: 500,
        },
      },
    ]),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);
  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.equal(summary.conversations[0].totals_by_kind.reasoning, 400);
  assert.equal(summary.conversations[1].totals_by_kind.assistant_message, 500);
});

test("does not double-count parent duration across multiple content blocks", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-content-duration-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  const records = [
    {
      timestamp: "2026-07-20T10:00:00Z",
      role: "assistant",
      duration_ms: 1000,
      content: [
        { type: "thinking", text: "Think." },
        { type: "tool_use", name: "exec_command", input: { cmd: "date" } },
      ],
    },
  ];
  writeFileSync(leftPath, JSON.stringify(records));
  writeFileSync(rightPath, JSON.stringify(records));

  const result = runProfiler([
    "--left",
    leftPath,
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);
  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.equal(summary.conversations[0].measured_duration_ms, 1000);
  assert.equal(summary.conversations[0].totals_by_kind.reasoning, 500);
  assert.equal(summary.conversations[0].totals_by_kind.tool_call, 500);
});

test("renders longest events and escapes Markdown table cells", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-markdown-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    JSON.stringify([
      {
        timestamp: "2026-07-20T10:00:00Z",
        type: "tool_use",
        name: "shell|exec",
        duration_ms: 700,
      },
    ]),
  );
  writeFileSync(
    rightPath,
    JSON.stringify([
      {
        timestamp: "2026-07-20T10:00:00Z",
        type: "thinking",
        text: "Longest event.",
        duration_ms: 300,
      },
    ]),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "left|run",
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);
  const summaryMarkdown = readFileSync(join(outDir, "summary.md"), "utf8");
  assert.match(summaryMarkdown, /## Longest Events/);
  assert.match(summaryMarkdown, /left\\\|run/);
  assert.match(summaryMarkdown, /shell\\\|exec/);
});

test("fails with exit code 2 for malformed input", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-bad-"));
  const badPath = join(temp, "bad.json");
  const goodPath = join(temp, "good.json");
  const outDir = join(temp, "out");

  writeFileSync(badPath, "{not json");
  writeFileSync(
    goodPath,
    JSON.stringify([{ timestamp: "2026-07-20T10:00:00Z", role: "user" }]),
  );

  const result = runProfiler([
    "--left",
    badPath,
    "--right",
    goodPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 2);
  assert.match(result.stderr, /malformed|invalid|parse/i);
});

function runProfiler(args) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
}
