# Locality vs Notion MCP Experiment

This experiment compares two agent paths for a launch-readiness workflow:

- **Locality path:** hydrate or reuse prehydrated Locality files, let the agent
  read mounted Markdown/text/JSON files across connected sources, and write
  local Markdown artifacts under `OUT_DIR`.
- **MCP path:** let the agent use MCP tools for Notion, Linear, and Slack when
  configured, plus local git/`gh` for repository evidence, without reading
  mounted Locality files or using `loc`.

The original benchmark context lives in Notion at:

`https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba`

The current runner is artifact-only. It does not create output pages in Notion,
write mounted report pages, run `loc diff`, or push.

## Files

- `run-agent-comparison.sh` - local Amika wrapper that runs launch-readiness
  Locality scenarios on `LOCALITY_SANDBOX` and MCP scenarios on `MCP_SANDBOX`.
- `run-claude-locality-comparison.sh` - local wrapper that compares Claude Code
  on the hosted MCP path against Claude Code on the Locality path.
- `run-codex-locality-comparison.sh` - local wrapper that compares Codex on the
  hosted MCP path against Codex on the Locality path.
- `run-launch-readiness-benchmark.sh` - core benchmark runner.
- `run-repeated.sh` - runs the split Amika benchmark multiple times.
- `setup-codex-azure.sh` - writes Codex Azure config without MCP servers.
- `prompts/Locality/*.md` - Locality-only scenario prompts.
- `prompts/MCP/*.md` - Notion-MCP-only scenario prompts, paired by filename with `prompts/Locality/*.md`.
- `scripts/timestamp-jsonl.py` - timestamps Codex JSON events from stdout.
- `scripts/codex-live-hook.py` - live Codex hook collector used by the benchmark
  to measure prompt handoff, tool calls, model thinking spans, and final output
  response spans while the session is running.
- `scripts/summarize-codex-events.py` - summarizes one Codex JSON trace.
- `scripts/deep-dive-report.py` - writes a per-run Markdown index of phase
  timings, tool buckets, timelines, and trace artifact paths.
- `scripts/summarize-runs.py` - summarizes multiple run folders.

## Separation Rules

The Locality agent receives the hydrated Locality context directories as added directories and is instructed not to use Notion MCP or direct Notion API.

The Notion MCP agent does not receive those mounted Locality directories and is instructed not to use `loc` or mounted Locality files.

This is workflow separation, not a hard security boundary, because the benchmark uses `--dangerously-bypass-approvals-and-sandbox` inside an externally sandboxed Amika environment.

## Setup In Amika

From the local machine:

```bash
export LOCALITY_SANDBOX=aseem-locality
export MCP_SANDBOX=aseem-mcp
```

Seed the Azure key into both sandboxes without printing it:

```bash
line="$(python3 - <<'PY'
import os, shlex
print("export AZURE_OPENAI_API_KEY=" + shlex.quote(os.environ["AZURE_OPENAI_API_KEY"]))
PY
)"

b64="$(printf '%s\n' "$line" | base64 | tr -d '\n')"

for sandbox in "$LOCALITY_SANDBOX" "$MCP_SANDBOX"; do
  ssh_target="$(amika sandbox ssh --print "$sandbox")"
  ssh -o StrictHostKeyChecking=accept-new "$ssh_target" "
    mkdir -p ~/.config/locality-experiment &&
    chmod 700 ~/.config/locality-experiment &&
    printf '%s' '$b64' | base64 -d > ~/.config/locality-experiment/env &&
    chmod 600 ~/.config/locality-experiment/env
  "
done
```

Verify `loc` is installed on the Locality sandbox. The split wrapper defaults
`REMOTE_LOC_BIN` to `/usr/bin/loc` and intentionally does not build the CLI from
source; if the installed binary is missing, the Locality run should fail until
the sandbox is fixed.

```bash
export REMOTE_LOC_BIN="${REMOTE_LOC_BIN:-/usr/bin/loc}"
ssh_target="$(amika sandbox ssh --print "$LOCALITY_SANDBOX")"
ssh -o StrictHostKeyChecking=accept-new "$ssh_target" "
  test -x '$REMOTE_LOC_BIN' &&
    '$REMOTE_LOC_BIN' --version
"
```

Prepare the Locality sandbox with the mounted files the benchmark should use.
The launch worker no longer creates an isolated Locality state or pulls context
URLs. It adds existing filesystem roots to the Locality agent. By default it
uses these roots when they exist:

```text
/home/amika/Locality/Notion
/home/amika/Locality/Slack
/home/amika/Locality/Linear
/home/amika/notion
/home/amika/slack
/home/amika/linear
```

If the roots differ, pass them explicitly. Use newline separation when paths
contain spaces:

```bash
export LOCALITY_CONTEXT_DIRS="$(cat <<'EOF'
/home/amika/Locality/Notion
/home/amika/slack
/home/amika/linear
EOF
)"
```

Verify the files are present on the Locality sandbox:

```bash
export REMOTE_LOC_BIN="${REMOTE_LOC_BIN:-/usr/bin/loc}"
ssh_target="$(amika sandbox ssh --print "$LOCALITY_SANDBOX")"
ssh -o StrictHostKeyChecking=accept-new "$ssh_target" "
  '$REMOTE_LOC_BIN' connections --json || true
  find ~/Locality ~/notion ~/slack ~/linear -maxdepth 3 -type f 2>/dev/null | head
"
```

## Run Claude Comparison

`run-claude-locality-comparison.sh` is a legacy comparison helper and is not the
supported split launch-readiness path. Prefer the Codex launch wrapper below for
the Amika split-sandbox benchmark.

If you still need the Claude helper, set its MCP credentials explicitly before
running it:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>
./experiment/locality-mcp-comparison/run-claude-locality-comparison.sh
```

The script owns its own credential files under
`~/.config/locality-claude-comparison` and its own Claude configuration.

## Run Codex Comparison

From the local machine, set token-backed MCP credentials for the MCP sandbox,
then run:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>
./experiment/locality-mcp-comparison/run-codex-locality-comparison.sh
```

The Codex comparison defaults to `gpt-5.6-luna` with low reasoning effort. It
uses `MCP_SANDBOX=aseem-mcp` and `LOCALITY_SANDBOX=aseem-locality` by default.
Override those variables to point at different prepared Amika sandboxes. It
copies `AZURE_OPENAI_API_KEY` into sandbox-local secret storage when that
environment variable is set locally; otherwise it uses the sandbox's existing
Codex auth/config or `~/.config/locality-experiment/env`.

## Launch Runner MCP Auth

When `run-launch-readiness-benchmark.sh` runs the `notion-mcp` strategy, it
validates MCP credentials during setup and configures Codex MCP auth before
running the MCP scenarios:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>
export SLACK_BOT_TOKEN=<slack-bot-token>
export SLACK_TEAM_ID=<slack-team-id>
export SLACK_CHANNEL_IDS=<comma-delimited-channel-ids>
```

`NOTION_TOKEN` and `NOTION_ACCESS_TOKEN` are accepted aliases for
`NOTION_API_TOKEN`. Slack MCP is mandatory for MCP launch-readiness runs:
`SLACK_BOT_TOKEN` and `SLACK_TEAM_ID` are required. `SLACK_CHANNEL_IDS` is an
optional comma-delimited channel allowlist.

The runner uses separate per-run Codex homes under `OUT_DIR/codex` by default.
The Locality strategy uses a config with all `mcp_servers.*` tables stripped.
The MCP strategy stores token-backed helper scripts and secret files under
`OUT_DIR/mcp` by default, then updates only the MCP strategy Codex home with
entries for `linear-server`, `notion`, and `slack`.

## Add Scenarios

The core runner discovers scenarios from `prompts/Locality/*.md`. To add a new
benchmark scenario, add the same filename to both prompt directories:

```text
prompts/Locality/scenario2.md
prompts/MCP/scenario2.md
```

Every Locality scenario must have a matching MCP prompt with the same basename
when the `notion-mcp` strategy runs. Scenario prompts may use
`{{SANDBOX_HOME}}` for sandbox-home paths and `{{AGENT_REPORT_PATH}}` for the
final report path. The runner renders those placeholders for the active sandbox
before passing the prompt to Codex, then copies the report into the scenario
artifacts as `report-body.md` for Locality and `notion-mcp-report-body.md` for
MCP.

The runner also sets `OUT_DIR`, `REPORT_FILE`, `TRACE_FILE`,
`LOCALITY_CONTEXT_PATHS_FILE`, `LOCALITY_CONTEXT_INVENTORY`,
`CONTEXT_PATHS_FILE`, and `CONTEXT_INVENTORY` for the agent process. If a
scenario needs repository context, it should inspect the repository directly
with `git`.

## Run Once

```bash
CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low \
  ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```

Run only the two multi-source scenarios against prehydrated Locality state:

```bash
export LOCALITY_SANDBOX=aseem-locality
export MCP_SANDBOX=aseem-mcp

CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low \
  ./experiment/locality-mcp-comparison/run-agent-comparison.sh \
  --scenario scenario7,scenario8
```

By default this is artifact-only. It writes local Markdown reports under
`target/launch-readiness-amika/<run-id>/artifacts/{locality,notion-mcp}` after
syncing the remote sandbox `OUT_DIR`s back to the local machine. It does not
create Notion pages, write mounted report pages, run `loc diff`, or push.

Each Codex scenario has a hard timeout so a stalled `codex exec` records a
failed phase instead of hanging the benchmark indefinitely. The default is 900
seconds per scenario. Override it with:

```bash
CODEX_EXEC_TIMEOUT_SECONDS=300 ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```

Use `CODEX_EXEC_TIMEOUT_SECONDS=0` to disable the timeout.

The launch wrapper always runs split Amika strategies. The default sandboxes are
`aseem-locality` for Locality and `aseem-mcp` for MCP:

```bash
LOCALITY_SANDBOX=my-locality MCP_SANDBOX=my-mcp \
  ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```

The wrapper prepares a clean detached worktree in each sandbox from
`BENCHMARK_REF` and runs both strategy pipelines concurrently:
`run-launch-readiness-benchmark.sh --strategy locality` in the Locality sandbox
and `run-launch-readiness-benchmark.sh --strategy notion-mcp` in the MCP
sandbox. Set `SYNC_ARTIFACTS=0` to leave outputs only on the remote sandboxes.

Hooks are enabled by default. The runner installs a benchmark-owned `hooks.json`
into each per-strategy `CODEX_HOME` and starts Codex with
`--dangerously-bypass-hook-trust`, because the hook source is generated by this
harness. The hook collector runs during the live Codex session and writes
measured `harness.phase` records for
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`.
Tool phases include the canonical hook tool name and Bash command, so `loc`
calls can be grouped by subcommand in the modern profiler.

Set `CODEX_HOOKS_MODE=no-hooks` only for an explicit non-comparison baseline
run.

The runner enables Locality span tracing for any `loc` commands the Codex agents
run. If a running daemon serves a command, the trace captures the CLI boundary
and daemon response.

## Run Five Times

```bash
RUNS=5 CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low \
  ./experiment/locality-mcp-comparison/run-repeated.sh
```

## Artifacts

Each split wrapper run writes local metadata to:

`target/launch-readiness-amika/<run-id>/`

The synced benchmark artifacts are under:

`target/launch-readiness-amika/<run-id>/artifacts/locality/`
`target/launch-readiness-amika/<run-id>/artifacts/notion-mcp/`

Important artifacts:

- `metrics.tsv` - phase wall-clock metrics with a `scenario` column.
- `summary.json` - machine-readable run summary.
- `scenarios.tsv` - scenario manifest with prompt paths and output directories.
- `locality-context-paths.txt` - Locality roots added to Locality Codex runs.
- `locality-context-inventory.txt` - bounded inventory of Markdown/text/JSON files under those roots.
- `mcp-auth-setup.out` and `mcp-auth-setup.err` - Codex MCP setup logs when the `notion-mcp` strategy runs.
- `scenarios/<scenario>/report-body.md` - Locality report for that scenario.
- `scenarios/<scenario>/notion-mcp-report-body.md` - MCP report for that scenario.
- `scenarios/<scenario>/locality-codex-events.jsonl` - timestamped Locality Codex JSON events.
- `scenarios/<scenario>/notion-mcp-codex-events.jsonl` - timestamped MCP Codex JSON events.
- `scenarios/<scenario>/locality-codex-events.raw.jsonl` - raw timestamped
  Locality Codex stdout events before hook merge.
- `scenarios/<scenario>/notion-mcp-codex-events.raw.jsonl` - raw timestamped
  MCP Codex stdout events before hook merge.
- `scenarios/<scenario>/locality-codex-hooks.jsonl` - live Locality Codex hook
  events and measured `harness.phase` records.
- `scenarios/<scenario>/notion-mcp-codex-hooks.jsonl` - live MCP Codex hook
  events and measured `harness.phase` records.
- `scenarios/<scenario>/locality-prompt.md` and `scenarios/<scenario>/notion-mcp-prompt.md` - exact prompts used for the scenario.
- `scenarios/<scenario>/locality-codex-command.txt` and `scenarios/<scenario>/notion-mcp-codex-command.txt` - exact `codex exec` commands and timeout wrappers.
- `scenarios/<scenario>/locality-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/notion-mcp-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/locality-speedscope.json` and `scenarios/<scenario>/notion-mcp-speedscope.json` - Speedscope-compatible flame graph files generated from the JSON events.
- `scenarios/<scenario>/locality.perfetto.json` and `scenarios/<scenario>/notion-mcp.perfetto.json` - Perfetto/Chrome trace timeline files with one row per activity, tool group, and command group. MCP tool slices include `tool_args`, `tool_args_json`, and `tool_args_keys` in the event args.
- `scenarios/<scenario>/locality.folded` and `scenarios/<scenario>/notion-mcp.folded` - FlameGraph-compatible folded stacks generated from the same timing spans.
- `scenarios/<scenario>/locality.snakeviz.prof` and `scenarios/<scenario>/notion-mcp.snakeviz.prof` - SnakeViz-compatible synthetic pstats profiles.
- `scenarios/<scenario>/locality.snakeviz.stats.md` and `scenarios/<scenario>/notion-mcp.snakeviz.stats.md` - text summary of the SnakeViz profile frames.
- `token-usage/by-trial-scenario/*.{svg,png}` - stacked token-usage charts
  with one Locality bar and one MCP bar for each trial/scenario pair.
- `token-usage/average.{svg,png}` - stacked token-usage chart averaged over
  paired scenarios and trials.
- `token-usage/cost/by-trial-scenario/*.{svg,png}` - stacked cost charts using
  the same token buckets and one Locality/MCP bar pair per trial/scenario.
- `token-usage/cost/average.{svg,png}` - stacked cost chart averaged over
  paired scenarios and trials.
- `token-usage/token-usage.tsv`, `token-usage/cost-usage.tsv`, and
  `token-usage/token-usage.json` - chart data, cost data, pricing, and manifest.
- `deep-dive.md` - local wrapper report that indexes each scenario/strategy
  with phase timings, event counts, token totals, tool buckets, chronological
  tool calls, and links to the report, transcript, spans, Speedscope, Perfetto,
  SnakeViz, and Locality trace artifacts.

Cost charts default to the `gpt-5.6-luna` Standard short-context rates used by
the benchmark harness. Override them for Azure/internal billing with
`CODEX_COST_INPUT_USD_PER_1M`, `CODEX_COST_CACHED_INPUT_USD_PER_1M`,
`CODEX_COST_CACHE_WRITE_INPUT_USD_PER_1M`, `CODEX_COST_OUTPUT_USD_PER_1M`, and
`CODEX_COST_REASONING_OUTPUT_USD_PER_1M`.

- `scenarios/<scenario>/locality-agent-locality-trace.jsonl` and `scenarios/<scenario>/notion-mcp-agent-locality-trace.jsonl` - Locality spans emitted by any `loc` commands the agents run.
- `scenarios/<scenario>/locality-transcript.md` and `scenarios/<scenario>/notion-mcp-transcript.md` - readable Codex event transcripts generated from the JSON events.
- `scenarios/<scenario>/locality-agent-trace.md` - agent-reported Locality trace.
- `scenarios/<scenario>/notion-mcp-agent-trace.md` - agent-reported MCP trace.

The runner generates Codex transcript, spans, Speedscope, Perfetto,
folded-stack, and SnakeViz artifacts automatically. Regenerate them manually for
a completed run with:

```bash
python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  target/launch-readiness-amika/<run-id>/artifacts/locality/scenarios/<scenario>/locality-codex-events.jsonl \
  target/launch-readiness-amika/<run-id>/artifacts/locality/scenarios/<scenario>/locality

python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  target/launch-readiness-amika/<run-id>/artifacts/notion-mcp/scenarios/<scenario>/notion-mcp-codex-events.jsonl \
  target/launch-readiness-amika/<run-id>/artifacts/notion-mcp/scenarios/<scenario>/notion-mcp
```

When live hook `harness.phase` records are present, the generated Speedscope
files use those measured spans. Otherwise they fall back to observed gaps
between consecutive Codex JSON events. Treat the model thinking/output spans as
hook-boundary timing; tool spans come from Codex `PreToolUse`/`PostToolUse`.

Generate Locality span artifacts for a raw trace manually with:

```bash
python3 experiment/locality-mcp-comparison/scripts/locality-trace-to-speedscope.py \
  target/launch-readiness-amika/<run-id>/artifacts/locality/scenarios/<scenario>/locality-agent-locality-trace.jsonl \
  target/launch-readiness-amika/<run-id>/artifacts/locality/scenarios/<scenario>/locality-agent-locality-trace
```

Use the Locality trace files to answer questions the Codex event graph cannot:
which `loc` commands the agents ran and which connector or daemon spans
dominated the time.

## Model Notes

The prior baseline used `gpt-5.5` with `xhigh` reasoning. This package defaults to `gpt-5.6-luna` with low reasoning for faster repeated benchmark runs.

In the current Azure resource, the working deployment names are the short names:

- `gpt-5.6-luna`
- `gpt-5.6-terra`

The dated names, such as `gpt-5.6-luna-2026-07-09`, returned deployment-not-found errors during setup.

Change the model with:

```bash
CODEX_MODEL=<deployment-name> CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```
