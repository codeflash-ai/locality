# Locality vs Notion MCP Experiment

This experiment compares two agent paths for a launch-readiness workflow:

- **Locality path:** hydrate Notion through Locality, let the agent read mounted Markdown files, write a mounted `page.md`, and run `loc diff`.
- **Notion MCP path:** let the agent use Notion MCP search/fetch for context, without reading mounted Locality files or using `loc`.

The benchmark case lives in Notion at:

`https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba`

The output parent page is:

`https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef`

## Files

- `run-agent-comparison.sh` - wrapper used inside Amika.
- `run-claude-locality-comparison.sh` - local wrapper that compares Claude Code
  on the hosted MCP path against Claude Code on the Locality path.
- `run-launch-readiness-benchmark.sh` - core benchmark runner.
- `run-repeated.sh` - runs the benchmark multiple times.
- `setup-codex-azure.sh` - writes Codex Azure config.
- `prompts/locality-agent-prompt.md` - Locality-only agent prompt.
- `prompts/notion-mcp-agent-prompt.md` - Notion-MCP-only agent prompt.
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

Verify Notion MCP and Locality:

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  codex mcp list
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

## Run Once

```bash
ssh -o StrictHostKeyChecking=accept-new "$SSH_TARGET" '
  export PATH="$HOME/.cargo/bin:$PATH"
  cd /home/amika/workspace/locality
  CODEX_MODEL=gpt-5.6-luna CODEX_REASONING_EFFORT=low ./experiment/locality-mcp-comparison/run-agent-comparison.sh
'
```

By default this is a dry run. It writes a mounted page and runs `loc diff`, but does not push.

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

To publish:

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

Each run writes to:

`experiment/runs/<run-id>/`

Important artifacts:

- `metrics.tsv` - phase wall-clock metrics.
- `summary.json` - machine-readable run summary.
- `report-body.md` - Locality report.
- `notion-mcp-report-body.md` - MCP report.
- `locality-codex-events.jsonl` - timestamped Codex JSON events.
- `notion-mcp-codex-events.jsonl` - timestamped Codex JSON events.
- `locality-prompt.md` and `notion-mcp-prompt.md` - exact prompts used for the run.
- `locality-codex-command.txt` and `notion-mcp-codex-command.txt` - exact `codex exec` command and timeout wrapper.
- `locality-codex-summary.json` - event counts, usage, errors.
- `notion-mcp-codex-summary.json` - event counts, usage, errors.
- `locality-speedscope.json` and `notion-mcp-speedscope.json` - Speedscope-compatible flame graph files generated from the JSON events.
- `locality-traces/*.jsonl` - raw Locality command and pull/hydration spans.
- `locality-traces/*-summary.json` - top Locality spans by duration.
- `locality-traces/*-spans.tsv` - tabular Locality span data.
- `locality-traces/*-speedscope.json` - Speedscope-compatible Locality spans.
- `locality-agent-locality-trace.jsonl` and `notion-mcp-agent-locality-trace.jsonl` - Locality spans emitted by any `loc` commands the agents run.
- `locality-transcript.md` and `notion-mcp-transcript.md` - readable Codex event transcripts generated from the JSON events.
- `locality-agent-trace.md` - agent-reported trace.
- `notion-mcp-agent-trace.md` - agent-reported trace.
- `loc-diff.out` - Locality push plan.

Generate flame graph artifacts for a completed run with:

```bash
python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/locality-codex-events.jsonl \
  experiment/runs/<run-id>/locality

python3 experiment/locality-mcp-comparison/scripts/codex-events-to-trace.py \
  experiment/runs/<run-id>/notion-mcp-codex-events.jsonl \
  experiment/runs/<run-id>/notion-mcp
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
