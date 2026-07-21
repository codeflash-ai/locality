#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-claude-locality-comparison.sh [--out-dir <path>] [--remote-run-root <path>]

Runs two Claude Code evaluations in Amika sandboxes, downloads their JSONL
transcripts, profiles the conversations, and summarizes how each agent spent
time.

Defaults:
  local artifacts:  target/claude-locality-comparison/<UTC_RUN_ID>/
  transcript capture: streamed back over amika sandbox ssh
  remote cwd:
    test-with-notion-connector: ~
    onyx-falcon: ~/Locality

Environment:
  CLAUDE_BIN           Claude executable in the sandbox. Default: claude
  CLAUDE_MODEL         Optional model passed as --model
  AMIKA_SANDBOX_FLAGS  Optional flags passed to amika sandbox commands,
                       for example: --remote
  --remote-run-root is accepted for compatibility and is not used by the
  current raw-SSH streaming capture path.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target/claude-locality-comparison/$RUN_ID}"
REMOTE_RUN_ROOT="${REMOTE_RUN_ROOT:-~/locality-claude-runs}"
CLAUDE_BIN="${CLAUDE_BIN:-claude}"
CLAUDE_MODEL="${CLAUDE_MODEL:-}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --out-dir)
      if [ "$#" -lt 2 ]; then
        echo "--out-dir requires a value" >&2
        exit 2
      fi
      OUT_DIR="$2"
      shift 2
      ;;
    --remote-run-root)
      if [ "$#" -lt 2 ]; then
        echo "--remote-run-root requires a value" >&2
        exit 2
      fi
      REMOTE_RUN_ROOT="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v amika >/dev/null 2>&1; then
  echo "amika is not available on PATH" >&2
  exit 127
fi

if ! command -v node >/dev/null 2>&1; then
  echo "node is not available on PATH" >&2
  exit 127
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

declare -a AMIKA_FLAGS=()
if [ -n "${AMIKA_SANDBOX_FLAGS:-}" ]; then
  read -r -a AMIKA_FLAGS <<< "$AMIKA_SANDBOX_FLAGS"
fi

MACHINES=(
  "test-with-notion-connector"
  "onyx-falcon"
)

REMOTE_CWDS=(
  "~"
  "~/Locality"
)

PROMPTS=(
  "i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash , read the linear issues for that time range and read the notion doc named 'Company' and create a notion doc in the page title \`Locality Launch\` which summarizes your findings."
  "i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash , read the linear issues for that time range and read the notion doc named 'Company' and create a notion doc in the page title \`Locality Launch\` which summarizes your findings. use \`loc\` exclusively to fulfil the tasks, do not rely on notion or linear mcp or api calls."
)

shell_quote() {
  printf "%q" "$1"
}

base64_one_line() {
  base64 | tr -d '\n'
}

remote_child_dir() {
  local root="$1"
  local run_id="$2"
  local label="$3"
  printf '%s/%s/%s' "${root%/}" "$run_id" "$label"
}

amika_sandbox_connect() {
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    amika sandbox connect "${AMIKA_FLAGS[@]}" "$@"
  else
    amika sandbox connect "$@"
  fi
}

amika_sandbox_ssh() {
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    amika sandbox ssh "${AMIKA_FLAGS[@]}" "$@"
  else
    amika sandbox ssh "$@"
  fi
}

run_remote_claude() {
  local machine="$1"
  local remote_cwd="$2"
  local prompt="$3"
  local local_dir="$OUT_DIR/$machine"
  local connect_out="$local_dir/$machine.connect.out"
  local connect_err="$local_dir/$machine.connect.err"
  local status_file="$local_dir/$machine.status"
  local metadata_file="$local_dir/$machine.metadata.json"
  local stderr_file="$local_dir/$machine.claude.stderr"
  local transcript_file="$local_dir/$machine.claude.jsonl"
  local normalizer_file="$local_dir/timestamp-jsonl.py"
  local prompt_file="$local_dir/$machine.prompt.txt"
  local ssh_target
  local remote_home
  local remote_cwd_resolved
  local remote_prompt
  local remote_command
  local started_at
  local ended_at
  local connect_rc
  local claude_rc
  local normalizer_rc

  echo "Running Claude in $machine..."
  mkdir -p "$local_dir"
  printf '%s' "$prompt" > "$prompt_file"

  cat > "$normalizer_file" <<'PY'
#!/usr/bin/env python3
import datetime
import json
import sys

def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")

for raw in sys.stdin:
    line = raw.rstrip("\n")
    if not line:
        continue
    observed_at = now_iso()
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        event = {"type": "unparsed", "raw": line}
    if not isinstance(event, dict):
        event = {"type": "non_object", "value": event}
    event.setdefault("timestamp", observed_at)
    event.setdefault("created_at", observed_at)
    print(json.dumps(event, separators=(",", ":")), flush=True)
PY
  chmod +x "$normalizer_file"

  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  # Keep the requested "connect first" step lightweight; the actual
  # non-interactive Claude run is streamed through raw ssh because Amika's
  # command wrapper does not preserve Claude's stream-json invocation reliably.
  set +e
  amika_sandbox_connect "$machine" --shell bash > "$connect_out" 2> "$connect_err" <<'REMOTE_CONNECT_CHECK'
exit 0
REMOTE_CONNECT_CHECK
  connect_rc=$?
  set -e

  ssh_target="$(amika_sandbox_ssh --print "$machine")"
  remote_home="$(ssh -o StrictHostKeyChecking=accept-new "$ssh_target" /usr/bin/printenv HOME)"
  case "$remote_cwd" in
    "~") remote_cwd_resolved="$remote_home" ;;
    "~/"*) remote_cwd_resolved="$remote_home/${remote_cwd#\~/}" ;;
    "") remote_cwd_resolved="$remote_home" ;;
    *) remote_cwd_resolved="$remote_cwd" ;;
  esac

  remote_command="cd $(shell_quote "$remote_cwd_resolved") && $(shell_quote "$CLAUDE_BIN") -p --output-format stream-json --verbose --dangerously-skip-permissions"
  if [ -n "$CLAUDE_MODEL" ]; then
    remote_command+=" --model $(shell_quote "$CLAUDE_MODEL")"
  fi
  remote_prompt="$(shell_quote "$prompt")"
  remote_command+=" $remote_prompt"

  set +e
  set -o pipefail
  ssh -tt -o StrictHostKeyChecking=accept-new "$ssh_target" "$remote_command" 2> "$stderr_file" | python3 "$normalizer_file" > "$transcript_file"
  local pipe_status=("${PIPESTATUS[@]}")
  claude_rc="${pipe_status[0]}"
  normalizer_rc="${pipe_status[1]}"
  set +o pipefail
  set -e

  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '%s\n' "$claude_rc" > "$status_file"
  python3 - "$metadata_file" "$machine" "$RUN_ID" "$remote_cwd_resolved" "$started_at" "$ended_at" "$connect_rc" "$claude_rc" "$normalizer_rc" <<'PY'
import json
import pathlib
import sys

path, machine, run_id, cwd, started_at, ended_at, connect_rc, claude_rc, normalizer_rc = sys.argv[1:10]
pathlib.Path(path).write_text(json.dumps({
    "machine": machine,
    "label": machine,
    "run_id": run_id,
    "cwd": cwd,
    "capture": "local_stream_via_amika_sandbox_ssh",
    "started_at": started_at,
    "ended_at": ended_at,
    "connect_exit_code": int(connect_rc),
    "claude_exit_code": int(claude_rc),
    "normalizer_exit_code": int(normalizer_rc),
}, indent=2) + "\n")
PY

  return "$claude_rc"
}

run_profiler() {
  local left="$OUT_DIR/test-with-notion-connector/test-with-notion-connector.claude.jsonl"
  local right="$OUT_DIR/onyx-falcon/onyx-falcon.claude.jsonl"

  if [ ! -s "$left" ]; then
    echo "missing transcript: $left" >&2
    return 1
  fi
  if [ ! -s "$right" ]; then
    echo "missing transcript: $right" >&2
    return 1
  fi

  node "$REPO_ROOT/experiment/agent-conversation-profile.mjs" \
    --left "$left" \
    --left-label "test-with-notion-connector" \
    --right "$right" \
    --right-label "onyx-falcon" \
    --out "$OUT_DIR/profile"
}

generate_time_summary() {
  local summary_json="$OUT_DIR/profile/summary.json"
  local summary_md="$OUT_DIR/time-summary.md"

  node --input-type=module - "$summary_json" "$summary_md" "$OUT_DIR" <<'JS'
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [summaryPath, outPath, outDir] = process.argv.slice(2);
const summary = JSON.parse(readFileSync(summaryPath, "utf8"));

function formatMs(ms) {
  if (!Number.isFinite(ms)) return "n/a";
  if (ms < 1000) return `${ms}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  const remaining = seconds - minutes * 60;
  return `${minutes}m ${remaining.toFixed(1)}s`;
}

function tableCell(value) {
  return String(value ?? "").replace(/\|/g, "\\|").replace(/\n/g, " ");
}

function readStatus(label) {
  const path = join(outDir, label, `${label}.status`);
  if (!existsSync(path)) return "missing";
  return readFileSync(path, "utf8").trim() || "missing";
}

const lines = [
  "# Claude Locality Comparison Time Summary",
  "",
  `Run artifacts: \`${outDir}\``,
  "",
  "## Run Status",
  "",
  "| Machine | Claude exit code | Transcript | Stderr |",
  "| --- | ---: | --- | --- |",
];

for (const conversation of summary.conversations) {
  const label = conversation.label;
  lines.push(
    `| ${tableCell(label)} | ${tableCell(readStatus(label))} | \`${label}/${label}.claude.jsonl\` | \`${label}/${label}.claude.stderr\` |`,
  );
}

lines.push("", "## Wall Time", "");
lines.push("| Machine | Events | Wall time | Measured | Inferred |");
lines.push("| --- | ---: | ---: | ---: | ---: |");
for (const conversation of summary.conversations) {
  lines.push(
    `| ${tableCell(conversation.label)} | ${conversation.event_count} | ${formatMs(conversation.wall_time_ms)} | ${formatMs(conversation.measured_duration_ms)} | ${formatMs(conversation.inferred_duration_ms)} |`,
  );
}

lines.push("", "## Activity Mix", "");
for (const conversation of summary.conversations) {
  lines.push(`### ${conversation.label}`, "");
  lines.push("| Activity | Duration | Percent of wall time |");
  lines.push("| --- | ---: | ---: |");
  for (const [activity, duration] of Object.entries(conversation.totals_by_activity)) {
    const percent = conversation.percent_by_activity[activity] ?? 0;
    lines.push(`| ${tableCell(activity)} | ${formatMs(duration)} | ${percent}% |`);
  }
  if (Object.keys(conversation.totals_by_activity).length === 0) {
    lines.push("| none | 0ms | 0% |");
  }
  lines.push("");
}

lines.push("## Top Tool Groups", "");
for (const conversation of summary.conversations) {
  lines.push(`### ${conversation.label}`, "");
  if (conversation.tool_groups.length === 0) {
    lines.push("No tool calls.", "");
    continue;
  }
  lines.push("| Tool group | Count | Duration |");
  lines.push("| --- | ---: | ---: |");
  for (const tool of conversation.tool_groups.slice(0, 8)) {
    lines.push(`| ${tableCell(tool.tool_group)} | ${tool.count} | ${formatMs(tool.duration_ms)} |`);
  }
  lines.push("");
}

lines.push("## Longest Profile Entries", "");
for (const conversation of summary.conversations) {
  lines.push(`### ${conversation.label}`, "");
  if (conversation.longest_profile_entries.length === 0) {
    lines.push("No profile entries.", "");
    continue;
  }
  lines.push("| Activity | Kind | Tool group | Duration | Timing | Excerpt |");
  lines.push("| --- | --- | --- | ---: | --- | --- |");
  for (const entry of conversation.longest_profile_entries) {
    lines.push(
      `| ${tableCell(entry.activity)} | ${tableCell(entry.kind)} | ${tableCell(entry.tool_group ?? "")} | ${formatMs(entry.duration_ms)} | ${tableCell(entry.timing_quality)} | ${tableCell(entry.excerpt)} |`,
    );
  }
  lines.push("");
}

lines.push("## Profiler Warnings", "");
if (summary.warnings.length === 0) {
  lines.push("No profiler warnings.", "");
} else {
  lines.push("| Machine | Source index | Code | Message |");
  lines.push("| --- | ---: | --- | --- |");
  for (const warning of summary.warnings) {
    lines.push(
      `| ${tableCell(warning.conversation_label)} | ${warning.source_index} | ${tableCell(warning.code)} | ${tableCell(warning.message)} |`,
    );
  }
  lines.push("");
}

lines.push("## Viewer Files", "");
lines.push("- Profile summary: `profile/summary.md`");
lines.push("- Combined Perfetto trace: `profile/combined.perfetto.json`");
lines.push("- Combined Speedscope profile: `profile/combined.speedscope.json`");
lines.push("- Combined SnakeViz profile: `profile/combined.snakeviz.prof`");
lines.push("- Combined folded stack: `profile/combined.folded`");
lines.push("");

writeFileSync(outPath, lines.join("\n"));
JS
}

failures=0

for index in "${!MACHINES[@]}"; do
  machine="${MACHINES[$index]}"
  remote_cwd="${REMOTE_CWDS[$index]}"
  prompt="${PROMPTS[$index]}"
  if ! run_remote_claude "$machine" "$remote_cwd" "$prompt"; then
    failures=$((failures + 1))
  fi
done

run_profiler
generate_time_summary

echo "Wrote Claude comparison artifacts to $OUT_DIR"
echo "Time summary: $OUT_DIR/time-summary.md"

for machine in "${MACHINES[@]}"; do
  status_path="$OUT_DIR/$machine/$machine.status"
  if [ ! -f "$status_path" ]; then
    failures=$((failures + 1))
    continue
  fi
  status="$(tr -d '[:space:]' < "$status_path")"
  if [ "$status" != "0" ]; then
    failures=$((failures + 1))
  fi
done

if [ "$failures" -ne 0 ]; then
  echo "completed with $failures failure(s); inspect $OUT_DIR/time-summary.md" >&2
  exit 1
fi
