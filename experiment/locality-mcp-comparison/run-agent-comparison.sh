#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-agent-comparison.sh [--out-dir <path>] [--remote-worktree <path>] [benchmark args...]

Runs the launch-readiness benchmark concurrently on two remote sandboxes or instances:
  - Locality strategy in a new LOCALITY_SANDBOX
  - MCP strategy in a new MCP_SANDBOX

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
  LOCALITY_SANDBOX               Name for the new Locality Amika sandbox.
  MCP_SANDBOX                    Name for the new MCP Amika sandbox.
  LOCALITY_SNAPSHOT              Snapshot used to create the Locality sandbox.
  MCP_SNAPSHOT                   Snapshot used to create the MCP sandbox.
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
  AMIKA_CREATE_ATTEMPTS          Create/readiness attempts per sandbox. Default: 3.
  AMIKA_READINESS_TIMEOUT_SECONDS
                                  Readiness deadline per lifecycle operation.
                                  Default: 180.
  AMIKA_READINESS_POLL_SECONDS   Readiness polling interval. Default: 3.
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
                                  A failed copy retains both Amika sandboxes.
                                  Setting 0 warns, then deletes the sandboxes
                                  and discards remote-only outputs.

MCP credentials can also live in the MCP sandbox under:
  ~/.config/locality-launch-readiness/mcp/{linear-api-key,notion-token,slack-bot-token,slack-team-id,slack-channel-ids}

For REMOTE_PROVIDER=amika, both named sandboxes are created from their snapshots.
The wrapper refuses to reuse an existing sandbox with either name. Overrides set
the names of newly created sandboxes; they do not select reusable sandboxes.
Created Amika sandboxes are deleted after both artifact copies succeed, even
when a benchmark failed. If either copy fails, both are retained and exact
recovery commands are printed. Setup failures and signals clean up owned
sandboxes. With SYNC_ARTIFACTS=0, cleanup intentionally discards remote-only
outputs.

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
PENDING_AMIKA_SANDBOX=""
RETAIN_AMIKA_SANDBOXES=0
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

AMIKA_CREATE_ATTEMPTS="${AMIKA_CREATE_ATTEMPTS:-3}"
AMIKA_READINESS_TIMEOUT_SECONDS="${AMIKA_READINESS_TIMEOUT_SECONDS:-180}"
AMIKA_READINESS_POLL_SECONDS="${AMIKA_READINESS_POLL_SECONDS:-3}"
AMIKA_LIFECYCLE_LOG="$LOCAL_OUT_DIR/amika-lifecycle.log"

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
  if [ -n "$SSH_OPTIONS" ]; then
    ssh "${SSH_ARGS[@]}" "$(remote_ssh_target "$sandbox")" "$@"
  else
    ssh "$(remote_ssh_target "$sandbox")" "$@"
  fi
}

amika_table_state() {
  local name="$1"
  local table="$2"

  printf '%s\n' "$table" | awk -v name="$name" '
    NR > 1 && !found && $1 == name {
      state = $2
      found = 1
    }
    END {
      if (found) print state
    }
  '
}

validate_positive_integer() {
  local name="$1"
  local value="$2"

  if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
    printf '%s must be a positive integer\n' "$name" >&2
    return 2
  fi
}

lifecycle_now_millis() {
  python3 -c 'import time; print(int(time.monotonic() * 1000))'
}

new_amika_lifecycle_deadline() {
  printf '%s\n' "$(( $(lifecycle_now_millis) + AMIKA_READINESS_TIMEOUT_SECONDS * 1000 ))"
}

sleep_until_amika_poll() {
  local deadline="$1"
  local remaining=$((deadline - $(lifecycle_now_millis)))
  local poll_millis=$((AMIKA_READINESS_POLL_SECONDS * 1000))
  local sleep_millis

  [ "$remaining" -gt 0 ] || return 1
  if [ "$poll_millis" -lt "$remaining" ]; then
    sleep_millis="$poll_millis"
  else
    sleep_millis="$remaining"
  fi
  sleep "$((sleep_millis / 1000)).$(printf '%03d' "$((sleep_millis % 1000))")"
}

preflight_amika_environment() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0

  validate_positive_integer AMIKA_CREATE_ATTEMPTS "$AMIKA_CREATE_ATTEMPTS" || return $?
  validate_positive_integer AMIKA_READINESS_TIMEOUT_SECONDS "$AMIKA_READINESS_TIMEOUT_SECONDS" || return $?
  validate_positive_integer AMIKA_READINESS_POLL_SECONDS "$AMIKA_READINESS_POLL_SECONDS" || return $?

  local sandbox_table
  local snapshot_table
  local name
  local state
  local deadline

  deadline="$(new_amika_lifecycle_deadline)"
  load_amika_sandbox_table_until "$deadline" preflight || return $?
  sandbox_table="$AMIKA_SANDBOX_TABLE_RESULT"
  for name in "$LOCALITY_SANDBOX" "$MCP_SANDBOX"; do
    state="$(amika_table_state "$name" "$sandbox_table")"
    if [ -n "$state" ]; then
      printf 'amika sandbox already exists: %s\n' "$name" >&2
      return 2
    fi
  done

  deadline="$(new_amika_lifecycle_deadline)"
  load_amika_snapshot_table_until "$deadline" || return $?
  snapshot_table="$AMIKA_SNAPSHOT_TABLE_RESULT"
  for name in "$LOCALITY_SNAPSHOT" "$MCP_SNAPSHOT"; do
    state="$(amika_table_state "$name" "$snapshot_table")"
    if [ -z "$state" ]; then
      printf 'required Amika snapshot not found: %s\n' "$name" >&2
      return 2
    fi
    if [ "$state" != "active" ]; then
      printf 'required Amika snapshot is not active: %s (state=%s)\n' "$name" "$state" >&2
      return 2
    fi
  done
}

add_owned_amika_sandbox() {
  local name="$1"
  local owned

  if [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ]; then
    for owned in "${CREATED_AMIKA_SANDBOXES[@]}"; do
      [ "$owned" != "$name" ] || return 0
    done
  fi
  CREATED_AMIKA_SANDBOXES+=("$name")
}

remove_owned_amika_sandbox() {
  local name="$1"
  local owned
  local -a remaining=()

  if [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ]; then
    for owned in "${CREATED_AMIKA_SANDBOXES[@]}"; do
      [ "$owned" = "$name" ] || remaining+=("$owned")
    done
  fi
  if [ "${#remaining[@]}" -gt 0 ]; then
    CREATED_AMIKA_SANDBOXES=("${remaining[@]}")
  else
    CREATED_AMIKA_SANDBOXES=()
  fi
}

wait_for_amika_sandbox_absent() {
  local name="$1"
  local deadline="${2:-}"
  local table
  local state

  [ -n "$deadline" ] || deadline="$(new_amika_lifecycle_deadline)"
  while :; do
    if [ "$(lifecycle_now_millis)" -ge "$deadline" ]; then
      printf 'Timed out waiting for owned Amika sandbox to disappear: %s\n' "$name" >&2
      return 1
    fi
    load_amika_sandbox_table_until "$deadline" || return $?
    table="$AMIKA_SANDBOX_TABLE_RESULT"
    state="$(amika_table_state "$name" "$table")"
    if [ -z "$state" ]; then
      return 0
    fi
    if [ "$(lifecycle_now_millis)" -ge "$deadline" ]; then
      printf 'Timed out waiting for owned Amika sandbox to disappear: %s (state=%s)\n' "$name" "$state" >&2
      return 1
    fi
    sleep_until_amika_poll "$deadline" || return 1
  done
}

delete_owned_amika_sandbox() {
  local name="$1"
  local owned
  local is_owned=0
  local deadline

  if [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ]; then
    for owned in "${CREATED_AMIKA_SANDBOXES[@]}"; do
      if [ "$owned" = "$name" ]; then
        is_owned=1
        break
      fi
    done
  fi
  [ "$is_owned" -eq 1 ] || return 0

  deadline="$(new_amika_lifecycle_deadline)"
  run_managed_command_until "$deadline" amika sandbox delete --remote --force "$name" || return $?
  wait_for_amika_sandbox_absent "$name" "$deadline" || return $?
  remove_owned_amika_sandbox "$name"
}

AMIKA_LAST_STATE="missing"
AMIKA_SANDBOX_TABLE_RESULT=""

wait_for_amika_sandbox_ready() {
  local name="$1"
  local strategy="$2"
  local started_at
  local elapsed
  local deadline
  local table
  local state
  local remaining

  started_at="$(lifecycle_now_millis)"
  deadline=$((started_at + AMIKA_READINESS_TIMEOUT_SECONDS * 1000))
  while :; do
    if [ "$(lifecycle_now_millis)" -ge "$deadline" ]; then
      return 1
    fi
    load_amika_sandbox_table_until "$deadline" || return $?
    table="$AMIKA_SANDBOX_TABLE_RESULT"
    state="$(amika_table_state "$name" "$table")"
    AMIKA_LAST_STATE="${state:-missing}"
    elapsed=$(( ($(lifecycle_now_millis) - started_at) / 1000 ))
    printf 'strategy=%s sandbox=%s observed_state=%s elapsed_seconds=%s\n' \
      "$strategy" "$name" "$AMIKA_LAST_STATE" "$elapsed" >> "$AMIKA_LIFECYCLE_LOG"

    case "$state" in
      failed|'') return 1 ;;
      started)
        if run_managed_command_until "$deadline" amika_sandbox_ssh "$name" -- true; then
          elapsed=$(( ($(lifecycle_now_millis) - started_at) / 1000 ))
          printf 'strategy=%s sandbox=%s readiness_seconds=%s\n' \
            "$strategy" "$name" "$elapsed" >> "$AMIKA_LIFECYCLE_LOG"
          return 0
        fi
        ;;
    esac

    remaining=$((deadline - $(lifecycle_now_millis)))
    if [ "$remaining" -le 0 ]; then
      return 1
    fi
    sleep_until_amika_poll "$deadline" || return 1
  done
}

provision_amika_sandbox() {
  local name="$1"
  local snapshot="$2"
  local strategy="$3"
  local attempt
  local create_rc
  local delete_rc
  local attempt_deadline
  local table
  local state

  for ((attempt = 1; attempt <= AMIKA_CREATE_ATTEMPTS; attempt += 1)); do
    printf 'strategy=%s sandbox=%s snapshot=%s attempt=%s phase=create\n' \
      "$strategy" "$name" "$snapshot" "$attempt" >> "$AMIKA_LIFECYCLE_LOG"
    PENDING_AMIKA_SANDBOX="$name"
    attempt_deadline="$(new_amika_lifecycle_deadline)"
    if run_managed_command_until "$attempt_deadline" amika sandbox create --remote --no-git --snapshot "$snapshot" --name "$name"; then
      create_rc=0
    else
      create_rc=$?
    fi

    load_amika_sandbox_table_until "$attempt_deadline" || return $?
    table="$AMIKA_SANDBOX_TABLE_RESULT"
    state="$(amika_table_state "$name" "$table")"
    AMIKA_LAST_STATE="${state:-missing}"
    printf 'strategy=%s sandbox=%s snapshot=%s attempt=%s create_rc=%s state=%s\n' \
      "$strategy" "$name" "$snapshot" "$attempt" "$create_rc" "$AMIKA_LAST_STATE" >> "$AMIKA_LIFECYCLE_LOG"
    if [ -n "$state" ]; then
      add_owned_amika_sandbox "$name"
    fi
    PENDING_AMIKA_SANDBOX=""

    if [ "$create_rc" -eq 0 ] && [ -n "$state" ] && [ "$state" != "failed" ]; then
      if wait_for_amika_sandbox_ready "$name" "$strategy"; then
        return 0
      fi
    fi

    delete_rc=0
    delete_owned_amika_sandbox "$name" || delete_rc=$?
    if [ "$delete_rc" -ne 0 ]; then
      printf 'Failed to delete owned Amika sandbox after %s provisioning attempt %s: %s\n' \
        "$strategy" "$attempt" "$name" >&2
      return "$delete_rc"
    fi
  done

  printf 'Amika provisioning exhausted for %s: exact snapshot %s failed after %s attempts (last state=%s)\n' \
    "$strategy" "$snapshot" "$AMIKA_CREATE_ATTEMPTS" "$AMIKA_LAST_STATE" >&2
  return 1
}

prerequisite_overrides_b64() {
  local strategy="$1"
  local name
  local value
  local -a names=()

  if [ "$strategy" = "locality" ]; then
    names=(LOCALITY_CONTEXT_DIRS LOCALITY_CONTEXT_ROOTS)
  else
    names=(LINEAR_API_KEY NOTION_API_TOKEN NOTION_TOKEN NOTION_ACCESS_TOKEN SLACK_BOT_TOKEN SLACK_TEAM_ID SLACK_CHANNEL_IDS)
  fi

  for name in "${names[@]}"; do
    if declare -p "$name" >/dev/null 2>&1; then
      value="${!name}"
      if [ "$strategy" != "locality" ] && [ -n "$value" ]; then
        value="__present__"
      fi
      printf 'export %s=%q\n' "$name" "$value"
    fi
  done | base64_one_line
}

verify_amika_prerequisites() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0

  local locality_command
  local mcp_command
  local locality_overrides_b64
  local mcp_overrides_b64
  local deadline

  locality_overrides_b64="$(prerequisite_overrides_b64 locality)"
  mcp_overrides_b64="$(prerequisite_overrides_b64 notion-mcp)"

  locality_command="$(cat <<EOF
set -euo pipefail
AMIKA_PREREQUISITE_CHECK=locality
env_file="\${LOCALITY_EXPERIMENT_ENV:-\$HOME/.config/locality-experiment/env}"
if [ -f "\$env_file" ]; then
  set -a
  source "\$env_file"
  set +a
fi
prerequisite_overrides_b64=$locality_overrides_b64
if [ -n "\$prerequisite_overrides_b64" ]; then
  eval "\$(printf '%s' "\$prerequisite_overrides_b64" | base64 -d)"
fi
if [ ! -d $(shell_quote "$REMOTE_SOURCE_REPO")/.git ] && [ ! -f $(shell_quote "$REMOTE_SOURCE_REPO")/.git ]; then
  exit 1
fi
command -v codex >/dev/null 2>&1
test -x $(shell_quote "$REMOTE_LOC_BIN") || command -v loc >/dev/null 2>&1
configured_roots="\${LOCALITY_CONTEXT_DIRS:-\${LOCALITY_CONTEXT_ROOTS:-}}"
while IFS= read -r root; do
  [ -z \"\$root\" ] || test -d \"\$root\"
done < <(printf '%s\n' "\$configured_roots" | tr ':' '\n')
EOF
)"
  deadline="$(new_amika_lifecycle_deadline)"
  if ! run_managed_command_until "$deadline" amika_sandbox_ssh "$LOCALITY_SANDBOX" -- bash -lc "$locality_command"; then
    printf 'Amika prerequisite check failed for locality sandbox %s\n' "$LOCALITY_SANDBOX" >&2
    return 1
  fi

  mcp_command="$(cat <<EOF
set -euo pipefail
AMIKA_PREREQUISITE_CHECK=notion-mcp
env_file="\${LOCALITY_EXPERIMENT_ENV:-\$HOME/.config/locality-experiment/env}"
if [ -f "\$env_file" ]; then
  set -a
  source "\$env_file"
  set +a
fi
prerequisite_overrides_b64=$mcp_overrides_b64
if [ -n "\$prerequisite_overrides_b64" ]; then
  eval "\$(printf '%s' "\$prerequisite_overrides_b64" | base64 -d)"
fi
if [ ! -d $(shell_quote "$REMOTE_SOURCE_REPO")/.git ] && [ ! -f $(shell_quote "$REMOTE_SOURCE_REPO")/.git ]; then
  exit 1
fi
command -v codex >/dev/null 2>&1
secret_dir=\"\${LOCALITY_LAUNCH_READINESS_SECRET_DIR:-\$HOME/.config/locality-launch-readiness/mcp}\"
[ -n \"\${LINEAR_API_KEY:-}\" ] || test -s \"\$secret_dir/linear-api-key\"
[ -n \"\${NOTION_API_TOKEN:-\${NOTION_TOKEN:-\${NOTION_ACCESS_TOKEN:-}}}\" ] || test -s \"\$secret_dir/notion-token\"
[ -n \"\${SLACK_BOT_TOKEN:-}\" ] || test -s \"\$secret_dir/slack-bot-token\"
[ -n \"\${SLACK_TEAM_ID:-}\" ] || test -s \"\$secret_dir/slack-team-id\"
EOF
)"
  deadline="$(new_amika_lifecycle_deadline)"
  if ! run_managed_command_until "$deadline" amika_sandbox_ssh "$MCP_SANDBOX" -- bash -lc "$mcp_command"; then
    printf 'Amika prerequisite check failed for notion-mcp sandbox %s\n' "$MCP_SANDBOX" >&2
    return 1
  fi
}

create_amika_sandboxes() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0

  provision_amika_sandbox "$LOCALITY_SANDBOX" "$LOCALITY_SNAPSHOT" locality
  provision_amika_sandbox "$MCP_SANDBOX" "$MCP_SNAPSHOT" notion-mcp
  verify_amika_prerequisites
}

cleanup_amika_sandboxes() {
  local owned
  local deadline

  [ "$REMOTE_PROVIDER" = "amika" ] || return 0
  [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ] || return 0

  deadline="$(new_amika_lifecycle_deadline)"
  run_managed_command_until "$deadline" amika sandbox delete --remote --force "${CREATED_AMIKA_SANDBOXES[@]}" || return $?
  for owned in "${CREATED_AMIKA_SANDBOXES[@]}"; do
    wait_for_amika_sandbox_absent "$owned" "$deadline" || return $?
  done
  CREATED_AMIKA_SANDBOXES=()
}

run_managed_command() {
  local command_rc

  "$@" &
  active_operation_pid=$!
  set +e
  wait "$active_operation_pid"
  command_rc=$?
  set -e
  active_operation_pid=""
  return "$command_rc"
}

run_managed_command_until() {
  local deadline="$1"
  shift
  local command_rc
  local command_pid
  local remaining
  local restore_errexit=0
  local timeout_marker

  case "$-" in
    *e*) restore_errexit=1 ;;
  esac

  "$@" &
  active_operation_pid=$!
  command_pid="$active_operation_pid"
  remaining=$((deadline - $(lifecycle_now_millis)))
  if [ "$remaining" -le 0 ]; then
    kill_process_tree_now "$command_pid"
    active_operation_pid=""
    return 124
  fi
  timeout_marker="$(mktemp "${TMPDIR:-/tmp}/amika-command-timeout.XXXXXX")"
  (
    sleep "$((remaining / 1000)).$(printf '%03d' "$((remaining % 1000))")"
    printf 'timeout\n' > "$timeout_marker"
    kill_process_tree_now "$command_pid"
  ) &
  active_deadline_watchdog_pid=$!
  set +e
  wait "$command_pid"
  command_rc=$?
  if [ "$restore_errexit" -eq 1 ]; then
    set -e
  else
    set +e
  fi
  kill_process_tree_now "$active_deadline_watchdog_pid"
  wait "$active_deadline_watchdog_pid" >/dev/null 2>&1 || true
  active_deadline_watchdog_pid=""
  active_operation_pid=""
  if [ -s "$timeout_marker" ]; then
    rm -f "$timeout_marker"
    return 124
  fi
  rm -f "$timeout_marker"
  return "$command_rc"
}

load_amika_sandbox_table_until() {
  local deadline="$1"
  local purpose="${2:-lifecycle}"
  local output_file
  local command_rc

  output_file="$(mktemp "${TMPDIR:-/tmp}/amika-sandbox-list.XXXXXX")"
  if run_managed_command_until "$deadline" amika sandbox list --remote > "$output_file"; then
    AMIKA_SANDBOX_TABLE_RESULT="$(< "$output_file")"
    rm -f "$output_file"
    return 0
  else
    command_rc=$?
  fi
  rm -f "$output_file"
  if [ "$purpose" = "preflight" ] && [ "$command_rc" -eq 124 ]; then
    printf 'Amika remote sandbox preflight timed out\n' >&2
  elif [ "$purpose" = "preflight" ]; then
    printf 'Amika remote sandbox preflight failed (amika sandbox list --remote exited %s)\n' "$command_rc" >&2
  elif [ "$command_rc" -eq 124 ]; then
    printf 'Amika sandbox lifecycle list timed out\n' >&2
  else
    printf 'Amika sandbox lifecycle list failed (amika sandbox list --remote exited %s)\n' "$command_rc" >&2
  fi
  return "$command_rc"
}

load_amika_snapshot_table_until() {
  local deadline="$1"
  local output_file
  local command_rc

  output_file="$(mktemp "${TMPDIR:-/tmp}/amika-snapshot-list.XXXXXX")"
  if run_managed_command_until "$deadline" amika snapshot list > "$output_file"; then
    AMIKA_SNAPSHOT_TABLE_RESULT="$(< "$output_file")"
    rm -f "$output_file"
    return 0
  else
    command_rc=$?
  fi
  rm -f "$output_file"
  if [ "$command_rc" -eq 124 ]; then
    printf 'Amika snapshot preflight timed out\n' >&2
  else
    printf 'Amika snapshot preflight failed (amika snapshot list exited %s)\n' "$command_rc" >&2
  fi
  return "$command_rc"
}

reconcile_pending_amika_sandbox() {
  [ "$REMOTE_PROVIDER" = "amika" ] || return 0
  [ -n "$PENDING_AMIKA_SANDBOX" ] || return 0

  local table
  local state
  local deadline

  deadline="$(new_amika_lifecycle_deadline)"
  while [ "$(lifecycle_now_millis)" -lt "$deadline" ]; do
    load_amika_sandbox_table_until "$deadline" || return $?
    table="$AMIKA_SANDBOX_TABLE_RESULT"
    state="$(amika_table_state "$PENDING_AMIKA_SANDBOX" "$table")"
    if [ -n "$state" ]; then
      add_owned_amika_sandbox "$PENDING_AMIKA_SANDBOX"
      PENDING_AMIKA_SANDBOX=""
      return 0
    fi
    sleep_until_amika_poll "$deadline" || break
  done
  printf 'Timed out reconciling pending Amika sandbox registration: %s\n' "$PENDING_AMIKA_SANDBOX" >&2
  return 124
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
    return 127
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
    if [ "$ssh_rc" -ne 0 ]; then
      return "$ssh_rc"
    fi
    rm -rf "$dest"
    mkdir -p "$dest"
    local extract_rc
    set +e
    tr -d '\r' < "$archive_b64" | base64 -d | tar -xzf - -C "$dest"
    extract_rc=$?
    set -e
    return "$extract_rc"
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
  local local_dir="$LOCAL_OUT_DIR/$sandbox"
  local status_file="$local_dir/$strategy-pipeline-status.env"
  local status_tmp
  local setup_rc=0
  local benchmark_rc=0
  local sync_attempted=0
  local sync_rc=0

  mkdir -p "$local_dir"
  prepare_worktree "$sandbox" || setup_rc=$?
  if [ "$setup_rc" -eq 0 ]; then
    sync_local_experiment "$sandbox" || setup_rc=$?
  fi
  if [ "$setup_rc" -eq 0 ]; then
    run_launch_strategy_with_args "$sandbox" "$strategy" "$remote_out_dir" || benchmark_rc=$?
    if [ "$SYNC_ARTIFACTS" = "1" ]; then
      sync_attempted=1
      sync_artifacts "$sandbox" "$strategy" "$remote_out_dir" || sync_rc=$?
    fi
  fi

  status_tmp="$(mktemp "$local_dir/.$strategy-pipeline-status.env.XXXXXX")"
  {
    printf 'setup_rc=%s\n' "$setup_rc"
    printf 'benchmark_rc=%s\n' "$benchmark_rc"
    printf 'sync_attempted=%s\n' "$sync_attempted"
    printf 'sync_rc=%s\n' "$sync_rc"
  } > "$status_tmp"
  mv "$status_tmp" "$status_file"

  if [ "$setup_rc" -ne 0 ]; then
    return "$setup_rc"
  fi
  if [ "$benchmark_rc" -ne 0 ]; then
    return "$benchmark_rc"
  fi
  return "$sync_rc"
}

read_pipeline_status() {
  local file="$1"
  local prefix="$2"
  local key
  local value
  local setup_rc=""
  local benchmark_rc=""
  local sync_attempted=""
  local sync_rc=""

  if [ ! -f "$file" ]; then
    echo "pipeline status file is missing: $file" >&2
    return 1
  fi
  while IFS='=' read -r key value; do
    case "$key" in
      setup_rc) setup_rc="$value" ;;
      benchmark_rc) benchmark_rc="$value" ;;
      sync_attempted) sync_attempted="$value" ;;
      sync_rc) sync_rc="$value" ;;
      *)
        echo "unexpected pipeline status field in $file: $key" >&2
        return 1
        ;;
    esac
  done < "$file"

  if ! [[ "$setup_rc" =~ ^[0-9]+$ ]] ||
    ! [[ "$benchmark_rc" =~ ^[0-9]+$ ]] ||
    ! [[ "$sync_attempted" =~ ^[01]$ ]] ||
    ! [[ "$sync_rc" =~ ^[0-9]+$ ]]; then
    echo "invalid pipeline status record: $file" >&2
    return 1
  fi

  printf -v "${prefix}_setup_rc" '%s' "$setup_rc"
  printf -v "${prefix}_benchmark_rc" '%s' "$benchmark_rc"
  printf -v "${prefix}_sync_attempted" '%s' "$sync_attempted"
  printf -v "${prefix}_sync_rc" '%s' "$sync_rc"
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

collect_process_tree_pids() {
  local parent_pid="$1"
  local child_pid

  while IFS= read -r child_pid; do
    [ -n "$child_pid" ] || continue
    collect_process_tree_pids "$child_pid"
    printf '%s\n' "$child_pid"
  done < <(ps -axo pid=,ppid= | awk -v parent="$parent_pid" '$2 == parent { print $1 }')
}

process_is_active() {
  local pid="$1"
  local state

  kill -0 "$pid" >/dev/null 2>&1 || return 1
  state="$(ps -o stat= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  case "$state" in
    Z*|'') return 1 ;;
    *) return 0 ;;
  esac
}

wait_for_process_tree_exit() {
  local attempts="$1"
  shift
  local pid
  local active
  local attempt=0

  while [ "$attempt" -lt "$attempts" ]; do
    active=0
    for pid in "$@"; do
      if process_is_active "$pid"; then
        active=1
        break
      fi
    done
    [ "$active" -eq 1 ] || return 0
    sleep 0.1
    attempt=$((attempt + 1))
  done
  return 1
}

kill_process_tree_now() {
  local root_pid="$1"
  local pid
  local -a tree_pids=()

  while IFS= read -r pid; do
    [ -n "$pid" ] && tree_pids+=("$pid")
  done < <(collect_process_tree_pids "$root_pid")
  tree_pids+=("$root_pid")
  for pid in "${tree_pids[@]}"; do
    kill -KILL "$pid" >/dev/null 2>&1 || true
  done
}

terminate_process_tree() {
  local root_pid="$1"
  local pid
  local -a tree_pids=()

  while IFS= read -r pid; do
    [ -n "$pid" ] && tree_pids+=("$pid")
  done < <(collect_process_tree_pids "$root_pid")
  tree_pids+=("$root_pid")

  for pid in "${tree_pids[@]}"; do
    process_is_active "$pid" && kill -TERM "$pid" >/dev/null 2>&1 || true
  done
  if ! wait_for_process_tree_exit 50 "${tree_pids[@]}"; then
    for pid in "${tree_pids[@]}"; do
      process_is_active "$pid" && kill -KILL "$pid" >/dev/null 2>&1 || true
    done
    wait_for_process_tree_exit 50 "${tree_pids[@]}" || true
  fi
  wait "$root_pid" >/dev/null 2>&1 || true
}

stop_strategy_pipelines() {
  local signal_rc="$1"
  local pid

  RETAIN_AMIKA_SANDBOXES=0
  trap - INT TERM
  if [ -n "${active_deadline_watchdog_pid:-}" ]; then
    kill_process_tree_now "$active_deadline_watchdog_pid"
  fi
  for pid in "${active_operation_pid:-}" "${locality_pipeline_pid:-}" "${mcp_pipeline_pid:-}"; do
    if [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1; then
      terminate_process_tree "$pid"
    fi
  done
  active_operation_pid=""
  active_deadline_watchdog_pid=""
  exit "$signal_rc"
}

cleanup_amika_sandboxes_on_exit() {
  local original_rc=$?
  local cleanup_rc=0
  local current_cleanup_rc

  trap - EXIT INT TERM
  set +e
  if [ "$RETAIN_AMIKA_SANDBOXES" -eq 1 ]; then
    exit "$original_rc"
  fi
  reconcile_pending_amika_sandbox || cleanup_rc=$?
  cleanup_amika_sandboxes
  current_cleanup_rc=$?
  if [ "$cleanup_rc" -eq 0 ]; then
    cleanup_rc="$current_cleanup_rc"
  fi

  if [ "$cleanup_rc" -ne 0 ]; then
    printf 'Amika sandbox cleanup failed with exit code %s; owned sandboxes:' "$cleanup_rc" >&2
    if [ "${#CREATED_AMIKA_SANDBOXES[@]}" -gt 0 ]; then
      printf ' %s' "${CREATED_AMIKA_SANDBOXES[@]}" >&2
    fi
    printf '\n' >&2
  fi

  if [ "$original_rc" -ne 0 ]; then
    exit "$original_rc"
  fi
  exit "$cleanup_rc"
}

shell_quote_posix() {
  local value="$1"
  local prefix

  printf "'"
  while case "$value" in *"'"*) true ;; *) false ;; esac; do
    prefix="${value%%\'*}"
    printf '%s' "$prefix"
    printf '%s' "'\\''"
    value="${value#*\'}"
  done
  printf "%s'" "$value"
}

shell_append_quoted_args() {
  local arg

  for arg in "$@"; do
    printf ' '
    shell_quote_posix "$arg"
  done
}

amika_recovery_ssh_command() {
  local sandbox="$1"
  printf 'amika sandbox ssh'
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    shell_append_quoted_args "${AMIKA_FLAGS[@]}"
  fi
  shell_append_quoted_args "$sandbox"
}

amika_recovery_target_command() {
  local sandbox="$1"
  printf 'amika sandbox ssh'
  if [ "${#AMIKA_FLAGS[@]}" -gt 0 ]; then
    shell_append_quoted_args "${AMIKA_FLAGS[@]}"
  fi
  shell_append_quoted_args --print "$sandbox"
}

amika_recovery_rsync_command() {
  local sandbox="$1"
  local remote_out_dir="$2"
  local local_artifact_dir="$3"
  local target_command
  local remote_source_path_q
  local local_artifact_dir_q

  target_command="$(amika_recovery_target_command "$sandbox")"
  remote_source_path_q="$(shell_quote_posix "$remote_out_dir/")"
  local_artifact_dir_q="$(shell_quote_posix "$local_artifact_dir/")"
  printf '_amika_recovery_target=$(%s) && rsync -az --delete "${_amika_recovery_target}":%s %s' \
    "$target_command" "$remote_source_path_q" "$local_artifact_dir_q"
}

record_amika_recovery_line() {
  local line="$1"
  printf '%s\n' "$line" >&2
  printf 'recovery=%q\n' "$line" >> "$AMIKA_LIFECYCLE_LOG"
}

print_amika_recovery_instructions() {
  local locality_ssh
  local mcp_ssh
  local locality_rsync
  local mcp_rsync
  local delete_command

  locality_ssh="$(amika_recovery_ssh_command "$LOCALITY_SANDBOX")"
  mcp_ssh="$(amika_recovery_ssh_command "$MCP_SANDBOX")"
  locality_rsync="$(amika_recovery_rsync_command "$LOCALITY_SANDBOX" "$LOCALITY_REMOTE_OUT_DIR" "$LOCAL_OUT_DIR/artifacts/locality")"
  mcp_rsync="$(amika_recovery_rsync_command "$MCP_SANDBOX" "$MCP_REMOTE_OUT_DIR" "$LOCAL_OUT_DIR/artifacts/notion-mcp")"
  delete_command="amika sandbox delete --remote --force$(shell_append_quoted_args "$LOCALITY_SANDBOX" "$MCP_SANDBOX")"

  record_amika_recovery_line "Retaining Amika sandboxes because artifact sync failed"
  record_amika_recovery_line "Locality sandbox: $LOCALITY_SANDBOX"
  record_amika_recovery_line "Locality remote artifact directory: $LOCALITY_REMOTE_OUT_DIR"
  record_amika_recovery_line "  $locality_ssh"
  record_amika_recovery_line "  $locality_rsync"
  record_amika_recovery_line "MCP sandbox: $MCP_SANDBOX"
  record_amika_recovery_line "MCP remote artifact directory: $MCP_REMOTE_OUT_DIR"
  record_amika_recovery_line "  $mcp_ssh"
  record_amika_recovery_line "  $mcp_rsync"
  record_amika_recovery_line "Delete both sandboxes after recovery:"
  record_amika_recovery_line "  $delete_command"
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
trap 'stop_strategy_pipelines 130' INT
trap 'stop_strategy_pipelines 143' TERM
preflight_amika_environment
if [ "$REMOTE_PROVIDER" = "amika" ] && [ "$SYNC_ARTIFACTS" = "0" ]; then
  echo "SYNC_ARTIFACTS=0; ephemeral Amika sandboxes will be deleted without retaining remote artifacts" >&2
fi
create_amika_sandboxes
write_artifacts_manifest

echo "Launching Locality and MCP strategy pipelines in parallel"
run_strategy_pipeline "$LOCALITY_SANDBOX" "locality" "$LOCALITY_REMOTE_OUT_DIR" &
locality_pipeline_pid=$!
run_strategy_pipeline "$MCP_SANDBOX" "notion-mcp" "$MCP_REMOTE_OUT_DIR" &
mcp_pipeline_pid=$!

locality_pipeline_rc=0
mcp_pipeline_rc=0
wait_for_strategy_pipeline "$locality_pipeline_pid" "locality" || locality_pipeline_rc=$?
wait_for_strategy_pipeline "$mcp_pipeline_pid" "notion-mcp" || mcp_pipeline_rc=$?

locality_setup_rc=0
locality_benchmark_rc=0
locality_sync_attempted=0
locality_sync_rc=0
mcp_setup_rc=0
mcp_benchmark_rc=0
mcp_sync_attempted=0
mcp_sync_rc=0
locality_status_read_rc=0
mcp_status_read_rc=0
read_pipeline_status \
  "$LOCAL_OUT_DIR/$LOCALITY_SANDBOX/locality-pipeline-status.env" \
  locality || locality_status_read_rc=$?
read_pipeline_status \
  "$LOCAL_OUT_DIR/$MCP_SANDBOX/notion-mcp-pipeline-status.env" \
  mcp || mcp_status_read_rc=$?

printf 'pipeline_status strategy=locality setup_rc=%s benchmark_rc=%s sync_attempted=%s sync_rc=%s\n' \
  "$locality_setup_rc" "$locality_benchmark_rc" "$locality_sync_attempted" "$locality_sync_rc" >> "$AMIKA_LIFECYCLE_LOG"
printf 'pipeline_status strategy=notion-mcp setup_rc=%s benchmark_rc=%s sync_attempted=%s sync_rc=%s\n' \
  "$mcp_setup_rc" "$mcp_benchmark_rc" "$mcp_sync_attempted" "$mcp_sync_rc" >> "$AMIKA_LIFECYCLE_LOG"

if [ "$REMOTE_PROVIDER" = "amika" ] && [ "$SYNC_ARTIFACTS" = "1" ] &&
  { { [ "$locality_sync_attempted" -eq 1 ] && [ "$locality_sync_rc" -ne 0 ]; } ||
    { [ "$mcp_sync_attempted" -eq 1 ] && [ "$mcp_sync_rc" -ne 0 ]; }; }; then
  RETAIN_AMIKA_SANDBOXES=1
  printf 'retained_sandboxes locality=%q mcp=%q\n' "$LOCALITY_SANDBOX" "$MCP_SANDBOX" >> "$AMIKA_LIFECYCLE_LOG"
  print_amika_recovery_instructions
fi

operation_rc=0
for phase_rc in \
  "$locality_setup_rc" "$mcp_setup_rc" \
  "$locality_benchmark_rc" "$mcp_benchmark_rc" \
  "$locality_sync_rc" "$mcp_sync_rc"; do
  if [ "$phase_rc" -ne 0 ]; then
    operation_rc="$phase_rc"
    break
  fi
done
if [ "$operation_rc" -eq 0 ] && [ "$locality_status_read_rc" -ne 0 ]; then
  operation_rc="$locality_pipeline_rc"
  [ "$operation_rc" -ne 0 ] || operation_rc="$locality_status_read_rc"
fi
if [ "$operation_rc" -eq 0 ] && [ "$mcp_status_read_rc" -ne 0 ]; then
  operation_rc="$mcp_pipeline_rc"
  [ "$operation_rc" -ne 0 ] || operation_rc="$mcp_status_read_rc"
fi

if [ "$operation_rc" -ne 0 ]; then
  echo "Launch-readiness strategy pipelines failed: locality=$locality_pipeline_rc notion-mcp=$mcp_pipeline_rc" >&2
  exit "$operation_rc"
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
