#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WRAPPER="${ROOT}/experiment/locality-mcp-comparison/run-agent-comparison.sh"

fail() {
  printf 'launch readiness Amika split wrapper test: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local path="$1"
  local needle="$2"
  grep -F -q -- "$needle" "$path" || fail "missing ${needle} in ${path}"
}

assert_not_contains() {
  local path="$1"
  local needle="$2"
  if grep -F -q -- "$needle" "$path"; then
    fail "unexpected ${needle} in ${path}"
  fi
}

assert_line_before() {
  local path="$1"
  local first="$2"
  local second="$3"
  local first_line
  local second_line

  first_line="$(grep -nF -- "$first" "$path" | tail -n 1 | cut -d: -f1)"
  second_line="$(grep -nF -- "$second" "$path" | head -n 1 | cut -d: -f1)"
  if [ -z "$first_line" ] || [ -z "$second_line" ] || [ "$first_line" -ge "$second_line" ]; then
    fail "expected ${first} before ${second} in ${path}"
  fi
}

assert_occurrences() {
  local path="$1"
  local needle="$2"
  local expected="$3"
  local actual

  actual="$(grep -F -c -- "$needle" "$path" || true)"
  if [ "$actual" -ne "$expected" ]; then
    fail "expected ${expected} occurrences of ${needle} in ${path}, got ${actual}"
  fi
}

assert_line_between() {
  local path="$1"
  local first="$2"
  local middle="$3"
  local last="$4"
  local first_line
  local middle_line
  local last_line

  first_line="$(grep -nF -- "$first" "$path" | head -n 1 | cut -d: -f1 || true)"
  last_line="$(grep -nF -- "$last" "$path" | tail -n 1 | cut -d: -f1 || true)"
  middle_line="$(awk -v first="$first_line" -v last="$last_line" -v middle="$middle" '
    NR > first && NR < last && index($0, middle) { print NR; exit }
  ' "$path")"
  if [ -z "$first_line" ] || [ -z "$middle_line" ] || [ -z "$last_line" ]; then
    fail "expected ${middle} between ${first} and ${last} in ${path}"
  fi
}

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-launch-readiness-amika-wrapper-test.XXXXXX")"
cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

export AMIKA_READINESS_TIMEOUT_SECONDS=3
export AMIKA_READINESS_POLL_SECONDS=1

fake_bin="${tmp_root}/bin"
fake_log="${tmp_root}/amika.log"
mkdir -p "$fake_bin"

cat > "${fake_bin}/amika" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

joined_args=""
logged_command="amika"
for arg in "$@"; do
  printf -v quoted_arg '%q' "$arg"
  logged_command="${logged_command} ${quoted_arg}"
  joined_args="${joined_args} ${arg}"
done
printf '%s\n' "$logged_command" >> "${FAKE_AMIKA_LOG:?}"

state_dir="${FAKE_AMIKA_STATE_DIR:-${FAKE_AMIKA_LOG}.state}"
mkdir -p "$state_dir"

argument_value() {
  local wanted="$1"
  shift
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "$wanted" ] && [ "$#" -gt 1 ]; then
      printf '%s\n' "$2"
      return 0
    fi
    shift
  done
  return 1
}

block_operation_if_requested() {
  local operation="$1"
  if [ "${FAKE_AMIKA_BLOCK_OPERATION:-}" != "$operation" ]; then
    return 0
  fi
  if [ -n "${FAKE_AMIKA_BLOCK_MATCH:-}" ] && [[ "$joined_args" != *"${FAKE_AMIKA_BLOCK_MATCH}"* ]]; then
    return 0
  fi

  local activity_dir="${FAKE_AMIKA_ACTIVITY_DIR:?}"
  local active_file="$activity_dir/active.$$"
  mkdir -p "$activity_dir"
  if [ "$operation" = "create" ] && [ "${FAKE_AMIKA_DELAY_REGISTRATION_AFTER_STOP:-0}" = "1" ]; then
    : > "$state_dir/$sandbox_name.pending-registration"
  fi
  : > "$active_file"
  : > "$activity_dir/ready.$$"
  stop_blocked_operation() {
    rm -f "$active_file"
    : > "$activity_dir/stopped.$$"
    exit 143
  }
  trap stop_blocked_operation INT TERM
  while :; do
    sleep 0.1
  done
}

fail_delete_while_operation_is_active() {
  local active_file
  local active_pid
  [ -n "${FAKE_AMIKA_ACTIVITY_DIR:-}" ] || return 0
  for active_file in "$FAKE_AMIKA_ACTIVITY_DIR"/active.*; do
    [ -e "$active_file" ] || continue
    active_pid="${active_file##*.}"
    if kill -0 "$active_pid" >/dev/null 2>&1; then
      printf 'delete while remote child active: %s\n' "$active_pid" >> "$FAKE_AMIKA_LOG"
      exit 98
    fi
  done
}

if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "delete" ] && [ "${FAKE_AMIKA_FAIL_DELETE:-0}" = "1" ]; then
  exit 29
fi
if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "ssh" ] && [ -n "${FAKE_AMIKA_FAIL_SSH_RC:-}" ] && [[ "$joined_args" != *" -- true"* ]] && [[ "$joined_args" != *"AMIKA_PREREQUISITE_CHECK"* ]]; then
  exit "$FAKE_AMIKA_FAIL_SSH_RC"
fi
if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "ssh" ] && [ "${3:-}" != "--print" ] && [ -n "${FAKE_AMIKA_FAIL_SSH_CALL:-}" ] && [[ "$joined_args" != *" -- true"* ]] && [[ "$joined_args" != *"AMIKA_PREREQUISITE_CHECK"* ]]; then
  call_file="${FAKE_AMIKA_CALL_DIR:?}/${3}.count"
  mkdir -p "$FAKE_AMIKA_CALL_DIR"
  call_count=0
  if [ -f "$call_file" ]; then
    call_count="$(cat "$call_file")"
  fi
  call_count=$((call_count + 1))
  printf '%s\n' "$call_count" > "$call_file"
  if [ "$call_count" -eq "$FAKE_AMIKA_FAIL_SSH_CALL" ]; then
    exit "${FAKE_AMIKA_FAIL_SSH_CALL_RC:-47}"
  fi
fi

case "${1:-}:${2:-}" in
  sandbox:list)
    if [ "$#" -ne 3 ] || [ "${3:-}" != "--remote" ]; then
      printf 'unexpected fake amika command: %s\n' "$*" >&2
      exit 2
    fi
    if [ -n "${FAKE_AMIKA_SANDBOX_LIST_RC:-}" ]; then
      printf 'fake remote sandbox list failure\n' >&2
      exit "$FAKE_AMIKA_SANDBOX_LIST_RC"
    fi
    if [ "${FAKE_AMIKA_BLOCK_LIST_AFTER_CREATE:-0}" = "1" ] && compgen -G "$state_dir/*.state" >/dev/null; then
      block_operation_if_requested list
    fi
    printf '%s\n' "${FAKE_AMIKA_SANDBOX_TABLE:-NAME STATE LOCATION BRANCH REPO CREATOR CREATED
existing-box stopped remote - - Test 2026-08-02T00:00:00Z}"
    for pending_file in "$state_dir"/*.pending-registration; do
      [ -e "$pending_file" ] || continue
      sandbox_name="$(basename "$pending_file" .pending-registration)"
      delayed_count_file="$state_dir/$sandbox_name.delayed-list-count"
      delayed_count=0
      [ ! -f "$delayed_count_file" ] || delayed_count="$(cat "$delayed_count_file")"
      delayed_count=$((delayed_count + 1))
      printf '%s\n' "$delayed_count" > "$delayed_count_file"
      if [ "$delayed_count" -gt "${FAKE_AMIKA_DELAY_REGISTRATION_LISTS:-1}" ]; then
        printf 'failed\n' > "$state_dir/$sandbox_name.state"
        rm -f "$pending_file"
      fi
    done
    for state_file in "$state_dir"/*.state; do
      [ -e "$state_file" ] || continue
      sandbox_name="$(basename "$state_file" .state)"
      sandbox_state="$(cat "$state_file")"
      if [ "$sandbox_state" = "initializing" ] && { [ -z "${FAKE_AMIKA_INITIALIZING_SANDBOX:-}" ] || [ "$sandbox_name" = "$FAKE_AMIKA_INITIALIZING_SANDBOX" ]; }; then
        list_count_file="$state_dir/$sandbox_name.list-count"
        list_count=0
        [ ! -f "$list_count_file" ] || list_count="$(cat "$list_count_file")"
        list_count=$((list_count + 1))
        printf '%s\n' "$list_count" > "$list_count_file"
        if [ "$list_count" -gt "${FAKE_AMIKA_INITIALIZING_LISTS:-1}" ]; then
          sandbox_state="started"
          printf '%s\n' "$sandbox_state" > "$state_file"
        fi
      fi
      printf '%s %s remote - - Test 2026-08-02T00:00:00Z\n' "$sandbox_name" "$sandbox_state"
    done
    exit 0
    ;;
  snapshot:list)
    if [ "$#" -ne 2 ]; then
      printf 'unexpected fake amika command: %s\n' "$*" >&2
      exit 2
    fi
    if [ -n "${FAKE_AMIKA_SNAPSHOT_TABLE_PATH:-}" ]; then
      cat "$FAKE_AMIKA_SNAPSHOT_TABLE_PATH"
      exit 0
    fi
    printf '%s\n' "${FAKE_AMIKA_SNAPSHOT_TABLE:-NAME STATE PROVIDER SOURCE CREATED
locality-snapshot active daytona aseem-locality 2026-08-02T00:00:00Z
mcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z}"
    exit 0
    ;;
  sandbox:create)
    sandbox_name="$(argument_value --name "$@")"
    snapshot_name="$(argument_value --snapshot "$@")"
    if [ "${FAKE_AMIKA_REGISTER_BEFORE_CREATE_BLOCK:-0}" = "1" ]; then
      printf 'failed\n' > "$state_dir/$sandbox_name.state"
    fi
    block_operation_if_requested create
    attempt_file="$state_dir/$sandbox_name.create-count"
    create_attempt=0
    [ ! -f "$attempt_file" ] || create_attempt="$(cat "$attempt_file")"
    create_attempt=$((create_attempt + 1))
    printf '%s\n' "$create_attempt" > "$attempt_file"
    if { [ -z "${FAKE_AMIKA_CREATE_FAIL_SNAPSHOT:-}" ] || [ "$snapshot_name" = "$FAKE_AMIKA_CREATE_FAIL_SNAPSHOT" ]; } && [ "$create_attempt" -le "${FAKE_AMIKA_CREATE_FAIL_COUNT:-0}" ]; then
      printf 'failed\n' > "$state_dir/$sandbox_name.state"
      exit "${FAKE_AMIKA_FAIL_CREATE_RC:-23}"
    fi
    if [ -n "${FAKE_AMIKA_INITIALIZING_SANDBOX:-}" ] && [ "$sandbox_name" = "$FAKE_AMIKA_INITIALIZING_SANDBOX" ]; then
      printf 'initializing\n' > "$state_dir/$sandbox_name.state"
    else
      printf 'started\n' > "$state_dir/$sandbox_name.state"
    fi
    exit 0
    ;;
  sandbox:delete)
    fail_delete_while_operation_is_active
    shift 2
    for arg in "$@"; do
      case "$arg" in
        --remote|--force) ;;
        *) rm -f "$state_dir/$arg.state" "$state_dir/$arg.list-count" "$state_dir/$arg.pending-registration" "$state_dir/$arg.delayed-list-count" ;;
      esac
    done
    exit 0
    ;;
  sandbox:ssh)
    if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK"* ]] && [ "${FAKE_AMIKA_BLOCK_PREREQUISITE:-0}" = "1" ]; then
      block_operation_if_requested prerequisite
    elif [[ "$joined_args" == *" -- true"* ]] && [ "${FAKE_AMIKA_BLOCK_READINESS:-0}" = "1" ]; then
      block_operation_if_requested ssh
    elif [[ "$joined_args" != *" -- true"* ]] && [[ "$joined_args" != *"AMIKA_PREREQUISITE_CHECK"* ]]; then
      block_operation_if_requested ssh
    fi
    ;;
  *)
    printf 'unexpected fake amika command: %s\n' "$*" >&2
    exit 2
    ;;
esac

if [ "${3:-}" = "--print" ]; then
  printf 'fake-user@fake-host-%s\n' "${4:-missing}"
  exit 0
fi

if [[ "$joined_args" == *" -- true"* ]]; then
  readiness_sandbox="${3:-}"
  if [ -z "${FAKE_AMIKA_READINESS_SSH_FAIL_SANDBOX:-}" ] || [ "$readiness_sandbox" = "$FAKE_AMIKA_READINESS_SSH_FAIL_SANDBOX" ]; then
    readiness_count_file="$state_dir/$readiness_sandbox.readiness-count"
    readiness_count=0
    [ ! -f "$readiness_count_file" ] || readiness_count="$(cat "$readiness_count_file")"
    readiness_count=$((readiness_count + 1))
    printf '%s\n' "$readiness_count" > "$readiness_count_file"
    if [ "$readiness_count" -le "${FAKE_AMIKA_READINESS_SSH_FAILURES:-0}" ]; then
      exit 255
    fi
  fi
  exit 0
fi

evaluate_prerequisite_if_requested() {
  [ "${FAKE_AMIKA_EVALUATE_PREREQUISITES:-0}" = "1" ] || return 0
  local overrides_b64
  local overrides
  overrides_b64="$(printf '%s\n' "$joined_args" | sed -n 's/.*prerequisite_overrides_b64=\([A-Za-z0-9+\/=]*\).*/\1/p' | head -n 1)"
  overrides="$(printf '%s' "$overrides_b64" | base64 -d 2>/dev/null || true)"
  (
    export LOCALITY_CONTEXT_DIRS="${FAKE_REMOTE_LOCALITY_CONTEXT_DIRS:-}"
    export LOCALITY_CONTEXT_ROOTS="${FAKE_REMOTE_LOCALITY_CONTEXT_ROOTS:-}"
    export LINEAR_API_KEY="${FAKE_REMOTE_LINEAR_API_KEY:-}"
    export NOTION_API_TOKEN="${FAKE_REMOTE_NOTION_API_TOKEN:-}"
    export NOTION_TOKEN="${FAKE_REMOTE_NOTION_TOKEN:-}"
    export NOTION_ACCESS_TOKEN="${FAKE_REMOTE_NOTION_ACCESS_TOKEN:-}"
    export SLACK_BOT_TOKEN="${FAKE_REMOTE_SLACK_BOT_TOKEN:-}"
    export SLACK_TEAM_ID="${FAKE_REMOTE_SLACK_TEAM_ID:-}"
    eval "$overrides"

    if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK=locality"* ]]; then
      effective_roots="${LOCALITY_CONTEXT_DIRS:-${LOCALITY_CONTEXT_ROOTS:-}}"
      if [ -n "$effective_roots" ] && [ "${FAKE_REMOTE_LOCALITY_ROOTS_EXIST:-0}" != "1" ]; then
        exit 64
      fi
      exit 0
    fi

    [ -n "${LINEAR_API_KEY:-}" ] || [ "${FAKE_REMOTE_LINEAR_SECRET_FILE:-0}" = "1" ] || exit 65
    [ -n "${NOTION_API_TOKEN:-${NOTION_TOKEN:-${NOTION_ACCESS_TOKEN:-}}}" ] || [ "${FAKE_REMOTE_NOTION_SECRET_FILE:-0}" = "1" ] || exit 66
    [ -n "${SLACK_BOT_TOKEN:-}" ] || [ "${FAKE_REMOTE_SLACK_BOT_SECRET_FILE:-0}" = "1" ] || exit 67
    [ -n "${SLACK_TEAM_ID:-}" ] || [ "${FAKE_REMOTE_SLACK_TEAM_SECRET_FILE:-0}" = "1" ] || exit 68
  )
}

if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK"* ]]; then
  evaluate_prerequisite_if_requested || exit $?
fi

if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK"* ]] && [ "${FAKE_AMIKA_FAIL_LOCALITY_PREREQUISITE:-0}" = "1" ] && [[ "${3:-}" == *locality* ]]; then
  exit 61
fi
if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK=notion-mcp"* ]] && [ "${FAKE_AMIKA_REQUIRE_EXPERIMENT_ENV_SOURCE:-0}" = "1" ] && [[ "$joined_args" != *".config/locality-experiment/env"* ]]; then
  exit 62
fi
if [[ "$joined_args" == *"AMIKA_PREREQUISITE_CHECK=locality"* ]] && [ "${FAKE_AMIKA_REQUIRE_COLON_ROOT_SPLIT:-0}" = "1" ] && [[ "$joined_args" != *"tr"* ]]; then
  exit 63
fi

if [ -n "${FAKE_AMIKA_CONCURRENCY_DIR:-}" ] && [[ "$joined_args" != *"AMIKA_PREREQUISITE_CHECK"* ]]; then
  strategy=""
  other_strategy=""
  case "$joined_args" in
    *launch-readiness-testrun-locality*)
      strategy="locality"
      other_strategy="notion-mcp"
      ;;
    *launch-readiness-testrun-mcp*)
      strategy="notion-mcp"
      other_strategy="locality"
      ;;
  esac
  if [ -n "$strategy" ]; then
    mkdir -p "$FAKE_AMIKA_CONCURRENCY_DIR"
    : > "$FAKE_AMIKA_CONCURRENCY_DIR/$strategy.started"
    attempt=0
    while [ "$attempt" -lt 30 ] && [ ! -f "$FAKE_AMIKA_CONCURRENCY_DIR/$other_strategy.started" ]; do
      sleep 0.1
      attempt=$((attempt + 1))
    done
    if [ ! -f "$FAKE_AMIKA_CONCURRENCY_DIR/$other_strategy.started" ]; then
      printf '%s did not overlap %s\n' "$strategy" "$other_strategy" >&2
      exit 42
    fi
    : > "$FAKE_AMIKA_CONCURRENCY_DIR/$strategy.overlapped"
  fi
fi

printf 'fake remote ok\n'
SH
chmod +x "${fake_bin}/amika"

cat > "${fake_bin}/zsh" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "${fake_bin}/zsh"

cat > "${fake_bin}/rsync" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

logged_command="rsync"
for arg in "$@"; do
  printf -v quoted_arg '%q' "$arg"
  logged_command="${logged_command} ${quoted_arg}"
done
printf '%s\n' "$logged_command" >> "${FAKE_AMIKA_LOG:?}"
SH
chmod +x "${fake_bin}/rsync"

cat > "${fake_bin}/ssh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

logged_command="ssh"
for arg in "$@"; do
  printf -v quoted_arg '%q' "$arg"
  logged_command="${logged_command} ${quoted_arg}"
done
printf '%s\n' "$logged_command" >> "${FAKE_TRANSPORT_LOG:?}"
SH
chmod +x "${fake_bin}/ssh"

signal_runner="${tmp_root}/run-and-signal.py"
cat > "$signal_runner" <<'PY'
import glob
import os
import signal
import subprocess
import sys
import time

signal_name, activity_dir, ready_count, stdout_path, stderr_path, wrapper, *wrapper_args = sys.argv[1:]
with open(stdout_path, "wb") as stdout_file, open(stderr_path, "wb") as stderr_file:
    process = subprocess.Popen(
        [wrapper, *wrapper_args],
        stdout=stdout_file,
        stderr=stderr_file,
        start_new_session=True,
    )
    deadline = time.monotonic() + 10
    while len(glob.glob(os.path.join(activity_dir, "ready.*"))) < int(ready_count):
        if process.poll() is not None:
            raise SystemExit(f"wrapper exited before signal injection: {process.returncode}")
        if time.monotonic() >= deadline:
            os.killpg(process.pid, signal.SIGKILL)
            raise SystemExit("timed out waiting for blocked fake Amika child")
        time.sleep(0.05)

    os.kill(process.pid, getattr(signal, f"SIG{signal_name}"))
    try:
        return_code = process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        raise SystemExit("wrapper did not exit after signal injection")

try:
    os.killpg(process.pid, signal.SIGKILL)
except ProcessLookupError:
    pass
print(return_code)
PY

deadline_runner="${tmp_root}/run-with-deadline.py"
cat > "$deadline_runner" <<'PY'
import os
import signal
import subprocess
import sys

timeout_seconds, stdout_path, stderr_path, wrapper, *wrapper_args = sys.argv[1:]
with open(stdout_path, "wb") as stdout_file, open(stderr_path, "wb") as stderr_file:
    process = subprocess.Popen(
        [wrapper, *wrapper_args],
        stdout=stdout_file,
        stderr=stderr_file,
        start_new_session=True,
    )
    try:
        return_code = process.wait(timeout=float(timeout_seconds))
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        raise SystemExit(125)
raise SystemExit(return_code)
PY

pty_runner="${tmp_root}/run-with-pty.py"
cat > "$pty_runner" <<'PY'
import errno
import os
import pty
import sys

transcript_path, *command = sys.argv[1:]
child_pid, master_fd = pty.fork()
if child_pid == 0:
    os.execvpe(command[0], command, os.environ)

with open(transcript_path, "wb") as transcript:
    while True:
        try:
            output = os.read(master_fd, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not output:
            break
        transcript.write(output)
os.close(master_fd)
_, wait_status = os.waitpid(child_pid, 0)
raise SystemExit(os.waitstatus_to_exitcode(wait_status))
PY

forced_tty_bin="${tmp_root}/forced-tty-bin"
mkdir -p "$forced_tty_bin"
cat > "$forced_tty_bin/ssh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

printf 'ssh' >> "${FAKE_TRANSPORT_LOG:?}"
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_TRANSPORT_LOG"
done
printf '\n' >> "$FAKE_TRANSPORT_LOG"

if [ "${FAKE_SSH_INVALID_ARTIFACT:-0}" = "1" ] && [[ " $* " == *tar* ]]; then
  printf 'not-a-base64-tar-archive\n'
  exit 0
fi
printf '\n__AMIKA_REMOTE_RC__=0\n'
SH
chmod +x "$forced_tty_bin/ssh"

real_tar="$(command -v tar)"
cat > "$forced_tty_bin/tar" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = "-xzf" ] && [ "${FAKE_TAR_EXTRACT_RC:-0}" -ne 0 ]; then
  exit "$FAKE_TAR_EXTRACT_RC"
fi
exec "${REAL_TAR:?}" "$@"
SH
chmod +x "$forced_tty_bin/tar"

help_out="${tmp_root}/help.out"
"$WRAPPER" --help > "$help_out"
assert_contains "$help_out" "LOCALITY_SNAPSHOT=locality-snapshot"
assert_contains "$help_out" "MCP_SNAPSHOT=mcp-snapshot"
assert_contains "$help_out" "Created Amika sandboxes are deleted automatically after artifact collection."

run_default_out="${tmp_root}/default-out"
concurrency_dir="${tmp_root}/concurrency"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$fake_log" \
  FAKE_AMIKA_CONCURRENCY_DIR="$concurrency_dir" \
  RUN_ID="testrun" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="$run_default_out" \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  CODEX_EXEC_TIMEOUT_SECONDS=12 \
  "$WRAPPER" --scenario scenario2 >/dev/null

assert_contains "$run_default_out/run.env" "locality_sandbox=launch-readiness-testrun-locality"
assert_contains "$run_default_out/run.env" "mcp_sandbox=launch-readiness-testrun-mcp"
assert_contains "$run_default_out/run.env" "locality_snapshot=locality-snapshot"
assert_contains "$run_default_out/run.env" "mcp_snapshot=mcp-snapshot"
assert_contains "$run_default_out/run.env" "remote_worktree=/home/amika/workspace/locality-launch-readiness-testrun"
assert_contains "$run_default_out/run.env" "locality_remote_out_dir=/home/amika/workspace/locality-launch-readiness-testrun/target/launch-readiness-testrun-locality"
assert_contains "$run_default_out/run.env" "mcp_remote_out_dir=/home/amika/workspace/locality-launch-readiness-testrun/target/launch-readiness-testrun-mcp"
assert_contains "$run_default_out/run.env" "remote_loc_bin=/usr/bin/loc"
assert_contains "$run_default_out/run.env" "sync_artifacts=0"
assert_contains "$run_default_out/run.env" "strategy_execution=parallel"
assert_contains "$run_default_out/artifacts.tsv" "locality"$'\t'"launch-readiness-testrun-locality"
assert_contains "$run_default_out/artifacts.tsv" "notion-mcp"$'\t'"launch-readiness-testrun-mcp"
assert_contains "$fake_log" "amika sandbox list --remote"
assert_contains "$fake_log" "amika snapshot list"
assert_not_contains "$fake_log" "-o json"
assert_contains "$fake_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-testrun-locality"
assert_contains "$fake_log" "amika sandbox create --remote --no-git --snapshot mcp-snapshot --name launch-readiness-testrun-mcp"
assert_contains "$fake_log" "amika sandbox delete --remote --force launch-readiness-testrun-locality launch-readiness-testrun-mcp"
assert_not_contains "$fake_log" "amika sandbox start"
assert_line_before "$fake_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-testrun-locality" "amika sandbox ssh launch-readiness-testrun-locality -- true"
assert_line_before "$fake_log" "amika sandbox ssh launch-readiness-testrun-locality -- true" "amika sandbox create --remote --no-git --snapshot mcp-snapshot --name launch-readiness-testrun-mcp"
assert_line_before "$fake_log" "amika sandbox create --remote --no-git --snapshot mcp-snapshot --name launch-readiness-testrun-mcp" "amika sandbox ssh launch-readiness-testrun-mcp -- true"
assert_line_before "$fake_log" "amika sandbox ssh" "amika sandbox delete --remote --force launch-readiness-testrun-locality launch-readiness-testrun-mcp"
test -f "$concurrency_dir/locality.overlapped" || fail "Locality launch did not overlap MCP launch"
test -f "$concurrency_dir/notion-mcp.overlapped" || fail "MCP launch did not overlap Locality launch"

assert_contains "$fake_log" "launch-readiness-testrun-locality"
assert_contains "$fake_log" "launch-readiness-testrun-mcp"
assert_contains "$fake_log" "locality"
assert_contains "$fake_log" "notion-mcp"
assert_contains "$fake_log" "--scenario"
assert_contains "$fake_log" "scenario2"
assert_not_contains "$fake_log" "test-with-notion-connector"
assert_not_contains "$fake_log" "onyx-falcon"

preflight_list_failure_log="${tmp_root}/preflight-list-failure-amika.log"
preflight_list_failure_err="${tmp_root}/preflight-list-failure.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$preflight_list_failure_log" \
  FAKE_AMIKA_SANDBOX_LIST_RC=41 \
  RUN_ID="preflight-list-failure" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/preflight-list-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$preflight_list_failure_err"
preflight_list_failure_rc=$?
set -e
if [ "$preflight_list_failure_rc" -ne 41 ]; then
  fail "remote sandbox-list failure should preserve exit 41, got ${preflight_list_failure_rc}"
fi
assert_contains "$preflight_list_failure_err" "Amika remote sandbox preflight failed"
assert_contains "$preflight_list_failure_err" "fake remote sandbox list failure"
assert_not_contains "$preflight_list_failure_log" "amika sandbox create"

missing_snapshot_log="${tmp_root}/missing-snapshot-amika.log"
missing_snapshot_err="${tmp_root}/missing-snapshot.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$missing_snapshot_log" \
  FAKE_AMIKA_SNAPSHOT_TABLE=$'NAME STATE PROVIDER SOURCE CREATED\nmcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z' \
  RUN_ID="missing-snapshot" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/missing-snapshot-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$missing_snapshot_err"
missing_snapshot_rc=$?
set -e
if [ "$missing_snapshot_rc" -eq 0 ]; then
  fail "a missing required Amika snapshot should fail before creation"
fi
assert_contains "$missing_snapshot_err" "required Amika snapshot not found: locality-snapshot"
assert_not_contains "$missing_snapshot_log" "amika sandbox create"

failed_snapshot_log="${tmp_root}/failed-snapshot-amika.log"
failed_snapshot_err="${tmp_root}/failed-snapshot.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$failed_snapshot_log" \
  FAKE_AMIKA_SNAPSHOT_TABLE=$'NAME STATE PROVIDER SOURCE CREATED\nlocality-snapshot failed daytona aseem-locality 2026-08-02T00:00:00Z\nmcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z' \
  RUN_ID="failed-snapshot" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/failed-snapshot-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$failed_snapshot_err"
failed_snapshot_rc=$?
set -e
if [ "$failed_snapshot_rc" -eq 0 ]; then
  fail "an inactive required Amika snapshot should fail before creation"
fi
assert_contains "$failed_snapshot_err" "required Amika snapshot is not active: locality-snapshot (state=failed)"
assert_not_contains "$failed_snapshot_log" "amika sandbox create"

large_snapshot_log="${tmp_root}/large-snapshot-amika.log"
large_snapshot_payload="$(printf '%*s' 350000 '' | tr ' ' x)"
large_snapshot_table_path="${tmp_root}/large-snapshot-table.txt"
printf '%s\n' 'NAME STATE PROVIDER SOURCE CREATED' > "$large_snapshot_table_path"
printf '%s\n' 'locality-snapshot active daytona aseem-locality 2026-08-02T00:00:00Z' >> "$large_snapshot_table_path"
printf 'filler-snapshot active daytona %s 2026-08-02T00:00:00Z\n' "$large_snapshot_payload" >> "$large_snapshot_table_path"
printf '%s\n' 'mcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z' >> "$large_snapshot_table_path"
if [ "$(wc -c < "$large_snapshot_table_path")" -lt 349000 ]; then
  fail "large snapshot table fixture must exceed the pipe buffer"
fi
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$large_snapshot_log" \
  FAKE_AMIKA_SNAPSHOT_TABLE_PATH="$large_snapshot_table_path" \
  RUN_ID="large-snapshot-table" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/large-snapshot-table-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/large-snapshot-table.err"
large_snapshot_rc=$?
set -e
if [ "$large_snapshot_rc" -ne 0 ]; then
  cat "${tmp_root}/large-snapshot-table.err" >&2
  fail "an active large snapshot table should reach sandbox creation, got ${large_snapshot_rc}"
fi
assert_contains "$large_snapshot_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-large-snapshot-table-locality"

invalid_attempts_log="${tmp_root}/invalid-attempts-amika.log"
invalid_attempts_err="${tmp_root}/invalid-attempts.err"
: > "$invalid_attempts_log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$invalid_attempts_log" \
  AMIKA_CREATE_ATTEMPTS=0 \
  RUN_ID="invalid-attempts" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/invalid-attempts-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$invalid_attempts_err"
invalid_attempts_rc=$?
set -e
if [ "$invalid_attempts_rc" -eq 0 ]; then
  fail "AMIKA_CREATE_ATTEMPTS=0 should fail before creation"
fi
assert_contains "$invalid_attempts_err" "AMIKA_CREATE_ATTEMPTS must be a positive integer"
assert_not_contains "$invalid_attempts_log" "amika sandbox create"

collision_log="${tmp_root}/collision-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$collision_log" \
  FAKE_AMIKA_SANDBOX_TABLE=$'NAME STATE LOCATION BRANCH REPO CREATOR CREATED\nexisting-box stopped remote - - Test 2026-08-02T00:00:00Z\nlaunch-readiness-collision-locality stopped remote - - Test 2026-08-02T00:00:00Z' \
  RUN_ID="collision" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/collision-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/collision.err"
collision_rc=$?
set -e
if [ "$collision_rc" -eq 0 ]; then
  fail "an existing Amika sandbox name should fail before creation"
fi
assert_not_contains "$collision_log" "amika sandbox create"
assert_not_contains "$collision_log" "amika sandbox delete"

mcp_collision_log="${tmp_root}/mcp-collision-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$mcp_collision_log" \
  FAKE_AMIKA_SANDBOX_TABLE=$'NAME STATE LOCATION BRANCH REPO CREATOR CREATED\nexisting-box stopped remote - - Test 2026-08-02T00:00:00Z\nlaunch-readiness-mcp-collision-mcp stopped remote - - Test 2026-08-02T00:00:00Z' \
  RUN_ID="mcp-collision" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/mcp-collision-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/mcp-collision.err"
mcp_collision_rc=$?
set -e
if [ "$mcp_collision_rc" -eq 0 ]; then
  fail "an existing MCP Amika sandbox name should fail before creation"
fi
assert_not_contains "$mcp_collision_log" "amika sandbox create"
assert_not_contains "$mcp_collision_log" "amika sandbox delete"

retry_create_log="${tmp_root}/retry-create-amika.log"
retry_create_state="${tmp_root}/retry-create-state"
retry_create_out="${tmp_root}/retry-create-out"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$retry_create_log" \
  FAKE_AMIKA_STATE_DIR="$retry_create_state" \
  FAKE_AMIKA_CREATE_FAIL_SNAPSHOT="locality-snapshot" \
  FAKE_AMIKA_CREATE_FAIL_COUNT=1 \
  FAKE_AMIKA_READINESS_SSH_FAIL_SANDBOX="launch-readiness-retry-create-locality" \
  FAKE_AMIKA_READINESS_SSH_FAILURES=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="retry-create" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="$retry_create_out" \
  "$WRAPPER" --scenario scenario2 >/dev/null
assert_occurrences "$retry_create_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-retry-create-locality" 2
assert_contains "$retry_create_log" "amika sandbox delete --remote --force launch-readiness-retry-create-locality"
assert_line_between "$retry_create_log" \
  "amika sandbox delete --remote --force launch-readiness-retry-create-locality" \
  "amika sandbox list --remote" \
  "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-retry-create-locality"
assert_occurrences "$retry_create_log" "amika sandbox ssh launch-readiness-retry-create-locality -- true" 2
assert_contains "$retry_create_out/amika-lifecycle.log" "strategy=locality sandbox=launch-readiness-retry-create-locality readiness_seconds="

exhausted_log="${tmp_root}/exhausted-amika.log"
exhausted_err="${tmp_root}/exhausted.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$exhausted_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/exhausted-state" \
  FAKE_AMIKA_CREATE_FAIL_SNAPSHOT="locality-snapshot" \
  FAKE_AMIKA_CREATE_FAIL_COUNT=3 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="exhausted" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/exhausted-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$exhausted_err"
exhausted_rc=$?
set -e
if [ "$exhausted_rc" -eq 0 ]; then
  fail "three failed exact-snapshot creates should fail provisioning"
fi
assert_occurrences "$exhausted_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-exhausted-locality" 3
assert_occurrences "$exhausted_log" "amika sandbox delete --remote --force launch-readiness-exhausted-locality" 3
assert_contains "$exhausted_err" "Amika provisioning exhausted for locality: exact snapshot locality-snapshot failed after 3 attempts (last state=failed)"
assert_not_contains "$exhausted_err" "fallback"

initializing_log="${tmp_root}/initializing-amika.log"
initializing_state="${tmp_root}/initializing-state"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$initializing_log" \
  FAKE_AMIKA_STATE_DIR="$initializing_state" \
  FAKE_AMIKA_INITIALIZING_SANDBOX="launch-readiness-initializing-locality" \
  FAKE_AMIKA_INITIALIZING_LISTS=1 \
  FAKE_AMIKA_READINESS_SSH_FAIL_SANDBOX="launch-readiness-initializing-locality" \
  FAKE_AMIKA_READINESS_SSH_FAILURES=2 \
  FAKE_AMIKA_REQUIRE_EXPERIMENT_ENV_SOURCE=1 \
  FAKE_AMIKA_REQUIRE_COLON_ROOT_SPLIT=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  LOCALITY_CONTEXT_DIRS="/fake/context/one:/fake/context/two" \
  RUN_ID="initializing" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/initializing-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null
assert_occurrences "$initializing_log" "amika sandbox ssh launch-readiness-initializing-locality -- true" 3
assert_line_before "$initializing_log" "amika sandbox ssh launch-readiness-initializing-locality -- true" "--scenario"

prerequisite_log="${tmp_root}/prerequisite-amika.log"
prerequisite_err="${tmp_root}/prerequisite.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$prerequisite_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/prerequisite-state" \
  FAKE_AMIKA_FAIL_LOCALITY_PREREQUISITE=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="prerequisite" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/prerequisite-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$prerequisite_err"
prerequisite_rc=$?
set -e
if [ "$prerequisite_rc" -eq 0 ]; then
  fail "a missing Locality prerequisite should fail before benchmark launch"
fi
assert_contains "$prerequisite_err" "Amika prerequisite check failed for locality sandbox launch-readiness-prerequisite-locality"
assert_contains "$prerequisite_log" "amika sandbox delete --remote --force launch-readiness-prerequisite-locality launch-readiness-prerequisite-mcp"
assert_not_contains "$prerequisite_log" "--scenario"

empty_context_log="${tmp_root}/empty-context-amika.log"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$empty_context_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/empty-context-state" \
  FAKE_AMIKA_EVALUATE_PREREQUISITES=1 \
  FAKE_REMOTE_LOCALITY_CONTEXT_DIRS="/remote/context/that-is-not-mounted" \
  FAKE_REMOTE_LOCALITY_ROOTS_EXIST=0 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  LOCALITY_CONTEXT_DIRS="" \
  LINEAR_API_KEY="local-linear" \
  NOTION_API_TOKEN="local-notion" \
  SLACK_BOT_TOKEN="local-slack" \
  SLACK_TEAM_ID="local-team" \
  RUN_ID="empty-context" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/empty-context-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null
assert_contains "$empty_context_log" "--scenario"

empty_credential_log="${tmp_root}/empty-credential-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$empty_credential_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/empty-credential-state" \
  FAKE_AMIKA_EVALUATE_PREREQUISITES=1 \
  FAKE_REMOTE_LINEAR_API_KEY="remote-linear" \
  FAKE_REMOTE_NOTION_API_TOKEN="remote-notion" \
  FAKE_REMOTE_SLACK_BOT_TOKEN="remote-slack" \
  FAKE_REMOTE_SLACK_TEAM_ID="remote-team" \
  FAKE_REMOTE_LINEAR_SECRET_FILE=0 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  LINEAR_API_KEY="" \
  NOTION_API_TOKEN="local-notion" \
  SLACK_BOT_TOKEN="local-slack" \
  SLACK_TEAM_ID="local-team" \
  RUN_ID="empty-credential" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/empty-credential-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/empty-credential.err"
empty_credential_rc=$?
set -e
if [ "$empty_credential_rc" -eq 0 ]; then
  fail "an explicitly empty forwarded credential must override a nonempty remote env value"
fi
assert_not_contains "$empty_credential_log" "--scenario"

delete_failure_log="${tmp_root}/retry-delete-failure-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$delete_failure_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/retry-delete-failure-state" \
  FAKE_AMIKA_CREATE_FAIL_SNAPSHOT="locality-snapshot" \
  FAKE_AMIKA_CREATE_FAIL_COUNT=1 \
  FAKE_AMIKA_FAIL_DELETE=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="retry-delete-failure" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/retry-delete-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/retry-delete-failure.err"
delete_failure_rc=$?
set -e
if [ "$delete_failure_rc" -ne 29 ]; then
  fail "retry deletion failure should preserve exit 29, got ${delete_failure_rc}"
fi
assert_occurrences "$delete_failure_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-retry-delete-failure-locality" 1

no_sync_log="${tmp_root}/no-sync-amika.log"
no_sync_err="${tmp_root}/no-sync.err"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$no_sync_log" \
  RUN_ID="no-sync" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/no-sync-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$no_sync_err"
assert_contains "$no_sync_err" "SYNC_ARTIFACTS=0; ephemeral Amika sandboxes will be deleted without retaining remote artifacts"
assert_contains "$no_sync_log" "amika sandbox delete --remote --force launch-readiness-no-sync-locality launch-readiness-no-sync-mcp"

sync_after_failure_log="${tmp_root}/sync-after-failure.log"
sync_after_failure_call_dir="${tmp_root}/sync-after-failure-calls"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$sync_after_failure_log" \
  FAKE_AMIKA_FAIL_SSH_CALL=2 \
  FAKE_AMIKA_FAIL_SSH_CALL_RC=47 \
  FAKE_AMIKA_CALL_DIR="$sync_after_failure_call_dir" \
  RUN_ID="sync-after-failure" \
  SYNC_ARTIFACTS=1 \
  LOCAL_OUT_DIR="${tmp_root}/sync-after-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/sync-after-failure.err"
sync_after_failure_rc=$?
set -e
if [ "$sync_after_failure_rc" -ne 47 ]; then
  fail "benchmark failure 47 should win after artifact sync, got ${sync_after_failure_rc}"
fi
assert_contains "$sync_after_failure_log" "rsync -az --delete"
assert_line_before "$sync_after_failure_log" "rsync -az --delete" "amika sandbox delete --remote --force launch-readiness-sync-after-failure-locality launch-readiness-sync-after-failure-mcp"

ssh_provider_amika_log="${tmp_root}/ssh-provider-amika.log"
ssh_provider_transport_log="${tmp_root}/ssh-provider-transport.log"
: > "$ssh_provider_amika_log"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$ssh_provider_amika_log" \
  FAKE_TRANSPORT_LOG="$ssh_provider_transport_log" \
  REMOTE_PROVIDER=ssh \
  LOCALITY_SSH_TARGET="locality@example.invalid" \
  MCP_SSH_TARGET="mcp@example.invalid" \
  RUN_ID="ssh-provider" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/ssh-provider-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null
assert_not_contains "$ssh_provider_amika_log" "amika sandbox create"
assert_not_contains "$ssh_provider_amika_log" "amika sandbox delete"
assert_contains "$ssh_provider_transport_log" "ssh locality@example.invalid"
assert_contains "$ssh_provider_transport_log" "ssh mcp@example.invalid"

term_log="${tmp_root}/term-amika.log"
term_activity="${tmp_root}/term-activity"
term_rc="$(
  PATH="${fake_bin}:$PATH" \
    FAKE_AMIKA_LOG="$term_log" \
    FAKE_AMIKA_BLOCK_OPERATION=ssh \
    FAKE_AMIKA_ACTIVITY_DIR="$term_activity" \
    RUN_ID="term-signal" \
    SYNC_ARTIFACTS=0 \
    LOCAL_OUT_DIR="${tmp_root}/term-out" \
    python3 "$signal_runner" TERM "$term_activity" 2 \
      "${tmp_root}/term.out" "${tmp_root}/term.err" "$WRAPPER" --scenario scenario2
)"
if [ "$term_rc" -ne 143 ]; then
  fail "TERM should return 143, got ${term_rc}"
fi
assert_not_contains "$term_log" "delete while remote child active"
assert_contains "$term_log" "amika sandbox delete --remote --force launch-readiness-term-signal-locality launch-readiness-term-signal-mcp"

int_log="${tmp_root}/int-amika.log"
int_activity="${tmp_root}/int-activity"
int_rc="$(
  PATH="${fake_bin}:$PATH" \
    FAKE_AMIKA_LOG="$int_log" \
    FAKE_AMIKA_BLOCK_OPERATION=create \
    FAKE_AMIKA_BLOCK_MATCH='--snapshot mcp-snapshot' \
    FAKE_AMIKA_ACTIVITY_DIR="$int_activity" \
    RUN_ID="int-signal" \
    SYNC_ARTIFACTS=0 \
    LOCAL_OUT_DIR="${tmp_root}/int-out" \
    python3 "$signal_runner" INT "$int_activity" 1 \
      "${tmp_root}/int.out" "${tmp_root}/int.err" "$WRAPPER" --scenario scenario2
)"
if [ "$int_rc" -ne 130 ]; then
  fail "INT during sandbox creation should return 130, got ${int_rc}"
fi
assert_not_contains "$int_log" "delete while remote child active"
assert_contains "$int_log" "amika sandbox delete --remote --force launch-readiness-int-signal-locality"
assert_not_contains "$int_log" "amika sandbox delete --remote --force launch-readiness-int-signal-locality launch-readiness-int-signal-mcp"

registered_signal_log="${tmp_root}/registered-signal-amika.log"
registered_signal_activity="${tmp_root}/registered-signal-activity"
registered_signal_rc="$(
  PATH="${fake_bin}:$PATH" \
    FAKE_AMIKA_LOG="$registered_signal_log" \
    FAKE_AMIKA_STATE_DIR="${tmp_root}/registered-signal-state" \
    FAKE_AMIKA_REGISTER_BEFORE_CREATE_BLOCK=1 \
    FAKE_AMIKA_BLOCK_OPERATION=create \
    FAKE_AMIKA_BLOCK_MATCH='--snapshot locality-snapshot' \
    FAKE_AMIKA_ACTIVITY_DIR="$registered_signal_activity" \
    RUN_ID="registered-signal" \
    SYNC_ARTIFACTS=0 \
    LOCAL_OUT_DIR="${tmp_root}/registered-signal-out" \
    python3 "$signal_runner" TERM "$registered_signal_activity" 1 \
      "${tmp_root}/registered-signal.out" "${tmp_root}/registered-signal.err" "$WRAPPER" --scenario scenario2
)"
if [ "$registered_signal_rc" -ne 143 ]; then
  fail "TERM after sandbox registration should return 143, got ${registered_signal_rc}"
fi
assert_contains "$registered_signal_log" "amika sandbox delete --remote --force launch-readiness-registered-signal-locality"

delayed_signal_log="${tmp_root}/delayed-signal-amika.log"
delayed_signal_state="${tmp_root}/delayed-signal-state"
delayed_signal_activity="${tmp_root}/delayed-signal-activity"
delayed_signal_rc="$(
  PATH="${fake_bin}:$PATH" \
    FAKE_AMIKA_LOG="$delayed_signal_log" \
    FAKE_AMIKA_STATE_DIR="$delayed_signal_state" \
    FAKE_AMIKA_DELAY_REGISTRATION_AFTER_STOP=1 \
    FAKE_AMIKA_BLOCK_OPERATION=create \
    FAKE_AMIKA_BLOCK_MATCH='--snapshot locality-snapshot' \
    FAKE_AMIKA_ACTIVITY_DIR="$delayed_signal_activity" \
    AMIKA_READINESS_TIMEOUT_SECONDS=3 \
    AMIKA_READINESS_POLL_SECONDS=1 \
    RUN_ID="delayed-signal" \
    SYNC_ARTIFACTS=0 \
    LOCAL_OUT_DIR="${tmp_root}/delayed-signal-out" \
    python3 "$signal_runner" TERM "$delayed_signal_activity" 1 \
      "${tmp_root}/delayed-signal.out" "${tmp_root}/delayed-signal.err" "$WRAPPER" --scenario scenario2
)"
if [ "$delayed_signal_rc" -ne 143 ]; then
  fail "TERM before delayed sandbox registration should return 143, got ${delayed_signal_rc}"
fi
assert_contains "$delayed_signal_log" "amika sandbox delete --remote --force launch-readiness-delayed-signal-locality"
if [ -e "$delayed_signal_state/launch-readiness-delayed-signal-locality.state" ]; then
  fail "delayed sandbox registration leaked after signal cleanup"
fi

blocked_list_log="${tmp_root}/blocked-list-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$blocked_list_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/blocked-list-state" \
  FAKE_AMIKA_BLOCK_OPERATION=list \
  FAKE_AMIKA_BLOCK_LIST_AFTER_CREATE=1 \
  FAKE_AMIKA_ACTIVITY_DIR="${tmp_root}/blocked-list-activity" \
  AMIKA_CREATE_ATTEMPTS=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=1 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="blocked-list" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/blocked-list-out" \
  python3 "$deadline_runner" 5 "${tmp_root}/blocked-list.out" "${tmp_root}/blocked-list.err" \
    "$WRAPPER" --scenario scenario2
blocked_list_rc=$?
set -e
if [ "$blocked_list_rc" -eq 125 ]; then
  fail "post-create sandbox listing exceeded the lifecycle deadline"
fi
if [ "$blocked_list_rc" -eq 0 ]; then
  fail "blocked post-create listing should fail provisioning"
fi

blocked_prerequisite_log="${tmp_root}/blocked-prerequisite-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$blocked_prerequisite_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/blocked-prerequisite-state" \
  FAKE_AMIKA_BLOCK_OPERATION=prerequisite \
  FAKE_AMIKA_BLOCK_PREREQUISITE=1 \
  FAKE_AMIKA_ACTIVITY_DIR="${tmp_root}/blocked-prerequisite-activity" \
  AMIKA_READINESS_TIMEOUT_SECONDS=1 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="blocked-prerequisite" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/blocked-prerequisite-out" \
  python3 "$deadline_runner" 5 "${tmp_root}/blocked-prerequisite.out" "${tmp_root}/blocked-prerequisite.err" \
    "$WRAPPER" --scenario scenario2
blocked_prerequisite_rc=$?
set -e
if [ "$blocked_prerequisite_rc" -eq 125 ]; then
  fail "prerequisite SSH exceeded the lifecycle deadline"
fi
if [ "$blocked_prerequisite_rc" -eq 0 ]; then
  fail "blocked prerequisite SSH should fail before benchmark launch"
fi
assert_not_contains "$blocked_prerequisite_log" "--scenario"

readiness_deadline_log="${tmp_root}/readiness-deadline-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$readiness_deadline_log" \
  FAKE_AMIKA_STATE_DIR="${tmp_root}/readiness-deadline-state" \
  FAKE_AMIKA_BLOCK_OPERATION=ssh \
  FAKE_AMIKA_BLOCK_READINESS=1 \
  FAKE_AMIKA_ACTIVITY_DIR="${tmp_root}/readiness-deadline-activity" \
  AMIKA_CREATE_ATTEMPTS=1 \
  AMIKA_READINESS_TIMEOUT_SECONDS=3 \
  AMIKA_READINESS_POLL_SECONDS=1 \
  RUN_ID="readiness-deadline" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/readiness-deadline-out" \
  python3 "$deadline_runner" 6 "${tmp_root}/readiness-deadline.out" "${tmp_root}/readiness-deadline.err" \
    "$WRAPPER" --scenario scenario2
readiness_deadline_rc=$?
set -e
if [ "$readiness_deadline_rc" -eq 125 ]; then
  fail "readiness polling exceeded its configured deadline"
fi
if [ "$readiness_deadline_rc" -eq 0 ]; then
  fail "blocked readiness should fail provisioning"
fi
assert_contains "$readiness_deadline_log" "amika sandbox delete --remote --force launch-readiness-readiness-deadline-locality"

forced_tty_log="${tmp_root}/forced-tty-amika.log"
forced_tty_transport_log="${tmp_root}/forced-tty-transport.log"
forced_tty_transcript="${tmp_root}/forced-tty-extract.transcript"
set +e
PATH="${forced_tty_bin}:${fake_bin}:$PATH" \
  REAL_TAR="$real_tar" \
  FAKE_TAR_EXTRACT_RC=37 \
  FAKE_SSH_INVALID_ARTIFACT=1 \
  FAKE_TRANSPORT_LOG="$forced_tty_transport_log" \
  FAKE_AMIKA_LOG="$forced_tty_log" \
  RUN_ID="forced-tty-extract" \
  AMIKA_SSH_FORCE_TTY=1 \
  SYNC_ARTIFACTS=1 \
  LOCAL_OUT_DIR="${tmp_root}/forced-tty-extract-out" \
  python3 "$pty_runner" "$forced_tty_transcript" "$WRAPPER" --scenario scenario2
forced_tty_rc=$?
set -e
if [ "$forced_tty_rc" -ne 37 ]; then
  fail "forced-TTY artifact extraction failure should return 37, got ${forced_tty_rc}"
fi
assert_contains "$forced_tty_transcript" "pipeline failed with exit code 37"
assert_contains "$forced_tty_log" "amika sandbox delete --remote --force launch-readiness-forced-tty-extract-locality launch-readiness-forced-tty-extract-mcp"

custom_log="${tmp_root}/custom-amika.log"
custom_out="${tmp_root}/custom-out"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$custom_log" \
  FAKE_AMIKA_SNAPSHOT_TABLE=$'NAME STATE PROVIDER SOURCE CREATED\ncustom-locality-snapshot active daytona aseem-locality 2026-08-02T00:00:00Z\ncustom-mcp-snapshot active daytona aseem-mcp 2026-08-02T00:00:00Z' \
  RUN_ID="customrun" \
  SYNC_ARTIFACTS=0 \
  LOCALITY_SANDBOX="custom-locality" \
  MCP_SANDBOX="custom-mcp" \
  LOCALITY_SNAPSHOT="custom-locality-snapshot" \
  MCP_SNAPSHOT="custom-mcp-snapshot" \
  REMOTE_WORKTREE="/tmp/custom-worktree" \
  REMOTE_LOC_BIN="/opt/locality/bin/loc" \
  LOCAL_OUT_DIR="$custom_out" \
  "$WRAPPER" --scenario custom-scenario >/dev/null

assert_contains "$custom_out/run.env" "locality_sandbox=custom-locality"
assert_contains "$custom_out/run.env" "mcp_sandbox=custom-mcp"
assert_contains "$custom_out/run.env" "locality_snapshot=custom-locality-snapshot"
assert_contains "$custom_out/run.env" "mcp_snapshot=custom-mcp-snapshot"
assert_contains "$custom_out/run.env" "remote_worktree=/tmp/custom-worktree"
assert_contains "$custom_out/run.env" "remote_loc_bin=/opt/locality/bin/loc"
assert_contains "$custom_log" "amika sandbox create --remote --no-git --snapshot custom-locality-snapshot --name custom-locality"
assert_contains "$custom_log" "amika sandbox create --remote --no-git --snapshot custom-mcp-snapshot --name custom-mcp"
assert_contains "$custom_log" "amika sandbox delete --remote --force custom-locality custom-mcp"
assert_contains "$custom_log" "--scenario"
assert_contains "$custom_log" "custom-scenario"

cleanup_failure_log="${tmp_root}/cleanup-failure-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$cleanup_failure_log" \
  FAKE_AMIKA_FAIL_DELETE=1 \
  RUN_ID="cleanup-failure" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/cleanup-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/cleanup-failure.err"
cleanup_failure_rc=$?
set -e
if [ "$cleanup_failure_rc" -ne 29 ]; then
  fail "successful benchmark should return cleanup failure, got ${cleanup_failure_rc}"
fi
assert_contains "$cleanup_failure_log" "amika sandbox delete --remote --force launch-readiness-cleanup-failure-locality launch-readiness-cleanup-failure-mcp"

combined_failure_log="${tmp_root}/combined-failure-amika.log"
combined_failure_err="${tmp_root}/combined-failure.err"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$combined_failure_log" \
  FAKE_AMIKA_FAIL_SSH_RC=41 \
  FAKE_AMIKA_FAIL_DELETE=1 \
  RUN_ID="combined-failure" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/combined-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"$combined_failure_err"
combined_failure_rc=$?
set -e
if [ "$combined_failure_rc" -ne 41 ]; then
  fail "operation failure 41 should win over cleanup failure, got ${combined_failure_rc}"
fi
assert_contains "$combined_failure_err" "Amika sandbox cleanup failed with exit code 29; owned sandboxes: launch-readiness-combined-failure-locality launch-readiness-combined-failure-mcp"
assert_contains "$combined_failure_log" "amika sandbox delete --remote --force launch-readiness-combined-failure-locality launch-readiness-combined-failure-mcp"

set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="${tmp_root}/same-sandbox.log" \
  RUN_ID="same" \
  SYNC_ARTIFACTS=0 \
  LOCALITY_SANDBOX="same-box" \
  MCP_SANDBOX="same-box" \
  LOCAL_OUT_DIR="${tmp_root}/same-out" \
  "$WRAPPER" >/dev/null 2>"${tmp_root}/same.err"
same_rc=$?
set -e
if [ "$same_rc" -eq 0 ]; then
  fail "same sandbox configuration should fail"
fi
assert_contains "${tmp_root}/same.err" "must be different"

set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="${tmp_root}/strategy.log" \
  RUN_ID="strategy" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/strategy-out" \
  "$WRAPPER" --strategy locality >/dev/null 2>"${tmp_root}/strategy.err"
strategy_rc=$?
set -e
if [ "$strategy_rc" -eq 0 ]; then
  fail "--strategy should be rejected by the split wrapper"
fi
assert_contains "${tmp_root}/strategy.err" "owns --strategy"

set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="${tmp_root}/unsupported.log" \
  RUN_ID="unsupported" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/unsupported-out" \
  "$WRAPPER" --write-mounted-page >/dev/null 2>"${tmp_root}/unsupported.err"
unsupported_rc=$?
set -e
if [ "$unsupported_rc" -eq 0 ]; then
  fail "--write-mounted-page should be rejected by the split wrapper"
fi
assert_contains "${tmp_root}/unsupported.err" "not supported"

printf 'launch readiness Amika split wrapper tests passed\n'
