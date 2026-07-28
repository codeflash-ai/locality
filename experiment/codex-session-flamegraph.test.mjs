import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scriptPath = join(repoRoot, "experiment", "codex-session-flamegraph.mjs");

test("builds flamegraph artifacts from Codex TUI session JSONL", () => {
  const temp = mkdtempSync(join(tmpdir(), "codex-session-flamegraph-"));
  const input = join(temp, "rollout.jsonl");
  const outDir = join(temp, "out");

  writeFileSync(
    input,
    [
      record("2026-07-21T10:00:00.000Z", "session_meta", {
        session_id: "session-1",
      }),
      record("2026-07-21T10:00:01.000Z", "response_item", {
        type: "message",
        role: "user",
        content: [{ type: "input_text", text: "Create a checklist." }],
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:02.000Z", "response_item", {
        type: "reasoning",
        summary: [{ text: "Need to read Locality state." }],
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:03.000Z", "response_item", {
        type: "function_call",
        name: "exec_command",
        arguments: JSON.stringify({ cmd: "loc status ." }),
        call_id: "call-loc",
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:07.000Z", "response_item", {
        type: "function_call_output",
        call_id: "call-loc",
        output:
          "Chunk ID: 1\nWall time: 4.0000 seconds\nProcess exited with code 0\nOutput:\nstatus clean\n",
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:08.000Z", "response_item", {
        type: "function_call",
        name: "exec_command",
        arguments: JSON.stringify({ cmd: "sqlite3 state.sqlite3 '.tables'" }),
        call_id: "call-sqlite",
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:10.500Z", "response_item", {
        type: "function_call_output",
        call_id: "call-sqlite",
        output:
          "Chunk ID: 2\nWall time: 2.5000 seconds\nProcess exited with code 1\nOutput:\ndatabase is locked\n",
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:11.000Z", "response_item", {
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: "Done." }],
        phase: "final_answer",
        internal_chat_message_metadata_passthrough: { turn_id: "turn-1" },
      }),
      record("2026-07-21T10:00:12.000Z", "event_msg", {
        type: "task_complete",
        turn_id: "turn-1",
        duration_ms: 11000,
      }),
    ].join("\n") + "\n",
  );

  const result = runScript([
    "--input",
    input,
    "--label",
    "codex sample",
    "--out",
    outDir,
  ]);

  assert.equal(result.status, 0, result.stderr || result.stdout);

  for (const file of [
    "codex-sample.folded",
    "codex-sample.speedscope.json",
    "codex-sample.svg",
    "codex-sample.summary.json",
    "codex-sample.summary.md",
    "codex-sample.timeline.tsv",
  ]) {
    assert.ok(existsSync(join(outDir, file)), `${file} should be written`);
  }

  const folded = readFileSync(join(outDir, "codex-sample.folded"), "utf8");
  assert.match(
    folded,
    /^conversation:codex sample;turn:1 Create a checklist\.;activity:tool;tool:loc;status:ok;timing:measured 4000000$/m,
  );
  assert.match(
    folded,
    /^conversation:codex sample;turn:1 Create a checklist\.;activity:tool;tool:sqlite3;status:error;timing:measured 2500000$/m,
  );
  assert.match(folded, /activity:reasoning/);
  assert.match(folded, /activity:user_query/);
  assert.match(folded, /activity:agent_response/);

  const speedscope = JSON.parse(
    readFileSync(join(outDir, "codex-sample.speedscope.json"), "utf8"),
  );
  assert.equal(
    speedscope.$schema,
    "https://www.speedscope.app/file-format-schema.json",
  );
  assert.equal(speedscope.exporter, "codex-session-flamegraph");
  assert.equal(speedscope.profiles[0].type, "sampled");

  const summary = JSON.parse(
    readFileSync(join(outDir, "codex-sample.summary.json"), "utf8"),
  );
  assert.equal(summary.tool_totals_ms.loc, 4000);
  assert.equal(summary.tool_totals_ms.sqlite3, 2500);
  assert.equal(summary.status_totals_ms.error, 2500);
});

function record(timestamp, type, payload) {
  return JSON.stringify({ timestamp, type, payload });
}

function runScript(args) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
  });
}
