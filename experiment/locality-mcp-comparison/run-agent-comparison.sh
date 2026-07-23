#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-agent-comparison.sh [--out-dir <path>] [--remote-worktree <path>] [benchmark args...]

Runs the launch-readiness benchmark on two Amika sandboxes:
  - Locality strategy on LOCALITY_SANDBOX
  - Notion MCP strategy on MCP_SANDBOX

Defaults:
  LOCALITY_SANDBOX=aseem-locality
  MCP_SANDBOX=aseem-mcp
  LOCAL_OUT_DIR=target/launch-readiness-amika/<UTC_RUN_ID>/
  REMOTE_SOURCE_REPO=/home/amika/workspace/locality
  REMOTE_WORKTREE=/home/amika/workspace/locality-launch-readiness-<UTC_RUN_ID>
  LOCALITY_REMOTE_OUT_DIR=<REMOTE_WORKTREE>/target/launch-readiness-<UTC_RUN_ID>-locality
  MCP_REMOTE_OUT_DIR=<REMOTE_WORKTREE>/target/launch-readiness-<UTC_RUN_ID>-mcp

Environment:
  RUN_ID                         Run id shared by both sandboxes.
  LOCALITY_SANDBOX               Amika sandbox for Locality runs.
  MCP_SANDBOX                    Amika sandbox for MCP runs.
  LOCAL_OUT_DIR or OUT_DIR       Local metadata/log output directory.
  REMOTE_SOURCE_REPO             Existing git checkout inside each sandbox.
  REMOTE_WORKTREE_ROOT           Parent for clean detached benchmark worktrees.
  REMOTE_WORKTREE                Exact clean detached worktree path.
  REMOTE_LOC_BIN                 loc binary inside the sandboxes.
                                  Default: <REMOTE_SOURCE_REPO>/target/debug/loc.
  BENCHMARK_REF                  Git ref checked out in each sandbox. Default: origin/main.
  AMIKA_SANDBOX_FLAGS            Optional flags passed to amika sandbox ssh.
  CODEX_MODEL                    Passed through to the benchmark worker.
  CODEX_REASONING_EFFORT         Passed through to the benchmark worker.
  CODEX_EXEC_TIMEOUT_SECONDS     Passed through to the benchmark worker.
  SYNC_ARTIFACTS                 Copy remote OUT_DIRs back locally. Default: 1.

Any remaining arguments are passed to run-launch-readiness-benchmark.sh.
Do not pass --strategy; this wrapper owns the split strategy execution.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"

LOCALITY_SANDBOX="${LOCALITY_SANDBOX:-aseem-locality}"
MCP_SANDBOX="${MCP_SANDBOX:-aseem-mcp}"
REMOTE_SOURCE_REPO="${REMOTE_SOURCE_REPO:-/home/amika/workspace/locality}"
REMOTE_WORKTREE_ROOT="${REMOTE_WORKTREE_ROOT:-/home/amika/workspace}"
REMOTE_WORKTREE="${REMOTE_WORKTREE:-$REMOTE_WORKTREE_ROOT/locality-launch-readiness-$RUN_ID}"
BENCHMARK_REF="${BENCHMARK_REF:-origin/main}"
REMOTE_LOC_BIN="${REMOTE_LOC_BIN:-$REMOTE_SOURCE_REPO/target/debug/loc}"

LOCAL_OUT_DIR="${LOCAL_OUT_DIR:-${OUT_DIR:-$REPO_ROOT/target/launch-readiness-amika/$RUN_ID}}"
LOCALITY_REMOTE_OUT_DIR_INPUT="${LOCALITY_REMOTE_OUT_DIR:-}"
MCP_REMOTE_OUT_DIR_INPUT="${MCP_REMOTE_OUT_DIR:-}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-luna}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
SYNC_ARTIFACTS="${SYNC_ARTIFACTS:-1}"

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
      LOCAL_OUT_DIR="$2"
      shift 2
      ;;
    --remote-worktree)
      if [ "$#" -lt 2 ]; then
        echo "--remote-worktree requires a value" >&2
        exit 2
      fi
      REMOTE_WORKTREE="$2"
      shift 2
      ;;
    --strategy|--strategy=*)
      echo "run-agent-comparison.sh owns --strategy; set LOCALITY_SANDBOX/MCP_SANDBOX or pass benchmark args only" >&2
      exit 2
      ;;
    --compare-hooks)
      echo "--compare-hooks is not supported by the split-sandbox wrapper; run the worker directly for hook studies" >&2
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

LOCALITY_REMOTE_OUT_DIR="${LOCALITY_REMOTE_OUT_DIR_INPUT:-$REMOTE_WORKTREE/target/launch-readiness-$RUN_ID-locality}"
MCP_REMOTE_OUT_DIR="${MCP_REMOTE_OUT_DIR_INPUT:-$REMOTE_WORKTREE/target/launch-readiness-$RUN_ID-mcp}"

if [ "$LOCALITY_SANDBOX" = "$MCP_SANDBOX" ]; then
  echo "LOCALITY_SANDBOX and MCP_SANDBOX must be different Amika sandboxes" >&2
  exit 2
fi

if ! command -v amika >/dev/null 2>&1; then
  echo "amika is not available on PATH" >&2
  exit 127
fi

mkdir -p "$LOCAL_OUT_DIR"
LOCAL_OUT_DIR="$(cd "$LOCAL_OUT_DIR" && pwd)"

declare -a AMIKA_FLAGS=()
if [ -n "${AMIKA_SANDBOX_FLAGS:-}" ]; then
  read -r -a AMIKA_FLAGS <<< "$AMIKA_SANDBOX_FLAGS"
fi

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

amika_sandbox_ssh_target() {
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    amika sandbox ssh "${AMIKA_FLAGS[@]}" --print "$1"
  else
    amika sandbox ssh --print "$1"
  fi
}

run_remote_script() {
  local sandbox="$1"
  local stdout_file="$2"
  local stderr_file="$3"
  local script="$4"
  shift 4

  local script_b64
  local remote_command
  local remote_shell_command
  local arg

  script_b64="$(printf '%s' "$script" | base64_one_line)"
  remote_command="printf %s $(shell_quote "$script_b64") | base64 -d | bash -s --"
  for arg in "$@"; do
    remote_command+=" $(shell_quote "$arg")"
  done
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  amika_sandbox_ssh "$sandbox" -- "$remote_shell_command" > "$stdout_file" 2> "$stderr_file"
}

prepare_worktree() {
  local sandbox="$1"
  local local_dir="$LOCAL_OUT_DIR/$sandbox"
  local script

  mkdir -p "$local_dir"
  echo "Preparing $BENCHMARK_REF in $sandbox:$REMOTE_WORKTREE"

  script="$(cat <<'REMOTE_PREPARE'
set -euo pipefail

source_repo="$1"
worktree="$2"
ref="$3"

if [ ! -d "$source_repo/.git" ] && [ ! -f "$source_repo/.git" ]; then
  echo "missing source repository: $source_repo" >&2
  exit 2
fi

cd "$source_repo"
git fetch origin

if [ -e "$worktree" ]; then
  if ! git -C "$worktree" rev-parse --git-dir >/dev/null 2>&1; then
    echo "remote worktree path exists but is not a git checkout: $worktree" >&2
    exit 2
  fi
  if [ -n "$(git -C "$worktree" status --porcelain)" ]; then
    echo "remote worktree is dirty: $worktree" >&2
    exit 2
  fi
  git -C "$worktree" fetch origin
  git -C "$worktree" checkout --detach "$ref"
else
  mkdir -p "$(dirname "$worktree")"
  git worktree add --detach "$worktree" "$ref"
fi

git -C "$worktree" rev-parse HEAD
REMOTE_PREPARE
)"

  run_remote_script \
    "$sandbox" \
    "$local_dir/worktree-setup.out" \
    "$local_dir/worktree-setup.err" \
    "$script" \
    "$REMOTE_SOURCE_REPO" \
    "$REMOTE_WORKTREE" \
    "$BENCHMARK_REF"
}

run_launch_strategy() {
  local sandbox="$1"
  local strategy="$2"
  local remote_out_dir="$3"
  local local_dir="$LOCAL_OUT_DIR/$sandbox"
  local script

  mkdir -p "$local_dir"
  echo "Running $strategy on $sandbox"

  script="$(cat <<'REMOTE_RUN'
set -euo pipefail

strategy="$1"
repo_dir="$2"
out_dir="$3"
run_id="$4"
model="$5"
effort="$6"
timeout_seconds="$7"
loc_bin="$8"
shift 8

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

env_file="${LOCALITY_EXPERIMENT_ENV:-$HOME/.config/locality-experiment/env}"
if [ -f "$env_file" ]; then
  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  set +a
fi

secret_dir="${LOCALITY_LAUNCH_READINESS_SECRET_DIR:-$HOME/.config/locality-launch-readiness/mcp}"
notion_token_file="${NOTION_API_TOKEN_FILE:-$secret_dir/notion-token}"
linear_key_file="${LINEAR_API_KEY_FILE:-$secret_dir/linear-api-key}"
slack_bot_token_file="${SLACK_BOT_TOKEN_FILE:-$secret_dir/slack-bot-token}"
slack_team_id_file="${SLACK_TEAM_ID_FILE:-$secret_dir/slack-team-id}"
slack_channel_ids_file="${SLACK_CHANNEL_IDS_FILE:-$secret_dir/slack-channel-ids}"

if [ -z "${AZURE_OPENAI_API_KEY:-}" ]; then
  echo "AZURE_OPENAI_API_KEY is missing in $env_file or the sandbox environment" >&2
  exit 2
fi

if [ "$strategy" = "notion-mcp" ]; then
  if [ -z "${LINEAR_API_KEY:-}" ] && [ -f "$linear_key_file" ]; then
    export LINEAR_API_KEY="$(cat "$linear_key_file")"
  fi
  if [ -z "${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}" ] && [ -f "$notion_token_file" ]; then
    export NOTION_API_TOKEN="$(cat "$notion_token_file")"
  fi
  if [ -z "${SLACK_BOT_TOKEN:-}" ] && [ -f "$slack_bot_token_file" ]; then
    export SLACK_BOT_TOKEN="$(cat "$slack_bot_token_file")"
  fi
  if [ -z "${SLACK_TEAM_ID:-}" ] && [ -f "$slack_team_id_file" ]; then
    export SLACK_TEAM_ID="$(cat "$slack_team_id_file")"
  fi
  if [ -z "${SLACK_CHANNEL_IDS:-}" ] && [ -f "$slack_channel_ids_file" ]; then
    export SLACK_CHANNEL_IDS="$(cat "$slack_channel_ids_file")"
  fi
fi

if [ "$strategy" = "locality" ]; then
  if [ ! -x "$loc_bin" ]; then
    if command -v loc >/dev/null 2>&1; then
      loc_bin="$(command -v loc)"
    else
      echo "loc binary is not executable: $loc_bin" >&2
      exit 127
    fi
  fi
  if [ -z "${LOCALITY_STATE_DIR:-}" ]; then
    if [ ! -f "$notion_token_file" ]; then
      echo "missing $notion_token_file; set LOCALITY_STATE_DIR or provide a Notion token file" >&2
      exit 2
    fi
    export LOCALITY_STATE_DIR="$out_dir/loc-state"
    export LOCALITY_CREDENTIAL_STORE="${LOCALITY_CREDENTIAL_STORE:-file}"
    mkdir -p "$LOCALITY_STATE_DIR" "$out_dir/mount/notion"
    if ! "$loc_bin" connect notion --name launch-readiness --token-stdin < "$notion_token_file" >/dev/null; then
      "$loc_bin" connections --json 2>/dev/null | grep -F -q '"launch-readiness"' ||
        { echo "could not configure launch-readiness Notion connection" >&2; exit 2; }
    fi
    if ! "$loc_bin" mount notion --workspace --connection launch-readiness --mount-id launch-readiness --read-only "$out_dir/mount/notion" >/dev/null; then
      "$loc_bin" status "$out_dir/mount/notion" --json >/dev/null 2>&1 ||
        { echo "could not configure launch-readiness Notion mount" >&2; exit 2; }
    fi
  fi
fi

export RUN_ID="$run_id"
export REPO_DIR="$repo_dir"
export OUT_DIR="$out_dir"
export CODEX_SANDBOX_OUT_DIR="$out_dir"
export CODEX_SANDBOX_HARDCODED_OUT_DIR="$out_dir"
export CODEX_MODEL="$model"
export CODEX_REASONING_EFFORT="$effort"
export CODEX_EXEC_TIMEOUT_SECONDS="$timeout_seconds"
export LOC_BIN="${LOC_BIN:-$loc_bin}"

cd "$repo_dir"
"$repo_dir/experiment/locality-mcp-comparison/run-launch-readiness-benchmark.sh" --strategy "$strategy" "$@"
REMOTE_RUN
)"

  run_remote_script \
    "$sandbox" \
    "$local_dir/$strategy.out" \
    "$local_dir/$strategy.err" \
    "$script" \
    "$strategy" \
    "$REMOTE_WORKTREE" \
    "$remote_out_dir" \
    "$RUN_ID" \
    "$CODEX_MODEL" \
    "$CODEX_REASONING_EFFORT" \
    "$CODEX_EXEC_TIMEOUT_SECONDS" \
    "$REMOTE_LOC_BIN" \
    "$@"
}

sync_artifacts() {
  local sandbox="$1"
  local strategy="$2"
  local remote_out_dir="$3"
  local dest="$LOCAL_OUT_DIR/artifacts/$strategy"
  local ssh_target

  if [ "$SYNC_ARTIFACTS" != "1" ]; then
    return 0
  fi

  mkdir -p "$dest"
  ssh_target="$(amika_sandbox_ssh_target "$sandbox")"
  echo "Syncing $strategy artifacts from $sandbox:$remote_out_dir"

  if command -v rsync >/dev/null 2>&1; then
    rsync -az --delete "$ssh_target:$remote_out_dir/" "$dest/"
  elif command -v scp >/dev/null 2>&1; then
    scp -r "$ssh_target:$remote_out_dir/." "$dest/"
  else
    echo "rsync or scp is required to sync remote artifacts" >&2
    return 127
  fi
}

{
  printf 'run_id=%s\n' "$RUN_ID"
  printf 'locality_sandbox=%s\n' "$LOCALITY_SANDBOX"
  printf 'mcp_sandbox=%s\n' "$MCP_SANDBOX"
  printf 'remote_source_repo=%s\n' "$REMOTE_SOURCE_REPO"
  printf 'remote_worktree=%s\n' "$REMOTE_WORKTREE"
  printf 'remote_loc_bin=%s\n' "$REMOTE_LOC_BIN"
  printf 'benchmark_ref=%s\n' "$BENCHMARK_REF"
  printf 'locality_remote_out_dir=%s\n' "$LOCALITY_REMOTE_OUT_DIR"
  printf 'mcp_remote_out_dir=%s\n' "$MCP_REMOTE_OUT_DIR"
  printf 'codex_model=%s\n' "$CODEX_MODEL"
  printf 'codex_reasoning_effort=%s\n' "$CODEX_REASONING_EFFORT"
  printf 'codex_exec_timeout_seconds=%s\n' "$CODEX_EXEC_TIMEOUT_SECONDS"
  printf 'sync_artifacts=%s\n' "$SYNC_ARTIFACTS"
} > "$LOCAL_OUT_DIR/run.env"

prepare_worktree "$LOCALITY_SANDBOX"
prepare_worktree "$MCP_SANDBOX"
run_launch_strategy "$LOCALITY_SANDBOX" "locality" "$LOCALITY_REMOTE_OUT_DIR" "$@"
run_launch_strategy "$MCP_SANDBOX" "notion-mcp" "$MCP_REMOTE_OUT_DIR" "$@"
sync_artifacts "$LOCALITY_SANDBOX" "locality" "$LOCALITY_REMOTE_OUT_DIR"
sync_artifacts "$MCP_SANDBOX" "notion-mcp" "$MCP_REMOTE_OUT_DIR"

cat > "$LOCAL_OUT_DIR/artifacts.tsv" <<EOF
strategy	sandbox	remote_out_dir	local_stdout	local_stderr	local_artifact_dir
locality	$LOCALITY_SANDBOX	$LOCALITY_REMOTE_OUT_DIR	$LOCAL_OUT_DIR/$LOCALITY_SANDBOX/locality.out	$LOCAL_OUT_DIR/$LOCALITY_SANDBOX/locality.err	$LOCAL_OUT_DIR/artifacts/locality
notion-mcp	$MCP_SANDBOX	$MCP_REMOTE_OUT_DIR	$LOCAL_OUT_DIR/$MCP_SANDBOX/notion-mcp.out	$LOCAL_OUT_DIR/$MCP_SANDBOX/notion-mcp.err	$LOCAL_OUT_DIR/artifacts/notion-mcp
EOF

echo "Wrote split Amika launch-readiness metadata to $LOCAL_OUT_DIR"
echo "Locality artifacts: $LOCALITY_SANDBOX:$LOCALITY_REMOTE_OUT_DIR"
echo "MCP artifacts: $MCP_SANDBOX:$MCP_REMOTE_OUT_DIR"
if [ "$SYNC_ARTIFACTS" = "1" ]; then
  echo "Local copies: $LOCAL_OUT_DIR/artifacts/locality and $LOCAL_OUT_DIR/artifacts/notion-mcp"
fi
