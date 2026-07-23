import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scriptPath = join(
  repoRoot,
  "experiment",
  "agent-conversation-profile-modern-codex.mjs",
);
const hookScriptPath = join(
  repoRoot,
  "experiment",
  "locality-mcp-comparison",
  "scripts",
  "codex-live-hook.py",
);

test("live Codex hook collector emits measured phase records", () => {
  const temp = mkdtempSync(join(tmpdir(), "codex-live-hook-"));
  const eventsPath = join(temp, "hooks.jsonl");
  const statePath = join(temp, "hooks.state.json");
  const clockStatePath = join(temp, "clock.state");
  const hookEnv = {
    CODEX_HARNESS_HOOK_EVENTS_FILE: eventsPath,
    CODEX_HARNESS_HOOK_STATE_FILE: statePath,
    CODEX_HARNESS_HOOK_FAKE_CLOCK_MS: "1000,1100,2000,3500,4000",
    CODEX_HARNESS_HOOK_FAKE_CLOCK_STATE: clockStatePath,
  };

  runHook(
    {
      hook_event_name: "SessionStart",
      session_id: "session-1",
      source: "startup",
      cwd: repoRoot,
      model: "fake-model",
    },
    hookEnv,
  );
  runHook(
    {
      hook_event_name: "UserPromptSubmit",
      session_id: "session-1",
      turn_id: "turn-1",
      prompt: "Inspect the page.",
    },
    hookEnv,
  );
  runHook(
    {
      hook_event_name: "PreToolUse",
      session_id: "session-1",
      turn_id: "turn-1",
      tool_name: "Bash",
      tool_use_id: "tool-1",
      tool_input: { command: 'bash -lc "loc status page.md"' },
    },
    hookEnv,
  );
  runHook(
    {
      hook_event_name: "PostToolUse",
      session_id: "session-1",
      turn_id: "turn-1",
      tool_name: "Bash",
      tool_use_id: "tool-1",
      tool_input: { command: 'bash -lc "loc status page.md"' },
      tool_response: { exit_code: 0 },
    },
    hookEnv,
  );
  runHook(
    {
      hook_event_name: "Stop",
      session_id: "session-1",
      turn_id: "turn-1",
      last_assistant_message: "Done.",
    },
    hookEnv,
  );

  const events = readFileSync(eventsPath, "utf8")
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line).event);
  const phases = events.filter((event) => event.type === "harness.phase");

  assert.deepEqual(
    phases.map((phase) => [phase.phase, phase.duration_ms]),
    [
      ["input_query", 100],
      ["thinking", 900],
      ["tool_call", 1500],
      ["output_response", 500],
    ],
  );
  assert.equal(phases[2].tool_name, "Bash");
  assert.equal(phases[2].tool_command, 'bash -lc "loc status page.md"');
});

test("profiles modern Codex command, MCP, file-change, and agent-message records", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-modern-codex-"));
  const leftPath = join(temp, "modern-codex.jsonl");
  const rightPath = join(temp, "baseline.jsonl");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    [
      record("2026-07-22T16:00:00.000Z", { type: "thread.started" }),
      record("2026-07-22T16:00:00.000Z", { type: "turn.started" }),
      record("2026-07-22T16:00:01.000Z", {
        type: "item.completed",
        item: {
          id: "msg-1",
          type: "agent_message",
          text: "I will inspect the Locality mount.",
        },
      }),
      record("2026-07-22T16:00:02.000Z", {
        type: "item.started",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc status page.md"',
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:05.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc status page.md"',
          aggregated_output: "status clean\n",
          exit_code: 0,
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:06.000Z", {
        type: "item.started",
        item: {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "notion",
          tool: "API-post-search",
          arguments: { query: "Company" },
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:08.500Z", {
        type: "item.completed",
        item: {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "notion",
          tool: "API-post-search",
          arguments: { query: "Company" },
          result: {
            content: [
              {
                type: "text",
                text: JSON.stringify({ object: "list", results: [] }),
              },
            ],
          },
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:09.000Z", {
        type: "item.started",
        item: {
          id: "file-1",
          type: "file_change",
          changes: [{ path: "/tmp/page.md", kind: "update" }],
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:12.000Z", {
        type: "item.completed",
        item: {
          id: "file-1",
          type: "file_change",
          changes: [{ path: "/tmp/page.md", kind: "update" }],
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:13.000Z", { type: "turn.completed" }),
    ].join("\n"),
  );

  writeFileSync(
    rightPath,
    record("2026-07-22T16:00:00.000Z", {
      role: "assistant",
      content: "baseline",
    }),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "modern-codex",
    "--right",
    rightPath,
    "--right-label",
    "baseline",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  const modern = summary.conversations.find(
    (conversation) => conversation.label === "modern-codex",
  );

  assert.equal(modern.totals_by_activity.agent_response, 1000);
  assert.equal(modern.totals_by_activity.tool, 8500);
  assert.equal(modern.totals_by_activity.reasoning, 3500);
  assert.equal(modern.totals_by_activity.file_edit ?? 0, 0);
  assert.equal(modern.totals_by_activity.other ?? 0, 0);
  assert.match(summary.outputs.snakeviz_stats.combined, /combined\.snakeviz\.stats\.md$/);

  assert.equal(
    modern.tools.find((tool) => tool.tool_name === "command_execution").count,
    1,
  );
  assert.equal(
    modern.tools.find((tool) => tool.tool_name === "API-post-search").count,
    1,
  );
  assert.equal(
    modern.tools.find((tool) => tool.tool_name === "file_change").count,
    1,
  );
  assert.equal(
    modern.tool_groups.find((tool) => tool.tool_group === "loc").count,
    1,
  );
  assert.equal(
    modern.tool_groups.find((tool) => tool.tool_group === "non_loc").count,
    2,
  );
  const perfetto = JSON.parse(
    readFileSync(join(outDir, "modern-codex.perfetto.json"), "utf8"),
  );
  assert(
    perfetto.traceEvents.some(
      (event) =>
        event.ph === "M" &&
        event.name === "thread_name" &&
        event.args?.name === "activity:reasoning",
    ),
  );
  assertToolCommand(modern, "loc", "status", 1);
  assertToolCommand(modern, "non_loc", "API-post-search", 1);
  assertToolCommand(modern, "non_loc", "file_change", 1);
  assert.equal(modern.metadata.length, 3);
});

test("breaks loc command tool groups down by loc subcommand", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-modern-codex-"));
  const leftPath = join(temp, "modern-codex.jsonl");
  const rightPath = join(temp, "baseline.jsonl");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    [
      record("2026-07-22T16:00:00.000Z", { type: "turn.started" }),
      record("2026-07-22T16:00:01.000Z", {
        type: "item.started",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command:
            '/usr/bin/zsh -lc "loc create page --title Launch --private"',
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:02.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command:
            '/usr/bin/zsh -lc "loc create page --title Launch --private"',
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:03.000Z", {
        type: "item.started",
        item: {
          id: "cmd-2",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc push page.md -y"',
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:04.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-2",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc push page.md -y"',
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:05.000Z", {
        type: "item.started",
        item: {
          id: "cmd-3",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc status page.md && loc diff page.md"',
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:06.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-3",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc status page.md && loc diff page.md"',
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:08.000Z", {
        type: "item.started",
        item: {
          id: "cmd-4",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "git status --short"',
          status: "in_progress",
        },
      }),
      record("2026-07-22T16:00:09.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-4",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "git status --short"',
          status: "completed",
        },
      }),
      record("2026-07-22T16:00:10.000Z", { type: "turn.completed" }),
    ].join("\n"),
  );

  writeFileSync(
    rightPath,
    record("2026-07-22T16:00:00.000Z", {
      role: "assistant",
      content: "baseline",
    }),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "modern-codex",
    "--right",
    rightPath,
    "--right-label",
    "baseline",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  const modern = summary.conversations.find(
    (conversation) => conversation.label === "modern-codex",
  );
  const groupCounts = Object.fromEntries(
    modern.tool_groups.map((group) => [group.tool_group, group.count]),
  );

  assert.equal(groupCounts.loc, 3);
  assert.equal(groupCounts.non_loc, 1);
  assertToolCommand(modern, "loc", "create-page", 1);
  assertToolCommand(modern, "loc", "push", 1);
  assertToolCommand(modern, "loc", "diff+status", 1);
  assertToolCommand(modern, "non_loc", "git", 1);

  const folded = readFileSync(join(outDir, "modern-codex.folded"), "utf8");
  assert.match(folded, /tool:loc;command:loc:create-page/);
  assert.match(folded, /tool:loc;command:loc:push/);
  assert.match(folded, /tool:loc;command:loc:diff\+status/);
  assert.match(folded, /tool:non_loc;command:non_loc:git/);
  assert.doesNotMatch(folded, /(?:^|;)timing:/m);

  const statsTable = readFileSync(
    join(outDir, "modern-codex.snakeviz.stats.md"),
    "utf8",
  );
  assert.match(statsTable, /# SnakeViz Stats/);
  assert.match(statsTable, /## Tool Command Breakdown/);
  assert.match(statsTable, /command:loc:push/);
  assert.match(statsTable, /command:non_loc:git/);
  assert.match(statsTable, /\|\s*modern-codex\s*\|\s*loc\s*\|\s*push\s*\|/);
  assert.match(statsTable, /\|\s*modern-codex\s*\|\s*non_loc\s*\|\s*git\s*\|/);
});

test("prefers live Codex hook phases for activity timing", () => {
  const temp = mkdtempSync(join(tmpdir(), "agent-profile-modern-codex-"));
  const leftPath = join(temp, "modern-codex.jsonl");
  const rightPath = join(temp, "baseline.jsonl");
  const outDir = join(temp, "out");

  writeFileSync(
    leftPath,
    [
      record("2026-07-22T16:00:00.000Z", { type: "turn.started" }),
      hookPhase("2026-07-22T16:00:00.000Z", {
        phase: "input_query",
        activity: "user_query",
        started_at_ms: Date.parse("2026-07-22T16:00:00.000Z"),
        duration_ms: 100,
      }),
      hookPhase("2026-07-22T16:00:00.100Z", {
        phase: "thinking",
        activity: "reasoning",
        started_at_ms: Date.parse("2026-07-22T16:00:00.100Z"),
        duration_ms: 900,
      }),
      record("2026-07-22T16:00:01.000Z", {
        type: "item.started",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc diff page.md"',
          status: "in_progress",
        },
      }),
      hookPhase("2026-07-22T16:00:01.000Z", {
        phase: "tool_call",
        activity: "tool",
        started_at_ms: Date.parse("2026-07-22T16:00:01.000Z"),
        duration_ms: 1000,
        tool_name: "Bash",
        tool_call_id: "tool-1",
        tool_command: '/usr/bin/zsh -lc "loc diff page.md"',
      }),
      record("2026-07-22T16:00:09.000Z", {
        type: "item.completed",
        item: {
          id: "cmd-1",
          type: "command_execution",
          command: '/usr/bin/zsh -lc "loc diff page.md"',
          status: "completed",
        },
      }),
      hookPhase("2026-07-22T16:00:02.000Z", {
        phase: "output_response",
        activity: "agent_response",
        started_at_ms: Date.parse("2026-07-22T16:00:02.000Z"),
        duration_ms: 500,
      }),
      record("2026-07-22T16:00:09.500Z", {
        type: "item.completed",
        item: {
          id: "msg-1",
          type: "agent_message",
          text: "Done.",
        },
      }),
      record("2026-07-22T16:00:10.000Z", { type: "turn.completed" }),
    ].join("\n"),
  );

  writeFileSync(
    rightPath,
    record("2026-07-22T16:00:00.000Z", {
      role: "assistant",
      content: "baseline",
    }),
  );

  const result = runProfiler([
    "--left",
    leftPath,
    "--left-label",
    "modern-codex",
    "--right",
    rightPath,
    "--right-label",
    "baseline",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  const modern = summary.conversations.find(
    (conversation) => conversation.label === "modern-codex",
  );

  assert.equal(modern.totals_by_activity.user_query, 100);
  assert.equal(modern.totals_by_activity.reasoning, 900);
  assert.equal(modern.totals_by_activity.tool, 1000);
  assert.equal(modern.totals_by_activity.agent_response, 500);
  assertToolCommand(modern, "loc", "diff", 1);
  assert.equal(
    modern.metadata.find((entry) => entry.category === "turn.started").count,
    1,
  );
});

function assertToolCommand(summary, toolGroup, command, count) {
  assert.equal(
    summary.tool_commands.find(
      (entry) => entry.tool_group === toolGroup && entry.command === command,
    )?.count,
    count,
  );
}

function record(timestamp, value) {
  return JSON.stringify({ timestamp, created_at: timestamp, ...value });
}

function hookPhase(timestamp, value) {
  return JSON.stringify({
    timestamp,
    created_at: timestamp,
    event: {
      type: "harness.phase",
      harness_source: "codex_hook",
      timing_quality: "measured",
      ...value,
    },
  });
}

function runProfiler(args) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
}

function runHook(payload, env) {
  const result = spawnSync("python3", [hookScriptPath], {
    cwd: repoRoot,
    input: JSON.stringify(payload),
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
}
