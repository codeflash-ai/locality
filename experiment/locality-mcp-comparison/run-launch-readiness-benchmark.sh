#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-launch-readiness-benchmark.sh [--push] [--compare-mcp] [--write-mounted-page] [--scenario NAME[,NAME...]] [--compare-hooks] [--strategy NAME]

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

Study mode:
  --scenario NAME       Run one or more prompt scenarios, by basename or
                        filename. Use a comma-delimited list for multiple
                        scenarios, for example --scenario scenario7,scenario8.
  --compare-hooks       Run that scenario four times: Locality without hooks,
                        Locality with hooks, MCP without hooks, and MCP with
                        hooks. This implies --compare-mcp and is artifact-only.
  --strategy NAME       Run only one strategy: locality, notion-mcp, or all.
                        Default: all. Without --compare-mcp, all means
                        Locality-only for backward compatibility.

Important environment:
  REPO_DIR                 Repository path. Default: /home/amika/workspace/locality
  LOC_BIN                  loc binary. Default: $REPO_DIR/target/debug/loc
  PROMPT_ROOT              Prompt root. Default: <script-dir>/prompts
  LOCALITY_PROMPT_DIR      Locality prompt directory. Default: $PROMPT_ROOT/Locality
  MCP_PROMPT_DIR           MCP prompt directory. Default: $PROMPT_ROOT/MCP
  TARGET_URL               Notion page URL for benchmark output parent.
  CONTEXT_URLS             Newline-delimited Notion URLs to hydrate as directories.
  LOCALITY_CONTEXT_DIRS    Newline-delimited or colon-delimited prehydrated
                           Locality directories to add to the Locality agent.
  LOCALITY_CONTEXT_HYDRATE Pull all context directories before running the
                           Locality agent. Default: 1. Set 0 for a sandbox that
                           already has prehydrated multi-source files.
  CODEX_MODEL              Model passed to codex exec. Default: gpt-5.6-luna
  CODEX_REASONING_EFFORT   Codex reasoning effort. Default: low
  CODEX_SANDBOX_OUT_DIR    Agent-visible OUT_DIR for scenarios after scenario1.
                           Default: /home/amika
  CODEX_SANDBOX_HARDCODED_OUT_DIR
                           Compatibility output directory for prompts that
                           write absolute sandbox paths. Default: /home/amika
  CODEX_HOOKS_MODE         Codex hooks mode for normal runs: hooks or no-hooks.
                           Default: hooks. Hook comparison mode overrides this
                           per variant.
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
COMPARE_HOOKS=0
SCENARIO_FILTER="${SCENARIO_FILTER:-}"
RUN_STRATEGY="${RUN_STRATEGY:-all}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --push) PUSH=1; WRITE_MOUNTED_PAGE=1 ;;
    --compare-mcp) COMPARE_MCP=1 ;;
    --compare-hooks) COMPARE_HOOKS=1 ;;
    --write-mounted-page) WRITE_MOUNTED_PAGE=1 ;;
    --scenario)
      shift
      if [ "$#" -eq 0 ]; then
        echo "--scenario requires a scenario name or filename" >&2
        usage >&2
        exit 2
      fi
      SCENARIO_FILTER="$1"
      ;;
    --scenario=*) SCENARIO_FILTER="${1#--scenario=}" ;;
    --strategy)
      shift
      if [ "$#" -eq 0 ]; then
        echo "--strategy requires locality, notion-mcp, or all" >&2
        usage >&2
        exit 2
      fi
      RUN_STRATEGY="$1"
      ;;
    --strategy=*) RUN_STRATEGY="${1#--strategy=}" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$WRITE_MOUNTED_PAGE" in
  0|1) ;;
  *) echo "WRITE_MOUNTED_PAGE must be 0 or 1" >&2; exit 2 ;;
esac

case "$RUN_STRATEGY" in
  all|locality|notion-mcp) ;;
  *) echo "--strategy must be locality, notion-mcp, or all" >&2; exit 2 ;;
esac

if [ "$COMPARE_HOOKS" -eq 1 ]; then
  COMPARE_MCP=1
  if [ "$RUN_STRATEGY" != "all" ]; then
    echo "--compare-hooks requires --strategy all" >&2
    exit 2
  fi
  if [ "$WRITE_MOUNTED_PAGE" = "1" ] || [ "$PUSH" -eq 1 ]; then
    echo "--compare-hooks is artifact-only and cannot be combined with --write-mounted-page or --push" >&2
    exit 2
  fi
fi

if [ "$RUN_STRATEGY" = "notion-mcp" ]; then
  COMPARE_MCP=1
  if [ "$WRITE_MOUNTED_PAGE" = "1" ] || [ "$PUSH" -eq 1 ]; then
    echo "--strategy notion-mcp is artifact-only and cannot be combined with --write-mounted-page or --push" >&2
    exit 2
  fi
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${REPO_DIR:-/home/amika/workspace/locality}"
LOC_BIN="${LOC_BIN:-$REPO_DIR/target/debug/loc}"
TARGET_URL="${TARGET_URL:-https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef}"
CONTEXT_URLS="${CONTEXT_URLS:-https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba}"
CONTEXT_PULL_PATHS="${CONTEXT_PULL_PATHS:-}"
CONTEXT_SEARCH_QUERY="${CONTEXT_SEARCH_QUERY:-benchmark|launch readiness|safe diff|push|review|Live Mode|File Provider|Windows Cloud Files|distribution|Homebrew|install|connector|standup|blocker|risk}"
LOCALITY_CONTEXT_DIRS="${LOCALITY_CONTEXT_DIRS:-${LOCALITY_CONTEXT_ROOTS:-}}"
LOCALITY_CONTEXT_HYDRATE="${LOCALITY_CONTEXT_HYDRATE:-1}"
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
CODEX_SANDBOX_OUT_DIR="${CODEX_SANDBOX_OUT_DIR:-/home/amika}"
CODEX_SANDBOX_HARDCODED_OUT_DIR="${CODEX_SANDBOX_HARDCODED_OUT_DIR:-/home/amika}"
CODEX_HOOKS_MODE="${CODEX_HOOKS_MODE:-hooks}"
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

RUN_LOCALITY_AGENT=0
RUN_MCP_AGENT=0
case "$RUN_STRATEGY" in
  locality)
    RUN_LOCALITY_AGENT=1
    ;;
  notion-mcp)
    RUN_MCP_AGENT=1
    ;;
  all)
    RUN_LOCALITY_AGENT=1
    if [ "$COMPARE_MCP" -eq 1 ]; then
      RUN_MCP_AGENT=1
    fi
    ;;
esac

case "$CODEX_HOOKS_MODE" in
  hooks|no-hooks) ;;
  *) echo "CODEX_HOOKS_MODE must be hooks or no-hooks" >&2; exit 2 ;;
esac

case "$LOCALITY_CONTEXT_HYDRATE" in
  0|1) ;;
  *) echo "LOCALITY_CONTEXT_HYDRATE must be 0 or 1" >&2; exit 2 ;;
esac

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
  for trace_file in "$TRACE_DIR"/*.jsonl "$SCENARIO_ROOT"/*/*-agent-locality-trace.jsonl "$SCENARIO_ROOT"/*/variants/*/*-agent-locality-trace.jsonl; do
    if [ ! -s "$trace_file" ]; then
      continue
    fi
    python3 "$SCRIPT_DIR/scripts/locality-trace-to-speedscope.py" \
      "$trace_file" "${trace_file%.jsonl}" >/dev/null
  done
}

render_codex_event_artifacts() {
  local events_file="$1"
  local out_prefix="$2"
  if [ ! -s "$events_file" ]; then
    return 0
  fi
  python3 "$SCRIPT_DIR/scripts/codex-events-to-trace.py" "$events_file" "$out_prefix" >/dev/null
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
  install_codex_harness_hooks "$codex_home"
  chmod 600 "$codex_home/config.toml"
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
    hook = {
        "type": "command",
        "command": command,
        "timeout": 10,
    }
    if status_message:
        hook["statusMessage"] = status_message
    return hook

payload = {
    "description": "Locality benchmark live Codex timing hooks.",
    "hooks": {
        "SessionStart": [
            {
                "matcher": "startup|resume|clear|compact",
                "hooks": [command_hook(None)],
            }
        ],
        "UserPromptSubmit": [
            {
                "hooks": [command_hook(None)],
            }
        ],
        "PreToolUse": [
            {
                "matcher": "*",
                "hooks": [command_hook("Recording tool start")],
            }
        ],
        "PostToolUse": [
            {
                "matcher": "*",
                "hooks": [command_hook("Recording tool finish")],
            }
        ],
        "Stop": [
            {
                "hooks": [command_hook(None)],
            }
        ],
    },
}

hooks_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
  chmod 600 "$codex_home/hooks.json"
}

merge_codex_event_streams() {
  local raw_events_file="$1"
  local hook_events_file="$2"
  local events_file="$3"
  python3 - "$raw_events_file" "$hook_events_file" "$events_file" <<'PY'
import json
import sys
from pathlib import Path

inputs = [Path(sys.argv[1]), Path(sys.argv[2])]
output = Path(sys.argv[3])
records = []

for source_order, path in enumerate(inputs):
    if not path.exists():
        continue
    for line_order, line in enumerate(path.read_text(encoding="utf-8").splitlines()):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        records.append(
            (
                int(record.get("observed_at_ms") or 0),
                source_order,
                line_order,
                record,
            )
        )

records.sort(key=lambda item: (item[0], item[1], item[2]))
output.write_text(
    "".join(json.dumps(record, separators=(",", ":")) + "\n" for *_, record in records),
    encoding="utf-8",
)
PY
}

candidate_agent_output_paths() {
  local artifact_out_dir="$1"
  local agent_out_dir="$2"
  local basename="$3"
  local sandbox_out_dir="${CODEX_SANDBOX_OUT_DIR%/}"
  local hardcoded_out_dir="${CODEX_SANDBOX_HARDCODED_OUT_DIR%/}"
  local candidates=(
    "$agent_out_dir/$basename"
    "$artifact_out_dir/$basename"
    "$sandbox_out_dir/$basename"
    "$hardcoded_out_dir/$basename"
  )
  local seen="|"
  local candidate
  for candidate in "${candidates[@]}"; do
    if [ -z "$candidate" ] || [ "$candidate" = "/$basename" ]; then
      continue
    fi
    case "$seen" in
      *"|$candidate|"*) continue ;;
    esac
    seen="$seen$candidate|"
    printf '%s\n' "$candidate"
  done
}

cleanup_agent_output_candidates() {
  local artifact_out_dir="$1"
  local agent_out_dir="$2"
  shift 2
  local basename
  local candidate
  for basename in "$@"; do
    while IFS= read -r candidate; do
      [ -n "$candidate" ] || continue
      rm -f "$candidate" 2>/dev/null || true
    done < <(candidate_agent_output_paths "$artifact_out_dir" "$agent_out_dir" "$basename")
  done
}

retrieve_one_agent_output() {
  local kind="$1"
  local destination="$2"
  local require_nonempty="$3"
  local manifest="$4"
  shift 4
  local source
  local status="missing"
  local chosen=""

  for source in "$@"; do
    if { [ "$require_nonempty" = "1" ] && [ -s "$source" ]; } ||
      { [ "$require_nonempty" != "1" ] && [ -f "$source" ]; }; then
      mkdir -p "$(dirname "$destination")"
      if [ "$source" != "$destination" ]; then
        cp "$source" "$destination"
        status="copied"
      else
        status="present"
      fi
      chosen="$source"
      break
    fi
  done

  printf '%s\t%s\t%s\t%s\n' "$kind" "$status" "$chosen" "$destination" >> "$manifest"
  [ "$status" != "missing" ]
}

retrieve_agent_outputs() {
  local strategy="$1"
  local artifact_out_dir="$2"
  local agent_out_dir="$3"
  local report_name="$4"
  local trace_name="$5"
  local report_file="$6"
  local manifest="$artifact_out_dir/$strategy-agent-artifacts.tsv"
  local report_candidates=()
  local trace_candidates=()
  local candidate

  printf 'kind\tstatus\tsource\tdestination\n' > "$manifest"
  while IFS= read -r candidate; do
    report_candidates+=("$candidate")
  done < <(candidate_agent_output_paths "$artifact_out_dir" "$agent_out_dir" "$report_name")
  while IFS= read -r candidate; do
    trace_candidates+=("$candidate")
  done < <(candidate_agent_output_paths "$artifact_out_dir" "$agent_out_dir" "$trace_name")

  local report_rc=0
  retrieve_one_agent_output "report" "$report_file" 1 "$manifest" "${report_candidates[@]}" || report_rc=$?
  retrieve_one_agent_output "trace" "$artifact_out_dir/$trace_name" 0 "$manifest" "${trace_candidates[@]}" || true
  return "$report_rc"
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
  if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
    prepare_codex_home_without_mcp "$LOCALITY_CODEX_HOME"
  fi
  if [ "$RUN_MCP_AGENT" -eq 1 ]; then
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
  local artifact_out_dir="$OUT_DIR"
  local agent_out_dir="${CURRENT_AGENT_OUT_DIR:-$artifact_out_dir}"
  local report_name
  local agent_report_file
  local agent_trace_name
  local agent_trace_file
  local events_file="$artifact_out_dir/$strategy-codex-events.jsonl"
  local raw_events_file="$artifact_out_dir/$strategy-codex-events.raw.jsonl"
  local hook_events_file="$artifact_out_dir/$strategy-codex-hooks.jsonl"
  local hook_state_file="$artifact_out_dir/$strategy-codex-hooks.state.json"
  local err_file="$artifact_out_dir/$strategy-codex.err"
  local out_file="$artifact_out_dir/$strategy-codex.out"
  local summary_file="$artifact_out_dir/$strategy-codex-summary.json"
  local events_tsv="$artifact_out_dir/$strategy-codex-events.tsv"
  local prompt_snapshot="$artifact_out_dir/$strategy-prompt.md"
  local command_snapshot="$artifact_out_dir/$strategy-codex-command.txt"
  local agent_loc_trace="$artifact_out_dir/$strategy-agent-locality-trace.jsonl"
  local git_data_file="$artifact_out_dir/git-data.json"
  local prompt
  local run_cmd
  local codex_home
  local hooks_mode="${CURRENT_CODEX_HOOKS_MODE:-$CODEX_HOOKS_MODE}"
  case "$hooks_mode" in
    hooks|no-hooks) ;;
    *) echo "invalid Codex hooks mode: $hooks_mode" >&2; return 2 ;;
  esac
  codex_home="$(codex_home_for_strategy "$strategy")"
  prompt="$(cat "$prompt_file")"
  cp "$prompt_file" "$prompt_snapshot"
  mkdir -p "$artifact_out_dir" "$agent_out_dir"
  report_name="$(basename "$report_file")"
  agent_report_file="$agent_out_dir/$report_name"
  case "$strategy" in
    locality) agent_trace_name="locality-agent-trace.md" ;;
    notion-mcp) agent_trace_name="notion-mcp-agent-trace.md" ;;
    *) agent_trace_name="$strategy-agent-trace.md" ;;
  esac
  agent_trace_file="$agent_out_dir/$agent_trace_name"
  cleanup_agent_output_candidates "$artifact_out_dir" "$agent_out_dir" "$report_name" "$agent_trace_name"
  rm -f "$final_file"

  local cmd=(
    codex exec
    --json
    --model "$CODEX_MODEL"
    -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\""
    --dangerously-bypass-approvals-and-sandbox
    -C "$REPO_DIR"
    --add-dir "$artifact_out_dir"
  )
  if [ "$agent_out_dir" != "$artifact_out_dir" ]; then
    cmd+=(--add-dir "$agent_out_dir")
  fi
  cmd+=(--output-last-message "$final_file")
  if [ "$hooks_mode" = "hooks" ]; then
    cmd+=(--enable hooks --dangerously-bypass-hook-trust)
  else
    cmd+=(--disable hooks)
  fi
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
    printf 'hooks_mode=%s\n' "$hooks_mode"
    printf 'artifact_out_dir=%s\n' "$artifact_out_dir"
    printf 'agent_out_dir=%s\n' "$agent_out_dir"
    printf 'hardcoded_out_dir=%s\n' "$CODEX_SANDBOX_HARDCODED_OUT_DIR"
    printf 'report_file=%s\n' "$agent_report_file"
    printf 'trace_file=%s\n' "$agent_trace_file"
    printf 'git_data_file=%s\n' "$git_data_file"
    printf 'context_paths_file=%s\n' "$CONTEXT_PATHS_FILE"
    printf 'context_inventory=%s\n' "$CONTEXT_INVENTORY"
    printf 'context_search_results=%s\n' "$CONTEXT_SEARCH_RESULTS"
    printf 'codex_command='
    printf '%q ' "${cmd[@]}"
    printf '\nwrapped_command='
    printf '%q ' "${run_cmd[@]}"
    printf '\n'
  } > "$command_snapshot"

  set +e
  set -o pipefail
  : > "$hook_events_file"
  rm -f "$hook_state_file"
  if [ "$hooks_mode" = "hooks" ]; then
    CODEX_HOME="$codex_home" \
      OUT_DIR="$agent_out_dir" \
      AGENT_OUT_DIR="$agent_out_dir" \
      ARTIFACT_OUT_DIR="$artifact_out_dir" \
      SCENARIO_OUT_DIR="$artifact_out_dir" \
      CODEX_SANDBOX_HARDCODED_OUT_DIR="$CODEX_SANDBOX_HARDCODED_OUT_DIR" \
      REPORT_FILE="$agent_report_file" \
      TRACE_FILE="$agent_trace_file" \
      GIT_DATA_FILE="$git_data_file" \
      LOCALITY_CONTEXT_PATHS_FILE="$CONTEXT_PATHS_FILE" \
      LOCALITY_CONTEXT_INVENTORY="$CONTEXT_INVENTORY" \
      LOCALITY_CONTEXT_SEARCH_RESULTS="$CONTEXT_SEARCH_RESULTS" \
      CONTEXT_PATHS_FILE="$CONTEXT_PATHS_FILE" \
      CONTEXT_INVENTORY="$CONTEXT_INVENTORY" \
      CONTEXT_SEARCH_RESULTS="$CONTEXT_SEARCH_RESULTS" \
      CODEX_HARNESS_HOOK_EVENTS_FILE="$hook_events_file" \
      CODEX_HARNESS_HOOK_STATE_FILE="$hook_state_file" \
      LOCALITY_TRACE_FILE="$agent_loc_trace" \
      LOCALITY_TRACE_RUN_ID="$RUN_ID" \
      "${run_cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$raw_events_file"
  else
    CODEX_HOME="$codex_home" \
      OUT_DIR="$agent_out_dir" \
      AGENT_OUT_DIR="$agent_out_dir" \
      ARTIFACT_OUT_DIR="$artifact_out_dir" \
      SCENARIO_OUT_DIR="$artifact_out_dir" \
      CODEX_SANDBOX_HARDCODED_OUT_DIR="$CODEX_SANDBOX_HARDCODED_OUT_DIR" \
      REPORT_FILE="$agent_report_file" \
      TRACE_FILE="$agent_trace_file" \
      GIT_DATA_FILE="$git_data_file" \
      LOCALITY_CONTEXT_PATHS_FILE="$CONTEXT_PATHS_FILE" \
      LOCALITY_CONTEXT_INVENTORY="$CONTEXT_INVENTORY" \
      LOCALITY_CONTEXT_SEARCH_RESULTS="$CONTEXT_SEARCH_RESULTS" \
      CONTEXT_PATHS_FILE="$CONTEXT_PATHS_FILE" \
      CONTEXT_INVENTORY="$CONTEXT_INVENTORY" \
      CONTEXT_SEARCH_RESULTS="$CONTEXT_SEARCH_RESULTS" \
      LOCALITY_TRACE_FILE="$agent_loc_trace" \
      LOCALITY_TRACE_RUN_ID="$RUN_ID" \
      "${run_cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$raw_events_file"
  fi
  local pipe_status=("${PIPESTATUS[@]}")
  local rc="${pipe_status[0]}"
  set +o pipefail
  set -e
  : > "$out_file"

  local retrieve_rc=0
  retrieve_agent_outputs "$strategy" "$artifact_out_dir" "$agent_out_dir" "$report_name" "$agent_trace_name" "$report_file" || retrieve_rc=$?
  merge_codex_event_streams "$raw_events_file" "$hook_events_file" "$events_file"
  python3 "$SCRIPT_DIR/scripts/summarize-codex-events.py" "$events_file" "$summary_file" "$events_tsv"
  render_codex_event_artifacts "$events_file" "$artifact_out_dir/$strategy"
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi
  if [ "$retrieve_rc" -ne 0 ]; then
    return "$retrieve_rc"
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

filter_prompt_scenarios() {
  if [ -z "$SCENARIO_FILTER" ]; then
    return
  fi

  local requested
  local requested_base
  local requested_stem
  local scenario_file
  local scenario_stem
  local matches=()
  local token_matches=()
  local seen="|"
  local requested_items=()
  IFS=',' read -r -a requested_items <<< "$SCENARIO_FILTER"

  for requested in "${requested_items[@]}"; do
    requested="${requested#"${requested%%[![:space:]]*}"}"
    requested="${requested%"${requested##*[![:space:]]}"}"
    if [ -z "$requested" ]; then
      continue
    fi
    requested_base="$(basename "$requested")"
    requested_stem="${requested_base%.md}"
    token_matches=()

    for scenario_file in "${SCENARIO_FILES[@]}"; do
      scenario_stem="${scenario_file%.md}"
      if [ "$scenario_file" = "$requested_base" ] ||
        [ "$scenario_stem" = "$requested_stem" ] ||
        [ "$(locality_prompt_for "$scenario_file")" = "$requested" ]; then
        token_matches+=("$scenario_file")
      fi
    done

    if [ "${#token_matches[@]}" -eq 0 ]; then
      echo "no prompt scenario matched --scenario item $requested" >&2
      exit 2
    fi
    if [ "${#token_matches[@]}" -gt 1 ]; then
      echo "--scenario item $requested matched multiple prompt scenarios" >&2
      exit 2
    fi
    if [[ "$seen" != *"|${token_matches[0]}|"* ]]; then
      matches+=("${token_matches[0]}")
      seen="$seen${token_matches[0]}|"
    fi
  done

  if [ "${#matches[@]}" -eq 0 ]; then
    echo "no prompt scenario matched --scenario $SCENARIO_FILTER" >&2
    exit 2
  fi

  SCENARIO_FILES=("${matches[@]}")
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
  case "$(scenario_name_for_file "$scenario_file")" in
    scenario1|scenario7|scenario8) return 0 ;;
    *) return 1 ;;
  esac
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

out = Path(sys.argv[1])
raw = os.environ.get("LOCALITY_CONTEXT_DIRS_RAW", "")
if "\n" in raw:
    parts = raw.splitlines()
else:
    parts = re.split(r":", raw)

with out.open("a", encoding="utf-8") as handle:
    for part in parts:
        path = os.path.expanduser(part.strip())
        if path:
            handle.write(path + "\n")
PY
}

scenario_agent_out_dir() {
  local scenario_name="$1"
  local scenario_out_dir="$2"
  if [ "$scenario_name" = "scenario1" ]; then
    printf '%s\n' "$scenario_out_dir"
  else
    printf '%s\n' "${CODEX_SANDBOX_OUT_DIR%/}"
  fi
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

run_codex_agent_variant() {
  local strategy="$1"
  local hooks_mode="$2"
  local variant_out_dir="$3"
  local agent_out_dir="$4"
  local prompt_file="$5"
  local report_name="$6"
  local final_name="$7"
  shift 7

  mkdir -p "$variant_out_dir"

  local saved_out_dir="$OUT_DIR"
  OUT_DIR="$variant_out_dir"
  export OUT_DIR
  CURRENT_AGENT_OUT_DIR="$agent_out_dir" CURRENT_CODEX_HOOKS_MODE="$hooks_mode" run_codex_agent "$strategy" "$prompt_file" "$OUT_DIR/$report_name" "$OUT_DIR/$final_name" "$@"
  local rc=$?
  OUT_DIR="$saved_out_dir"
  export OUT_DIR
  return "$rc"
}

run_codex_variant_with_metric() {
  local strategy="$1"
  local metric_strategy="$2"
  local hooks_mode="$3"
  local variant_label="$4"
  local variant_out_dir="$5"
  local agent_out_dir="$6"
  local prompt_file="$7"
  local report_name="$8"
  local final_name="$9"
  shift 9

  local phase_name="codex_exec_wall_time"
  if [ "$variant_label" != "default" ]; then
    phase_name="codex_exec_wall_time.$variant_label"
  fi

  phase_start
  set +e
  run_codex_agent_variant "$strategy" "$hooks_mode" "$variant_out_dir" "$agent_out_dir" "$prompt_file" "$report_name" "$final_name" "$@"
  local agent_rc=$?
  set -e

  if [ "$agent_rc" -eq 0 ] && [ -s "$variant_out_dir/$report_name" ]; then
    phase_end "$metric_strategy" "$phase_name" "ok" "hooks=$hooks_mode; report=$variant_out_dir/$report_name"
    return 0
  fi

  phase_end "$metric_strategy" "$phase_name" "failed" "hooks=$hooks_mode; exit=$agent_rc; report=$variant_out_dir/$report_name"
  cat "$variant_out_dir/$strategy-codex.err" >&2 || true
  if [ "$agent_rc" -ne 0 ]; then
    return "$agent_rc"
  fi
  return 1
}

generate_hook_comparison_profiles() {
  local scenario_out_dir="$1"
  local scenario_name="$2"
  local profile_root="$scenario_out_dir/hook-comparison"
  local strategy

  mkdir -p "$profile_root"
  for strategy in locality notion-mcp; do
    local no_hooks_events="$scenario_out_dir/variants/$strategy-no-hooks/$strategy-codex-events.jsonl"
    local hooks_events="$scenario_out_dir/variants/$strategy-hooks/$strategy-codex-events.jsonl"
    if [ ! -s "$no_hooks_events" ] || [ ! -s "$hooks_events" ]; then
      continue
    fi
    node "$SCRIPT_DIR/../agent-conversation-profile-modern-codex.mjs" \
      --left "$no_hooks_events" \
      --left-label "$strategy-no-hooks" \
      --right "$hooks_events" \
      --right-label "$strategy-hooks" \
      --out "$profile_root/$strategy" >/dev/null
  done

  python3 - "$scenario_out_dir" "$scenario_name" <<'PY'
import json
import sys
from pathlib import Path

scenario_out = Path(sys.argv[1])
scenario_name = sys.argv[2]
strategies = ("locality", "notion-mcp")
modes = ("no-hooks", "hooks")

def load_json(path):
    if not path.exists():
        return {}
    return json.loads(path.read_text(encoding="utf-8"))

def fmt_ms(value):
    if value is None:
        return ""
    value = int(value)
    if abs(value) >= 1000:
        return f"{value / 1000:.3f}s"
    return f"{value}ms"

def fmt_percall(duration_ms, count):
    count = int(count or 0)
    if count <= 0:
        return ""
    return fmt_ms(round(int(duration_ms or 0) / count))

def cell(value):
    text = str(value)
    return text.replace("|", "\\|").replace("\n", " ")

def conversation_for(strategy, mode):
    summary_path = scenario_out / "hook-comparison" / strategy / "summary.json"
    summary = load_json(summary_path)
    label = f"{strategy}-{mode}"
    for conversation in summary.get("conversations", []):
        if conversation.get("label") == label:
            return conversation
    return {}

lines = [
    f"# Hook Comparison: {scenario_name}",
    "",
    "This study runs the selected prompt four ways: Locality without hooks, Locality with hooks, Notion MCP without hooks, and Notion MCP with hooks.",
    "",
    "Hooked runs add live `harness.phase` records for prompt handoff, thinking, tool calls, and final response spans. Hookless runs keep the raw Codex stdout event stream, so profile gaps are inferred when no native duration is present.",
    "",
    "## Timing Summary",
    "",
    "| Strategy | Mode | Codex observed | Profile wall | Measured | Inferred | Hook phases | Tool phases | User query | Reasoning | Tool | Agent response |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
]

tool_lines = [
    "## Tool Command Breakdown",
    "",
    "The profiler groups Bash `loc` calls by Locality subcommand and keeps non-`loc` tools separate.",
    "",
]

for strategy in strategies:
    for mode in modes:
        variant_dir = scenario_out / "variants" / f"{strategy}-{mode}"
        event_summary = load_json(variant_dir / f"{strategy}-codex-summary.json")
        conversation = conversation_for(strategy, mode)
        activities = conversation.get("totals_by_activity", {})
        phase_counts = event_summary.get("phase_counts", {})
        hook_phase_count = sum(int(value) for value in phase_counts.values())
        lines.append(
            "| {strategy} | {mode} | {observed} | {wall} | {measured} | {inferred} | {hook_phases} | {tool_phases} | {user_query} | {reasoning} | {tool} | {agent_response} |".format(
                strategy=cell(strategy),
                mode=cell(mode),
                observed=fmt_ms(event_summary.get("observed_duration_ms", 0)),
                wall=fmt_ms(conversation.get("wall_time_ms")),
                measured=fmt_ms(conversation.get("measured_duration_ms")),
                inferred=fmt_ms(conversation.get("inferred_duration_ms")),
                hook_phases=hook_phase_count,
                tool_phases=phase_counts.get("tool_call", 0),
                user_query=fmt_ms(activities.get("user_query", 0)),
                reasoning=fmt_ms(activities.get("reasoning", 0)),
                tool=fmt_ms(activities.get("tool", 0)),
                agent_response=fmt_ms(activities.get("agent_response", 0)),
            )
        )

for strategy in strategies:
    for mode in modes:
        conversation = conversation_for(strategy, mode)
        commands = conversation.get("tool_commands", [])
        tool_lines.extend([f"### {strategy} {mode}", ""])
        if not commands:
            tool_lines.extend(["No tool command records.", ""])
            continue
        tool_lines.extend([
            "| Tool group | Command | Count | Duration | Percall |",
            "| --- | --- | ---: | ---: | ---: |",
        ])
        for command in commands[:12]:
            count = command.get("count", 0)
            duration_ms = command.get("duration_ms", 0)
            tool_lines.append(
                "| {group} | {cmd} | {count} | {duration} | {percall} |".format(
                    group=cell(command.get("tool_group", "")),
                    cmd=cell(command.get("command", "")),
                    count=count,
                    duration=fmt_ms(duration_ms),
                    percall=fmt_percall(duration_ms, count),
                )
            )
        tool_lines.append("")

profile_lines = [
    "## Profile Artifacts",
    "",
    "| Strategy | Profile summary |",
    "| --- | --- |",
]
for strategy in strategies:
    summary_path = scenario_out / "hook-comparison" / strategy / "summary.md"
    if summary_path.exists():
        profile_lines.append(f"| {cell(strategy)} | {cell(summary_path)} |")

(scenario_out / "hooks-comparison.md").write_text(
    "\n".join(lines + [""] + tool_lines + profile_lines) + "\n",
    encoding="utf-8",
)
PY
}

discover_prompt_scenarios
filter_prompt_scenarios
if [ "$COMPARE_HOOKS" -eq 1 ] && [ "${#SCENARIO_FILES[@]}" -ne 1 ]; then
  echo "--compare-hooks requires exactly one scenario; pass --scenario when multiple scenarios exist" >&2
  exit 2
fi
validate_prompt_scenarios

echo -e "scenario\tstrategy\tphase\tstart_ms\tend_ms\tduration_ms\tstatus\tdetail" > "$METRICS_TSV"
printf 'scenario\tstrategy\tvariant\thooks\tlocality_prompt\tmcp_prompt\tout_dir\tagent_out_dir\treport_title\treport_page_path\n' > "$SCENARIO_MANIFEST"

phase_start
test -d "$REPO_DIR"
if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
  test -x "$LOC_BIN"
fi
git -C "$REPO_DIR" rev-parse --git-dir >/dev/null
phase_end "locality" "validate_environment" "ok" "repo=$REPO_DIR; model=$CODEX_MODEL; effort=$CODEX_REASONING_EFFORT; scenarios=${#SCENARIO_FILES[@]}; prompts=$LOCALITY_PROMPT_DIR; write_mounted_page=$WRITE_MOUNTED_PAGE; run_strategy=$RUN_STRATEGY"

phase_start
set +e
prepare_codex_strategy_homes > "$OUT_DIR/codex-strategy-setup.out" 2> "$OUT_DIR/codex-strategy-setup.err"
CODEX_STRATEGY_SETUP_RC=$?
set -e
if [ "$CODEX_STRATEGY_SETUP_RC" -eq 0 ]; then
  if [ "$RUN_MCP_AGENT" -eq 1 ]; then
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

PAGE_PATH=""
PARENT_DIR=""
locality_add_dirs=()
if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
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
  append_context_paths_from_var "$LOCALITY_CONTEXT_DIRS"
  sort -u "$CONTEXT_PATHS_FILE" -o "$CONTEXT_PATHS_FILE"
  context_path_count="$(grep -cve '^[[:space:]]*$' "$CONTEXT_PATHS_FILE" 2>/dev/null || true)"
  phase_end "locality" "notion_context_locate" "ok" "urls=$context_url_index; paths=$context_path_count; traces=$TRACE_DIR/context-locate-*.jsonl"

  phase_start
  hydrated_count=0
  if [ "$LOCALITY_CONTEXT_HYDRATE" = "1" ]; then
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
  else
    hydrated_count="$(grep -cve '^[[:space:]]*$' "$CONTEXT_PATHS_FILE" 2>/dev/null || true)"
    phase_end "locality" "notion_context_hydrate" "skipped" "prehydrated context; paths=$hydrated_count; list=$CONTEXT_PATHS_FILE"
  fi

  phase_start
  : > "$CONTEXT_INVENTORY"
  : > "$CONTEXT_SEARCH_RESULTS"
  while IFS= read -r context_path; do
    if [ -z "$context_path" ] || [ ! -d "$context_path" ]; then
      continue
    fi
    {
      echo "## $context_path"
      find "$context_path" \( -name page.md -o -name '*.md' -o -name '*.txt' -o -name '*.json' \) -type f | sort | head -5000
      echo
    } >> "$CONTEXT_INVENTORY"
    rg -n --no-heading "$CONTEXT_SEARCH_QUERY" "$context_path" >> "$CONTEXT_SEARCH_RESULTS" 2>/dev/null || true
  done < "$CONTEXT_PATHS_FILE"
  inventory_count="$(grep -Ec '(\.md|\.txt|\.json)$' "$CONTEXT_INVENTORY" 2>/dev/null || true)"
  search_hits="$(wc -l < "$CONTEXT_SEARCH_RESULTS" | tr -d ' ')"
  phase_end "locality" "notion_context_search" "ok" "files=$inventory_count; hits=$search_hits; query=$CONTEXT_SEARCH_QUERY"

  phase_start
  PARENT_DIR="$(dirname "$PAGE_PATH")"
  while IFS= read -r context_path; do
    if [ -n "$context_path" ]; then
      locality_add_dirs+=("$context_path")
    fi
  done < "$CONTEXT_PATHS_FILE"
  locality_add_dirs+=("$(dirname "$PAGE_PATH")")
  phase_end "locality" "prepare_context_add_dirs" "ok" "dirs=${#locality_add_dirs[@]}"
else
  : > "$CONTEXT_PATHS_FILE"
  : > "$CONTEXT_INVENTORY"
  : > "$CONTEXT_SEARCH_RESULTS"
  phase_start
  phase_end "locality" "notion_target_locate" "skipped" "run_strategy=$RUN_STRATEGY"
  phase_start
  phase_end "locality" "notion_target_prehydrate" "skipped" "run_strategy=$RUN_STRATEGY"
  phase_start
  phase_end "locality" "notion_context_locate" "skipped" "run_strategy=$RUN_STRATEGY"
  phase_start
  phase_end "locality" "notion_context_hydrate" "skipped" "run_strategy=$RUN_STRATEGY"
  phase_start
  phase_end "locality" "notion_context_search" "skipped" "run_strategy=$RUN_STRATEGY"
  phase_start
  phase_end "locality" "prepare_context_add_dirs" "skipped" "run_strategy=$RUN_STRATEGY"
fi

for scenario_file in "${SCENARIO_FILES[@]}"; do
  SCENARIO_NAME="$(scenario_name_for_file "$scenario_file")"
  CURRENT_SCENARIO="$SCENARIO_NAME"
  SCENARIO_OUT_DIR="$SCENARIO_ROOT/$SCENARIO_NAME"
  SCENARIO_AGENT_OUT_DIR="$(scenario_agent_out_dir "$SCENARIO_NAME" "$SCENARIO_OUT_DIR")"
  mkdir -p "$SCENARIO_OUT_DIR" "$SCENARIO_AGENT_OUT_DIR"
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
  SCENARIO_MANIFEST_STRATEGY="$RUN_STRATEGY"
  if [ "$RUN_STRATEGY" = "all" ] && [ "$RUN_MCP_AGENT" -eq 0 ]; then
    SCENARIO_MANIFEST_STRATEGY="locality"
  fi

  if [ "$COMPARE_HOOKS" -eq 1 ]; then
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$SCENARIO_NAME" "$SCENARIO_MANIFEST_STRATEGY" "hook-study" "mixed" "$LOCALITY_PROMPT_FILE" "$MCP_PROMPT_FILE" "$SCENARIO_OUT_DIR" "$SCENARIO_AGENT_OUT_DIR" "$SCENARIO_REPORT_TITLE" "$REPORT_PAGE_PATH" >> "$SCENARIO_MANIFEST"
  else
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$SCENARIO_NAME" "$SCENARIO_MANIFEST_STRATEGY" "default" "$CODEX_HOOKS_MODE" "$LOCALITY_PROMPT_FILE" "$MCP_PROMPT_FILE" "$SCENARIO_OUT_DIR" "$SCENARIO_AGENT_OUT_DIR" "$SCENARIO_REPORT_TITLE" "$REPORT_PAGE_PATH" >> "$SCENARIO_MANIFEST"
  fi

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

  if [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
    if [ "$COMPARE_HOOKS" -eq 1 ]; then
      if run_codex_variant_with_metric "locality" "locality" "no-hooks" "no-hooks" "$OUT_DIR/variants/locality-no-hooks" "$SCENARIO_AGENT_OUT_DIR" "$LOCALITY_PROMPT_FILE" "report-body.md" "locality-agent-final.md" "${locality_add_dirs[@]}"; then
        :
      else
        agent_rc=$?
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$agent_rc"
      fi
      if run_codex_variant_with_metric "locality" "locality" "hooks" "hooks" "$OUT_DIR/variants/locality-hooks" "$SCENARIO_AGENT_OUT_DIR" "$LOCALITY_PROMPT_FILE" "report-body.md" "locality-agent-final.md" "${locality_add_dirs[@]}"; then
        :
      else
        agent_rc=$?
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$agent_rc"
      fi
    else
      if run_codex_variant_with_metric "locality" "locality" "$CODEX_HOOKS_MODE" "default" "$OUT_DIR" "$SCENARIO_AGENT_OUT_DIR" "$LOCALITY_PROMPT_FILE" "report-body.md" "locality-agent-final.md" "${locality_add_dirs[@]}"; then
        :
      else
        agent_rc=$?
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$agent_rc"
      fi
    fi
  else
    phase_start
    phase_end "locality" "codex_exec_wall_time" "skipped" "run_strategy=$RUN_STRATEGY"
  fi

  if [ "$WRITE_MOUNTED_PAGE" = "1" ] && [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
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
  elif [ "$RUN_LOCALITY_AGENT" -eq 1 ]; then
    phase_start
    phase_end "locality" "write_mounted_page" "skipped" "artifact-only; report=$OUT_DIR/report-body.md"
    phase_start
    phase_end "locality" "loc_diff" "skipped" "artifact-only; pass --write-mounted-page to diff mounted page"
    phase_start
    phase_end "locality" "loc_push" "skipped" "artifact-only; pass --push to publish"
  else
    phase_start
    phase_end "locality" "write_mounted_page" "skipped" "run_strategy=$RUN_STRATEGY"
    phase_start
    phase_end "locality" "loc_diff" "skipped" "run_strategy=$RUN_STRATEGY"
    phase_start
    phase_end "locality" "loc_push" "skipped" "run_strategy=$RUN_STRATEGY"
  fi

  if [ "$RUN_MCP_AGENT" -eq 1 ]; then
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

    if [ "$COMPARE_HOOKS" -eq 1 ]; then
      if run_codex_variant_with_metric "notion-mcp" "notion_mcp" "no-hooks" "no-hooks" "$OUT_DIR/variants/notion-mcp-no-hooks" "$SCENARIO_AGENT_OUT_DIR" "$MCP_PROMPT_FILE" "notion-mcp-report-body.md" "notion-mcp-agent-final.md"; then
        :
      else
        mcp_rc=$?
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$mcp_rc"
      fi
      if run_codex_variant_with_metric "notion-mcp" "notion_mcp" "hooks" "hooks" "$OUT_DIR/variants/notion-mcp-hooks" "$SCENARIO_AGENT_OUT_DIR" "$MCP_PROMPT_FILE" "notion-mcp-report-body.md" "notion-mcp-agent-final.md"; then
        :
      else
        mcp_rc=$?
        OUT_DIR="$RUN_OUT_DIR"
        CONTEXT_PATHS_FILE="$RUN_CONTEXT_PATHS_FILE"
        CONTEXT_INVENTORY="$RUN_CONTEXT_INVENTORY"
        CONTEXT_SEARCH_RESULTS="$RUN_CONTEXT_SEARCH_RESULTS"
        export OUT_DIR CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS
        exit "$mcp_rc"
      fi
    else
      run_codex_variant_with_metric "notion-mcp" "notion_mcp" "$CODEX_HOOKS_MODE" "default" "$OUT_DIR" "$SCENARIO_AGENT_OUT_DIR" "$MCP_PROMPT_FILE" "notion-mcp-report-body.md" "notion-mcp-agent-final.md" || true
    fi
  fi

  if [ "$COMPARE_HOOKS" -eq 1 ]; then
    phase_start
    generate_hook_comparison_profiles "$OUT_DIR" "$SCENARIO_NAME"
    phase_end "comparison" "hook_comparison_profile" "ok" "report=$OUT_DIR/hooks-comparison.md"
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
    profile_artifacts = {}
    for name in ("locality", "notion-mcp"):
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
    variant_summaries = {}
    variants_root = scenario_out / "variants"
    if variants_root.exists():
        for variant_dir in sorted(p for p in variants_root.iterdir() if p.is_dir()):
            variant_agent_summaries = {}
            for name in ("locality", "notion-mcp"):
                p = variant_dir / f"{name}-codex-summary.json"
                if p.exists():
                    variant_agent_summaries[name] = json.loads(p.read_text())
            if variant_agent_summaries:
                variant_summaries[variant_dir.name] = variant_agent_summaries
    scenario_summaries[scenario["scenario"]] = {
        "out_dir": str(scenario_out),
        "strategy": scenario.get("strategy", ""),
        "variant": scenario.get("variant", ""),
        "hooks": scenario.get("hooks", ""),
        "agent_out_dir": scenario.get("agent_out_dir", ""),
        "report_title": scenario["report_title"],
        "page_path": scenario["report_page_path"],
        "locality_prompt": scenario["locality_prompt"],
        "mcp_prompt": scenario["mcp_prompt"],
        "agent_event_summaries": agent_summaries,
        "variant_agent_event_summaries": variant_summaries,
        "profile_artifacts": profile_artifacts,
        "hook_comparison_report": str(scenario_out / "hooks-comparison.md")
        if (scenario_out / "hooks-comparison.md").exists()
        else None,
    }

locality_trace_summaries = {}
for p in sorted((out / "locality-traces").glob("*-summary.json")):
    locality_trace_summaries[str(p.relative_to(out))] = json.loads(p.read_text())
for p in sorted((out / "scenarios").glob("*/*-agent-locality-trace-summary.json")):
    locality_trace_summaries[str(p.relative_to(out))] = json.loads(p.read_text())
for p in sorted((out / "scenarios").glob("*/variants/*/*-agent-locality-trace-summary.json")):
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

python3 "$SCRIPT_DIR/scripts/token-usage-charts.py" "$OUT_DIR" "$OUT_DIR/token-usage" >/dev/null
echo "Token usage charts: $OUT_DIR/token-usage"
