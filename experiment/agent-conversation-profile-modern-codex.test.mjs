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
  assert.match(folded, /tool:loc;command:create-page/);
  assert.match(folded, /tool:loc;command:push/);
  assert.match(folded, /tool:loc;command:diff\+status/);
  assert.match(folded, /tool:non_loc;command:git/);
  assert.doesNotMatch(folded, /(?:^|;)timing:/m);

  const statsTable = readFileSync(
    join(outDir, "modern-codex.snakeviz.stats.md"),
    "utf8",
  );
  assert.match(statsTable, /# SnakeViz Stats/);
  assert.match(statsTable, /## Tool Command Breakdown/);
  assert.match(statsTable, /command:push/);
  assert.match(statsTable, /command:git/);
  assert.match(statsTable, /\|\s*modern-codex\s*\|\s*loc\s*\|\s*push\s*\|/);
  assert.match(statsTable, /\|\s*modern-codex\s*\|\s*non_loc\s*\|\s*git\s*\|/);
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

function runProfiler(args) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
}
