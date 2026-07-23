# Locality vs Notion MCP Experiment

This experiment compares two agent paths for a launch-readiness workflow:

- **Locality path:** hydrate Notion through Locality, let the agent read mounted Markdown files, and write local Markdown artifacts under `OUT_DIR`.
- **Notion MCP path:** let the agent use Notion MCP search/fetch for context, without reading mounted Locality files or using `loc`.

The benchmark case lives in Notion at:

`https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba`

The output parent page is:

`https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef`

## Files

- `run-agent-comparison.sh` - wrapper used inside Amika.
- `run-claude-locality-comparison.sh` - local wrapper that compares Claude Code
  on the hosted MCP path against Claude Code on the Locality path.
- `run-codex-locality-comparison.sh` - local wrapper that compares Codex on the
  hosted MCP path against Codex on the Locality path.
- `run-launch-readiness-benchmark.sh` - core benchmark runner.
- `run-repeated.sh` - runs the benchmark multiple times.
- `setup-codex-azure.sh` - writes Codex Azure config without MCP servers.
- `prompts/Locality/*.md` - Locality-only scenario prompts.
- `prompts/MCP/*.md` - Notion-MCP-only scenario prompts, paired by filename with `prompts/Locality/*.md`.
- `prompts/locality-agent-prompt.md` - legacy Locality-only agent prompt used only when `prompts/Locality/` has no scenarios.
- `prompts/notion-mcp-agent-prompt.md` - legacy Notion-MCP-only agent prompt used only when `prompts/Locality/` has no scenarios.
- `scripts/timestamp-jsonl.py` - timestamps Codex JSON events from stdout.
- `scripts/codex-live-hook.py` - live Codex hook collector used by the benchmark
  to measure prompt handoff, tool calls, model thinking spans, and final output
  response spans while the session is running.
- `scripts/summarize-codex-events.py` - summarizes one Codex JSON trace.
- `scripts/summarize-runs.py` - summarizes multiple run folders.

## Separation Rules

The Locality agent receives the hydrated Locality context directories as added directories and is instructed not to use Notion MCP or direct Notion API.

The Notion MCP agent does not receive those mounted Locality directories and is instructed not to use `loc` or mounted Locality files.

This is workflow separation, not a hard security boundary, because the benchmark uses `--dangerously-bypass-approvals-and-sandbox` inside an externally sandboxed Amika environment.

## Setup In Amika

From the local machine:

```bash
export SANDBOX=locality-scrum-report
export SSH_TARGET="$(amika sandbox ssh --print "$SANDBOX")"
```

Seed the Azure key without printing it:

```bash
line="$(python3 - <<'PY'
import os, shlex
print("export AZURE_OPENAI_API_KEY=" + shlex.quote(os.environ["AZURE_OPENAI_API_KEY"]))
PY
)"

b64="$(printf '%s\n' "$line" | base64 | tr -d '\n')"

ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" "
  mkdir -p ~/.config/locality-experiment &&
  chmod 700 ~/.config/locality-experiment &&
  printf '%s' '$b64' | base64 -d > ~/.config/locality-experiment/env &&
  chmod 600 ~/.config/locality-experiment/env
"
```

Build sidecars and configure Codex:

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  cargo build -p loc-cli -p localityd
  CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/setup-codex-azure.sh
'
```

Verify Locality:

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  target/debug/loc connections --json
  target/debug/loc locate "https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba"
'
```

## Run Claude Comparison

From the local machine, set token-backed MCP credentials for the
`test-with-notion-connector` sandbox, then run:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>
./experiment/locality-mcp-comparison/run-claude-locality-comparison.sh
```

The `test-with-notion-connector` sandbox is prepared with token-backed MCP
servers before Claude starts:

- `LINEAR_API_KEY` configures Linear's remote MCP server through an
  authorization-header helper.
- `NOTION_API_TOKEN` configures the official `@notionhq/notion-mcp-server`
  stdio server through `OPENAPI_MCP_HEADERS`.

The script writes these credentials only into sandbox-local files under
`~/.config/locality-claude-comparison` and stores helper references in
`~/.claude.json`.

## Run Codex Comparison

From the local machine, set token-backed MCP credentials for the
`test-with-notion-connector` sandbox, then run:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>
./experiment/locality-mcp-comparison/run-codex-locality-comparison.sh
```

The Codex comparison defaults to `gpt-5.6-luna` with low reasoning effort. It
copies `AZURE_OPENAI_API_KEY` into sandbox-local secret storage when that
environment variable is set locally; otherwise it uses the sandbox's existing
Codex auth/config or `~/.config/locality-experiment/env`.

## Launch Runner MCP Auth

When `run-launch-readiness-benchmark.sh` is run with `--compare-mcp`, it
validates MCP credentials during setup and configures Codex MCP auth only before
running the MCP scenarios:

```bash
export LINEAR_API_KEY=<linear-api-key>
export NOTION_API_TOKEN=<notion-api-token>

# Optional Slack MCP support:
export SLACK_BOT_TOKEN=<slack-bot-token>
export SLACK_TEAM_ID=<slack-team-id>
export SLACK_CHANNEL_IDS=<comma-delimited-channel-ids>
```

`NOTION_TOKEN` and `NOTION_ACCESS_TOKEN` are accepted aliases for
`NOTION_API_TOKEN`. Slack is configured only when `SLACK_BOT_TOKEN` is set; if
any Slack MCP variable is set, both `SLACK_BOT_TOKEN` and `SLACK_TEAM_ID` are
required.

The runner uses separate per-run Codex homes under `OUT_DIR/codex` by default.
The Locality strategy uses a config with all `mcp_servers.*` tables stripped.
The MCP strategy stores token-backed helper scripts and secret files under
`OUT_DIR/mcp` by default, then updates only the MCP strategy Codex home with
entries for `linear-server`, `notion`, and optionally `slack`.

## Add Scenarios

The core runner discovers scenarios from `prompts/Locality/*.md`. To add a new
benchmark scenario, add the same filename to both prompt directories:

```text
prompts/Locality/scenario2.md
prompts/MCP/scenario2.md
```

When `--compare-mcp` is enabled, every Locality scenario must have a matching
MCP prompt with the same basename. The runner sets `OUT_DIR`, `REPORT_FILE`,
`TRACE_FILE`, `AGENT_OUT_DIR`, `ARTIFACT_OUT_DIR`, `GIT_DATA_FILE`,
`CONTEXT_PATHS_FILE`, `CONTEXT_INVENTORY`, and `CONTEXT_SEARCH_RESULTS` for the
agent process. Prompts should prefer `REPORT_FILE` and `TRACE_FILE`; the runner
also retrieves compatibility outputs written under
`CODEX_SANDBOX_HARDCODED_OUT_DIR` for prompts that use absolute sandbox paths
such as `/home/amika/report-body.md`.

Only `scenario1.md` receives precomputed git metadata at
`GIT_DATA_FILE`. Other scenarios should not require that file; if they need
repository context, they should inspect the repository directly with git.

## Run Once

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-agent-comparison.sh
'
```

By default this is artifact-only. It writes local Markdown reports under
`OUT_DIR` and does not create Notion pages, write mounted report pages, run
`loc diff`, or push.

To exercise mounted report page creation and push-plan generation without
publishing:

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-agent-comparison.sh --write-mounted-page
'
```

Each Codex strategy has a hard timeout so a stalled `codex exec` records a failed phase instead of hanging the benchmark indefinitely. The default is 900 seconds per strategy. Override it with:

```bash
CODEX_EXEC_TIMEOUT_SECONDS=300 ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```

Use `CODEX_EXEC_TIMEOUT_SECONDS=0` to disable the timeout.

Hooks are enabled by default. The runner installs a benchmark-owned `hooks.json`
into each per-strategy `CODEX_HOME` and starts Codex with
`--dangerously-bypass-hook-trust`, because the hook source is generated by this
harness. The hook collector runs during the live Codex session and writes
measured `harness.phase` records for
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`.
Tool phases include the canonical hook tool name and Bash command, so `loc`
calls can be grouped by subcommand in the modern profiler.

Set `CODEX_HOOKS_MODE=no-hooks` only for an explicit non-comparison baseline
run. `--compare-hooks` still controls hooks per variant.

To study hook quality on one scenario, run the launch runner directly with
`--compare-hooks` and a selected scenario. This implies `--compare-mcp` and runs
four artifact-only Codex sessions: Locality without hooks, Locality with hooks,
Notion MCP without hooks, and Notion MCP with hooks.

```bash
LINEAR_API_KEY=<linear-api-key> NOTION_API_TOKEN=<notion-api-token> \
  ./experiment/locality-mcp-comparison/run-launch-readiness-benchmark.sh \
  --scenario scenario2 --compare-hooks
```

The hookless variants pass `--disable hooks`; the hooked variants pass
`--enable hooks --dangerously-bypass-hook-trust`. The comparison report is
written to `scenarios/<scenario>/hooks-comparison.md`, with profiler artifacts
under `scenarios/<scenario>/hook-comparison/`.

The runner enables Locality span tracing for setup commands and for any `loc`
commands the Codex agents run. By default, if a running daemon serves a command,
the trace captures the CLI boundary and daemon response. For the deepest
hydration breakdown in a benchmark sandbox, force direct CLI execution:

```bash
LOCALITY_EXPERIMENT_TRACE_FORCE_DIRECT=1 ./experiment/locality-mcp-comparison/run-agent-comparison.sh
```

Use this only when the mounted target does not require daemon-only virtual
projection behavior.

To publish, which implies mounted report page creation:

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-agent-comparison.sh --push
'
```

## Run Five Times

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  RUNS=5 CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-repeated.sh
'
```

## Artifacts

Each run writes shared setup artifacts to:

`experiment/runs/<run-id>/`

Important artifacts:

- `metrics.tsv` - phase wall-clock metrics with a `scenario` column.
- `summary.json` - machine-readable run summary.
- `scenarios.tsv` - scenario manifest with prompt paths, output directories, and mounted report pages when `--write-mounted-page` or `--push` is used.
- `codex-strategy-setup.out` and `codex-strategy-setup.err` - per-strategy Codex config setup logs.
- `mcp-auth-setup.out` and `mcp-auth-setup.err` - Codex MCP setup logs when `--compare-mcp` is enabled.
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
- `scenarios/scenario1/git-data.json` - precomputed git metadata for the scenario1 prompts.
- `scenarios/<scenario>/locality-codex-command.txt` and `scenarios/<scenario>/notion-mcp-codex-command.txt` - exact `codex exec` commands and timeout wrappers.
- `scenarios/<scenario>/locality-agent-artifacts.tsv` and `scenarios/<scenario>/notion-mcp-agent-artifacts.tsv` - copy-back manifest showing where report and trace files were retrieved from.
- `scenarios/<scenario>/locality-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/notion-mcp-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/locality-speedscope.json` and `scenarios/<scenario>/notion-mcp-speedscope.json` - Speedscope-compatible flame graph files generated from the JSON events.
- `scenarios/<scenario>/locality.perfetto.json` and `scenarios/<scenario>/notion-mcp.perfetto.json` - Perfetto/Chrome trace timeline files with one row per activity, tool group, and command group.
- `scenarios/<scenario>/locality.folded` and `scenarios/<scenario>/notion-mcp.folded` - FlameGraph-compatible folded stacks generated from the same timing spans.
- `scenarios/<scenario>/locality.snakeviz.prof` and `scenarios/<scenario>/notion-mcp.snakeviz.prof` - SnakeViz-compatible synthetic pstats profiles.
- `scenarios/<scenario>/locality.snakeviz.stats.md` and `scenarios/<scenario>/notion-mcp.snakeviz.stats.md` - text summary of the SnakeViz profile frames.
- `locality-traces/*.jsonl` - raw Locality command and pull/hydration spans.
- `locality-traces/*-summary.json` - top Locality spans by duration.
- `locality-traces/*-spans.tsv` - tabular Locality span data.
- `locality-traces/*-speedscope.json` - Speedscope-compatible Locality spans.
- `scenarios/<scenario>/locality-agent-locality-trace.jsonl` and `scenarios/<scenario>/notion-mcp-agent-locality-trace.jsonl` - Locality spans emitted by any `loc` commands the agents run.
- `scenarios/<scenario>/locality-transcript.md` and `scenarios/<scenario>/notion-mcp-transcript.md` - readable Codex event transcripts generated from the JSON events.
- `scenarios/<scenario>/locality-agent-trace.md` - agent-reported Locality trace.
- `scenarios/<scenario>/notion-mcp-agent-trace.md` - agent-reported MCP trace.
- `scenarios/<scenario>/loc-diff.out` - Locality push plan when mounted report page writing is enabled.
- `scenarios/<scenario>/variants/<strategy>-<hooks-mode>/` - per-variant
  reports, raw Codex events, merged Codex events, hook event files, summaries,
  command snapshots, and Locality traces for `--compare-hooks` runs.
- `scenarios/<scenario>/hooks-comparison.md` - hookless-vs-hooked timing report
  for both Locality and Notion MCP in `--compare-hooks` runs.
- `scenarios/<scenario>/hook-comparison/<strategy>/summary.md` - modern profiler
  report comparing hookless and hooked traces for one strategy.

The runner generates Codex transcript, spans, Speedscope, Perfetto,
folded-stack, and SnakeViz artifacts automatically. Regenerate them manually for
a completed run with:

```bash
python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/scenarios/<scenario>/locality-codex-events.jsonl \
  experiment/runs/<run-id>/scenarios/<scenario>/locality

python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/scenarios/<scenario>/notion-mcp-codex-events.jsonl \
  experiment/runs/<run-id>/scenarios/<scenario>/notion-mcp
```

When live hook `harness.phase` records are present, the generated Speedscope
files use those measured spans. Otherwise they fall back to observed gaps
between consecutive Codex JSON events. Treat the model thinking/output spans as
hook-boundary timing; tool spans come from Codex `PreToolUse`/`PostToolUse`.

Generate Locality span artifacts for a raw trace manually with:

```bash
python3 experiment/locality-mcp-comparison/scripts/locality-trace-to-speedscope.py \
  experiment/runs/<run-id>/locality-traces/target-pull.jsonl \
  experiment/runs/<run-id>/locality-traces/target-pull
```

Use the Locality trace files to answer questions the Codex event graph cannot:
whether `loc locate` refreshed Notion metadata, which pull branch ran, how many
pages were recursively hydrated, and which connector calls dominated the time.

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
