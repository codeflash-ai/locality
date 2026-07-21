import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
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
    "combined.snakeviz.prof",
    "claude.snakeviz.prof",
    "codex.snakeviz.prof",
    "combined.speedscope.json",
    "claude.speedscope.json",
    "codex.speedscope.json",
    "combined.folded",
    "claude.folded",
    "codex.folded",
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
  assert.equal(
    summary.outputs.snakeviz.combined,
    join(outDir, "combined.snakeviz.prof"),
  );
  assert.equal(
    summary.outputs.flamegraph.combined,
    join(outDir, "combined.folded"),
  );
  assert.equal(
    summary.outputs.speedscope.combined,
    join(outDir, "combined.speedscope.json"),
  );
  assert.deepEqual(
    summary.outputs.snakeviz.split.map((output) => basename(output.path)),
    ["claude.snakeviz.prof", "codex.snakeviz.prof"],
  );
  assert.deepEqual(
    summary.outputs.speedscope.split.map((output) => basename(output.path)),
    ["claude.speedscope.json", "codex.speedscope.json"],
  );
  assert.deepEqual(
    summary.outputs.flamegraph.split.map((output) => basename(output.path)),
    ["claude.folded", "codex.folded"],
  );

  const claude = summary.conversations.find(
    (conversation) => conversation.label === "claude",
  );
  const codex = summary.conversations.find(
    (conversation) => conversation.label === "codex",
  );

  assert.equal(claude.totals_by_kind.reasoning, 1200);
  assert.equal(claude.totals_by_kind.tool_call, 2000);
  assert.equal(claude.totals_by_activity.reasoning, 1200);
  assert.equal(claude.totals_by_activity.tool, 2000);
  assert.equal(
    claude.tools.find((tool) => tool.tool_name === "exec_command").count,
    1,
  );
  assert.equal(
    claude.tool_groups.find((tool) => tool.tool_group === "exec_command")
      .duration_ms,
    2000,
  );
  assert.equal(codex.totals_by_kind.reasoning, 1000);
  assert.equal(codex.totals_by_kind.tool_call, 1500);
  assert.equal(codex.totals_by_activity.reasoning, 1000);
  assert.equal(codex.totals_by_activity.tool, 1500);
  assert.ok(claude.inferred_duration_ms > 0);

  const summaryMarkdown = readFileSync(join(outDir, "summary.md"), "utf8");
  assert.match(summaryMarkdown, /claude/);
  assert.match(summaryMarkdown, /codex/);
  assert.match(summaryMarkdown, /exec_command/);
  assert.match(summaryMarkdown, /## Viewer Files/);
  assert.match(summaryMarkdown, /speedscope <file>\.speedscope\.json/);
  assert.match(summaryMarkdown, /snakeviz <file>\.snakeviz\.prof/);
  assert.match(summaryMarkdown, /flamegraph\.pl --countname=us/);

  const combinedSpeedscope = JSON.parse(
    readFileSync(join(outDir, "combined.speedscope.json"), "utf8"),
  );
  assert.equal(
    combinedSpeedscope.$schema,
    "https://www.speedscope.app/file-format-schema.json",
  );
  assert.equal(combinedSpeedscope.exporter, "agent-conversation-profile");
  assert.equal(combinedSpeedscope.profiles.length, 1);
  assert.equal(combinedSpeedscope.profiles[0].type, "sampled");
  assert.equal(combinedSpeedscope.profiles[0].unit, "milliseconds");
  assert.equal(
    combinedSpeedscope.profiles[0].samples.length,
    combinedSpeedscope.profiles[0].weights.length,
  );
  assertSpeedscopeSample(
    combinedSpeedscope,
    [
      "agent-conversation-profile",
      "conversation:claude",
      "activity:tool",
      "tool:exec_command",
      "timing:measured",
    ],
    2000,
  );

  const claudeSpeedscope = JSON.parse(
    readFileSync(join(outDir, "claude.speedscope.json"), "utf8"),
  );
  assertSpeedscopeSample(
    claudeSpeedscope,
    [
      "conversation:claude",
      "activity:tool",
      "tool:exec_command",
      "timing:measured",
    ],
    2000,
  );
  assert.ok(
    !claudeSpeedscope.shared.frames.some(
      (frame) => frame.name === "agent-conversation-profile",
    ),
  );

  const combinedFolded = readFileSync(join(outDir, "combined.folded"), "utf8");
  assert.match(
    combinedFolded,
    /^agent-conversation-profile;conversation:claude;activity:tool;tool:exec_command;timing:measured 2000000$/m,
  );
  assert.doesNotMatch(combinedFolded, /activity:tool_result/);
  for (const line of combinedFolded.trim().split("\n")) {
    assert.match(line, / \d+$/, "folded stack weights should be integers");
  }

  const claudeFolded = readFileSync(join(outDir, "claude.folded"), "utf8");
  assert.match(
    claudeFolded,
    /^conversation:claude;activity:tool;tool:exec_command;timing:measured 2000000$/m,
  );
  assert.doesNotMatch(claudeFolded, /^agent-conversation-profile;/m);

  assertPstatsLoads(join(outDir, "combined.snakeviz.prof"));
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

test("matches Codex function call outputs by call_id before item id", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-call-id-"));
  const leftPath = join(temp, "codex.json");
  const rightPath = join(temp, "baseline.json");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    JSON.stringify(
      {
        events: [
          {
            created_at: "2026-07-20T10:00:00.000Z",
            item: {
              type: "function_call",
              id: "item-1",
              call_id: "call-1",
              name: "first_tool",
            },
          },
          {
            created_at: "2026-07-20T10:00:01.000Z",
            item: {
              type: "function_call",
              id: "item-2",
              call_id: "call-2",
              name: "second_tool",
            },
          },
          {
            created_at: "2026-07-20T10:00:04.000Z",
            item: {
              type: "function_call_output",
              call_id: "call-1",
              output: "first ok",
            },
          },
          {
            created_at: "2026-07-20T10:00:07.000Z",
            item: {
              type: "function_call_output",
              call_id: "call-2",
              output: "second ok",
            },
          },
        ],
      },
      null,
      2,
    ),
  );
  writeFileSync(
    rightPath,
    JSON.stringify([{ timestamp: "2026-07-20T10:00:00Z", role: "user" }]),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "codex",
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  const codex = summary.conversations.find(
    (conversation) => conversation.label === "codex",
  );
  assert.equal(codex.totals_by_activity.tool, 10000);
  assert.deepEqual(codex.tool_groups, [
    { tool_group: "second_tool", count: 1, duration_ms: 6000 },
    { tool_group: "first_tool", count: 1, duration_ms: 4000 },
  ]);
  assert.equal(codex.longest_profile_entries[0].activity, "tool");
  assert.equal(codex.longest_profile_entries[0].kind, "tool_call");
  assert.equal(codex.longest_profile_entries[0].tool_group, "second_tool");
  assert.equal(codex.longest_profile_entries[0].duration_ms, 6000);
  assert.equal(codex.longest_profile_entries[0].timing_quality, "inferred");
  assert.equal(codex.longest_profile_entries[0].source_index, 1);
  assert.match(codex.longest_profile_entries[0].excerpt, /call-2/);
  assert.match(codex.longest_profile_entries[0].excerpt, /second_tool/);

  const codexSpeedscope = JSON.parse(
    readFileSync(join(outDir, "codex.speedscope.json"), "utf8"),
  );
  assertSpeedscopeSample(
    codexSpeedscope,
    ["conversation:codex", "activity:tool", "tool:second_tool", "timing:inferred"],
    6000,
  );

  const folded = readFileSync(join(outDir, "codex.folded"), "utf8");
  assert.match(
    folded,
    /^conversation:codex;activity:tool;tool:second_tool;timing:inferred 6000000$/m,
  );
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
  assert.deepEqual(
    summary.outputs.split.map((output) => basename(output.path)),
    ["a_b.perfetto.json", "right-a_b.perfetto.json"],
  );
  assert.deepEqual(
    summary.outputs.snakeviz.split.map((output) => basename(output.path)),
    ["a_b.snakeviz.prof", "right-a_b.snakeviz.prof"],
  );
  assert.deepEqual(
    summary.outputs.flamegraph.split.map((output) => basename(output.path)),
    ["a_b.folded", "right-a_b.folded"],
  );
  assert.deepEqual(
    summary.outputs.speedscope.split.map((output) => basename(output.path)),
    ["a_b.speedscope.json", "right-a_b.speedscope.json"],
  );
  for (const output of [
    ...summary.outputs.split,
    ...summary.outputs.snakeviz.split,
    ...summary.outputs.flamegraph.split,
    ...summary.outputs.speedscope.split,
  ]) {
    assert.ok(existsSync(output.path), `${output.path} should be written`);
  }
});

test("sanitizes folded stack frame separators", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-folded-sanitize-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    JSON.stringify([
      {
        timestamp: "2026-07-20T10:00:00Z",
        type: "tool_use",
        name: "shell;exec\nrun",
        duration_ms: 25,
      },
    ]),
  );
  writeFileSync(
    rightPath,
    JSON.stringify([{ timestamp: "2026-07-20T10:00:00Z", role: "user" }]),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "left;run",
    "--right",
    rightPath,
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const folded = readFileSync(join(outDir, "combined.folded"), "utf8");
  assert.match(
    folded,
    /^agent-conversation-profile;conversation:left_run;activity:tool;tool:shell_exec_run;timing:measured 25000$/m,
  );
  assert.doesNotMatch(folded, /conversation:left;run/);
  assert.doesNotMatch(folded, /tool:shell;exec/);
});

test("groups Bash loc invocations separately from other Bash calls", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-bash-loc-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  const records = [
    {
      timestamp: "2026-07-20T10:00:00Z",
      type: "tool_use",
      name: "Bash",
      input: { command: "loc status" },
      duration_ms: 100,
    },
    {
      timestamp: "2026-07-20T10:00:01Z",
      type: "tool_use",
      name: "Bash",
      input: { command: "echo loc" },
      duration_ms: 200,
    },
    {
      timestamp: "2026-07-20T10:00:02Z",
      type: "tool_use",
      name: "Bash",
      input: { command: "cd repo && /usr/bin/loc diff" },
      duration_ms: 300,
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
  const left = summary.conversations[0];
  assert.equal(left.totals_by_activity.tool, 600);
  assert.deepEqual(left.tool_groups, [
    { tool_group: "bash_loc", count: 2, duration_ms: 400 },
    { tool_group: "bash_other", count: 1, duration_ms: 200 },
  ]);

  const folded = readFileSync(join(outDir, "left.folded"), "utf8");
  assert.match(
    folded,
    /^conversation:left;activity:tool;tool:bash_loc;timing:measured 400000$/m,
  );
  assert.match(
    folded,
    /^conversation:left;activity:tool;tool:bash_other;timing:measured 200000$/m,
  );
});

test("excludes harness metadata from high-level activity profiles", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-metadata-"));
  const leftPath = join(temp, "left.json");
  const rightPath = join(temp, "right.json");
  const outDir = join(temp, "out");

  const records = [
    {
      timestamp: "2026-07-20T10:00:00Z",
      role: "user",
      content: "Start.",
    },
    {
      timestamp: "2026-07-20T10:00:01Z",
      type: "system",
      subtype: "turn_duration",
      durationMs: 5000,
      isMeta: true,
    },
    {
      timestamp: "2026-07-20T10:00:02Z",
      type: "attachment",
      attachment: { type: "task_reminder", content: [] },
    },
    {
      timestamp: "2026-07-20T10:00:03Z",
      role: "assistant",
      content: "Done.",
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
  const left = summary.conversations[0];
  assert.equal(left.totals_by_kind.unknown, 6000);
  assert.equal(left.metadata_duration_ms, 6000);
  assert.equal(left.totals_by_activity.other, undefined);
  assert.deepEqual(left.metadata, [
    {
      category: "system:turn_duration",
      count: 1,
      duration_ms: 5000,
      measured_duration_ms: 5000,
      inferred_duration_ms: 0,
    },
    {
      category: "attachment:task_reminder",
      count: 1,
      duration_ms: 1000,
      measured_duration_ms: 0,
      inferred_duration_ms: 1000,
    },
  ]);

  const folded = readFileSync(join(outDir, "left.folded"), "utf8");
  assert.doesNotMatch(folded, /activity:metadata/);
  assert.doesNotMatch(folded, /activity:other/);

  const summaryMarkdown = readFileSync(join(outDir, "summary.md"), "utf8");
  assert.match(summaryMarkdown, /## Excluded Metadata/);
  assert.match(summaryMarkdown, /system:turn_duration/);
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

function assertPstatsLoads(path) {
  const result = spawnSync(
    "python3",
    [
      "-c",
      "import pstats, sys; stats = pstats.Stats(sys.argv[1]); assert stats.stats",
      path,
    ],
    {
      encoding: "utf8",
    },
  );
  assert.equal(result.status, 0, result.stderr || result.stdout);
}

function assertSpeedscopeSample(profileFile, expectedStack, expectedWeight) {
  const frameNames = profileFile.shared.frames.map((frame) => frame.name);
  const profile = profileFile.profiles[0];
  const sampleIndex = profile.samples.findIndex((sample) =>
    arrayEquals(
      sample.map((frameIndex) => frameNames[frameIndex]),
      expectedStack,
    ),
  );
  assert.notEqual(
    sampleIndex,
    -1,
    `expected Speedscope stack ${expectedStack.join(";")}`,
  );
  assert.equal(profile.weights[sampleIndex], expectedWeight);
}

function arrayEquals(left, right) {
  return (
    left.length === right.length &&
    left.every((value, index) => value === right[index])
  );
}

function runProfiler(args) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
}
