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
- `scripts/timestamp-jsonl.py` - timestamps Codex JSON events.
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
MCP prompt with the same basename. Each prompt should keep writing its report
and trace to the standard paths under `OUT_DIR`, such as `OUT_DIR/report-body.md`
for Locality and `OUT_DIR/notion-mcp-report-body.md` for MCP. The runner sets
`OUT_DIR` separately for each scenario.

Only `scenario1.md` receives precomputed git metadata at
`OUT_DIR/git-data.json`. Other scenarios should not require that file; if they
need repository context, they should inspect the repository directly with git.

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
- `scenarios/<scenario>/locality-prompt.md` and `scenarios/<scenario>/notion-mcp-prompt.md` - exact prompts used for the scenario.
- `scenarios/scenario1/git-data.json` - precomputed git metadata for the scenario1 prompts.
- `scenarios/<scenario>/locality-codex-command.txt` and `scenarios/<scenario>/notion-mcp-codex-command.txt` - exact `codex exec` commands and timeout wrappers.
- `scenarios/<scenario>/locality-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/notion-mcp-codex-summary.json` - event counts, usage, errors.
- `scenarios/<scenario>/locality-speedscope.json` and `scenarios/<scenario>/notion-mcp-speedscope.json` - Speedscope-compatible flame graph files generated from the JSON events.
- `locality-traces/*.jsonl` - raw Locality command and pull/hydration spans.
- `locality-traces/*-summary.json` - top Locality spans by duration.
- `locality-traces/*-spans.tsv` - tabular Locality span data.
- `locality-traces/*-speedscope.json` - Speedscope-compatible Locality spans.
- `scenarios/<scenario>/locality-agent-locality-trace.jsonl` and `scenarios/<scenario>/notion-mcp-agent-locality-trace.jsonl` - Locality spans emitted by any `loc` commands the agents run.
- `scenarios/<scenario>/locality-transcript.md` and `scenarios/<scenario>/notion-mcp-transcript.md` - readable Codex event transcripts generated from the JSON events.
- `scenarios/<scenario>/locality-agent-trace.md` - agent-reported Locality trace.
- `scenarios/<scenario>/notion-mcp-agent-trace.md` - agent-reported MCP trace.
- `scenarios/<scenario>/loc-diff.out` - Locality push plan when mounted report page writing is enabled.

Generate flame graph artifacts for a completed run with:

```bash
python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/scenarios/<scenario>/locality-codex-events.jsonl \
  experiment/runs/<run-id>/scenarios/<scenario>/locality

python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/scenarios/<scenario>/notion-mcp-codex-events.jsonl \
  experiment/runs/<run-id>/scenarios/<scenario>/notion-mcp
```

The generated Speedscope files use observed gaps between consecutive Codex JSON events. This makes the chart useful even when Codex flushes `item.started` and `item.completed` at the same timestamp. Treat these charts as agent-session timing, not exact internal shell, MCP, or model runtime profiling.

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
