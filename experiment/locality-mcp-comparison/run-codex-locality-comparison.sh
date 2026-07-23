#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-codex-locality-comparison.sh [--out-dir <path>] [--remote-run-root <path>]

Runs two Codex evaluations in Amika sandboxes, downloads their JSONL event
streams, profiles the conversations, and summarizes how each agent spent time.

Defaults:
  local artifacts:  target/codex-locality-comparison/<UTC_RUN_ID>/
  transcript capture: streamed back over amika sandbox ssh
  Codex model: gpt-5.6-luna
  Codex reasoning effort: low
  remote cwd:
    test-with-notion-connector: ~
    onyx-falcon: ~/Locality

Environment:
  CODEX_BIN                 Codex executable in the sandbox. Default: codex
  CODEX_MODEL               Model passed to codex exec. Default: gpt-5.6-luna
  CODEX_REASONING_EFFORT    Codex reasoning effort. Default: low
  CODEX_EXEC_TIMEOUT_SECONDS
                            Per-agent codex exec timeout. Default: 900.
                            Use 0 to disable.
  AZURE_OPENAI_API_KEY      Optional Azure OpenAI key copied to each sandbox.
                            If absent, the sandbox's existing Codex auth/config
                            or ~/.config/locality-experiment/env is used.
  AZURE_OPENAI_BASE_URL     Azure OpenAI base URL used when configuring Codex.
  LINEAR_API_KEY            Linear personal API key for test-with-notion-connector.
  NOTION_API_TOKEN          Notion internal/PAT token for test-with-notion-connector.
                            NOTION_TOKEN or NOTION_ACCESS_TOKEN are accepted aliases.
  LOCALITY_DEB_URL          Locality Linux .deb used for onyx-falcon setup.
                            Default: latest codeflash-ai/locality release asset
  AMIKA_SANDBOX_FLAGS       Optional flags passed to amika sandbox commands,
                            for example: --remote
  --remote-run-root is accepted for compatibility and is not used by the
  current raw-SSH streaming capture path.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target/codex-locality-comparison/$RUN_ID}"
REMOTE_RUN_ROOT="${REMOTE_RUN_ROOT:-~/locality-codex-runs}"
CODEX_BIN="${CODEX_BIN:-codex}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-luna}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
AZURE_OPENAI_API_KEY="${AZURE_OPENAI_API_KEY:-}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
LINEAR_API_KEY="${LINEAR_API_KEY:-}"
NOTION_API_TOKEN="${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}"
LOCALITY_DEB_URL="${LOCALITY_DEB_URL:-https://github.com/codeflash-ai/locality/releases/latest/download/Locality_Linux.deb}"

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
  "i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash , read the linear issues for that time range and read the notion doc named 'Company' and create a notion doc in the page title \`Locality Launch\` followed by the current date and time in the title to distinguish it which summarizes your findings."
  "i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash , read the linear issues for that time range and read the notion doc named 'Company' and create a notion doc in the page title \`Locality Launch\` followed by the current date and time in the title to distinguish it which summarizes your findings. use \`loc\` , \`git\` and \`gh\` to fulfil the tasks, do not rely on notion or linear mcp or api calls."
)

shell_quote() {
  printf "%q" "$1"
}

base64_one_line() {
  base64 | tr -d '\n'
}

amika_sandbox_ssh() {
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    amika sandbox ssh "${AMIKA_FLAGS[@]}" "$@"
  else
    amika sandbox ssh "$@"
  fi
}

prepare_codex_runtime() {
  local machine="$1"
  local local_dir="$2"
  local setup_out="$local_dir/$machine.codex-setup.out"
  local setup_err="$local_dir/$machine.codex-setup.err"
  local remote_script
  local remote_script_b64
  local remote_command
  local remote_shell_command
  local azure_key_b64

  echo "Preparing Codex on $machine..."
  remote_script="$(cat <<'REMOTE_CODEX_SETUP'
set -euo pipefail

codex_bin="$1"
model="$2"
effort="$3"
azure_base_url="$4"
azure_key_b64="$5"

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

secret_dir="$HOME/.config/locality-codex-comparison"
mkdir -p "$secret_dir" "$HOME/.codex"
chmod 700 "$secret_dir" "$HOME/.codex"

env_file="${LOCALITY_EXPERIMENT_ENV:-$HOME/.config/locality-experiment/env}"
if [ -f "$env_file" ]; then
  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  set +a
fi

if [ -n "$azure_key_b64" ]; then
  umask 077
  printf '%s' "$azure_key_b64" | base64 -d > "$secret_dir/azure-openai-api-key"
fi

if [ -f "$secret_dir/azure-openai-api-key" ]; then
  export AZURE_OPENAI_API_KEY="$(cat "$secret_dir/azure-openai-api-key")"
fi

if [ -n "${AZURE_OPENAI_API_KEY:-}" ]; then
  cat > "$HOME/.codex/config.toml" <<TOML
model = "$model"
model_provider = "azure"
model_reasoning_effort = "$effort"

[model_providers.azure]
name = "Azure OpenAI"
base_url = "$azure_base_url"
env_key = "AZURE_OPENAI_API_KEY"
wire_api = "responses"
TOML
  chmod 600 "$HOME/.codex/config.toml"
elif [ ! -f "$HOME/.codex/config.toml" ]; then
  cat > "$HOME/.codex/config.toml" <<TOML
model = "$model"
model_reasoning_effort = "$effort"
TOML
  chmod 600 "$HOME/.codex/config.toml"
fi

command -v "$codex_bin" >/dev/null
"$codex_bin" --version || true
REMOTE_CODEX_SETUP
)"
  remote_script_b64="$(printf '%s' "$remote_script" | base64_one_line)"
  azure_key_b64=""
  if [ -n "$AZURE_OPENAI_API_KEY" ]; then
    azure_key_b64="$(printf '%s' "$AZURE_OPENAI_API_KEY" | base64_one_line)"
  fi
  remote_command="printf %s $(shell_quote "$remote_script_b64") | base64 -d | bash -s -- $(shell_quote "$CODEX_BIN") $(shell_quote "$CODEX_MODEL") $(shell_quote "$CODEX_REASONING_EFFORT") $(shell_quote "$AZURE_OPENAI_BASE_URL") $(shell_quote "$azure_key_b64")"
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  amika_sandbox_ssh "$machine" -- "$remote_shell_command" > "$setup_out" 2> "$setup_err"
}

prepare_test_with_connector_mcp() {
  local machine="$1"
  local local_dir="$2"
  local setup_out="$local_dir/$machine.mcp-setup.out"
  local setup_err="$local_dir/$machine.mcp-setup.err"
  local remote_script
  local remote_script_b64
  local remote_command
  local remote_shell_command
  local linear_key_b64
  local notion_token_b64

  if [ -z "$LINEAR_API_KEY" ]; then
    echo "LINEAR_API_KEY is required to configure Linear MCP auth for $machine" >&2
    return 2
  fi
  if [ -z "$NOTION_API_TOKEN" ]; then
    echo "NOTION_API_TOKEN is required to configure token-backed Notion MCP auth for $machine" >&2
    return 2
  fi

  echo "Preparing Codex MCP auth on $machine..."
  remote_script="$(cat <<'REMOTE_MCP_SETUP'
set -euo pipefail

codex_bin="$1"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

secret_dir="$HOME/.config/locality-codex-comparison"
bin_dir="$HOME/.local/bin"
mkdir -p "$secret_dir" "$bin_dir" "$HOME/.codex"
chmod 700 "$secret_dir" "$HOME/.codex"

umask 077
printf '%s' "$LINEAR_API_KEY" > "$secret_dir/linear-api-key"
printf '%s' "$NOTION_API_TOKEN" > "$secret_dir/notion-token"

cat > "$bin_dir/notion-mcp-token" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

token_file="${NOTION_API_TOKEN_FILE:-$HOME/.config/locality-codex-comparison/notion-token}"
export OPENAPI_MCP_HEADERS="$(
python3 - "$token_file" <<'PY'
import json
import pathlib
import sys

token = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
print(json.dumps({"Authorization": "Bearer " + token}, separators=(",", ":")))
PY
)"
exec npx -y @notionhq/notion-mcp-server
SH

chmod 700 "$bin_dir/notion-mcp-token"

"$codex_bin" mcp remove linear-server >/dev/null 2>&1 || true
"$codex_bin" mcp remove notion >/dev/null 2>&1 || true
"$codex_bin" mcp add linear-server --url https://mcp.linear.app/mcp --bearer-token-env-var LINEAR_API_KEY
"$codex_bin" mcp add notion -- "$bin_dir/notion-mcp-token"

echo "Configured token-backed Codex MCP servers: linear-server, notion"
REMOTE_MCP_SETUP
)"
  remote_script_b64="$(printf '%s' "$remote_script" | base64_one_line)"
  linear_key_b64="$(printf '%s' "$LINEAR_API_KEY" | base64_one_line)"
  notion_token_b64="$(printf '%s' "$NOTION_API_TOKEN" | base64_one_line)"
  remote_command="export LINEAR_API_KEY=\"\$(printf %s $(shell_quote "$linear_key_b64") | base64 -d)\"; export NOTION_API_TOKEN=\"\$(printf %s $(shell_quote "$notion_token_b64") | base64 -d)\"; printf %s $(shell_quote "$remote_script_b64") | base64 -d | bash -s -- $(shell_quote "$CODEX_BIN")"
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  amika_sandbox_ssh "$machine" -- "$remote_shell_command" > "$setup_out" 2> "$setup_err"
}

prepare_onyx_falcon_locality() {
  local machine="$1"
  local local_dir="$2"
  local setup_out="$local_dir/onyx-falcon.locality-setup.out"
  local setup_err="$local_dir/onyx-falcon.locality-setup.err"
  local remote_script
  local remote_script_b64
  local remote_command
  local remote_shell_command

  echo "Preparing Locality on onyx-falcon..."
  remote_script="$(cat <<'REMOTE_LOCALITY_SETUP'
set -euo pipefail

deb_url="$1"
tmp_deb="$(mktemp "${TMPDIR:-/tmp}/locality.XXXXXX.deb")"
trap 'rm -f "$tmp_deb"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fsSL -o "$tmp_deb" "$deb_url"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp_deb" "$deb_url"
else
  echo "curl or wget is required to download Locality_Linux.deb" >&2
  exit 127
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "apt-get is required to install Locality_Linux.deb" >&2
  exit 127
fi

if command -v sudo >/dev/null 2>&1; then
  sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y "$tmp_deb"
else
  env DEBIAN_FRONTEND=noninteractive apt-get install -y "$tmp_deb"
fi

mkdir -p "$HOME/Locality/notion" "$HOME/Locality/linear"

if ! loc status "$HOME/Locality/notion" --json >/dev/null 2>&1; then
  loc mount notion --workspace "$HOME/Locality/notion" --connection notion-default ||
    echo "warning: could not register Notion mount before pull" >&2
fi
if ! loc status "$HOME/Locality/linear" --json >/dev/null 2>&1; then
  loc mount linear "$HOME/Locality/linear" --connection linear-default ||
    echo "warning: could not register Linear mount before pull" >&2
fi

loc pull "$HOME/Locality/notion" ||
  echo "warning: loc pull failed for $HOME/Locality/notion" >&2
loc pull "$HOME/Locality/linear" ||
  echo "warning: loc pull failed for $HOME/Locality/linear" >&2
REMOTE_LOCALITY_SETUP
)"
  remote_script_b64="$(printf '%s' "$remote_script" | base64_one_line)"
  remote_command="printf %s $(shell_quote "$remote_script_b64") | base64 -d | bash -s -- $(shell_quote "$LOCALITY_DEB_URL")"
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  amika_sandbox_ssh "$machine" -- "$remote_shell_command" > "$setup_out" 2> "$setup_err"
}

codex_transcript_completed() {
  local transcript_file="$1"
  python3 - "$transcript_file" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.exists():
    raise SystemExit(1)

for line in path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        continue
    if event.get("type") == "turn.completed":
        raise SystemExit(0)
raise SystemExit(1)
PY
}

run_remote_codex() {
  local machine="$1"
  local remote_cwd="$2"
  local prompt="$3"
  local local_dir="$OUT_DIR/$machine"
  local connect_out="$local_dir/$machine.connect.out"
  local connect_err="$local_dir/$machine.connect.err"
  local status_file="$local_dir/$machine.status"
  local metadata_file="$local_dir/$machine.metadata.json"
  local stderr_file="$local_dir/$machine.codex.stderr"
  local transcript_file="$local_dir/$machine.codex.jsonl"
  local normalizer_file="$local_dir/timestamp-codex-jsonl.py"
  local prompt_file="$local_dir/$machine.prompt.txt"
  local command_file="$local_dir/$machine.codex-command.txt"
  local remote_home
  local remote_home_output
  local remote_cwd_resolved
  local remote_script
  local remote_script_b64
  local remote_command
  local remote_shell_command
  local prompt_b64
  local started_at
  local ended_at
  local connect_rc
  local codex_rc
  local normalized_codex_rc
  local normalizer_rc
  local transcript_completed

  echo "Running Codex in $machine..."
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

  set +e
  amika_sandbox_ssh "$machine" -- true > "$connect_out" 2> "$connect_err"
  connect_rc=$?
  set -e

  if ! remote_home_output="$(amika_sandbox_ssh "$machine" -- /usr/bin/printenv HOME)"; then
    return 1
  fi
  remote_home="$(printf '%s\n' "$remote_home_output" | tr -d '\r' | sed -n '/^\//{p;q;}')"
  if [ -z "$remote_home" ]; then
    echo "could not resolve HOME for $machine" >&2
    return 1
  fi
  case "$remote_cwd" in
    "~") remote_cwd_resolved="$remote_home" ;;
    "~/"*) remote_cwd_resolved="$remote_home/${remote_cwd#\~/}" ;;
    "") remote_cwd_resolved="$remote_home" ;;
    *) remote_cwd_resolved="$remote_cwd" ;;
  esac

  if ! prepare_codex_runtime "$machine" "$local_dir"; then
    return 1
  fi

  if [ "$machine" = "test-with-notion-connector" ]; then
    if ! prepare_test_with_connector_mcp "$machine" "$local_dir"; then
      return 1
    fi
  fi

  if [ "$machine" = "onyx-falcon" ]; then
    if ! prepare_onyx_falcon_locality "$machine" "$local_dir"; then
      return 1
    fi
  fi

  {
    printf 'timeout_seconds=%s\n' "$CODEX_EXEC_TIMEOUT_SECONDS"
    printf 'codex_model=%s\n' "$CODEX_MODEL"
    printf 'codex_reasoning_effort=%s\n' "$CODEX_REASONING_EFFORT"
    printf 'codex_command='
    printf '%q ' "$CODEX_BIN" exec --json --model "$CODEX_MODEL" -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\"" --dangerously-bypass-approvals-and-sandbox -C "$remote_cwd_resolved" "$prompt"
    printf '\n'
  } > "$command_file"

  remote_script="$(cat <<'REMOTE_CODEX_RUN'
set -euo pipefail

codex_bin="$1"
model="$2"
effort="$3"
timeout_seconds="$4"
remote_cwd="$5"
prompt_b64="$6"

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

env_file="${LOCALITY_EXPERIMENT_ENV:-$HOME/.config/locality-experiment/env}"
if [ -f "$env_file" ]; then
  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  set +a
fi

secret_dir="$HOME/.config/locality-codex-comparison"
if [ -f "$secret_dir/azure-openai-api-key" ]; then
  export AZURE_OPENAI_API_KEY="$(cat "$secret_dir/azure-openai-api-key")"
fi
if [ -f "$secret_dir/linear-api-key" ]; then
  export LINEAR_API_KEY="$(cat "$secret_dir/linear-api-key")"
fi

prompt_file="$(mktemp "${TMPDIR:-/tmp}/codex-prompt.XXXXXX")"
trap 'rm -f "$prompt_file"' EXIT
printf '%s' "$prompt_b64" | base64 -d > "$prompt_file"
prompt="$(cat "$prompt_file")"

cmd=(
  "$codex_bin" exec
  --json
  --model "$model"
  -c "model_reasoning_effort=\"$effort\""
  --dangerously-bypass-approvals-and-sandbox
  -C "$remote_cwd"
  "$prompt"
)

if [ "$timeout_seconds" = "0" ]; then
  exec "${cmd[@]}"
elif command -v timeout >/dev/null 2>&1; then
  exec timeout --kill-after=30s "${timeout_seconds}s" "${cmd[@]}"
else
  exec "${cmd[@]}"
fi
REMOTE_CODEX_RUN
)"
  remote_script_b64="$(printf '%s' "$remote_script" | base64_one_line)"
  prompt_b64="$(printf '%s' "$prompt" | base64_one_line)"
  remote_command="printf %s $(shell_quote "$remote_script_b64") | base64 -d | bash -s -- $(shell_quote "$CODEX_BIN") $(shell_quote "$CODEX_MODEL") $(shell_quote "$CODEX_REASONING_EFFORT") $(shell_quote "$CODEX_EXEC_TIMEOUT_SECONDS") $(shell_quote "$remote_cwd_resolved") $(shell_quote "$prompt_b64")"

  set +e
  set -o pipefail
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  amika_sandbox_ssh "$machine" -- "$remote_shell_command" 2> "$stderr_file" | python3 "$normalizer_file" > "$transcript_file"
  local pipe_status=("${PIPESTATUS[@]}")
  codex_rc="${pipe_status[0]}"
  normalizer_rc="${pipe_status[1]}"
  set +o pipefail
  set -e

  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  transcript_completed=0
  if codex_transcript_completed "$transcript_file"; then
    transcript_completed=1
  fi
  normalized_codex_rc="$codex_rc"
  if [ "$codex_rc" -ne 0 ] && [ "$transcript_completed" -eq 1 ]; then
    normalized_codex_rc=0
  fi
  printf '%s\n' "$normalized_codex_rc" > "$status_file"
  python3 - "$metadata_file" "$machine" "$RUN_ID" "$remote_cwd_resolved" "$started_at" "$ended_at" "$connect_rc" "$codex_rc" "$normalized_codex_rc" "$normalizer_rc" "$transcript_completed" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" "$CODEX_EXEC_TIMEOUT_SECONDS" <<'PY'
import json
import pathlib
import sys

(
    path,
    machine,
    run_id,
    cwd,
    started_at,
    ended_at,
    connect_rc,
    codex_rc,
    normalized_codex_rc,
    normalizer_rc,
    transcript_completed,
    model,
    effort,
    timeout_seconds,
) = sys.argv[1:15]
pathlib.Path(path).write_text(json.dumps({
    "machine": machine,
    "label": machine,
    "run_id": run_id,
    "cwd": cwd,
    "capture": "local_stream_via_amika_sandbox_ssh",
    "started_at": started_at,
    "ended_at": ended_at,
    "connect_exit_code": int(connect_rc),
    "codex_exit_code": int(codex_rc),
    "normalized_codex_exit_code": int(normalized_codex_rc),
    "normalizer_exit_code": int(normalizer_rc),
    "transcript_completed": transcript_completed == "1",
    "model": model,
    "reasoning_effort": effort,
    "timeout_seconds": int(timeout_seconds),
}, indent=2) + "\n")
PY

  return "$normalized_codex_rc"
}

run_profiler() {
  local left="$OUT_DIR/test-with-notion-connector/test-with-notion-connector.codex.jsonl"
  local right="$OUT_DIR/onyx-falcon/onyx-falcon.codex.jsonl"

  if [ ! -s "$left" ]; then
    echo "missing transcript: $left" >&2
    return 1
  fi
  if [ ! -s "$right" ]; then
    echo "missing transcript: $right" >&2
    return 1
  fi

  node "$REPO_ROOT/experiment/agent-conversation-profile-modern-codex.mjs" \
    --left "$left" \
    --left-label "test-with-notion-connector" \
    --right "$right" \
    --right-label "onyx-falcon" \
    --out "$OUT_DIR/profile"
}

generate_time_summary() {
  local summary_json="$OUT_DIR/profile/summary.json"
  local summary_md="$OUT_DIR/time-summary.md"

  node --input-type=module - "$summary_json" "$summary_md" "$OUT_DIR" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" <<'JS'
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [summaryPath, outPath, outDir, model, effort] = process.argv.slice(2);
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
  "# Codex Locality Comparison Time Summary",
  "",
  `Run artifacts: \`${outDir}\``,
  `Model: \`${model}\``,
  `Reasoning effort: \`${effort}\``,
  "",
  "## Run Status",
  "",
  "| Machine | Codex exit code | Transcript | Stderr |",
  "| --- | ---: | --- | --- |",
];

for (const conversation of summary.conversations) {
  const label = conversation.label;
  lines.push(
    `| ${tableCell(label)} | ${tableCell(readStatus(label))} | \`${label}/${label}.codex.jsonl\` | \`${label}/${label}.codex.stderr\` |`,
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
  if ! run_remote_codex "$machine" "$remote_cwd" "$prompt"; then
    failures=$((failures + 1))
  fi
done

run_profiler
generate_time_summary

echo "Wrote Codex comparison artifacts to $OUT_DIR"
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
