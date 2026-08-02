#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-agent-comparison.sh [--out-dir <path>] [--remote-worktree <path>] [benchmark args...]

Runs the launch-readiness benchmark concurrently on two remote sandboxes or instances:
  - Locality strategy on LOCALITY_SANDBOX
  - MCP strategy on MCP_SANDBOX

Defaults:
  LOCALITY_SANDBOX=launch-readiness-<UTC_RUN_ID>-locality
  MCP_SANDBOX=launch-readiness-<UTC_RUN_ID>-mcp
  LOCALITY_SNAPSHOT=locality-snapshot
  MCP_SNAPSHOT=mcp-snapshot
  LOCAL_OUT_DIR=target/launch-readiness-amika/<UTC_RUN_ID>/
  REMOTE_HOME=/home/amika
  REMOTE_SOURCE_REPO=/home/amika/workspace/locality
  REMOTE_WORKTREE=/home/amika/workspace/locality-launch-readiness-<UTC_RUN_ID>
  LOCALITY_REMOTE_OUT_DIR=<REMOTE_WORKTREE>/target/launch-readiness-<UTC_RUN_ID>-locality
  MCP_REMOTE_OUT_DIR=<REMOTE_WORKTREE>/target/launch-readiness-<UTC_RUN_ID>-mcp

Environment:
  RUN_ID                         Run id shared by both sandboxes.
  LOCALITY_SANDBOX               Label or Amika sandbox for Locality runs.
  MCP_SANDBOX                    Label or Amika sandbox for MCP runs.
  LOCALITY_SNAPSHOT              Amika snapshot for Locality runs.
  MCP_SNAPSHOT                   Amika snapshot for MCP runs.
  LOCAL_OUT_DIR or OUT_DIR       Local metadata/log output directory.
  REMOTE_SOURCE_REPO             Existing git checkout inside each sandbox.
  REMOTE_HOME                    Home directory inside each sandbox.
                                  Default: /home/amika for REMOTE_PROVIDER=amika,
                                  /home/ubuntu for REMOTE_PROVIDER=ssh.
  REMOTE_WORKTREE_ROOT           Parent for clean detached benchmark worktrees.
  REMOTE_WORKTREE                Exact clean detached worktree path.
  REMOTE_LOC_BIN                 installed loc binary in the Locality sandbox.
                                  Default: /usr/bin/loc. Only required for
                                  Locality strategy runs.
  BENCHMARK_REF                  Git ref checked out in each sandbox. Default: origin/main.
  REMOTE_PROVIDER                Remote backend: amika or ssh. Default: amika.
  AMIKA_SANDBOX_FLAGS            Optional flags passed to amika sandbox ssh.
  AMIKA_SSH_FORCE_TTY            Use direct ssh -tt for unhealthy sandboxes
                                  that reject non-interactive exec. Default: 0.
  LOCALITY_SSH_TARGET            SSH target for Locality when REMOTE_PROVIDER=ssh.
                                  Example: ubuntu@203.0.113.10.
  MCP_SSH_TARGET                 SSH target for MCP when REMOTE_PROVIDER=ssh.
  SSH_OPTIONS                    Optional SSH options for REMOTE_PROVIDER=ssh.
                                  Example: -i /path/key.pem -o BatchMode=yes.
  CODEX_MODEL                    Passed through to the benchmark worker.
  CODEX_REASONING_EFFORT         Passed through to the benchmark worker.
  CODEX_EXEC_TIMEOUT_SECONDS     Passed through to the benchmark worker.
  CODEX_HOOKS_MODE               Passed through to the benchmark worker.
  AZURE_OPENAI_API_KEY           Forwarded to remote Codex when set locally.
  AZURE_OPENAI_BASE_URL          Forwarded to remote Codex when set locally.
  AGENT_REPORT_PATH              Agent report path. Default: $REMOTE_HOME/final_report.md.
  LOCALITY_CONTEXT_DIRS          Prehydrated Locality roots for the Locality worker.
  LOCALITY_CONTEXT_ROOTS         Alias accepted by the worker.
  LINEAR_API_KEY                 MCP credential forwarded when set.
  NOTION_API_TOKEN               MCP credential forwarded when set.
                                  NOTION_TOKEN and NOTION_ACCESS_TOKEN are aliases.
  SLACK_BOT_TOKEN                Required Slack MCP credential for MCP runs.
  SLACK_TEAM_ID                  Required Slack team id for MCP runs.
  SLACK_CHANNEL_IDS              Optional comma-delimited Slack channel allowlist.
  SYNC_LOCAL_EXPERIMENT          Copy this local comparison harness into each
                                  remote worktree before running. Default: 0.
  SYNC_ARTIFACTS                 Copy remote OUT_DIRs back locally. Default: 1.

MCP credentials can also live in the MCP sandbox under:
  ~/.config/locality-launch-readiness/mcp/{linear-api-key,notion-token,slack-bot-token,slack-team-id,slack-channel-ids}

Any remaining arguments are passed to run-launch-readiness-benchmark.sh.
This wrapper owns split strategy execution; --compare-mcp is accepted as a no-op
compatibility flag and --strategy is rejected.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"

LOCALITY_SANDBOX="${LOCALITY_SANDBOX:-launch-readiness-$RUN_ID-locality}"
MCP_SANDBOX="${MCP_SANDBOX:-launch-readiness-$RUN_ID-mcp}"
LOCALITY_SNAPSHOT="${LOCALITY_SNAPSHOT:-locality-snapshot}"
MCP_SNAPSHOT="${MCP_SNAPSHOT:-mcp-snapshot}"
declare -a CREATED_AMIKA_SANDBOXES=()
REMOTE_PROVIDER="${REMOTE_PROVIDER:-amika}"
REMOTE_HOME="${REMOTE_HOME:-}"
if [ -z "$REMOTE_HOME" ]; then
  case "$REMOTE_PROVIDER" in
    amika) REMOTE_HOME="/home/amika" ;;
    *) REMOTE_HOME="/home/ubuntu" ;;
  esac
fi
if [ "$REMOTE_HOME" != "/" ]; then
  REMOTE_HOME="${REMOTE_HOME%/}"
fi
REMOTE_SOURCE_REPO="${REMOTE_SOURCE_REPO:-$REMOTE_HOME/workspace/locality}"
REMOTE_WORKTREE_ROOT="${REMOTE_WORKTREE_ROOT:-$REMOTE_HOME/workspace}"
REMOTE_WORKTREE="${REMOTE_WORKTREE:-$REMOTE_WORKTREE_ROOT/locality-launch-readiness-$RUN_ID}"
BENCHMARK_REF="${BENCHMARK_REF:-origin/main}"
REMOTE_LOC_BIN="${REMOTE_LOC_BIN:-/usr/bin/loc}"
AMIKA_SSH_FORCE_TTY="${AMIKA_SSH_FORCE_TTY:-0}"
LOCALITY_SSH_TARGET="${LOCALITY_SSH_TARGET:-}"
MCP_SSH_TARGET="${MCP_SSH_TARGET:-}"
SSH_OPTIONS="${SSH_OPTIONS:-}"

LOCAL_OUT_DIR="${LOCAL_OUT_DIR:-${OUT_DIR:-$REPO_ROOT/target/launch-readiness-amika/$RUN_ID}}"
LOCALITY_REMOTE_OUT_DIR_INPUT="${LOCALITY_REMOTE_OUT_DIR:-}"
MCP_REMOTE_OUT_DIR_INPUT="${MCP_REMOTE_OUT_DIR:-}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
SYNC_LOCAL_EXPERIMENT="${SYNC_LOCAL_EXPERIMENT:-0}"
SYNC_ARTIFACTS="${SYNC_ARTIFACTS:-1}"

declare -a BENCHMARK_ARGS=()

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
    --compare-mcp)
      shift
      ;;
    --compare-hooks|--push|--write-mounted-page)
      echo "$1 is not supported by the simplified split-sandbox wrapper" >&2
      exit 2
      ;;
    --scenario)
      if [ "$#" -lt 2 ]; then
        echo "--scenario requires a value" >&2
        exit 2
      fi
      BENCHMARK_ARGS+=("$1" "$2")
      shift 2
      ;;
    --scenario=*)
      BENCHMARK_ARGS+=("$1")
      shift
      ;;
    *)
      BENCHMARK_ARGS+=("$1")
      shift
      ;;
  esac
done

LOCALITY_REMOTE_OUT_DIR="${LOCALITY_REMOTE_OUT_DIR_INPUT:-$REMOTE_WORKTREE/target/launch-readiness-$RUN_ID-locality}"
MCP_REMOTE_OUT_DIR="${MCP_REMOTE_OUT_DIR_INPUT:-$REMOTE_WORKTREE/target/launch-readiness-$RUN_ID-mcp}"

if [ "$LOCALITY_SANDBOX" = "$MCP_SANDBOX" ]; then
  echo "LOCALITY_SANDBOX and MCP_SANDBOX must be different labels or sandboxes" >&2
  exit 2
fi

case "$REMOTE_PROVIDER" in
  amika|ssh) ;;
  *) echo "REMOTE_PROVIDER must be amika or ssh" >&2; exit 2 ;;
esac

if [ "$REMOTE_PROVIDER" = "amika" ]; then
  if ! command -v amika >/dev/null 2>&1; then
    echo "amika is not available on PATH" >&2
    exit 127
  fi
else
  if [ -z "$LOCALITY_SSH_TARGET" ] || [ -z "$MCP_SSH_TARGET" ]; then
    echo "LOCALITY_SSH_TARGET and MCP_SSH_TARGET are required when REMOTE_PROVIDER=ssh" >&2
    exit 2
  fi
  if [ "$LOCALITY_SSH_TARGET" = "$MCP_SSH_TARGET" ]; then
    echo "LOCALITY_SSH_TARGET and MCP_SSH_TARGET must be different when REMOTE_PROVIDER=ssh" >&2
    exit 2
  fi
  if ! command -v ssh >/dev/null 2>&1; then
    echo "ssh is required when REMOTE_PROVIDER=ssh" >&2
    exit 127
  fi
fi

mkdir -p "$LOCAL_OUT_DIR"
LOCAL_OUT_DIR="$(cd "$LOCAL_OUT_DIR" && pwd)"

declare -a AMIKA_FLAGS=()
if [ -n "${AMIKA_SANDBOX_FLAGS:-}" ]; then
  read -r -a AMIKA_FLAGS <<< "$AMIKA_SANDBOX_FLAGS"
fi

declare -a SSH_ARGS=()
if [ -n "$SSH_OPTIONS" ]; then
  read -r -a SSH_ARGS <<< "$SSH_OPTIONS"
fi

load_mcp_credentials_from_zshrc() {
  command -v zsh >/dev/null 2>&1 || return 0

  local output
  output="$(
    zsh -ic '
      print -r -- __LOCALITY_MCP_ENV_BEGIN__
      for name in AZURE_OPENAI_API_KEY AZURE_OPENAI_BASE_URL LINEAR_API_KEY NOTION_API_TOKEN NOTION_TOKEN NOTION_ACCESS_TOKEN SLACK_BOT_TOKEN SLACK_TEAM_ID SLACK_CHANNEL_IDS; do
        value="${(P)name}"
        if [[ -n "$value" ]]; then
          printf "%s=" "$name"
          printf "%s" "$value" | base64 | tr -d "\n"
          printf "\n"
        fi
      done
      print -r -- __LOCALITY_MCP_ENV_END__
    ' 2>/dev/null
  )" || return 0

  local in_block=0
  local line name encoded decoded
  while IFS= read -r line; do
    case "$line" in
      __LOCALITY_MCP_ENV_BEGIN__)
        in_block=1
        continue
        ;;
      __LOCALITY_MCP_ENV_END__)
        break
        ;;
    esac
    [ "$in_block" -eq 1 ] || continue
    name="${line%%=*}"
    encoded="${line#*=}"
    case "$name" in
      AZURE_OPENAI_API_KEY|AZURE_OPENAI_BASE_URL|LINEAR_API_KEY|NOTION_API_TOKEN|NOTION_TOKEN|NOTION_ACCESS_TOKEN|SLACK_BOT_TOKEN|SLACK_TEAM_ID|SLACK_CHANNEL_IDS)
        if [ -z "${!name:-}" ] && [ -n "$encoded" ]; then
          decoded="$(printf '%s' "$encoded" | base64 -d 2>/dev/null || true)"
          if [ -n "$decoded" ]; then
            printf -v "$name" '%s' "$decoded"
            export "$name"
          fi
        fi
        ;;
    esac
  done <<< "$output"
}

shell_quote() {
  printf "%q" "$1"
}

base64_one_line() {
  base64 | tr -d '\n'
}

forwarded_worker_env_b64() {
  local strategy="${1:-all}"
  local name
  local names=(
    AGENT_REPORT_PATH
    SANDBOX_HOME
    CODEX_HOOKS_MODE
    AZURE_OPENAI_API_KEY
    AZURE_OPENAI_BASE_URL
    LOCALITY_CONTEXT_DIRS
    LOCALITY_CONTEXT_ROOTS
    LOCALITY_STATE_DIR
    LOCALITY_CREDENTIAL_STORE
    PROMPT_ROOT
    LOCALITY_PROMPT_DIR
    MCP_PROMPT_DIR
  )
  local credential_names=(
    LINEAR_API_KEY
    NOTION_API_TOKEN
    NOTION_TOKEN
    NOTION_ACCESS_TOKEN
    SLACK_BOT_TOKEN
    SLACK_TEAM_ID
    SLACK_CHANNEL_IDS
  )
  if [ "$strategy" = "notion-mcp" ] || [ "$strategy" = "all" ]; then
    names+=("${credential_names[@]}")
  fi
  for name in "${names[@]}"; do
    if [ "${!name+x}" ]; then
      printf 'export %s=%q\n' "$name" "${!name}"
    fi
  done | base64_one_line
}

amika_sandbox_ssh() {
  if remote_force_tty; then
    local sandbox="$1"
    shift
    if [ "${1:-}" = "--" ]; then
      shift
    fi
    if [ -r /dev/tty ]; then
      ssh -tt -o LogLevel=ERROR -o StrictHostKeyChecking=accept-new "$(amika_sandbox_ssh_target "$sandbox")" "$@" < /dev/tty
    else
      ssh -tt -o LogLevel=ERROR -o StrictHostKeyChecking=accept-new "$(amika_sandbox_ssh_target "$sandbox")" "$@"
    fi
    return
  fi
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

remote_force_tty() {
  [ "$REMOTE_PROVIDER" = "amika" ] && [ "$AMIKA_SSH_FORCE_TTY" = "1" ]
}

remote_ssh_target() {
  local sandbox="$1"
  if [ "$REMOTE_PROVIDER" = "amika" ]; then
    amika_sandbox_ssh_target "$sandbox"
    return
  fi

  if [ "$sandbox" = "$LOCALITY_SANDBOX" ]; then
    printf '%s\n' "$LOCALITY_SSH_TARGET"
  elif [ "$sandbox" = "$MCP_SANDBOX" ]; then
    printf '%s\n' "$MCP_SSH_TARGET"
  else
    echo "unknown SSH sandbox label: $sandbox" >&2
    return 2
  fi
}

remote_ssh() {
  local sandbox="$1"
  shift

  if [ "$REMOTE_PROVIDER" = "amika" ]; then
    amika_sandbox_ssh "$sandbox" "$@"
    return
  fi

  if [ "${1:-}" = "--" ]; then
    shift
  fi
  ssh "${SSH_ARGS[@]}" "$(remote_ssh_target "$sandbox")" "$@"
}

create_amika_sandboxes() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0

  local sandboxes_json
  sandboxes_json="$(amika sandbox list --remote -o json)"

  LOCALITY_SANDBOX="$LOCALITY_SANDBOX" MCP_SANDBOX="$MCP_SANDBOX" python3 -c '
import json
import os
import sys

try:
    sandboxes = json.load(sys.stdin)
except json.JSONDecodeError as error:
    raise SystemExit(f"could not parse amika sandbox list JSON: {error}")

existing_names = {
    sandbox.get("name")
    for sandbox in sandboxes
    if isinstance(sandbox, dict) and isinstance(sandbox.get("name"), str)
}
for name in (os.environ["LOCALITY_SANDBOX"], os.environ["MCP_SANDBOX"]):
    if name in existing_names:
        raise SystemExit(f"amika sandbox already exists: {name}")
' <<< "$sandboxes_json"

  amika sandbox create --remote --no-git --snapshot "$LOCALITY_SNAPSHOT" --name "$LOCALITY_SANDBOX"
  CREATED_AMIKA_SANDBOXES+=("$LOCALITY_SANDBOX")
  amika sandbox create --remote --no-git --snapshot "$MCP_SNAPSHOT" --name "$MCP_SANDBOX"
  CREATED_AMIKA_SANDBOXES+=("$MCP_SANDBOX")
}

cleanup_amika_sandboxes() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0
  [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ] || return 0

  amika sandbox delete --remote --force "${CREATED_AMIKA_SANDBOXES[@]}" || return $?
  CREATED_AMIKA_SANDBOXES=()
}

remote_rsync_ssh_command() {
  local command="ssh"
  local arg
  for arg in "${SSH_ARGS[@]}"; do
    command+=" $(shell_quote "$arg")"
  done
  printf '%s\n' "$command"
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
  if remote_force_tty; then
    remote_command="$remote_command; remote_rc=\$?; printf '\n__AMIKA_REMOTE_RC__=%s\n' \"\$remote_rc\"; exit 0"
  fi
  remote_shell_command="bash -lc $(shell_quote "$remote_command")"
  local attempt=1
  local max_attempts=1
  remote_force_tty && max_attempts=5
  while [ "$attempt" -le "$max_attempts" ]; do
    set +e
    remote_ssh "$sandbox" -- "$remote_shell_command" > "$stdout_file" 2> "$stderr_file"
    local_rc=$?
    set -e
    if remote_force_tty; then
      local remote_rc
      remote_rc="$(sed -n 's/.*__AMIKA_REMOTE_RC__=//p' "$stdout_file" | tr -d '\r' | tail -1)"
      if [ -n "$remote_rc" ]; then
        return "$remote_rc"
      fi
      if [ "$attempt" -lt "$max_attempts" ]; then
        printf 'retrying forced-tty command after missing remote rc marker; attempt=%s rc=%s\n' "$attempt" "$local_rc" >> "$stderr_file"
        sleep "$attempt"
      fi
    else
      return "$local_rc"
    fi
    attempt=$((attempt + 1))
  done
  return "$local_rc"
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
git update-index -q --refresh
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
  echo "source repository is not clean before pull: $source_repo" >&2
  git status --short --untracked-files=all >&2
  exit 2
fi

git fetch --prune origin
git checkout --detach "$ref"

git update-index -q --refresh
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
  echo "source repository is not clean after pull: $source_repo" >&2
  git status --short --untracked-files=all >&2
  exit 2
fi

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

sync_local_experiment() {
  local sandbox="$1"
  local local_dir="$LOCAL_OUT_DIR/$sandbox"
  local remote_dir="$REMOTE_WORKTREE/experiment/locality-mcp-comparison"
  local script
  local ssh_target

  if [ "$SYNC_LOCAL_EXPERIMENT" != "1" ]; then
    return 0
  fi

  mkdir -p "$local_dir"
  echo "Syncing local comparison harness into $sandbox:$remote_dir"
  if [ "$AMIKA_SSH_FORCE_TTY" = "1" ]; then
    local archive_b64="$local_dir/local-experiment.tar.gz.b64"
    local remote_b64="/tmp/locality-mcp-comparison-$RUN_ID-$sandbox.b64"
    local remote_chunk_dir="/tmp/locality-mcp-comparison-$RUN_ID-$sandbox-chunks"
    local chunk
    local chunk_index=0
    COPYFILE_DISABLE=1 tar -czf - -C "$SCRIPT_DIR" . | base64 | tr -d '\n' > "$archive_b64"

    script="$(cat <<'REMOTE_SYNC_INIT'
set -euo pipefail
rm -f "$1"
rm -rf "$2"
mkdir -p "$2"
REMOTE_SYNC_INIT
)"
    run_remote_script \
      "$sandbox" \
      "$local_dir/sync-local-experiment-init.out" \
      "$local_dir/sync-local-experiment-init.err" \
      "$script" \
      "$remote_b64" \
      "$remote_chunk_dir"

    while IFS= read -r chunk; do
      chunk_index=$((chunk_index + 1))
      local chunk_file="$remote_chunk_dir/chunk-$(printf '%05d' "$chunk_index")"
      local chunk_file_q
      local chunk_q
      local remote_cmd
      local chunk_attempt=1
      local chunk_ok=0
      chunk_file_q="$(shell_quote "$chunk_file")"
      chunk_q="$(shell_quote "$chunk")"
      remote_cmd="set -euo pipefail; mkdir -p $(shell_quote "$remote_chunk_dir"); printf %s $chunk_q > $chunk_file_q; printf '\n__AMIKA_REMOTE_RC__=0\n'"
      while [ "$chunk_attempt" -le 5 ]; do
        set +e
        remote_ssh "$sandbox" -- "bash -lc $(shell_quote "$remote_cmd")" \
          > "$local_dir/sync-local-experiment-chunk-$chunk_index.out" \
          2> "$local_dir/sync-local-experiment-chunk-$chunk_index.err"
        set -e
        if sed -n 's/.*__AMIKA_REMOTE_RC__=//p' "$local_dir/sync-local-experiment-chunk-$chunk_index.out" | tr -d '\r' | grep -qx '0'; then
          chunk_ok=1
          break
        fi
        if [ "$chunk_attempt" -lt 5 ]; then
          sleep "$chunk_attempt"
        fi
        chunk_attempt=$((chunk_attempt + 1))
      done
      if [ "$chunk_ok" -ne 1 ]; then
        echo "failed to upload harness chunk $chunk_index to $sandbox" >&2
        return 255
      fi
    done < <(fold -w 4000 "$archive_b64")

    script="$(cat <<'REMOTE_SYNC_EXTRACT'
set -euo pipefail
remote_b64="$1"
dest="$2"
chunk_dir="$3"
tmp="$(mktemp -d)"
cat "$chunk_dir"/chunk-* > "$remote_b64"
tr -d '\r\n' < "$remote_b64" | base64 -d > "$tmp/payload.tgz"
rm -rf "$dest"
mkdir -p "$dest"
tar -xzf "$tmp/payload.tgz" -C "$dest"
rm -f "$remote_b64"
rm -rf "$chunk_dir"
rm -rf "$tmp"
test -s "$dest/run-launch-readiness-benchmark.sh"
REMOTE_SYNC_EXTRACT
)"
    run_remote_script \
      "$sandbox" \
      "$local_dir/sync-local-experiment.out" \
      "$local_dir/sync-local-experiment.err" \
      "$script" \
      "$remote_b64" \
      "$remote_dir" \
      "$remote_chunk_dir"

    script="$(cat <<'REMOTE_SYNC_VERIFY'
set -euo pipefail
test -s "$1/run-launch-readiness-benchmark.sh"
REMOTE_SYNC_VERIFY
)"
    run_remote_script \
      "$sandbox" \
      "$local_dir/sync-local-experiment-verify.out" \
      "$local_dir/sync-local-experiment-verify.err" \
      "$script" \
      "$remote_dir"
    return
  fi

  script="$(cat <<'REMOTE_SYNC_PREP'
set -euo pipefail
mkdir -p "$1"
REMOTE_SYNC_PREP
)"
  run_remote_script \
    "$sandbox" \
    "$local_dir/sync-local-experiment.out" \
    "$local_dir/sync-local-experiment.err" \
    "$script" \
    "$remote_dir"

  ssh_target="$(remote_ssh_target "$sandbox")"
  if ! command -v rsync >/dev/null 2>&1; then
    echo "rsync is required when SYNC_LOCAL_EXPERIMENT=1" >&2
    exit 127
  fi
  if [ "$REMOTE_PROVIDER" = "ssh" ]; then
    rsync -az --delete -e "$(remote_rsync_ssh_command)" "$SCRIPT_DIR/" "$ssh_target:$remote_dir/"
  else
    rsync -az --delete "$SCRIPT_DIR/" "$ssh_target:$remote_dir/"
  fi
}

run_launch_strategy() {
  local sandbox="$1"
  local strategy="$2"
  local remote_out_dir="$3"
  shift 3
  local local_dir="$LOCAL_OUT_DIR/$sandbox"
  local script
  local worker_env_b64

  mkdir -p "$local_dir"
  echo "Running $strategy on $sandbox"
  worker_env_b64="$(forwarded_worker_env_b64 "$strategy")"

  if remote_force_tty; then
    local remote_benchmark_args=""
    local arg
    local remote_cmd
    local remote_cmd_b64
    local remote_shell_command
    local local_rc
    for arg in "$@"; do
      remote_benchmark_args+=" $(shell_quote "$arg")"
    done
    remote_cmd="$(cat <<REMOTE_TTY_RUN
set -euo pipefail
strategy=$(shell_quote "$strategy")
repo_dir=$(shell_quote "$REMOTE_WORKTREE")
out_dir=$(shell_quote "$remote_out_dir")
run_id=$(shell_quote "$RUN_ID")
model=$(shell_quote "$CODEX_MODEL")
effort=$(shell_quote "$CODEX_REASONING_EFFORT")
timeout_seconds=$(shell_quote "$CODEX_EXEC_TIMEOUT_SECONDS")
loc_bin=$(shell_quote "$REMOTE_LOC_BIN")
worker_env_b64=$(shell_quote "$worker_env_b64")
benchmark_args=($remote_benchmark_args)
export PATH="\$HOME/.cargo/bin:\$HOME/.local/bin:\$PATH"
env_file="\${LOCALITY_EXPERIMENT_ENV:-\$HOME/.config/locality-experiment/env}"
if [ -f "\$env_file" ]; then
  set -a
  source "\$env_file"
  set +a
fi
if [ -n "\$worker_env_b64" ]; then
  eval "\$(printf '%s' "\$worker_env_b64" | base64 -d)"
fi
read_secret_if_unset() {
  local name="\$1"
  local file="\$2"
  if [ -z "\${!name:-}" ] && [ -f "\$file" ]; then
    export "\$name=\$(cat "\$file")"
  fi
}
secret_dir="\${LOCALITY_LAUNCH_READINESS_SECRET_DIR:-\$HOME/.config/locality-launch-readiness/mcp}"
if [ "\$strategy" = "notion-mcp" ]; then
  read_secret_if_unset LINEAR_API_KEY "\$secret_dir/linear-api-key"
  if [ -z "\${NOTION_API_TOKEN:-\${NOTION_TOKEN:-\${NOTION_ACCESS_TOKEN:-}}}" ] && [ -f "\$secret_dir/notion-token" ]; then
    export NOTION_API_TOKEN="\$(cat "\$secret_dir/notion-token")"
  fi
  read_secret_if_unset SLACK_BOT_TOKEN "\$secret_dir/slack-bot-token"
  read_secret_if_unset SLACK_TEAM_ID "\$secret_dir/slack-team-id"
  read_secret_if_unset SLACK_CHANNEL_IDS "\$secret_dir/slack-channel-ids"
fi
if [ "\$strategy" = "locality" ]; then
  if [ ! -x "\$loc_bin" ]; then
    if command -v loc >/dev/null 2>&1; then
      loc_bin="\$(command -v loc)"
    else
      echo "installed loc is required for Locality runs; not executable or not found: \$loc_bin" >&2
      printf '\n__AMIKA_REMOTE_RC__=127\n'
      exit 0
    fi
  fi
fi
export RUN_ID="\$run_id"
export REPO_DIR="\$repo_dir"
export OUT_DIR="\$out_dir"
export CODEX_MODEL="\$model"
export CODEX_REASONING_EFFORT="\$effort"
export CODEX_EXEC_TIMEOUT_SECONDS="\$timeout_seconds"
sandbox_home="\${SANDBOX_HOME:-\$HOME}"
export SANDBOX_HOME="\$sandbox_home"
export AGENT_REPORT_PATH="\${AGENT_REPORT_PATH:-\$sandbox_home/final_report.md}"
if [ "\$strategy" = "locality" ]; then
  export LOC_BIN="\${LOC_BIN:-\$loc_bin}"
fi
cd "\$repo_dir"
set +e
"\$repo_dir/experiment/locality-mcp-comparison/run-launch-readiness-benchmark.sh" --strategy "\$strategy" "\${benchmark_args[@]}"
remote_rc=\$?
printf '\n__AMIKA_REMOTE_RC__=%s\n' "\$remote_rc"
exit 0
REMOTE_TTY_RUN
)"
    remote_cmd_b64="$(printf '%s' "$remote_cmd" | base64_one_line)"
    remote_shell_command="printf %s $(shell_quote "$remote_cmd_b64") | base64 -d | bash"
    local attempt=1
    local max_attempts=5
    local remote_rc
    while [ "$attempt" -le "$max_attempts" ]; do
      set +e
      remote_ssh "$sandbox" -- "bash -lc $(shell_quote "$remote_shell_command")" > "$local_dir/$strategy.out" 2> "$local_dir/$strategy.err"
      local_rc=$?
      set -e
      remote_rc="$(sed -n 's/.*__AMIKA_REMOTE_RC__=//p' "$local_dir/$strategy.out" | tr -d '\r' | tail -1)"
      if [ -n "$remote_rc" ]; then
        return "$remote_rc"
      fi
      if [ "$attempt" -lt "$max_attempts" ]; then
        printf 'retrying forced-tty launch after missing remote rc marker; attempt=%s rc=%s\n' "$attempt" "$local_rc" >> "$local_dir/$strategy.err"
        sleep "$attempt"
      fi
      attempt=$((attempt + 1))
    done
    return "$local_rc"
  fi

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
worker_env_b64="$9"
shift 9
echo "remote_launch stage=args strategy=$strategy" >&2

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

env_file="${LOCALITY_EXPERIMENT_ENV:-$HOME/.config/locality-experiment/env}"
if [ -f "$env_file" ]; then
  echo "remote_launch stage=source_env" >&2
  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  set +a
fi
if [ -n "$worker_env_b64" ]; then
  echo "remote_launch stage=eval_forwarded_env" >&2
  eval "$(printf '%s' "$worker_env_b64" | base64 -d)"
fi
echo "remote_launch stage=env_ready" >&2

read_secret_if_unset() {
  local name="$1"
  local file="$2"
  if [ -z "${!name:-}" ] && [ -f "$file" ]; then
    export "$name=$(cat "$file")"
  fi
}

secret_dir="${LOCALITY_LAUNCH_READINESS_SECRET_DIR:-$HOME/.config/locality-launch-readiness/mcp}"
if [ "$strategy" = "notion-mcp" ]; then
  read_secret_if_unset LINEAR_API_KEY "$secret_dir/linear-api-key"
  if [ -z "${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}" ] && [ -f "$secret_dir/notion-token" ]; then
    export NOTION_API_TOKEN="$(cat "$secret_dir/notion-token")"
  fi
  read_secret_if_unset SLACK_BOT_TOKEN "$secret_dir/slack-bot-token"
  read_secret_if_unset SLACK_TEAM_ID "$secret_dir/slack-team-id"
  read_secret_if_unset SLACK_CHANNEL_IDS "$secret_dir/slack-channel-ids"
fi

if [ "$strategy" = "locality" ]; then
  echo "remote_launch stage=loc_check" >&2
  if [ ! -x "$loc_bin" ]; then
    if command -v loc >/dev/null 2>&1; then
      loc_bin="$(command -v loc)"
    else
      echo "installed loc is required for Locality runs; not executable or not found: $loc_bin" >&2
      exit 127
    fi
  fi
fi
echo "remote_launch stage=exports" >&2

export RUN_ID="$run_id"
export REPO_DIR="$repo_dir"
export OUT_DIR="$out_dir"
export CODEX_MODEL="$model"
export CODEX_REASONING_EFFORT="$effort"
export CODEX_EXEC_TIMEOUT_SECONDS="$timeout_seconds"
sandbox_home="${SANDBOX_HOME:-$HOME}"
export SANDBOX_HOME="$sandbox_home"
export AGENT_REPORT_PATH="${AGENT_REPORT_PATH:-$sandbox_home/final_report.md}"
if [ "$strategy" = "locality" ]; then
  export LOC_BIN="${LOC_BIN:-$loc_bin}"
fi

cd "$repo_dir"
echo "remote_launch stage=worker" >&2
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
    "$worker_env_b64" \
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
  echo "Syncing $strategy artifacts from $sandbox:$remote_out_dir"
  if remote_force_tty; then
    local archive_b64="$LOCAL_OUT_DIR/$sandbox/$strategy-artifacts.tar.gz.b64"
    local remote_out_dir_q
    local remote_cmd
    local ssh_rc
    mkdir -p "$LOCAL_OUT_DIR/$sandbox"
    remote_out_dir_q="$(shell_quote "$remote_out_dir")"
    remote_cmd="set -euo pipefail; cd $remote_out_dir_q; tar -czf - . | base64"
    set +e
    remote_ssh "$sandbox" -- "bash -lc $(shell_quote "$remote_cmd")" > "$archive_b64"
    ssh_rc=$?
    set -e
    rm -rf "$dest"
    mkdir -p "$dest"
    if ! tr -d '\r' < "$archive_b64" | base64 -d | tar -xzf - -C "$dest"; then
      return "$ssh_rc"
    fi
    return
  fi

  ssh_target="$(remote_ssh_target "$sandbox")"
  if command -v rsync >/dev/null 2>&1; then
    if [ "$REMOTE_PROVIDER" = "ssh" ]; then
      rsync -az --delete -e "$(remote_rsync_ssh_command)" "$ssh_target:$remote_out_dir/" "$dest/"
    else
      rsync -az --delete "$ssh_target:$remote_out_dir/" "$dest/"
    fi
  elif command -v scp >/dev/null 2>&1; then
    if [ "$REMOTE_PROVIDER" = "ssh" ]; then
      scp "${SSH_ARGS[@]}" -r "$ssh_target:$remote_out_dir/." "$dest/"
    else
      scp -r "$ssh_target:$remote_out_dir/." "$dest/"
    fi
  else
    echo "rsync or scp is required to sync remote artifacts" >&2
    return 127
  fi
}

run_launch_strategy_with_args() {
  local sandbox="$1"
  local strategy="$2"
  local remote_out_dir="$3"
  if [ "${#BENCHMARK_ARGS[@]}" -gt 0 ]; then
    run_launch_strategy "$sandbox" "$strategy" "$remote_out_dir" "${BENCHMARK_ARGS[@]}"
  else
    run_launch_strategy "$sandbox" "$strategy" "$remote_out_dir"
  fi
}

run_strategy_pipeline() {
  local sandbox="$1"
  local strategy="$2"
  local remote_out_dir="$3"

  prepare_worktree "$sandbox"
  sync_local_experiment "$sandbox"
  run_launch_strategy_with_args "$sandbox" "$strategy" "$remote_out_dir"
  sync_artifacts "$sandbox" "$strategy" "$remote_out_dir"
}

wait_for_strategy_pipeline() {
  local pid="$1"
  local strategy="$2"
  local rc

  set +e
  wait "$pid"
  rc=$?
  set -e
  if [ "$rc" -ne 0 ]; then
    echo "$strategy pipeline failed with exit code $rc" >&2
  fi
  return "$rc"
}

stop_strategy_pipelines() {
  local rc=$?
  local pid

  trap - INT TERM
  for pid in "${locality_pipeline_pid:-}" "${mcp_pipeline_pid:-}"; do
    if [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1; then
      kill "$pid" >/dev/null 2>&1 || true
    fi
  done
  exit "$rc"
}

cleanup_amika_sandboxes_on_exit() {
  local original_rc=$?
  local cleanup_rc

  trap - EXIT INT TERM
  set +e
  cleanup_amika_sandboxes
  cleanup_rc=$?

  if [ "$original_rc" -ne 0 ]; then
    exit "$original_rc"
  fi
  exit "$cleanup_rc"
}

write_artifacts_manifest() {
  cat > "$LOCAL_OUT_DIR/artifacts.tsv" <<EOF
strategy	sandbox	remote_out_dir	local_stdout	local_stderr	local_artifact_dir
locality	$LOCALITY_SANDBOX	$LOCALITY_REMOTE_OUT_DIR	$LOCAL_OUT_DIR/$LOCALITY_SANDBOX/locality.out	$LOCAL_OUT_DIR/$LOCALITY_SANDBOX/locality.err	$LOCAL_OUT_DIR/artifacts/locality
notion-mcp	$MCP_SANDBOX	$MCP_REMOTE_OUT_DIR	$LOCAL_OUT_DIR/$MCP_SANDBOX/notion-mcp.out	$LOCAL_OUT_DIR/$MCP_SANDBOX/notion-mcp.err	$LOCAL_OUT_DIR/artifacts/notion-mcp
EOF
}

{
  printf 'run_id=%s\n' "$RUN_ID"
  printf 'locality_sandbox=%s\n' "$LOCALITY_SANDBOX"
  printf 'mcp_sandbox=%s\n' "$MCP_SANDBOX"
  printf 'remote_provider=%s\n' "$REMOTE_PROVIDER"
  if [ "$REMOTE_PROVIDER" = "amika" ]; then
    printf 'locality_snapshot=%s\n' "$LOCALITY_SNAPSHOT"
    printf 'mcp_snapshot=%s\n' "$MCP_SNAPSHOT"
  fi
  if [ "$REMOTE_PROVIDER" = "ssh" ]; then
    printf 'locality_ssh_target=%s\n' "$LOCALITY_SSH_TARGET"
    printf 'mcp_ssh_target=%s\n' "$MCP_SSH_TARGET"
    printf 'ssh_options=%s\n' "$SSH_OPTIONS"
  fi
  printf 'remote_home=%s\n' "$REMOTE_HOME"
  printf 'remote_source_repo=%s\n' "$REMOTE_SOURCE_REPO"
  printf 'remote_worktree=%s\n' "$REMOTE_WORKTREE"
  printf 'remote_loc_bin=%s\n' "$REMOTE_LOC_BIN"
  printf 'amika_ssh_force_tty=%s\n' "$AMIKA_SSH_FORCE_TTY"
  printf 'benchmark_ref=%s\n' "$BENCHMARK_REF"
  printf 'locality_remote_out_dir=%s\n' "$LOCALITY_REMOTE_OUT_DIR"
  printf 'mcp_remote_out_dir=%s\n' "$MCP_REMOTE_OUT_DIR"
  printf 'codex_model=%s\n' "$CODEX_MODEL"
  printf 'codex_reasoning_effort=%s\n' "$CODEX_REASONING_EFFORT"
  printf 'codex_exec_timeout_seconds=%s\n' "$CODEX_EXEC_TIMEOUT_SECONDS"
  printf 'sync_local_experiment=%s\n' "$SYNC_LOCAL_EXPERIMENT"
  printf 'sync_artifacts=%s\n' "$SYNC_ARTIFACTS"
  printf 'strategy_execution=parallel\n'
  printf 'benchmark_args='
  if [ "${#BENCHMARK_ARGS[@]}" -gt 0 ]; then
    printf '%q ' "${BENCHMARK_ARGS[@]}"
  fi
  printf '\n'
} > "$LOCAL_OUT_DIR/run.env"

load_mcp_credentials_from_zshrc
trap cleanup_amika_sandboxes_on_exit EXIT
create_amika_sandboxes
write_artifacts_manifest

echo "Launching Locality and MCP strategy pipelines in parallel"
trap stop_strategy_pipelines INT TERM
run_strategy_pipeline "$LOCALITY_SANDBOX" "locality" "$LOCALITY_REMOTE_OUT_DIR" &
locality_pipeline_pid=$!
run_strategy_pipeline "$MCP_SANDBOX" "notion-mcp" "$MCP_REMOTE_OUT_DIR" &
mcp_pipeline_pid=$!

locality_pipeline_rc=0
mcp_pipeline_rc=0
wait_for_strategy_pipeline "$locality_pipeline_pid" "locality" || locality_pipeline_rc=$?
wait_for_strategy_pipeline "$mcp_pipeline_pid" "notion-mcp" || mcp_pipeline_rc=$?
trap - INT TERM

if [ "$locality_pipeline_rc" -ne 0 ] || [ "$mcp_pipeline_rc" -ne 0 ]; then
  echo "Launch-readiness strategy pipelines failed: locality=$locality_pipeline_rc notion-mcp=$mcp_pipeline_rc" >&2
  if [ "$locality_pipeline_rc" -ne 0 ]; then
    exit "$locality_pipeline_rc"
  fi
  exit "$mcp_pipeline_rc"
fi

if [ "$SYNC_ARTIFACTS" = "1" ]; then
  python3 "$SCRIPT_DIR/scripts/token-usage-charts.py" "$LOCAL_OUT_DIR/artifacts" "$LOCAL_OUT_DIR/token-usage" >/dev/null
  python3 "$SCRIPT_DIR/scripts/deep-dive-report.py" "$LOCAL_OUT_DIR" "$LOCAL_OUT_DIR/deep-dive.md" >/dev/null
fi

echo "Wrote split launch-readiness metadata to $LOCAL_OUT_DIR"
echo "Locality artifacts: $LOCALITY_SANDBOX:$LOCALITY_REMOTE_OUT_DIR"
echo "MCP artifacts: $MCP_SANDBOX:$MCP_REMOTE_OUT_DIR"
if [ "$SYNC_ARTIFACTS" = "1" ]; then
  echo "Local copies: $LOCAL_OUT_DIR/artifacts/locality and $LOCAL_OUT_DIR/artifacts/notion-mcp"
  echo "Token usage charts: $LOCAL_OUT_DIR/token-usage"
  echo "Deep-dive report: $LOCAL_OUT_DIR/deep-dive.md"
fi
