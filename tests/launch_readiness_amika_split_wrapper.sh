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

printf 'amika' >> "${FAKE_AMIKA_LOG:?}"
joined_args=""
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_AMIKA_LOG"
  joined_args="${joined_args} ${arg}"
done
printf '\n' >> "$FAKE_AMIKA_LOG"

if [ "${1:-}" != "sandbox" ]; then
  printf 'unexpected fake amika command: %s\n' "$*" >&2
  exit 2
fi

case "${2:-}" in
  list)
    printf '%s\n' "${FAKE_AMIKA_LIST_JSON:?}"
    exit 0
    ;;
  create)
    shift 2
    exit 0
    ;;
  delete)
    shift 2
    if [ -n "${FAKE_AMIKA_DELETE_EXIT_CODE:-}" ]; then
      exit "$FAKE_AMIKA_DELETE_EXIT_CODE"
    fi
    exit 0
    ;;
  ssh)
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

run_default_out="${tmp_root}/default-out"
concurrency_dir="${tmp_root}/concurrency"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$fake_log" \
  FAKE_AMIKA_LIST_JSON='[]' \
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
assert_contains "$fake_log" "amika sandbox list --remote -o json"
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

custom_log="${tmp_root}/custom-amika.log"
custom_out="${tmp_root}/custom-out"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$custom_log" \
  FAKE_AMIKA_LIST_JSON='[]' \
  RUN_ID="customrun" \
  SYNC_ARTIFACTS=0 \
  LOCALITY_SANDBOX="custom-locality" \
  MCP_SANDBOX="custom-mcp" \
  REMOTE_WORKTREE="/tmp/custom-worktree" \
  REMOTE_LOC_BIN="/opt/locality/bin/loc" \
  LOCAL_OUT_DIR="$custom_out" \
  "$WRAPPER" --scenario custom-scenario >/dev/null

assert_contains "$custom_out/run.env" "locality_sandbox=custom-locality"
assert_contains "$custom_out/run.env" "mcp_sandbox=custom-mcp"
assert_contains "$custom_out/run.env" "remote_worktree=/tmp/custom-worktree"
assert_contains "$custom_out/run.env" "remote_loc_bin=/opt/locality/bin/loc"
assert_contains "$custom_log" "custom-locality"
assert_contains "$custom_log" "custom-mcp"
assert_contains "$custom_log" "--scenario"
assert_contains "$custom_log" "custom-scenario"

cleanup_failure_log="${tmp_root}/cleanup-failure-amika.log"
set +e
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$cleanup_failure_log" \
  FAKE_AMIKA_LIST_JSON='[]' \
  FAKE_AMIKA_DELETE_EXIT_CODE=43 \
  RUN_ID="cleanup-failure" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="${tmp_root}/cleanup-failure-out" \
  "$WRAPPER" --scenario scenario2 >/dev/null 2>"${tmp_root}/cleanup-failure.err"
cleanup_failure_rc=$?
set -e
if [ "$cleanup_failure_rc" -ne 43 ]; then
  fail "successful benchmark should return cleanup failure, got ${cleanup_failure_rc}"
fi
assert_contains "$cleanup_failure_log" "amika sandbox delete --remote --force launch-readiness-cleanup-failure-locality launch-readiness-cleanup-failure-mcp"

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
