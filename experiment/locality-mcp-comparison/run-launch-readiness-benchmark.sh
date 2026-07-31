#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-launch-readiness-benchmark.sh [--strategy locality|notion-mcp|all] [--scenario NAME[,NAME...]] [--compare-mcp]

Runs the artifact-only Locality vs MCP benchmark from paired prompt files:
  prompts/Locality/*.md
  prompts/MCP/*.md

The simplified prompts write their report to /home/ubuntu/final_report.md.
This runner copies that file into the scenario artifact directory as:
  - report-body.md for Locality
  - notion-mcp-report-body.md for MCP

Important environment:
  REPO_DIR                 Repository path. Default: /home/ubuntu/workspace/locality
  LOC_BIN                  installed loc binary for Locality runs.
                           Default: loc found on PATH. No source-build fallback.
  PROMPT_ROOT              Prompt root. Default: <script-dir>/prompts
  LOCALITY_PROMPT_DIR      Default: $PROMPT_ROOT/Locality
  MCP_PROMPT_DIR           Default: $PROMPT_ROOT/MCP
  LOCALITY_CONTEXT_DIRS    Newline-delimited or colon-delimited mounted Locality roots.
                           If unset, existing /home/ubuntu/Locality/{notion,slack,linear}
                           and legacy roots are added when present.
  CODEX_MODEL              Default: gpt-5.6-sol
  CODEX_REASONING_EFFORT   Default: low
  CODEX_HOOKS_MODE         hooks or no-hooks. Default: hooks
  CODEX_EXEC_TIMEOUT_SECONDS
                           Per-scenario timeout. Default: 900. Use 0 to disable.
  CLEAN_CODEX_SESSION_STATE
                           Delete Codex rollout/session history after each
                           scenario to keep sandbox disk usage bounded.
                           Default: 1.
  AGENT_REPORT_PATH        Agent-written report path. Default: /home/ubuntu/final_report.md
  LINEAR_API_KEY           Required for MCP strategy.
  NOTION_API_TOKEN         Required for MCP strategy. NOTION_TOKEN and
                           NOTION_ACCESS_TOKEN are accepted aliases.
  SLACK_BOT_TOKEN          Required Slack MCP token for MCP strategy.
  SLACK_TEAM_ID            Required Slack team id for MCP strategy.
  SLACK_CHANNEL_IDS        Optional comma-delimited Slack channel allowlist.
  OUT_DIR                  Run artifact directory.

--compare-mcp is kept as a compatibility alias for --strategy all.
This runner is artifact-only: it never creates Notion pages, writes mounted
report pages, runs loc diff, or pushes.
EOF
}

RUN_STRATEGY="${RUN_STRATEGY:-all}"
SCENARIO_FILTER="${SCENARIO_FILTER:-}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --strategy)
      shift
      if [ "$#" -eq 0 ]; then
        echo "--strategy requires locality, notion-mcp, or all" >&2
        exit 2
      fi
      RUN_STRATEGY="$1"
      ;;
    --strategy=*) RUN_STRATEGY="${1#--strategy=}" ;;
    --scenario)
      shift
      if [ "$#" -eq 0 ]; then
        echo "--scenario requires a scenario name or filename" >&2
        exit 2
      fi
      SCENARIO_FILTER="$1"
      ;;
    --scenario=*) SCENARIO_FILTER="${1#--scenario=}" ;;
    --compare-mcp) RUN_STRATEGY="all" ;;
    --push|--write-mounted-page|--compare-hooks)
      echo "$1 is no longer supported by the simplified artifact-only runner" >&2
      exit 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$RUN_STRATEGY" in
  all|locality|notion-mcp) ;;
  *) echo "--strategy must be locality, notion-mcp, or all" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${REPO_DIR:-/home/ubuntu/workspace/locality}"
LOC_BIN="${LOC_BIN:-$(command -v loc 2>/dev/null || true)}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${OUT_DIR:-$REPO_DIR/experiment/runs/$RUN_ID}"
PROMPT_ROOT="${PROMPT_ROOT:-$SCRIPT_DIR/prompts}"
LOCALITY_PROMPT_DIR="${LOCALITY_PROMPT_DIR:-$PROMPT_ROOT/Locality}"
MCP_PROMPT_DIR="${MCP_PROMPT_DIR:-$PROMPT_ROOT/MCP}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_HOOKS_MODE="${CODEX_HOOKS_MODE:-hooks}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
CLEAN_CODEX_SESSION_STATE="${CLEAN_CODEX_SESSION_STATE:-1}"
AGENT_REPORT_PATH="${AGENT_REPORT_PATH:-/home/ubuntu/final_report.md}"
LOCALITY_CONTEXT_DIRS="${LOCALITY_CONTEXT_DIRS:-${LOCALITY_CONTEXT_ROOTS:-}}"
BASE_CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
CODEX_STRATEGY_ROOT="${CODEX_STRATEGY_ROOT:-$OUT_DIR/codex}"
LOCALITY_CODEX_HOME="$CODEX_STRATEGY_ROOT/locality"
MCP_CODEX_HOME="$CODEX_STRATEGY_ROOT/notion-mcp"
MCP_SECRET_DIR="${MCP_SECRET_DIR:-$OUT_DIR/mcp/secrets}"
MCP_BIN_DIR="${MCP_BIN_DIR:-$OUT_DIR/mcp/bin}"
LINEAR_API_KEY="${LINEAR_API_KEY:-}"
NOTION_API_TOKEN="${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}"
SLACK_BOT_TOKEN="${SLACK_BOT_TOKEN:-}"
SLACK_TEAM_ID="${SLACK_TEAM_ID:-}"
SLACK_CHANNEL_IDS="${SLACK_CHANNEL_IDS:-}"

case "$CODEX_HOOKS_MODE" in
  hooks|no-hooks) ;;
  *) echo "CODEX_HOOKS_MODE must be hooks or no-hooks" >&2; exit 2 ;;
esac

RUN_LOCALITY_AGENT=0
RUN_MCP_AGENT=0
case "$RUN_STRATEGY" in
  locality) RUN_LOCALITY_AGENT=1 ;;
  notion-mcp) RUN_MCP_AGENT=1 ;;
  all) RUN_LOCALITY_AGENT=1; RUN_MCP_AGENT=1 ;;
esac

METRICS_TSV="$OUT_DIR/metrics.tsv"
SUMMARY_JSON="$OUT_DIR/summary.json"
SCENARIO_ROOT="$OUT_DIR/scenarios"
SCENARIO_MANIFEST="$OUT_DIR/scenarios.tsv"
CONTEXT_PATHS_FILE="$OUT_DIR/locality-context-paths.txt"
CONTEXT_INVENTORY="$OUT_DIR/locality-context-inventory.txt"
TRACE_DIR="$OUT_DIR/locality-traces"
CURRENT_SCENARIO="setup"
MCP_AUTH_CONFIGURED=0
MCP_AUTH_DETAIL="linear=skipped; notion=skipped; slack=skipped"

mkdir -p "$OUT_DIR" "$SCENARIO_ROOT" "$TRACE_DIR"
export LOCALITY_TRACE_RUN_ID="$RUN_ID"

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

record_metric() {
  local strategy="$1"
  local phase="$2"
  local start_ms="$3"
  local end_ms="$4"
  local status="$5"
  local detail="${6:-}"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$CURRENT_SCENARIO" "$strategy" "$phase" "$start_ms" "$end_ms" "$((end_ms - start_ms))" "$status" "$detail" >> "$METRICS_TSV"
}

phase_start() {
  PHASE_STARTED_AT="$(now_ms)"
}

phase_end() {
  local ended_at
  ended_at="$(now_ms)"
  record_metric "$1" "$2" "$PHASE_STARTED_AT" "$ended_at" "${3:-ok}" "${4:-}"
}

strip_codex_mcp_tables() {
  local source="$1"
  local destination="$2"
  python3 - "$source" "$destination" <<'PY'
import re
import sys
from pathlib import Path

source = Path(sys.argv[1])
destination = Path(sys.argv[2])
text = source.read_text(encoding="utf-8") if source.exists() else ""
skip = False
lines = []
for line in text.splitlines(keepends=True):
    stripped = line.strip()
    header = stripped.split("#", 1)[0].strip()
    if header.startswith("[") and header.endswith("]"):
        table = header.strip("[]").strip()
        skip = table == "mcp_servers" or table.startswith("mcp_servers.")
    if skip:
        continue
    if re.match(r"^mcp_servers\s*=", stripped):
        continue
    lines.append(line)

destination.parent.mkdir(parents=True, exist_ok=True)
body = "".join(lines).rstrip()
destination.write_text((body + "\n") if body else "", encoding="utf-8")
PY
}

install_codex_harness_hooks() {
  local codex_home="$1"
  local hook_script="$SCRIPT_DIR/scripts/codex-live-hook.py"
  python3 - "$codex_home/hooks.json" "$hook_script" <<'PY'
import json
import shlex
import sys
from pathlib import Path

hooks_path = Path(sys.argv[1])
hook_script = sys.argv[2]
command = f"python3 {shlex.quote(hook_script)}"

def command_hook(status_message):
    hook = {"type": "command", "command": command, "timeout": 10}
    if status_message:
        hook["statusMessage"] = status_message
    return hook

hooks_path.write_text(
    json.dumps(
        {
            "description": "Locality benchmark live Codex timing hooks.",
            "hooks": {
                "SessionStart": [{"matcher": "startup|resume|clear|compact", "hooks": [command_hook(None)]}],
                "UserPromptSubmit": [{"hooks": [command_hook(None)]}],
                "PreToolUse": [{"matcher": "*", "hooks": [command_hook("Recording tool start")]}],
                "PostToolUse": [{"matcher": "*", "hooks": [command_hook("Recording tool finish")]}],
                "Stop": [{"hooks": [command_hook(None)]}],
            },
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
PY
  chmod 600 "$codex_home/hooks.json"
}

prepare_codex_home_without_mcp() {
  local codex_home="$1"
  mkdir -p "$codex_home"
  chmod 700 "$codex_home"
  if [ -d "$BASE_CODEX_HOME" ] && [ "$BASE_CODEX_HOME" != "$codex_home" ]; then
    find "$BASE_CODEX_HOME" -maxdepth 1 -type f \
      ! -name config.toml \
      ! -name '*.sqlite' \
      ! -name '*.sqlite-shm' \
      ! -name '*.sqlite-wal' \
      ! -name 'history.jsonl' \
      ! -name 'session_index.jsonl' \
      -size -1M \
      -exec cp -p {} "$codex_home/" \; 2>/dev/null || true
  fi
  strip_codex_mcp_tables "$BASE_CODEX_HOME/config.toml" "$codex_home/config.toml"
  install_codex_harness_hooks "$codex_home"
  chmod 600 "$codex_home/config.toml"
}

ensure_locality_agents_md() {
  local codex_home="$1"
  local source="$HOME/AGENTS.md"
  if [ ! -s "$source" ] && [ -s "$REPO_DIR/AGENTS.md" ]; then
    cp -p "$REPO_DIR/AGENTS.md" "$source"
  fi
  if [ ! -s "$source" ]; then
    echo "Locality run requires $source so it can be copied into ~/.codex/AGENTS.md" >&2
    return 2
  fi
  mkdir -p "$BASE_CODEX_HOME" "$codex_home"
  cp -p "$source" "$BASE_CODEX_HOME/AGENTS.md"
  if [ "$BASE_CODEX_HOME" != "$codex_home" ]; then
    cp -p "$source" "$codex_home/AGENTS.md"
  fi
}

codex_home_for_strategy() {
  case "$1" in
    locality) printf '%s\n' "$LOCALITY_CODEX_HOME" ;;
    notion-mcp) printf '%s\n' "$MCP_CODEX_HOME" ;;
    *) printf '%s\n' "$BASE_CODEX_HOME" ;;
  esac
}

validate_mcp_inputs() {
  if [ -z "$LINEAR_API_KEY" ]; then
    echo "LINEAR_API_KEY is required for MCP runs" >&2
    return 2
  fi
  if [ -z "$NOTION_API_TOKEN" ]; then
    echo "NOTION_API_TOKEN is required for MCP runs; NOTION_TOKEN and NOTION_ACCESS_TOKEN are accepted aliases" >&2
    return 2
  fi
  if [ -z "$SLACK_BOT_TOKEN" ] || [ -z "$SLACK_TEAM_ID" ]; then
    echo "SLACK_BOT_TOKEN and SLACK_TEAM_ID are required for MCP runs" >&2
    return 2
  fi
}

configure_codex_mcp_auth() {
  command -v codex >/dev/null || {
    echo "codex is not available on PATH" >&2
    return 127
  }
  validate_mcp_inputs

  mkdir -p "$MCP_SECRET_DIR" "$MCP_BIN_DIR" "$MCP_CODEX_HOME"
  chmod 700 "$MCP_SECRET_DIR" "$MCP_BIN_DIR" "$MCP_CODEX_HOME"

  local notion_helper="$MCP_BIN_DIR/locality-launch-notion-mcp"
  (
    set -e
    umask 077
    printf '%s' "$LINEAR_API_KEY" > "$MCP_SECRET_DIR/linear-api-key"
    printf '%s' "$NOTION_API_TOKEN" > "$MCP_SECRET_DIR/notion-token"
    cat > "$notion_helper" <<SH
#!/usr/bin/env bash
set -euo pipefail
token_file="\${NOTION_API_TOKEN_FILE:-$MCP_SECRET_DIR/notion-token}"
export OPENAPI_MCP_HEADERS="\$(
python3 - "\$token_file" <<'PY'
import json
import pathlib
import sys
token = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
print(json.dumps({"Authorization": "Bearer " + token}, separators=(",", ":")))
PY
)"
exec npx -y @notionhq/notion-mcp-server
SH
    chmod 700 "$notion_helper"
  )

  export LINEAR_API_KEY
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove linear-server >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove notion >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove slack >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove slack-server >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp add linear-server --url https://mcp.linear.app/mcp --bearer-token-env-var LINEAR_API_KEY
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp add notion -- "$notion_helper"

  local slack_helper="$MCP_BIN_DIR/locality-launch-slack-mcp"
  (
    set -e
    umask 077
    printf '%s' "$SLACK_BOT_TOKEN" > "$MCP_SECRET_DIR/slack-bot-token"
    printf '%s' "$SLACK_TEAM_ID" > "$MCP_SECRET_DIR/slack-team-id"
    if [ -n "$SLACK_CHANNEL_IDS" ]; then
      printf '%s' "$SLACK_CHANNEL_IDS" > "$MCP_SECRET_DIR/slack-channel-ids"
    else
      rm -f "$MCP_SECRET_DIR/slack-channel-ids"
    fi
    cat > "$slack_helper" <<SH
#!/usr/bin/env bash
set -euo pipefail
secret_dir="$MCP_SECRET_DIR"
export SLACK_BOT_TOKEN="\$(cat "\$secret_dir/slack-bot-token")"
export SLACK_TEAM_ID="\$(cat "\$secret_dir/slack-team-id")"
if [ -f "\$secret_dir/slack-channel-ids" ]; then
  export SLACK_CHANNEL_IDS="\$(cat "\$secret_dir/slack-channel-ids")"
fi
exec npx -y @modelcontextprotocol/server-slack
SH
    chmod 700 "$slack_helper"
  )
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp add slack -- "$slack_helper"

  MCP_AUTH_DETAIL="linear=configured; notion=configured; slack=configured; codex_home=$MCP_CODEX_HOME"
  MCP_AUTH_CONFIGURED=1
  echo "Configured Codex MCP auth: $MCP_AUTH_DETAIL"
}

merge_codex_event_streams() {
  local raw_events_file="$1"
  local hook_events_file="$2"
  local events_file="$3"
  python3 - "$raw_events_file" "$hook_events_file" "$events_file" <<'PY'
import json
import sys
from pathlib import Path

records = []
for source_order, path in enumerate([Path(sys.argv[1]), Path(sys.argv[2])]):
    if not path.exists():
        continue
    for line_order, line in enumerate(path.read_text(encoding="utf-8").splitlines()):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        records.append((int(record.get("observed_at_ms") or 0), source_order, line_order, record))
records.sort(key=lambda item: (item[0], item[1], item[2]))
Path(sys.argv[3]).write_text(
    "".join(json.dumps(record, separators=(",", ":")) + "\n" for *_, record in records),
    encoding="utf-8",
)
PY
}

render_codex_event_artifacts() {
  local events_file="$1"
  local out_prefix="$2"
  if [ -s "$events_file" ]; then
    python3 "$SCRIPT_DIR/scripts/codex-events-to-trace.py" "$events_file" "$out_prefix" >/dev/null
  fi
}

render_locality_traces() {
  local trace_file
  for trace_file in "$TRACE_DIR"/*.jsonl "$SCENARIO_ROOT"/*/*-agent-locality-trace.jsonl; do
    if [ -s "$trace_file" ]; then
      python3 "$SCRIPT_DIR/scripts/locality-trace-to-speedscope.py" \
        "$trace_file" "${trace_file%.jsonl}" >/dev/null
    fi
  done
}

clean_codex_session_state() {
  local codex_home="$1"
  [ "$CLEAN_CODEX_SESSION_STATE" = "1" ] || return 0
  [ -d "$codex_home" ] || return 0
  rm -rf "$codex_home/sessions"
  rm -f "$codex_home/history.jsonl" "$codex_home/session_index.jsonl" "$codex_home"/logs_*.sqlite*
}

discover_prompt_scenarios() {
  SCENARIO_FILES=()
  local prompt_file
  while IFS= read -r -d '' prompt_file; do
    SCENARIO_FILES+=("$(basename "$prompt_file")")
  done < <(find "$LOCALITY_PROMPT_DIR" -maxdepth 1 -type f -name '*.md' ! -name '._*' -print0 | sort -z)
  if [ "${#SCENARIO_FILES[@]}" -eq 0 ]; then
    echo "no Locality prompt scenarios found in $LOCALITY_PROMPT_DIR" >&2
    exit 2
  fi
}

filter_prompt_scenarios() {
  if [ -z "$SCENARIO_FILTER" ]; then
    return 0
  fi
  local requested requested_base requested_stem scenario_file scenario_stem
  local matches=()
  local seen="|"
  local requested_items=()
  IFS=',' read -r -a requested_items <<< "$SCENARIO_FILTER"
  for requested in "${requested_items[@]}"; do
    requested="${requested#"${requested%%[![:space:]]*}"}"
    requested="${requested%"${requested##*[![:space:]]}"}"
    [ -n "$requested" ] || continue
    requested_base="$(basename "$requested")"
    requested_stem="${requested_base%.md}"
    local match=""
    for scenario_file in "${SCENARIO_FILES[@]}"; do
      scenario_stem="${scenario_file%.md}"
      if [ "$scenario_file" = "$requested_base" ] || [ "$scenario_stem" = "$requested_stem" ]; then
        if [ -n "$match" ]; then
          echo "--scenario item $requested matched multiple prompt scenarios" >&2
          exit 2
        fi
        match="$scenario_file"
      fi
    done
    if [ -z "$match" ]; then
      echo "no prompt scenario matched --scenario item $requested" >&2
      exit 2
    fi
    if [[ "$seen" != *"|$match|"* ]]; then
      matches+=("$match")
      seen="$seen$match|"
    fi
  done
  SCENARIO_FILES=("${matches[@]}")
}

validate_prompt_scenarios() {
  local missing=0
  local scenario_file
  for scenario_file in "${SCENARIO_FILES[@]}"; do
    if [ ! -s "$LOCALITY_PROMPT_DIR/$scenario_file" ]; then
      echo "missing or empty Locality prompt for scenario: $scenario_file" >&2
      missing=1
    fi
    if [ "$RUN_MCP_AGENT" -eq 1 ] && [ ! -s "$MCP_PROMPT_DIR/$scenario_file" ]; then
      echo "missing or empty MCP prompt for scenario: $scenario_file" >&2
      missing=1
    fi
  done
  if [ "$missing" -ne 0 ]; then
    exit 2
  fi
}

scenario_name_for_file() {
  printf '%s\n' "${1%.md}"
}

append_context_paths_from_var() {
  local raw="$1"
  if [ -z "$raw" ]; then
    return 0
  fi
  LOCALITY_CONTEXT_DIRS_RAW="$raw" python3 - "$CONTEXT_PATHS_FILE" <<'PY'
import os
import re
import sys
from pathlib import Path

raw = os.environ.get("LOCALITY_CONTEXT_DIRS_RAW", "")
parts = raw.splitlines() if "\n" in raw else re.split(r":", raw)
with Path(sys.argv[1]).open("a", encoding="utf-8") as handle:
    for part in parts:
        path = os.path.expanduser(part.strip())
        if path:
            handle.write(path + "\n")
PY
}

prepare_locality_context_files() {
  : > "$CONTEXT_PATHS_FILE"
  : > "$CONTEXT_INVENTORY"
  if [ -n "$LOCALITY_CONTEXT_DIRS" ]; then
    append_context_paths_from_var "$LOCALITY_CONTEXT_DIRS"
  else
    local dir
    for dir in \
      "$HOME/Locality/notion" \
      "$HOME/Locality/slack" \
      "$HOME/Locality/linear" \
      "$HOME/Locality/Notion" \
      "$HOME/Locality/Slack" \
      "$HOME/Locality/Linear" \
      "$HOME/notion" \
      "$HOME/slack" \
      "$HOME/linear"; do
      [ -d "$dir" ] && printf '%s\n' "$dir" >> "$CONTEXT_PATHS_FILE"
    done
  fi
  sort -u "$CONTEXT_PATHS_FILE" -o "$CONTEXT_PATHS_FILE"

  local context_path
  while IFS= read -r context_path; do
    if [ -z "$context_path" ] || [ ! -d "$context_path" ]; then
      continue
    fi
    {
      echo "## $context_path"
      find "$context_path" \( -name page.md -o -name '*.md' -o -name '*.txt' -o -name '*.json' \) -type f | sort | head -5000 || true
      echo
    } >> "$CONTEXT_INVENTORY"
  done < "$CONTEXT_PATHS_FILE"
}

copy_context_files_to_scenario() {
  local scenario_out_dir="$1"
  cp "$CONTEXT_PATHS_FILE" "$scenario_out_dir/locality-context-paths.txt"
  cp "$CONTEXT_INVENTORY" "$scenario_out_dir/locality-context-inventory.txt"
}

run_codex_agent() {
  local strategy="$1"
  local prompt_file="$2"
  local scenario_out_dir="$3"
  local report_name="$4"
  local final_name="$5"
  shift 5
  local add_dirs=("$@")
  local codex_home
  local prompt
  local events_file="$scenario_out_dir/$strategy-codex-events.jsonl"
  local raw_events_file="$scenario_out_dir/$strategy-codex-events.raw.jsonl"
  local hook_events_file="$scenario_out_dir/$strategy-codex-hooks.jsonl"
  local hook_state_file="$scenario_out_dir/$strategy-codex-hooks.state.json"
  local err_file="$scenario_out_dir/$strategy-codex.err"
  local out_file="$scenario_out_dir/$strategy-codex.out"
  local summary_file="$scenario_out_dir/$strategy-codex-summary.json"
  local events_tsv="$scenario_out_dir/$strategy-codex-events.tsv"
  local final_file="$scenario_out_dir/$final_name"
  local report_file="$scenario_out_dir/$report_name"
  local trace_file="$scenario_out_dir/$strategy-agent-trace.md"
  local agent_loc_trace="$scenario_out_dir/$strategy-agent-locality-trace.jsonl"
  local command_snapshot="$scenario_out_dir/$strategy-codex-command.txt"
  local prompt_snapshot="$scenario_out_dir/$strategy-prompt.md"

  codex_home="$(codex_home_for_strategy "$strategy")"
  prompt="$(cat "$prompt_file")"
  cp "$prompt_file" "$prompt_snapshot"
  rm -f "$AGENT_REPORT_PATH" "$report_file" "$trace_file" "$final_file"
  : > "$hook_events_file"
  rm -f "$hook_state_file"

  local cmd=(
    codex exec
    --json
    --model "$CODEX_MODEL"
    -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\""
    --dangerously-bypass-approvals-and-sandbox
    -C "$REPO_DIR"
    --add-dir "$scenario_out_dir"
    --output-last-message "$final_file"
  )
  if [ "$CODEX_HOOKS_MODE" = "hooks" ]; then
    cmd+=(--enable hooks --dangerously-bypass-hook-trust)
  else
    cmd+=(--disable hooks)
  fi
  local dir
  if [ "${#add_dirs[@]}" -gt 0 ]; then
    for dir in "${add_dirs[@]}"; do
      [ -d "$dir" ] && cmd+=(--add-dir "$dir")
    done
  fi
  cmd+=("$prompt")

  local run_cmd=()
  if [ "$CODEX_EXEC_TIMEOUT_SECONDS" = "0" ]; then
    run_cmd=("${cmd[@]}")
  elif command -v timeout >/dev/null 2>&1; then
    run_cmd=(timeout --kill-after=30s "${CODEX_EXEC_TIMEOUT_SECONDS}s" "${cmd[@]}")
  else
    run_cmd=(python3 "$SCRIPT_DIR/scripts/run-with-timeout.py" "$CODEX_EXEC_TIMEOUT_SECONDS" -- "${cmd[@]}")
  fi

  {
    printf 'timeout_seconds=%s\n' "$CODEX_EXEC_TIMEOUT_SECONDS"
    printf 'codex_home=%s\n' "$codex_home"
    printf 'hooks_mode=%s\n' "$CODEX_HOOKS_MODE"
    printf 'report_source=%s\n' "$AGENT_REPORT_PATH"
    printf 'report_file=%s\n' "$report_file"
    printf 'trace_file=%s\n' "$trace_file"
    printf 'context_paths_file=%s\n' "$scenario_out_dir/locality-context-paths.txt"
    printf 'context_inventory=%s\n' "$scenario_out_dir/locality-context-inventory.txt"
    printf 'codex_command='
    printf '%q ' "${cmd[@]}"
    printf '\nwrapped_command='
    printf '%q ' "${run_cmd[@]}"
    printf '\n'
  } > "$command_snapshot"

  set +e
  set -o pipefail
  if [ "$CODEX_HOOKS_MODE" = "hooks" ]; then
    CODEX_HOME="$codex_home" \
      OUT_DIR="$scenario_out_dir" \
      REPORT_FILE="$AGENT_REPORT_PATH" \
      TRACE_FILE="$trace_file" \
      LOCALITY_CONTEXT_PATHS_FILE="$scenario_out_dir/locality-context-paths.txt" \
      LOCALITY_CONTEXT_INVENTORY="$scenario_out_dir/locality-context-inventory.txt" \
      CONTEXT_PATHS_FILE="$scenario_out_dir/locality-context-paths.txt" \
      CONTEXT_INVENTORY="$scenario_out_dir/locality-context-inventory.txt" \
      CODEX_HARNESS_HOOK_EVENTS_FILE="$hook_events_file" \
      CODEX_HARNESS_HOOK_STATE_FILE="$hook_state_file" \
      LOCALITY_TRACE_FILE="$agent_loc_trace" \
      LOCALITY_TRACE_RUN_ID="$RUN_ID" \
      "${run_cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$raw_events_file"
  else
    CODEX_HOME="$codex_home" \
      OUT_DIR="$scenario_out_dir" \
      REPORT_FILE="$AGENT_REPORT_PATH" \
      TRACE_FILE="$trace_file" \
      LOCALITY_CONTEXT_PATHS_FILE="$scenario_out_dir/locality-context-paths.txt" \
      LOCALITY_CONTEXT_INVENTORY="$scenario_out_dir/locality-context-inventory.txt" \
      CONTEXT_PATHS_FILE="$scenario_out_dir/locality-context-paths.txt" \
      CONTEXT_INVENTORY="$scenario_out_dir/locality-context-inventory.txt" \
      LOCALITY_TRACE_FILE="$agent_loc_trace" \
      LOCALITY_TRACE_RUN_ID="$RUN_ID" \
      "${run_cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$raw_events_file"
  fi
  local pipe_status=("${PIPESTATUS[@]}")
  local rc="${pipe_status[0]}"
  set +o pipefail
  set -e
  : > "$out_file"

  merge_codex_event_streams "$raw_events_file" "$hook_events_file" "$events_file"
  python3 "$SCRIPT_DIR/scripts/summarize-codex-events.py" "$events_file" "$summary_file" "$events_tsv"
  render_codex_event_artifacts "$events_file" "$scenario_out_dir/$strategy"
  clean_codex_session_state "$codex_home"

  if [ -s "$AGENT_REPORT_PATH" ]; then
    cp "$AGENT_REPORT_PATH" "$report_file"
    if [ "$AGENT_REPORT_PATH" != "$report_file" ]; then
      rm -f "$AGENT_REPORT_PATH"
    fi
  fi
  if [ "$rc" -ne 0 ]; then
    cat "$err_file" >&2 || true
    return "$rc"
  fi
  if [ ! -s "$report_file" ]; then
    echo "agent did not write report to $AGENT_REPORT_PATH" >&2
    cat "$err_file" >&2 || true
    return 1
  fi
}

discover_prompt_scenarios
filter_prompt_scenarios
validate_prompt_scenarios

echo -e "scenario\tstrategy\tphase\tstart_ms\tend_ms\tduration_ms\tstatus\tdetail" > "$METRICS_TSV"
printf 'scenario\tstrategy\tvariant\thooks\tlocality_prompt\tmcp_prompt\tout_dir\tagent_out_dir\treport_title\treport_page_path\n' > "$SCENARIO_MANIFEST"

phase_start
test -d "$REPO_DIR"
git -C "$REPO_DIR" rev-parse --git-dir >/dev/null
if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
  if [ -z "$LOC_BIN" ]; then
    echo "installed loc is required on PATH for Locality runs" >&2
    exit 127
  fi
  if [ ! -x "$LOC_BIN" ]; then
    echo "installed loc binary is not executable for Locality runs: $LOC_BIN" >&2
    exit 127
  fi
fi
phase_end "setup" "validate_environment" "ok" "repo=$REPO_DIR; strategy=$RUN_STRATEGY; scenarios=${#SCENARIO_FILES[@]}; model=$CODEX_MODEL"

phase_start
prepare_codex_home_without_mcp "$LOCALITY_CODEX_HOME"
prepare_codex_home_without_mcp "$MCP_CODEX_HOME"
if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
  ensure_locality_agents_md "$LOCALITY_CODEX_HOME"
fi
if [ "$RUN_MCP_AGENT" -eq 1 ]; then
  if ! validate_mcp_inputs; then
    rc=$?
    phase_end "setup" "codex_strategy_config" "failed" "exit=$rc; mcp inputs invalid"
    exit "$rc"
  fi
fi
phase_end "setup" "codex_strategy_config" "ok" "locality_home=$LOCALITY_CODEX_HOME; mcp_home=$MCP_CODEX_HOME"

phase_start
prepare_locality_context_files
locality_context_count="$(grep -cve '^[[:space:]]*$' "$CONTEXT_PATHS_FILE" 2>/dev/null || true)"
phase_end "locality" "prepare_context_add_dirs" "ok" "dirs=$locality_context_count; list=$CONTEXT_PATHS_FILE"

locality_add_dirs=()
if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
  while IFS= read -r dir; do
    [ -n "$dir" ] && locality_add_dirs+=("$dir")
  done < "$CONTEXT_PATHS_FILE"
fi

if [ "$RUN_MCP_AGENT" -eq 1 ]; then
  phase_start
  if configure_codex_mcp_auth > "$OUT_DIR/mcp-auth-setup.out" 2> "$OUT_DIR/mcp-auth-setup.err"; then
    phase_end "notion_mcp" "mcp_auth_setup" "ok" "$MCP_AUTH_DETAIL; out=$OUT_DIR/mcp-auth-setup.out"
  else
    rc=$?
    phase_end "notion_mcp" "mcp_auth_setup" "failed" "exit=$rc; err=$OUT_DIR/mcp-auth-setup.err"
    cat "$OUT_DIR/mcp-auth-setup.err" >&2 || true
    exit "$rc"
  fi
fi

for scenario_file in "${SCENARIO_FILES[@]}"; do
  SCENARIO_NAME="$(scenario_name_for_file "$scenario_file")"
  CURRENT_SCENARIO="$SCENARIO_NAME"
  SCENARIO_OUT_DIR="$SCENARIO_ROOT/$SCENARIO_NAME"
  mkdir -p "$SCENARIO_OUT_DIR"
  copy_context_files_to_scenario "$SCENARIO_OUT_DIR"

  LOCALITY_PROMPT_FILE="$LOCALITY_PROMPT_DIR/$scenario_file"
  MCP_PROMPT_FILE="$MCP_PROMPT_DIR/$scenario_file"
  REPORT_TITLE="Launch Readiness Benchmark $RUN_ID - $SCENARIO_NAME"
  MANIFEST_STRATEGY="$RUN_STRATEGY"

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$SCENARIO_NAME" "$MANIFEST_STRATEGY" "default" "$CODEX_HOOKS_MODE" \
    "$LOCALITY_PROMPT_FILE" "$MCP_PROMPT_FILE" "$SCENARIO_OUT_DIR" "$SCENARIO_OUT_DIR" "$REPORT_TITLE" "" >> "$SCENARIO_MANIFEST"

  if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
    phase_start
    if [ "${#locality_add_dirs[@]}" -gt 0 ]; then
      if run_codex_agent "locality" "$LOCALITY_PROMPT_FILE" "$SCENARIO_OUT_DIR" "report-body.md" "locality-agent-final.md" "${locality_add_dirs[@]}"; then
        codex_rc=0
      else
        codex_rc=$?
      fi
    else
      if run_codex_agent "locality" "$LOCALITY_PROMPT_FILE" "$SCENARIO_OUT_DIR" "report-body.md" "locality-agent-final.md"; then
        codex_rc=0
      else
        codex_rc=$?
      fi
    fi
    if [ "$codex_rc" -eq 0 ]; then
      phase_end "locality" "codex_exec_wall_time" "ok" "hooks=$CODEX_HOOKS_MODE; report=$SCENARIO_OUT_DIR/report-body.md"
    else
      rc=$codex_rc
      phase_end "locality" "codex_exec_wall_time" "failed" "exit=$rc; report=$SCENARIO_OUT_DIR/report-body.md"
      exit "$rc"
    fi
  else
    phase_start
    phase_end "locality" "codex_exec_wall_time" "skipped" "run_strategy=$RUN_STRATEGY"
  fi

  if [ "$RUN_MCP_AGENT" -eq 1 ]; then
    phase_start
    if run_codex_agent "notion-mcp" "$MCP_PROMPT_FILE" "$SCENARIO_OUT_DIR" "notion-mcp-report-body.md" "notion-mcp-agent-final.md"; then
      phase_end "notion_mcp" "codex_exec_wall_time" "ok" "hooks=$CODEX_HOOKS_MODE; report=$SCENARIO_OUT_DIR/notion-mcp-report-body.md"
    else
      rc=$?
      phase_end "notion_mcp" "codex_exec_wall_time" "failed" "exit=$rc; report=$SCENARIO_OUT_DIR/notion-mcp-report-body.md"
      exit "$rc"
    fi
  fi
done

CURRENT_SCENARIO="setup"
render_locality_traces

python3 - "$METRICS_TSV" "$SUMMARY_JSON" "$SCENARIO_MANIFEST" "$OUT_DIR" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" <<'PY'
import csv
import json
import sys
from pathlib import Path

metrics_path, summary_path, manifest_path, out_dir, model, effort = sys.argv[1:7]
out = Path(out_dir)
with open(metrics_path, encoding="utf-8") as f:
    metrics = list(csv.DictReader(f, delimiter="\t"))
with open(manifest_path, encoding="utf-8") as f:
    scenarios = list(csv.DictReader(f, delimiter="\t"))

scenario_summaries = {}
for scenario in scenarios:
    scenario_out = Path(scenario["out_dir"])
    agent_summaries = {}
    profile_artifacts = {}
    for name in ("locality", "notion-mcp"):
        summary_file = scenario_out / f"{name}-codex-summary.json"
        if summary_file.exists():
            agent_summaries[name] = json.loads(summary_file.read_text(encoding="utf-8"))
        prefix = scenario_out / name
        files = {
            "transcript": prefix.with_name(prefix.name + "-transcript.md"),
            "spans": prefix.with_name(prefix.name + "-spans.tsv"),
            "flamegraph_folded": prefix.with_name(prefix.name + ".folded"),
            "snakeviz": prefix.with_name(prefix.name + ".snakeviz.prof"),
            "snakeviz_stats": prefix.with_name(prefix.name + ".snakeviz.stats.md"),
            "speedscope": prefix.with_name(prefix.name + "-speedscope.json"),
            "perfetto": prefix.with_name(prefix.name + ".perfetto.json"),
        }
        existing = {key: str(path) for key, path in files.items() if path.exists()}
        if existing:
            profile_artifacts[name] = existing
    scenario_summaries[scenario["scenario"]] = {
        "out_dir": str(scenario_out),
        "strategy": scenario.get("strategy", ""),
        "variant": scenario.get("variant", ""),
        "hooks": scenario.get("hooks", ""),
        "agent_out_dir": scenario.get("agent_out_dir", ""),
        "report_title": scenario.get("report_title", ""),
        "page_path": "",
        "locality_prompt": scenario.get("locality_prompt", ""),
        "mcp_prompt": scenario.get("mcp_prompt", ""),
        "agent_event_summaries": agent_summaries,
        "variant_agent_event_summaries": {},
        "profile_artifacts": profile_artifacts,
        "hook_comparison_report": None,
    }

locality_trace_summaries = {}
for pattern in ("locality-traces/*-summary.json", "scenarios/*/*-agent-locality-trace-summary.json"):
    for path in sorted(out.glob(pattern)):
        locality_trace_summaries[str(path.relative_to(out))] = json.loads(path.read_text(encoding="utf-8"))

summary = {
    "ok": True,
    "model": model,
    "reasoning_effort": effort,
    "scenario_count": len(scenarios),
    "page_path": "",
    "page_paths": {name: "" for name in scenario_summaries},
    "out_dir": out_dir,
    "pushed": False,
    "write_mounted_page": False,
    "metrics": [
        {
            "scenario": row.get("scenario", ""),
            "strategy": row.get("strategy", ""),
            "phase": row.get("phase", ""),
            "duration_ms": int(row.get("duration_ms") or 0),
            "status": row.get("status", ""),
            "detail": row.get("detail", ""),
        }
        for row in metrics
    ],
    "agent_event_summaries": next(iter(scenario_summaries.values()), {}).get("agent_event_summaries", {}),
    "scenarios": scenario_summaries,
    "locality_trace_summaries": locality_trace_summaries,
}
Path(summary_path).write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
print(json.dumps(summary, indent=2))
PY

python3 "$SCRIPT_DIR/scripts/token-usage-charts.py" "$OUT_DIR" "$OUT_DIR/token-usage" >/dev/null
echo "Token usage charts: $OUT_DIR/token-usage"
