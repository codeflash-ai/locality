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
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_AMIKA_LOG"
done
printf '\n' >> "$FAKE_AMIKA_LOG"

if [ "${1:-}" != "sandbox" ] || [ "${2:-}" != "ssh" ]; then
  printf 'unexpected fake amika command: %s\n' "$*" >&2
  exit 2
fi

if [ "${3:-}" = "--print" ]; then
  printf 'fake-user@fake-host-%s\n' "${4:-missing}"
  exit 0
fi

printf 'fake remote ok\n'
SH
chmod +x "${fake_bin}/amika"

run_default_out="${tmp_root}/default-out"
PATH="${fake_bin}:$PATH" \
  FAKE_AMIKA_LOG="$fake_log" \
  RUN_ID="testrun" \
  SYNC_ARTIFACTS=0 \
  LOCAL_OUT_DIR="$run_default_out" \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  CODEX_EXEC_TIMEOUT_SECONDS=12 \
  "$WRAPPER" --scenario scenario2 >/dev/null

assert_contains "$run_default_out/run.env" "locality_sandbox=aseem-locality"
assert_contains "$run_default_out/run.env" "mcp_sandbox=aseem-mcp"
assert_contains "$run_default_out/run.env" "remote_worktree=/home/amika/workspace/locality-launch-readiness-testrun"
assert_contains "$run_default_out/run.env" "locality_remote_out_dir=/home/amika/workspace/locality-launch-readiness-testrun/target/launch-readiness-testrun-locality"
assert_contains "$run_default_out/run.env" "mcp_remote_out_dir=/home/amika/workspace/locality-launch-readiness-testrun/target/launch-readiness-testrun-mcp"
assert_contains "$run_default_out/run.env" "remote_loc_bin=/home/amika/workspace/locality/target/debug/loc"
assert_contains "$run_default_out/run.env" "sync_artifacts=0"
assert_contains "$run_default_out/artifacts.tsv" "locality"$'\t'"aseem-locality"
assert_contains "$run_default_out/artifacts.tsv" "notion-mcp"$'\t'"aseem-mcp"

assert_contains "$fake_log" "aseem-locality"
assert_contains "$fake_log" "aseem-mcp"
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
  RUN_ID="customrun" \
  SYNC_ARTIFACTS=0 \
  LOCALITY_SANDBOX="custom-locality" \
  MCP_SANDBOX="custom-mcp" \
  REMOTE_WORKTREE="/tmp/custom-worktree" \
  REMOTE_LOC_BIN="/opt/locality/bin/loc" \
  LOCAL_OUT_DIR="$custom_out" \
  "$WRAPPER" --write-mounted-page >/dev/null

assert_contains "$custom_out/run.env" "locality_sandbox=custom-locality"
assert_contains "$custom_out/run.env" "mcp_sandbox=custom-mcp"
assert_contains "$custom_out/run.env" "remote_worktree=/tmp/custom-worktree"
assert_contains "$custom_out/run.env" "remote_loc_bin=/opt/locality/bin/loc"
assert_contains "$custom_log" "custom-locality"
assert_contains "$custom_log" "custom-mcp"
assert_contains "$custom_log" "--write-mounted-page"

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

printf 'launch readiness Amika split wrapper tests passed\n'
