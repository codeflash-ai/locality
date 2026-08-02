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

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-launch-readiness-amika-wrapper-test.XXXXXX")"
cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

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

if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "create" ] && [[ " $* " == *" --snapshot ${FAKE_AMIKA_FAIL_CREATE_SNAPSHOT:-__never__} "* ]]; then
  exit "${FAKE_AMIKA_FAIL_CREATE_RC:-23}"
fi
if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "delete" ] && [ "${FAKE_AMIKA_FAIL_DELETE:-0}" = "1" ]; then
  exit 29
fi
if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "ssh" ] && [ -n "${FAKE_AMIKA_FAIL_SSH_RC:-}" ]; then
  exit "$FAKE_AMIKA_FAIL_SSH_RC"
fi
if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "ssh" ] && [ "${3:-}" != "--print" ] && [ -n "${FAKE_AMIKA_FAIL_SSH_CALL:-}" ]; then
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
    printf '%s\n' "${FAKE_AMIKA_SANDBOX_TABLE:-NAME STATE LOCATION BRANCH REPO CREATOR CREATED
existing-box stopped remote - - Test 2026-08-02T00:00:00Z}"
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
    block_operation_if_requested create
    shift 2
    exit 0
    ;;
  sandbox:delete)
    fail_delete_while_operation_is_active
    shift 2
    exit 0
    ;;
  sandbox:ssh)
    block_operation_if_requested ssh
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

if [ -n "${FAKE_AMIKA_CONCURRENCY_DIR:-}" ]; then
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
assert_line_before "$fake_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-testrun-locality" "amika sandbox ssh"
assert_line_before "$fake_log" "amika sandbox create --remote --no-git --snapshot mcp-snapshot --name launch-readiness-testrun-mcp" "amika sandbox ssh"
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

first_create_log="${tmp_root}/first-create-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$first_create_log" \
  FAKE_AMIKA_FAIL_CREATE_SNAPSHOT="locality-snapshot" \
  RUN_ID="first-create" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/first-create-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/first-create.err"
first_create_rc=$?
set -e
if [ "$first_create_rc" -ne 23 ]; then
  fail "first Amika creation should preserve create failure 23, got ${first_create_rc}"
fi
assert_contains "$first_create_log" "amika sandbox create --remote --no-git --snapshot locality-snapshot --name launch-readiness-first-create-locality"
assert_not_contains "$first_create_log" "amika sandbox create --remote --no-git --snapshot mcp-snapshot"
assert_not_contains "$first_create_log" "amika sandbox delete"

partial_create_log="${tmp_root}/partial-create-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$partial_create_log" \
  FAKE_AMIKA_FAIL_CREATE_SNAPSHOT="mcp-snapshot" \
  RUN_ID="partial" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/partial-create-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/partial-create.err"
partial_create_rc=$?
set -e
if [ "$partial_create_rc" -ne 23 ]; then
  fail "partial Amika creation should preserve create failure 23, got ${partial_create_rc}"
fi
assert_contains "$partial_create_log" "amika sandbox delete --remote --force launch-readiness-partial-locality"
assert_not_contains "$partial_create_log" "amika sandbox delete --remote --force launch-readiness-partial-locality launch-readiness-partial-mcp"

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
