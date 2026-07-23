#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-launch-readiness-benchmark.sh [--push] [--compare-mcp] [--write-mounted-page]

Runs the Locality vs Notion MCP launch-readiness benchmark:
  1. discover prompt scenarios from prompts/Locality/*.md
  2. hydrate target and context through Locality
  3. inventory/search hydrated Locality context
  4. collect git metadata for scenario1 only
  5. run each Locality-backed Codex scenario with timed JSON events
  6. write local report artifacts under OUT_DIR
  7. optionally write each mounted page.md and run loc diff
  8. optionally run each matching Notion-MCP-only Codex scenario
  9. write run summary artifacts

Important environment:
  REPO_DIR                 Repository path. Default: /home/amika/workspace/locality
  LOC_BIN                  loc binary. Default: $REPO_DIR/target/debug/loc
  PROMPT_ROOT              Prompt root. Default: <script-dir>/prompts
  LOCALITY_PROMPT_DIR      Locality prompt directory. Default: $PROMPT_ROOT/Locality
  MCP_PROMPT_DIR           MCP prompt directory. Default: $PROMPT_ROOT/MCP
  TARGET_URL               Notion page URL for benchmark output parent.
  CONTEXT_URLS             Newline-delimited Notion URLs to hydrate as directories.
  CODEX_MODEL              Model passed to codex exec. Default: gpt-5.6-luna
  CODEX_REASONING_EFFORT   Codex reasoning effort. Default: low
  CODEX_EXEC_TIMEOUT_SECONDS
                           Per-strategy codex exec timeout. Default: 900. Use 0 to disable.
  LINEAR_API_KEY           Required with --compare-mcp. Linear MCP bearer token.
  NOTION_API_TOKEN         Required with --compare-mcp. NOTION_TOKEN and
                           NOTION_ACCESS_TOKEN are accepted aliases.
  SLACK_BOT_TOKEN          Optional with --compare-mcp. Slack bot token for Slack MCP.
  SLACK_TEAM_ID            Required when SLACK_BOT_TOKEN is set.
  SLACK_CHANNEL_IDS        Optional comma-delimited Slack channel allowlist.
  WRITE_MOUNTED_PAGE       Set to 1 to create/write mounted report pages and
                           run loc diff. Default: 0.
  LOCALITY_EXPERIMENT_TRACE_FORCE_DIRECT
                           Set to 1 to force CLI direct pull during traced Locality setup.
  SINCE                    Git window. Default: 24 hours ago
  BASE_REF                 Git ref. Default: origin/main
  OUT_DIR                  Run artifact directory.

By default this is artifact-only and does not create report pages. Pass
--write-mounted-page to create/write mounted report pages and run loc diff.
Pass --push to publish the mounted report page; --push implies
--write-mounted-page.
EOF
}

PUSH=0
WRITE_MOUNTED_PAGE="${WRITE_MOUNTED_PAGE:-0}"
COMPARE_MCP=0
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1; WRITE_MOUNTED_PAGE=1 ;;
    --compare-mcp) COMPARE_MCP=1 ;;
    --write-mounted-page) WRITE_MOUNTED_PAGE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

case "$WRITE_MOUNTED_PAGE" in
  0|1) ;;
  *) echo "WRITE_MOUNTED_PAGE must be 0 or 1" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${REPO_DIR:-/home/amika/workspace/locality}"
LOC_BIN="${LOC_BIN:-$REPO_DIR/target/debug/loc}"
TARGET_URL="${TARGET_URL:-https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef}"
CONTEXT_URLS="${CONTEXT_URLS:-https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba}"
CONTEXT_PULL_PATHS="${CONTEXT_PULL_PATHS:-}"
CONTEXT_SEARCH_QUERY="${CONTEXT_SEARCH_QUERY:-benchmark|launch readiness|safe diff|push|review|Live Mode|File Provider|Windows Cloud Files|distribution|Homebrew|install|connector|standup|blocker|risk}"
SINCE="${SINCE:-24 hours ago}"
BASE_REF="${BASE_REF:-origin/main}"
REPORT_TZ="${REPORT_TZ:-Asia/Kolkata}"
REPORT_DATE="${REPORT_DATE:-$(TZ="$REPORT_TZ" date +%F)}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
REPORT_TITLE="${REPORT_TITLE:-Launch Readiness Benchmark $RUN_ID}"
OUT_DIR="${OUT_DIR:-$REPO_DIR/experiment/runs/$RUN_ID}"
RUN_ROOT_OUT_DIR="$OUT_DIR"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-luna}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
LINEAR_API_KEY="${LINEAR_API_KEY:-}"
NOTION_API_TOKEN="${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}"
SLACK_BOT_TOKEN="${SLACK_BOT_TOKEN:-}"
SLACK_TEAM_ID="${SLACK_TEAM_ID:-}"
SLACK_CHANNEL_IDS="${SLACK_CHANNEL_IDS:-}"
LOCALITY_EXPERIMENT_TRACE_FORCE_DIRECT="${LOCALITY_EXPERIMENT_TRACE_FORCE_DIRECT:-0}"
PROMPT_ROOT="${PROMPT_ROOT:-$SCRIPT_DIR/prompts}"
LOCALITY_PROMPT_DIR="${LOCALITY_PROMPT_DIR:-$PROMPT_ROOT/Locality}"
MCP_PROMPT_DIR="${MCP_PROMPT_DIR:-$PROMPT_ROOT/MCP}"
BASE_CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
CODEX_STRATEGY_ROOT="${CODEX_STRATEGY_ROOT:-$RUN_ROOT_OUT_DIR/codex}"
LOCALITY_CODEX_HOME="$CODEX_STRATEGY_ROOT/locality"
MCP_CODEX_HOME="$CODEX_STRATEGY_ROOT/notion-mcp"
MCP_SECRET_DIR="${MCP_SECRET_DIR:-$RUN_ROOT_OUT_DIR/mcp/secrets}"
MCP_BIN_DIR="${MCP_BIN_DIR:-$RUN_ROOT_OUT_DIR/mcp/bin}"
MCP_AUTH_SETUP_OUT="$RUN_ROOT_OUT_DIR/mcp-auth-setup.out"
MCP_AUTH_SETUP_ERR="$RUN_ROOT_OUT_DIR/mcp-auth-setup.err"
MCP_AUTH_CONFIGURED=0
METRICS_TSV="$OUT_DIR/metrics.tsv"
SUMMARY_JSON="$OUT_DIR/summary.json"
CONTEXT_PATHS_FILE="$OUT_DIR/locality-context-paths.txt"
CONTEXT_INVENTORY="$OUT_DIR/locality-context-inventory.txt"
CONTEXT_SEARCH_RESULTS="$OUT_DIR/locality-context-search.txt"
TRACE_DIR="$OUT_DIR/locality-traces"
SCENARIO_ROOT="$OUT_DIR/scenarios"
SCENARIO_MANIFEST="$OUT_DIR/scenarios.tsv"
CURRENT_SCENARIO="setup"

mkdir -p "$OUT_DIR" "$TRACE_DIR" "$SCENARIO_ROOT"
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
  local duration_ms=$((end_ms - start_ms))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$CURRENT_SCENARIO" "$strategy" "$phase" "$start_ms" "$end_ms" "$duration_ms" "$status" "$detail" >> "$METRICS_TSV"
}

phase_start() {
  PHASE_STARTED_AT="$(now_ms)"
}

phase_end() {
  local strategy="$1"
  local phase="$2"
  local status="${3:-ok}"
  local detail="${4:-}"
  local ended_at
  ended_at="$(now_ms)"
  record_metric "$strategy" "$phase" "$PHASE_STARTED_AT" "$ended_at" "$status" "$detail"
}

trace_safe_name() {
  printf '%s' "$1" | tr '/:[:space:]?&=%#' '__________' | tr -cd '[:alnum:]_.-'
}

run_loc_traced() {
  local trace_name="$1"
  shift
  local trace_file="$TRACE_DIR/$trace_name.jsonl"
  if [ "$LOCALITY_EXPERIMENT_TRACE_FORCE_DIRECT" = "1" ]; then
    LOCALITY_DAEMON_DISABLE=1 LOCALITY_TRACE_FILE="$trace_file" LOCALITY_TRACE_RUN_ID="$RUN_ID" "$@"
  else
    LOCALITY_TRACE_FILE="$trace_file" LOCALITY_TRACE_RUN_ID="$RUN_ID" "$@"
  fi
}

render_locality_traces() {
  local trace_file
  for trace_file in "$TRACE_DIR"/*.jsonl "$SCENARIO_ROOT"/*/*-agent-locality-trace.jsonl; do
    if [ ! -s "$trace_file" ]; then
      continue
    fi
    python3 "$SCRIPT_DIR/scripts/locality-trace-to-speedscope.py" \
      "$trace_file" "${trace_file%.jsonl}" >/dev/null
  done
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

prepare_codex_home_without_mcp() {
  local codex_home="$1"
  mkdir -p "$codex_home"
  chmod 700 "$codex_home"

  if [ -d "$BASE_CODEX_HOME" ] && [ "$BASE_CODEX_HOME" != "$codex_home" ]; then
    find "$BASE_CODEX_HOME" -maxdepth 1 -type f ! -name config.toml -exec cp -p {} "$codex_home/" \; 2>/dev/null || true
  fi

  strip_codex_mcp_tables "$BASE_CODEX_HOME/config.toml" "$codex_home/config.toml"
  chmod 600 "$codex_home/config.toml"
}

validate_codex_mcp_auth_inputs() {
  if [ -z "$LINEAR_API_KEY" ]; then
    echo "LINEAR_API_KEY is required when --compare-mcp is enabled" >&2
    return 2
  fi
  if [ -z "$NOTION_API_TOKEN" ]; then
    echo "NOTION_API_TOKEN is required when --compare-mcp is enabled; NOTION_TOKEN and NOTION_ACCESS_TOKEN are accepted aliases" >&2
    return 2
  fi
  if { [ -n "$SLACK_BOT_TOKEN" ] || [ -n "$SLACK_TEAM_ID" ] || [ -n "$SLACK_CHANNEL_IDS" ]; } &&
    { [ -z "$SLACK_BOT_TOKEN" ] || [ -z "$SLACK_TEAM_ID" ]; }; then
    echo "SLACK_BOT_TOKEN and SLACK_TEAM_ID are both required to configure Slack MCP" >&2
    return 2
  fi
}

prepare_codex_strategy_homes() {
  prepare_codex_home_without_mcp "$LOCALITY_CODEX_HOME"
  if [ "$COMPARE_MCP" -eq 1 ]; then
    validate_codex_mcp_auth_inputs || return
    prepare_codex_home_without_mcp "$MCP_CODEX_HOME"
  fi
}

codex_home_for_strategy() {
  case "$1" in
    locality) printf '%s\n' "$LOCALITY_CODEX_HOME" ;;
    notion-mcp) printf '%s\n' "$MCP_CODEX_HOME" ;;
    *) printf '%s\n' "$BASE_CODEX_HOME" ;;
  esac
}

configure_codex_mcp_auth() {
  MCP_AUTH_DETAIL="linear=skipped; notion=skipped; slack=skipped"

  command -v codex >/dev/null || return
  validate_codex_mcp_auth_inputs || return

  mkdir -p "$MCP_SECRET_DIR" "$MCP_BIN_DIR" "$MCP_CODEX_HOME" || return
  chmod 700 "$MCP_SECRET_DIR" "$MCP_BIN_DIR" "$MCP_CODEX_HOME" || return

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
  ) || return

  export LINEAR_API_KEY
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove linear-server >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove notion >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove slack >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp remove slack-server >/dev/null 2>&1 || true
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp add linear-server --url https://mcp.linear.app/mcp --bearer-token-env-var LINEAR_API_KEY || return
  CODEX_HOME="$MCP_CODEX_HOME" codex mcp add notion -- "$notion_helper" || return

  local slack_status="skipped"
  if [ -n "$SLACK_BOT_TOKEN" ]; then
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
    ) || return
    CODEX_HOME="$MCP_CODEX_HOME" codex mcp add slack -- "$slack_helper" || return
    slack_status="configured"
  else
    rm -f "$MCP_SECRET_DIR/slack-bot-token" "$MCP_SECRET_DIR/slack-team-id" "$MCP_SECRET_DIR/slack-channel-ids"
  fi

  MCP_AUTH_DETAIL="linear=configured; notion=configured; slack=$slack_status; codex_home=$MCP_CODEX_HOME"
  MCP_AUTH_CONFIGURED=1
  echo "Configured Codex MCP auth: $MCP_AUTH_DETAIL"
}

run_codex_agent() {
  local strategy="$1"
  local prompt_file="$2"
  local report_file="$3"
  local final_file="$4"
  shift 4
  local add_dirs=("$@")
  local events_file="$OUT_DIR/$strategy-codex-events.jsonl"
  local err_file="$OUT_DIR/$strategy-codex.err"
  local out_file="$OUT_DIR/$strategy-codex.out"
  local summary_file="$OUT_DIR/$strategy-codex-summary.json"
  local events_tsv="$OUT_DIR/$strategy-codex-events.tsv"
  local prompt_snapshot="$OUT_DIR/$strategy-prompt.md"
  local command_snapshot="$OUT_DIR/$strategy-codex-command.txt"
  local agent_loc_trace="$OUT_DIR/$strategy-agent-locality-trace.jsonl"
  local prompt
  local run_cmd
  local codex_home
  codex_home="$(codex_home_for_strategy "$strategy")"
  prompt="$(cat "$prompt_file")"
  cp "$prompt_file" "$prompt_snapshot"

  local cmd=(
    codex exec
    --json
    --model "$CODEX_MODEL"
    -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\""
    --dangerously-bypass-approvals-and-sandbox
    -C "$REPO_DIR"
    --add-dir "$OUT_DIR"
    --output-last-message "$final_file"
  )
  local dir
  for dir in "${add_dirs[@]}"; do
    cmd+=(--add-dir "$dir")
  done
  cmd+=("$prompt")
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
    printf 'codex_command='
    printf '%q ' "${cmd[@]}"
    printf '\nwrapped_command='
    printf '%q ' "${run_cmd[@]}"
    printf '\n'
  } > "$command_snapshot"

  set +e
  set -o pipefail
  CODEX_HOME="$codex_home" LOCALITY_TRACE_FILE="$agent_loc_trace" LOCALITY_TRACE_RUN_ID="$RUN_ID" \
    "${run_cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$events_file"
  local pipe_status=("${PIPESTATUS[@]}")
  local rc="${pipe_status[0]}"
  set +o pipefail
  set -e
  : > "$out_file"

  python3 "$SCRIPT_DIR/scripts/summarize-codex-events.py" "$events_file" "$summary_file" "$events_tsv"
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi
  test -s "$report_file"
}

discover_prompt_scenarios() {
  SCENARIO_FILES=()
  if [ -d "$LOCALITY_PROMPT_DIR" ]; then
    local prompt_file
    while IFS= read -r -d '' prompt_file; do
      SCENARIO_FILES+=("$(basename "$prompt_file")")
    done < <(find "$LOCALITY_PROMPT_DIR" -maxdepth 1 -type f -name '*.md' -print0 | sort -z)
  fi

  if [ "${#SCENARIO_FILES[@]}" -eq 0 ]; then
    if [ -f "$PROMPT_ROOT/locality-agent-prompt.md" ]; then
      SCENARIO_FILES=("default.md")
      USE_LEGACY_PROMPTS=1
    else
      echo "no Locality prompt scenarios found in $LOCALITY_PROMPT_DIR" >&2
      exit 2
    fi
  else
    USE_LEGACY_PROMPTS=0
  fi
}

locality_prompt_for() {
  local scenario_file="$1"
  if [ "$USE_LEGACY_PROMPTS" -eq 1 ]; then
    printf '%s\n' "$PROMPT_ROOT/locality-agent-prompt.md"
  else
    printf '%s\n' "$LOCALITY_PROMPT_DIR/$scenario_file"
  fi
}

mcp_prompt_for() {
  local scenario_file="$1"
  if [ "$USE_LEGACY_PROMPTS" -eq 1 ]; then
    printf '%s\n' "$PROMPT_ROOT/notion-mcp-agent-prompt.md"
  else
    printf '%s\n' "$MCP_PROMPT_DIR/$scenario_file"
  fi
}

validate_prompt_scenarios() {
  local scenario_file
  local missing=0
  for scenario_file in "${SCENARIO_FILES[@]}"; do
    if [ ! -s "$(locality_prompt_for "$scenario_file")" ]; then
      echo "missing or empty Locality prompt for scenario: $scenario_file" >&2
      missing=1
    fi
    if [ "$COMPARE_MCP" -eq 1 ] && [ ! -s "$(mcp_prompt_for "$scenario_file")" ]; then
      echo "missing or empty MCP prompt for scenario: $scenario_file" >&2
      missing=1
    fi
  done

  if [ "$COMPARE_MCP" -eq 1 ] && [ "$USE_LEGACY_PROMPTS" -eq 0 ] && [ -d "$MCP_PROMPT_DIR" ]; then
    local mcp_prompt
    local mcp_scenario_file
    for mcp_prompt in "$MCP_PROMPT_DIR"/*.md; do
      if [ ! -e "$mcp_prompt" ]; then
        continue
      fi
      mcp_scenario_file="$(basename "$mcp_prompt")"
      if [ ! -f "$LOCALITY_PROMPT_DIR/$mcp_scenario_file" ]; then
        echo "MCP prompt has no matching Locality prompt: $mcp_scenario_file" >&2
        missing=1
      fi
    done
  fi

  if [ "$missing" -ne 0 ]; then
    exit 2
  fi
}

scenario_name_for_file() {
  local scenario_file="$1"
  printf '%s\n' "${scenario_file%.md}"
}

scenario_report_title() {
  local scenario_name="$1"
  if [ "${#SCENARIO_FILES[@]}" -eq 1 ]; then
    printf '%s\n' "$REPORT_TITLE"
  else
    printf '%s - %s\n' "$REPORT_TITLE" "$scenario_name"
  fi
}

prepare_scenario_inputs() {
  local scenario_out_dir="$1"
  cp "$CONTEXT_PATHS_FILE" "$scenario_out_dir/locality-context-paths.txt"
  cp "$CONTEXT_INVENTORY" "$scenario_out_dir/locality-context-inventory.txt"
  cp "$CONTEXT_SEARCH_RESULTS" "$scenario_out_dir/locality-context-search.txt"
}

scenario_needs_git_metadata() {
  local scenario_file="$1"
  [ "$(scenario_name_for_file "$scenario_file")" = "scenario1" ]
}

collect_git_metadata() {
  local out_path="$1"
  local commit_count

  phase_start
  git -C "$REPO_DIR" fetch --quiet origin || true
  phase_end "locality" "git_fetch" "ok" "ref=$BASE_REF"

  phase_start
  python3 - "$REPO_DIR" "$BASE_REF" "$SINCE" "$out_path" <<'PY'
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

repo, ref, since, out_path = sys.argv[1:5]

def git(*args):
    return subprocess.check_output(["git", "-C", repo, *args], text=True)

fmt = "%H%x09%h%x09%an%x09%ae%x09%ad%x09%s"
raw = git("log", ref, f"--since={since}", "--date=iso-strict", f"--pretty=format:{fmt}")

commits = []
for line in raw.splitlines():
    if not line.strip():
        continue
    full, short, author, email, date, subject = line.split("\t", 5)
    files = [f for f in git("diff-tree", "--no-commit-id", "--name-only", "-r", full).splitlines() if f.strip()]
    commits.append({"full": full, "short": short, "author": author, "email": email, "date": date, "subject": subject, "files": files})

by_email = defaultdict(list)
for commit in commits:
    by_email[commit["email"]].append(commit)

people = []
for email, items in sorted(by_email.items(), key=lambda kv: (-len(kv[1]), kv[0])):
    files = sorted({f for item in items for f in item["files"]})
    top_dirs = defaultdict(int)
    for path in files:
        top_dirs[path.split("/", 1)[0]] += 1
    names = [item["author"] for item in items if item["author"]]
    display = sorted(names, key=lambda n: (-(" " in n), -len(n), n))[0] if names else "Unknown"
    people.append({"name": display, "email": email, "commit_count": len(items), "commits": items, "top_dirs": sorted(top_dirs.items(), key=lambda kv: (-kv[1], kv[0]))[:8], "file_count": len(files)})

Path(out_path).write_text(json.dumps({"repo": repo, "ref": ref, "since": since, "commit_count": len(commits), "people": people}, indent=2) + "\n")
PY
  commit_count="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["commit_count"])' "$out_path")"
  phase_end "locality" "git_collect" "ok" "commits=$commit_count; file=$out_path"
}

discover_prompt_scenarios
validate_prompt_scenarios

echo -e "scenario\tstrategy\tphase\tstart_ms\tend_ms\tduration_ms\tstatus\tdetail" > "$METRICS_TSV"
printf 'scenario\tlocality_prompt\tmcp_prompt\tout_dir\treport_title\treport_page_path\n' > "$SCENARIO_MANIFEST"

phase_start
test -d "$REPO_DIR"
test -x "$LOC_BIN"
git -C "$REPO_DIR" rev-parse --git-dir >/dev/null
phase_end "locality" "validate_environment" "ok" "repo=$REPO_DIR; model=$CODEX_MODEL; effort=$CODEX_REASONING_EFFORT; scenarios=${#SCENARIO_FILES[@]}; prompts=$LOCALITY_PROMPT_DIR; write_mounted_page=$WRITE_MOUNTED_PAGE"

phase_start
set +e
prepare_codex_strategy_homes > "$OUT_DIR/codex-strategy-setup.out" 2> "$OUT_DIR/codex-strategy-setup.err"
CODEX_STRATEGY_SETUP_RC=$?
set -e
if [ "$CODEX_STRATEGY_SETUP_RC" -eq 0 ]; then
  if [ "$COMPARE_MCP" -eq 1 ]; then
    phase_end "locality" "codex_strategy_config" "ok" "locality_home=$LOCALITY_CODEX_HOME; mcp_home=$MCP_CODEX_HOME; mcp_auth=validated"
  else
    phase_end "locality" "codex_strategy_config" "ok" "locality_home=$LOCALITY_CODEX_HOME; mcp_auth=not requested"
  fi
else
  phase_end "locality" "codex_strategy_config" "failed" "exit=$CODEX_STRATEGY_SETUP_RC; out=$OUT_DIR/codex-strategy-setup.out; err=$OUT_DIR/codex-strategy-setup.err"
  cat "$OUT_DIR/codex-strategy-setup.out" >&2 || true
  cat "$OUT_DIR/codex-strategy-setup.err" >&2 || true
  exit "$CODEX_STRATEGY_SETUP_RC"
fi

phase_start
PAGE_PATH="$(run_loc_traced "target-locate" "$LOC_BIN" locate "$TARGET_URL")"
phase_end "locality" "notion_target_locate" "ok" "path=$PAGE_PATH; trace=$TRACE_DIR/target-locate.jsonl"

phase_start
set +e
run_loc_traced "target-pull" "$LOC_BIN" pull "$PAGE_PATH" > "$OUT_DIR/loc-pull.out" 2> "$OUT_DIR/loc-pull.err"
PULL_RC=$?
set -e
PULL_DETAIL="$(tr '\n' ' ' < "$OUT_DIR/loc-pull.out" | sed 's/[[:space:]]*$//')"
if [ "$PULL_RC" -eq 0 ]; then
  phase_end "locality" "notion_target_prehydrate" "ok" "path=$PAGE_PATH; trace=$TRACE_DIR/target-pull.jsonl; $PULL_DETAIL"
elif grep -qi "dirty" "$OUT_DIR/loc-pull.out" "$OUT_DIR/loc-pull.err"; then
  phase_end "locality" "notion_target_prehydrate" "ok" "path=$PAGE_PATH; trace=$TRACE_DIR/target-pull.jsonl; pull skipped dirty target"
else
  phase_end "locality" "notion_target_prehydrate" "failed" "exit=$PULL_RC; path=$PAGE_PATH; trace=$TRACE_DIR/target-pull.jsonl"
  cat "$OUT_DIR/loc-pull.out" >&2
  cat "$OUT_DIR/loc-pull.err" >&2
  exit "$PULL_RC"
fi

phase_start
: > "$CONTEXT_PATHS_FILE"
context_url_index=0
while IFS= read -r context_url; do
  context_url="${context_url#"${context_url%%[![:space:]]*}"}"
  context_url="${context_url%"${context_url##*[![:space:]]}"}"
  if [ -z "$context_url" ]; then
    continue
  fi
  context_url_index=$((context_url_index + 1))
  context_page="$(run_loc_traced "context-locate-$context_url_index" "$LOC_BIN" locate "$context_url")"
  printf '%s\n' "$(dirname "$context_page")" >> "$CONTEXT_PATHS_FILE"
done <<< "$CONTEXT_URLS"
while IFS= read -r context_path; do
  context_path="${context_path#"${context_path%%[![:space:]]*}"}"
  context_path="${context_path%"${context_path##*[![:space:]]}"}"
  if [ -n "$context_path" ]; then
    printf '%s\n' "$context_path" >> "$CONTEXT_PATHS_FILE"
  fi
done <<< "$CONTEXT_PULL_PATHS"
sort -u "$CONTEXT_PATHS_FILE" -o "$CONTEXT_PATHS_FILE"
context_path_count="$(grep -cve '^[[:space:]]*$' "$CONTEXT_PATHS_FILE" 2>/dev/null || true)"
phase_end "locality" "notion_context_locate" "ok" "urls=$context_url_index; paths=$context_path_count; traces=$TRACE_DIR/context-locate-*.jsonl"

phase_start
hydrated_count=0
while IFS= read -r context_path; do
  if [ -z "$context_path" ]; then
    continue
  fi
  safe_name="$(trace_safe_name "$context_path")"
  set +e
  run_loc_traced "context-pull-$safe_name" "$LOC_BIN" pull "$context_path" --json > "$OUT_DIR/loc-context-pull-$safe_name.json" 2> "$OUT_DIR/loc-context-pull-$safe_name.err"
  pull_rc=$?
  set -e
  if [ "$pull_rc" -ne 0 ] && ! grep -qi "dirty" "$OUT_DIR/loc-context-pull-$safe_name.json" "$OUT_DIR/loc-context-pull-$safe_name.err"; then
    phase_end "locality" "notion_context_hydrate" "failed" "exit=$pull_rc; path=$context_path; trace=$TRACE_DIR/context-pull-$safe_name.jsonl"
    cat "$OUT_DIR/loc-context-pull-$safe_name.json" >&2
    cat "$OUT_DIR/loc-context-pull-$safe_name.err" >&2
    exit "$pull_rc"
  fi
  hydrated_count=$((hydrated_count + 1))
done < "$CONTEXT_PATHS_FILE"
phase_end "locality" "notion_context_hydrate" "ok" "paths=$hydrated_count; list=$CONTEXT_PATHS_FILE; traces=$TRACE_DIR/context-pull-*.jsonl"

phase_start
: > "$CONTEXT_INVENTORY"
: > "$CONTEXT_SEARCH_RESULTS"
while IFS= read -r context_path; do
  if [ -z "$context_path" ] || [ ! -d "$context_path" ]; then
    continue
  fi
  {
    echo "## $context_path"
    find "$context_path" -name page.md -type f | sort
    echo
  } >> "$CONTEXT_INVENTORY"
  rg -n --no-heading "$CONTEXT_SEARCH_QUERY" "$context_path" >> "$CONTEXT_SEARCH_RESULTS" 2>/dev/null || true
done < "$CONTEXT_PATHS_FILE"
inventory_count="$(grep -c '/page.md$' "$CONTEXT_INVENTORY" 2>/dev/null || true)"
search_hits="$(wc -l < "$CONTEXT_SEARCH_RESULTS" | tr -d ' ')"
phase_end "locality" "notion_context_search" "ok" "pages=$inventory_count; hits=$search_hits; query=$CONTEXT_SEARCH_QUERY"

phase_start
PARENT_DIR="$(dirname "$PAGE_PATH")"
locality_add_dirs=()
while IFS= read -r context_path; do
  if [ -n "$context_path" ]; then
    locality_add_dirs+=("$context_path")
  fi
done < "$CONTEXT_PATHS_FILE"
locality_add_dirs+=("$(dirname "$PAGE_PATH")")
phase_end "locality" "prepare_context_add_dirs" "ok" "dirs=${#locality_add_dirs[@]}"

for scenario_file in "${SCENARIO_FILES[@]}"; do
  SCENARIO_NAME="$(scenario_name_for_file "$scenario_file")"
  CURRENT_SCENARIO="$SCENARIO_NAME"
  SCENARIO_OUT_DIR="$SCENARIO_ROOT/$SCENARIO_NAME"
  mkdir -p "$SCENARIO_OUT_DIR"
  LOCALITY_PROMPT_FILE="$(locality_prompt_for "$scenario_file")"
  MCP_PROMPT_FILE="$(mcp_prompt_for "$scenario_file")"
  SCENARIO_REPORT_TITLE="$(scenario_report_title "$SCENARIO_NAME")"

  phase_start
  REPORT_PAGE_PATH=""
  if [ "$WRITE_MOUNTED_PAGE" = "1" ]; then
    REPORT_PAGE_PATH="$PARENT_DIR/$SCENARIO_REPORT_TITLE/page.md"
    if [ ! -f "$REPORT_PAGE_PATH" ]; then
      "$LOC_BIN" create page --title "$SCENARIO_REPORT_TITLE" --parent "$PARENT_DIR" --json > "$SCENARIO_OUT_DIR/loc-create-page.json"
      REPORT_PAGE_PATH="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["path"])' "$SCENARIO_OUT_DIR/loc-create-page.json")"
    fi
    phase_end "locality" "prepare_report_target" "ok" "path=$REPORT_PAGE_PATH"
  else
    phase_end "locality" "prepare_report_target" "skipped" "artifact-only; report=$SCENARIO_OUT_DIR/report-body.md"
  fi

  prepare_scenario_inputs "$SCENARIO_OUT_DIR"
  if scenario_needs_git_metadata "$scenario_file"; then
    collect_git_metadata "$SCENARIO_OUT_DIR/git-data.json"
  fi
  SCENARIO_CONTEXT_PATHS_FILE="$SCENARIO_OUT_DIR/locality-context-paths.txt"
  SCENARIO_CONTEXT_INVENTORY="$SCENARIO_OUT_DIR/locality-context-inventory.txt"
  SCENARIO_CONTEXT_SEARCH_RESULTS="$SCENARIO_OUT_DIR/locality-context-search.txt"

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$SCENARIO_NAME" "$LOCALITY_PROMPT_FILE" "$MCP_PROMPT_FILE" "$SCENARIO_OUT_DIR" "$SCENARIO_REPORT_TITLE" "$REPORT_PAGE_PATH" >> "$SCENARIO_MANIFEST"

  phase_start
  RUN_OUT_DIR="$OUT_DIR"
  RUN_CONTEXT_PATHS_FILE="$CONTEXT_PATHS_FILE"
  RUN_CONTEXT_INVENTORY="$CONTEXT_INVENTORY"
  RUN_CONTEXT_SEARCH_RESULTS="$CONTEXT_SEARCH_RESULTS"
  OUT_DIR="$SCENARIO_OUT_DIR"
  CONTEXT_PATHS_FILE="$SCENARIO_CONTEXT_PATHS_FILE"
  CONTEXT_INVENTORY="$SCENARIO_CONTEXT_INVENTORY"
  CONTEXT_SEARCH_RESULTS="$SCENARIO_CONTEXT_SEARCH_RESULTS"
  export REPO_DIR LOC_BIN TARGET_URL PAGE_PATH REPORT_PAGE_PATH OUT_DIR METRICS_TSV SUMMARY_JSON
  export CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS CONTEXT_SEARCH_QUERY
  set +e
  run_codex_agent "locality" "$LOCALITY_PROMPT_FILE" "$OUT_DIR/report-body.md" "$OUT_DIR/locality-agent-final.md" "${locality_add_dirs[@]}"
  agent_rc=$?
  set -e
  if [ "$agent_rc" -eq 0 ] && [ -s "$OUT_DIR/report-body.md" ]; then
    phase_end "locality" "codex_exec_wall_time" "ok" "report=$OUT_DIR/report-body.md"
  else
    phase_end "locality" "codex_exec_wall_time" "failed" "exit=$agent_rc"
    cat "$OUT_DIR/locality-codex.err" >&2 || true
    OUT_DIR="$RUN_OUT_DIR"
    CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
    CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
    CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
    export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
    exit "$agent_rc"
  fi

  if [ "$WRITE_MOUNTED_PAGE" = "1" ]; then
    phase_start
    python3 - "$REPORT_PAGE_PATH" "$OUT_DIR/report-body.md" "$SCENARIO_REPORT_TITLE" <<'PY'
import sys
from pathlib import Path

page = Path(sys.argv[1])
body = Path(sys.argv[2]).read_text()
title = sys.argv[3]
current = page.read_text() if page.exists() else ""
frontmatter = f'---\ntitle: "{title}"\n---\n'
if current.startswith("---\n"):
    end = current.find("\n---\n", 4)
    if end != -1:
        frontmatter = current[: end + len("\n---\n")]
page.write_text(frontmatter.rstrip() + "\n\n" + body)
PY
    phase_end "locality" "write_mounted_page" "ok" "path=$REPORT_PAGE_PATH"

    phase_start
    "$LOC_BIN" diff "$REPORT_PAGE_PATH" > "$OUT_DIR/loc-diff.out"
    DIFF_SUMMARY="$(sed -n '1p' "$OUT_DIR/loc-diff.out")"
    phase_end "locality" "loc_diff" "ok" "$DIFF_SUMMARY"

    if [ "$PUSH" -eq 1 ]; then
      phase_start
      "$LOC_BIN" push "$REPORT_PAGE_PATH" -y > "$OUT_DIR/loc-push.out"
      PUSH_SUMMARY="$(tr '\n' ' ' < "$OUT_DIR/loc-push.out" | sed 's/[[:space:]]*$//')"
      phase_end "locality" "loc_push" "ok" "$PUSH_SUMMARY"
    else
      phase_start
      phase_end "locality" "loc_push" "skipped" "dry-run; pass --push to publish"
    fi
  else
    phase_start
    phase_end "locality" "write_mounted_page" "skipped" "artifact-only; report=$OUT_DIR/report-body.md"
    phase_start
    phase_end "locality" "loc_diff" "skipped" "artifact-only; pass --write-mounted-page to diff mounted page"
    phase_start
    phase_end "locality" "loc_push" "skipped" "artifact-only; pass --push to publish"
  fi

  if [ "$COMPARE_MCP" -eq 1 ]; then
    phase_start
    if [ "$MCP_AUTH_CONFIGURED" -eq 0 ]; then
      set +e
      configure_codex_mcp_auth > "$MCP_AUTH_SETUP_OUT" 2> "$MCP_AUTH_SETUP_ERR"
      MCP_AUTH_RC=$?
      set -e
      if [ "$MCP_AUTH_RC" -eq 0 ]; then
        phase_end "notion_mcp" "mcp_auth_setup" "ok" "$MCP_AUTH_DETAIL; out=$MCP_AUTH_SETUP_OUT"
      else
        phase_end "notion_mcp" "mcp_auth_setup" "failed" "exit=$MCP_AUTH_RC; out=$MCP_AUTH_SETUP_OUT; err=$MCP_AUTH_SETUP_ERR"
        cat "$MCP_AUTH_SETUP_OUT" >&2 || true
        cat "$MCP_AUTH_SETUP_ERR" >&2 || true
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$MCP_AUTH_RC"
      fi
    else
      phase_end "notion_mcp" "mcp_auth_setup" "skipped" "already configured; codex_home=$MCP_CODEX_HOME"
    fi

    phase_start
    set +e
    run_codex_agent "notion-mcp" "$MCP_PROMPT_FILE" "$OUT_DIR/notion-mcp-report-body.md" "$OUT_DIR/notion-mcp-agent-final.md"
    mcp_rc=$?
    set -e
    if [ "$mcp_rc" -eq 0 ] && [ -s "$OUT_DIR/notion-mcp-report-body.md" ]; then
      phase_end "notion_mcp" "codex_exec_wall_time" "ok" "report=$OUT_DIR/notion-mcp-report-body.md"
    else
      phase_end "notion_mcp" "codex_exec_wall_time" "failed" "exit=$mcp_rc"
    fi
  fi

  OUT_DIR="$RUN_OUT_DIR"
  CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
  CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
  CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
  export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
done

CURRENT_SCENARIO="setup"

render_locality_traces

python3 - "$METRICS_TSV" "$SUMMARY_JSON" "$SCENARIO_MANIFEST" "$OUT_DIR" "$PUSH" "$WRITE_MOUNTED_PAGE" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" <<'PY'
import csv
import json
import sys
from pathlib import Path

metrics_path, summary_path, manifest_path, out_dir, push, write_mounted_page, model, effort = sys.argv[1:9]
with open(metrics_path) as f:
    metrics = list(csv.DictReader(f, delimiter="\t"))

out = Path(out_dir)
with open(manifest_path) as f:
    scenarios = list(csv.DictReader(f, delimiter="\t"))

scenario_summaries = {}
for scenario in scenarios:
    scenario_out = Path(scenario["out_dir"])
    agent_summaries = {}
    for name in ("locality", "notion-mcp"):
        p = scenario_out / f"{name}-codex-summary.json"
        if p.exists():
            agent_summaries[name] = json.loads(p.read_text())
    scenario_summaries[scenario["scenario"]] = {
        "out_dir": str(scenario_out),
        "report_title": scenario["report_title"],
        "page_path": scenario["report_page_path"],
        "locality_prompt": scenario["locality_prompt"],
        "mcp_prompt": scenario["mcp_prompt"],
        "agent_event_summaries": agent_summaries,
    }

locality_trace_summaries = {}
for p in sorted((out / "locality-traces").glob("*-summary.json")):
    locality_trace_summaries[str(p.relative_to(out))] = json.loads(p.read_text())
for p in sorted((out / "scenarios").glob("*/*-agent-locality-trace-summary.json")):
    locality_trace_summaries[str(p.relative_to(out))] = json.loads(p.read_text())

first_scenario = scenarios[0] if scenarios else {}
first_scenario_summary = scenario_summaries.get(first_scenario.get("scenario", ""), {})
summary = {
    "ok": True,
    "model": model,
    "reasoning_effort": effort,
    "scenario_count": len(scenarios),
    "page_path": first_scenario_summary.get("page_path"),
    "page_paths": {name: data["page_path"] for name, data in scenario_summaries.items()},
    "out_dir": out_dir,
    "pushed": push == "1",
    "write_mounted_page": write_mounted_page == "1",
    "metrics": [
        {
            "scenario": row.get("scenario", ""),
            "strategy": row["strategy"],
            "phase": row["phase"],
            "duration_ms": int(row["duration_ms"]),
            "status": row["status"],
            "detail": row["detail"],
        }
        for row in metrics
    ],
    "agent_event_summaries": first_scenario_summary.get("agent_event_summaries", {}),
    "scenarios": scenario_summaries,
    "locality_trace_summaries": locality_trace_summaries,
}
Path(summary_path).write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
PY
