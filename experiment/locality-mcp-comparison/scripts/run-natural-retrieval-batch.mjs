#!/usr/bin/env node

import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, relative, resolve } from "node:path";

const SCRIPT_DIR = dirname(new URL(import.meta.url).pathname);
const EXPERIMENT_DIR = resolve(SCRIPT_DIR, "..");
const REPO_DIR = resolve(process.env.REPO_DIR ?? "/home/amika/workspace/locality");
const OUT_ROOT = resolve(process.env.NATURAL_OUT_ROOT ?? join(REPO_DIR, "experiment/runs-2"));
const BATCH_ID = process.env.NATURAL_BATCH_ID ?? utcStamp();
const RUNS = positiveInteger(process.env.NATURAL_RUNS ?? "2", "NATURAL_RUNS");
const CODEX_MODEL = process.env.CODEX_MODEL ?? "gpt-5.6-luna";
const CODEX_REASONING_EFFORT = process.env.CODEX_REASONING_EFFORT ?? "low";
const CODEX_EXEC_TIMEOUT_SECONDS = process.env.CODEX_EXEC_TIMEOUT_SECONDS ?? "900";
const LOC_BIN = resolve(process.env.LOC_BIN ?? join(REPO_DIR, "target/debug/loc"));
const LOCALITY_SOURCE_ROOT = resolve(process.env.LOCALITY_SOURCE_ROOT ?? "/home/amika/notion");
const INCLUDE_FILES_ONLY = process.env.NATURAL_INCLUDE_FILES_ONLY === "1";
const SINGLE_PAIR = process.env.NATURAL_SINGLE_PAIR === "1";

const scenarios = [
  {
    id: "daily-engineering-update",
    title: "Daily Engineering Update",
    variants: [
      {
        id: "a",
        prompt:
          "Prepare today's engineering update for the team. Look at recent repository work and any relevant company context you can access. Summarize what changed, why it matters, risks, blockers, and suggested next actions. Write the result as a Markdown draft. Do not publish it remotely.",
      },
      {
        id: "b",
        prompt:
          "I need a short standup-style update for Locality based on what changed recently. Please discover the relevant context yourself, connect code changes to product or launch work where possible, and produce a grounded Markdown draft. Do not push or update any remote source.",
      },
    ],
  },
  {
    id: "launch-readiness-review",
    title: "Launch Readiness Review",
    variants: [
      {
        id: "a",
        prompt:
          "We are considering whether Locality is ready for a broader launch. Review recent engineering work and relevant internal context, then draft a launch-readiness assessment with evidence, risks, blockers, and the next validation steps. Do not publish it remotely.",
      },
      {
        id: "b",
        prompt:
          "Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo. Do not push anything.",
      },
    ],
  },
];

const strategies = [
  {
    id: "locality-natural",
    label: "Locality Natural",
    report: "report-body.md",
    allowed: [
      "local git commands in REPO_DIR",
      "mounted Locality files under LOCALITY_SOURCE_ROOT",
      "`loc` CLI commands",
    ],
    forbidden: [
      "Notion MCP tools",
      "direct Notion API calls",
      "publishing or pushing remote changes",
    ],
    guidance:
      "Use Locality-connected files and the `loc` CLI when helpful. For discovery, prefer `loc search <query>` first, then inspect mounted files. Use `loc info`, `loc status`, and `loc diff` when you need source or sync state. Do not use Notion MCP or direct Notion APIs.",
    addDirs: () => [LOCALITY_SOURCE_ROOT],
  },
  {
    id: "notion-mcp-natural",
    label: "Notion MCP Natural",
    report: "report-body.md",
    allowed: [
      "local git commands in REPO_DIR",
      "Notion MCP tools for company context",
    ],
    forbidden: [
      "mounted Locality files",
      "`loc` commands",
      "publishing or updating Notion",
    ],
    guidance:
      "Use Notion MCP for company context and local git commands for repository context. Do not read Locality-mounted files and do not use `loc`.",
    addDirs: () => [],
  },
];

if (INCLUDE_FILES_ONLY) {
  strategies.push({
    id: "locality-files-only",
    label: "Locality Files Only",
    report: "report-body.md",
    allowed: [
      "local git commands in REPO_DIR",
      "mounted Locality files under LOCALITY_SOURCE_ROOT",
    ],
    forbidden: [
      "`loc` commands",
      "Notion MCP tools",
      "direct Notion API calls",
      "publishing or pushing remote changes",
    ],
    guidance:
      "Use the mounted Locality files and local git commands only. Do not use `loc`, Notion MCP, or direct Notion APIs.",
    addDirs: () => [LOCALITY_SOURCE_ROOT],
  });
}

function main() {
  ensureRepo();
  const batchDir = join(OUT_ROOT, BATCH_ID);
  mkdirSync(batchDir, { recursive: true });

  const selectedScenarios = SINGLE_PAIR ? scenarios.slice(0, 1) : scenarios;
  const selectedVariants = (scenario) =>
    SINGLE_PAIR ? scenario.variants.slice(0, 1) : scenario.variants;
  const selectedRuns = SINGLE_PAIR ? 1 : RUNS;
  const pairSummaries = [];

  for (const scenario of selectedScenarios) {
    for (const variant of selectedVariants(scenario)) {
      for (let repeat = 1; repeat <= selectedRuns; repeat += 1) {
        const pairDir = join(
          batchDir,
          scenario.id,
          `variant-${variant.id}`,
          `repeat-${repeat}`,
        );
        mkdirSync(pairDir, { recursive: true });
        writeJson(join(pairDir, "scenario.json"), {
          batch_id: BATCH_ID,
          scenario: {
            id: scenario.id,
            title: scenario.title,
          },
          variant,
          repeat,
          model: CODEX_MODEL,
          reasoning_effort: CODEX_REASONING_EFFORT,
          locality_source_root: LOCALITY_SOURCE_ROOT,
        });

        const runSummaries = [];
        for (const strategy of strategiesForPair(scenario, strategies)) {
          const runDir = join(pairDir, strategy.id);
          mkdirSync(runDir, { recursive: true });
          runSummaries.push(runStrategy({ scenario, variant, repeat, strategy, pairDir, runDir }));
        }

        const profileSummary = profilePair(pairDir, runSummaries);
        const pairSummary = {
          scenario_id: scenario.id,
          variant_id: variant.id,
          repeat,
          pair_dir: relative(REPO_DIR, pairDir),
          runs: runSummaries,
          profile_summary: profileSummary,
        };
        pairSummaries.push(pairSummary);
        writeJson(join(pairDir, "pair-summary.json"), pairSummary);
      }
    }
  }

  writeBatchSummary(batchDir, pairSummaries);
  console.log(`Natural retrieval batch written to ${batchDir}`);
}

function strategiesForPair(scenario, allStrategies) {
  if (!INCLUDE_FILES_ONLY) {
    return allStrategies;
  }
  if (scenario.id !== "daily-engineering-update") {
    return allStrategies.filter((strategy) => strategy.id !== "locality-files-only");
  }
  return allStrategies;
}

function runStrategy({ scenario, variant, repeat, strategy, runDir }) {
  const promptPath = join(runDir, "prompt.md");
  const reportPath = join(runDir, strategy.report);
  const finalPath = join(runDir, "agent-final.md");
  const tracePath = join(runDir, "agent-trace.md");
  const evidencePath = join(runDir, "evidence-manifest.json");
  const eventsPath = join(runDir, "codex-events.jsonl");
  const errPath = join(runDir, "codex.err");
  const summaryPath = join(runDir, "codex-summary.json");
  const eventsTsvPath = join(runDir, "codex-events.tsv");
  const commandPath = join(runDir, "codex-command.txt");
  const localityTracePath = join(runDir, "agent-locality-trace.jsonl");
  const prompt = renderPrompt({
    scenario,
    variant,
    repeat,
    strategy,
    reportPath,
    evidencePath,
    tracePath,
  });

  writeFileSync(promptPath, prompt);
  writeJson(join(runDir, "strategy.json"), {
    id: strategy.id,
    label: strategy.label,
    allowed: strategy.allowed,
    forbidden: strategy.forbidden,
    guidance: strategy.guidance,
  });

  const addDirs = [runDir, ...strategy.addDirs().filter((dir) => existsSync(dir))];
  const startedAt = Date.now();
  const rc = runCodex({
    promptPath,
    finalPath,
    eventsPath,
    errPath,
    commandPath,
    localityTracePath,
    addDirs,
  });
  const endedAt = Date.now();
  const durationMs = endedAt - startedAt;

  runIfExists("python3", [
    join(EXPERIMENT_DIR, "scripts/summarize-codex-events.py"),
    eventsPath,
    summaryPath,
    eventsTsvPath,
  ]);
  runIfExists("python3", [
    join(EXPERIMENT_DIR, "scripts/codex-events-to-trace.py"),
    eventsPath,
    join(runDir, "codex"),
  ]);

  const codexSummary = readJsonIfExists(summaryPath);
  const result = {
    strategy_id: strategy.id,
    strategy_label: strategy.label,
    run_dir: relative(REPO_DIR, runDir),
    prompt_path: relative(REPO_DIR, promptPath),
    report_path: relative(REPO_DIR, reportPath),
    evidence_manifest_path: relative(REPO_DIR, evidencePath),
    agent_trace_path: relative(REPO_DIR, tracePath),
    events_path: relative(REPO_DIR, eventsPath),
    summary_path: relative(REPO_DIR, summaryPath),
    exit_code: rc,
    status: rc === 0 && existsSync(reportPath) ? "ok" : "failed",
    duration_ms: durationMs,
    usage: codexSummary?.usage ?? {},
    event_counts: codexSummary?.event_counts ?? {},
    item_counts: codexSummary?.item_counts ?? {},
    tool_counts: codexSummary?.tool_counts ?? {},
    errors: codexSummary?.errors ?? [],
  };
  writeJson(join(runDir, "run-summary.json"), result);
  return result;
}

function runCodex({
  promptPath,
  finalPath,
  eventsPath,
  errPath,
  commandPath,
  localityTracePath,
  addDirs,
}) {
  const addDirsFile = `${commandPath}.add-dirs`;
  writeFileSync(addDirsFile, `${addDirs.join("\n")}\n`);
  const bash = String.raw`
set -euo pipefail
prompt="$(cat "$PROMPT_PATH")"
cmd=(
  codex exec
  --json
  --model "$CODEX_MODEL"
  -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\""
  --dangerously-bypass-approvals-and-sandbox
  -C "$REPO_DIR"
  --output-last-message "$FINAL_PATH"
)
while IFS= read -r add_dir; do
  if [ -n "$add_dir" ]; then
    cmd+=(--add-dir "$add_dir")
  fi
done < "$ADD_DIRS_FILE"
cmd+=("$prompt")
if [ "$CODEX_EXEC_TIMEOUT_SECONDS" = "0" ]; then
  run_cmd=("${"$"}{cmd[@]}")
elif command -v timeout >/dev/null 2>&1; then
  run_cmd=(timeout --kill-after=30s "${"$"}{CODEX_EXEC_TIMEOUT_SECONDS}s" "${"$"}{cmd[@]}")
else
  run_cmd=(python3 "$EXPERIMENT_DIR/scripts/run-with-timeout.py" "$CODEX_EXEC_TIMEOUT_SECONDS" -- "${"$"}{cmd[@]}")
fi
{
  printf 'timeout_seconds=%s\n' "$CODEX_EXEC_TIMEOUT_SECONDS"
  printf 'codex_command='
  printf '%q ' "${"$"}{cmd[@]}"
  printf '\nwrapped_command='
  printf '%q ' "${"$"}{run_cmd[@]}"
  printf '\n'
} > "$COMMAND_PATH"
set +e
set -o pipefail
LOCALITY_TRACE_FILE="$LOCALITY_TRACE_FILE" LOCALITY_TRACE_RUN_ID="$NATURAL_BATCH_ID" \
  "${"$"}{run_cmd[@]}" < /dev/null 2> "$ERR_PATH" | python3 "$EXPERIMENT_DIR/scripts/timestamp-jsonl.py" > "$EVENTS_PATH"
pipe_status=("${"$"}{PIPESTATUS[@]}")
rc="${"$"}{pipe_status[0]}"
set +o pipefail
set -e
exit "$rc"
`;
  const result = spawnSync("bash", ["-lc", bash], {
    cwd: REPO_DIR,
    env: {
      ...process.env,
      REPO_DIR,
      EXPERIMENT_DIR,
      PROMPT_PATH: promptPath,
      FINAL_PATH: finalPath,
      EVENTS_PATH: eventsPath,
      ERR_PATH: errPath,
      COMMAND_PATH: commandPath,
      ADD_DIRS_FILE: addDirsFile,
      LOCALITY_TRACE_FILE: localityTracePath,
      NATURAL_BATCH_ID: BATCH_ID,
      CODEX_MODEL,
      CODEX_REASONING_EFFORT,
      CODEX_EXEC_TIMEOUT_SECONDS,
    },
    stdio: "inherit",
  });
  return result.status ?? 1;
}

function renderPrompt({ scenario, variant, repeat, strategy, reportPath, evidencePath, tracePath }) {
  return `You are participating in the Locality natural retrieval benchmark.

Scenario: ${scenario.title}
Variant: ${variant.id}
Repeat: ${repeat}

Natural user request:

${variant.prompt}

Allowed context sources:
${strategy.allowed.map((item) => `- ${item}`).join("\n")}

Forbidden context sources/actions:
${strategy.forbidden.map((item) => `- ${item}`).join("\n")}

Strategy guidance:

${strategy.guidance}

Important benchmark rules:

- Discover relevant company context yourself. Do not assume a known Notion URL, page title, or mounted path.
- Do not use precomputed context inventories or previous experiment output directories.
- You may inspect recent git history and repository files as needed.
- Be explicit when evidence is missing or only partially verified.
- Keep the final report human, specific, and grounded in inspected evidence.

Required outputs:

1. Write the final Markdown report to:
   ${reportPath}
2. Write a compact evidence manifest JSON to:
   ${evidencePath}
3. Write an agent trace Markdown file to:
   ${tracePath}

Evidence manifest shape:

{
  "task": "${scenario.id}",
  "strategy": "${strategy.id}",
  "evidence": [
    {
      "kind": "git_commit | locality_file | notion_mcp_page | repo_file | other",
      "id": "stable identifier when available",
      "path": "local path when available",
      "title": "source title when available",
      "reason": "why this evidence mattered"
    }
  ],
  "limitations": [
    "what you could not verify"
  ]
}
`;
}

function profilePair(pairDir, runSummaries) {
  const locality = runSummaries.find((run) => run.strategy_id === "locality-natural");
  const mcp = runSummaries.find((run) => run.strategy_id === "notion-mcp-natural");
  if (!locality || !mcp) {
    return null;
  }
  const outDir = join(pairDir, "profile-locality-vs-mcp");
  mkdirSync(outDir, { recursive: true });
  const result = spawnSync(
    "node",
    [
      join(REPO_DIR, "experiment/agent-conversation-profile.mjs"),
      "--left",
      join(REPO_DIR, locality.events_path),
      "--left-label",
      "locality-natural",
      "--right",
      join(REPO_DIR, mcp.events_path),
      "--right-label",
      "notion-mcp-natural",
      "--out",
      outDir,
    ],
    { cwd: REPO_DIR, encoding: "utf8" },
  );
  if (result.status !== 0) {
    writeFileSync(join(outDir, "profile-error.log"), `${result.stdout}\n${result.stderr}`);
    return { status: "failed", out_dir: relative(REPO_DIR, outDir) };
  }
  return {
    status: "ok",
    out_dir: relative(REPO_DIR, outDir),
    summary_md: relative(REPO_DIR, join(outDir, "summary.md")),
    summary_json: relative(REPO_DIR, join(outDir, "summary.json")),
  };
}

function writeBatchSummary(batchDir, pairSummaries) {
  writeJson(join(batchDir, "batch-summary.json"), {
    batch_id: BATCH_ID,
    model: CODEX_MODEL,
    reasoning_effort: CODEX_REASONING_EFFORT,
    runs_per_prompt: RUNS,
    include_files_only: INCLUDE_FILES_ONLY,
    generated_at: new Date().toISOString(),
    pairs: pairSummaries,
  });

  const rows = [
    [
      "scenario",
      "variant",
      "repeat",
      "strategy",
      "status",
      "duration_ms",
      "input_tokens",
      "cached_input_tokens",
      "output_tokens",
      "mcp_tool_calls",
      "report_path",
    ],
  ];
  for (const pair of pairSummaries) {
    for (const run of pair.runs) {
      rows.push([
        pair.scenario_id,
        pair.variant_id,
        String(pair.repeat),
        run.strategy_id,
        run.status,
        String(run.duration_ms),
        String(run.usage?.input_tokens ?? ""),
        String(run.usage?.cached_input_tokens ?? ""),
        String(run.usage?.output_tokens ?? ""),
        String(run.tool_counts?.mcp_tool_call ?? ""),
        run.report_path,
      ]);
    }
  }
  writeFileSync(join(batchDir, "batch-summary.tsv"), rows.map((row) => row.join("\t")).join("\n") + "\n");

  const md = [
    "# Natural Retrieval Batch Summary",
    "",
    `Batch: \`${BATCH_ID}\``,
    "",
    "| Scenario | Variant | Repeat | Strategy | Status | Wall time | Input tokens | Output tokens | MCP calls | Report |",
    "| --- | --- | ---: | --- | --- | ---: | ---: | ---: | ---: | --- |",
  ];
  for (const pair of pairSummaries) {
    for (const run of pair.runs) {
      md.push(
        `| ${pair.scenario_id} | ${pair.variant_id} | ${pair.repeat} | ${run.strategy_id} | ${run.status} | ${formatMs(run.duration_ms)} | ${run.usage?.input_tokens ?? ""} | ${run.usage?.output_tokens ?? ""} | ${run.tool_counts?.mcp_tool_call ?? ""} | ${run.report_path} |`,
      );
    }
  }
  writeFileSync(join(batchDir, "batch-summary.md"), md.join("\n") + "\n");
}

function ensureRepo() {
  if (!existsSync(REPO_DIR)) {
    throw new Error(`REPO_DIR does not exist: ${REPO_DIR}`);
  }
}

function runIfExists(command, args) {
  const result = spawnSync(command, args, {
    cwd: REPO_DIR,
    encoding: "utf8",
    stdio: "inherit",
  });
  return result.status ?? 1;
}

function readJsonIfExists(path) {
  if (!existsSync(path)) {
    return null;
  }
  return JSON.parse(readFileSync(path, "utf8"));
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(value, null, 2) + "\n");
}

function formatMs(ms) {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

function positiveInteger(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function utcStamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
}

main();
