#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  echo "CI workflow contract failed: $*" >&2
  exit 1
}

job_block() {
  local workflow="$1"
  local job="$2"

  awk -v job="$job" '
    $0 == "  " job ":" {
      found = 1
      print
      next
    }
    found && /^  [[:alnum:]_-]+:$/ { exit }
    found { print }
  ' "$ROOT/$workflow"
}

assert_job_line() {
  local workflow="$1"
  local job="$2"
  local expected="$3"
  local block

  block="$(job_block "$workflow" "$job")"
  [[ -n "$block" ]] || fail "$workflow is missing job $job"
  grep -Fqx "$expected" <<<"$block" ||
    fail "$workflow job $job is missing exact line: $expected"
}

assert_job_omits() {
  local workflow="$1"
  local job="$2"
  local forbidden="$3"
  local block

  block="$(job_block "$workflow" "$job")"
  [[ -n "$block" ]] || fail "$workflow is missing job $job"
  if grep -Fq "$forbidden" <<<"$block"; then
    fail "$workflow job $job contains forbidden text: $forbidden"
  fi
}

# These jobs exercise real kernel FUSE mounts inside the privileged CI
# environment. Keep their product-path test commands exact so a runner
# workaround cannot silently reduce coverage.
assert_job_line ".github/workflows/ci.yml" "linux" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/ci.yml" "linux-fuse" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/ci.yml" "linux-fuse" \
  "        run: LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 tests/run_linux_fuse_ci.sh tests/linux_fuse_smoke.sh"
assert_job_line ".github/workflows/ci.yml" "linux" \
  "        run: npm test -- --run"
assert_job_line ".github/workflows/ci.yml" "macos" \
  "        run: swift test --package-path platform/macos/LocalityFileProvider"

assert_job_line ".github/workflows/e2e.yml" "linux-fuse" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/e2e.yml" "linux-fuse" \
  "        run: LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 tests/run_linux_fuse_ci.sh tests/linux_fuse_smoke.sh"

assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_connector_common.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_connector_common_selftest.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_google_docs_mutation_scenario.sh"

assert_job_line ".github/workflows/notion-live-e2e.yml" "linux-fuse-live" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/notion-live-e2e.yml" "linux-fuse-live" \
  "        run: tests/run_linux_fuse_ci.sh env -u NOTION_TOKEN -u NOTION_AT tests/live_notion_vfs_push_pull.sh"

assert_job_line ".github/workflows/granola-live-e2e.yml" "linux-fuse-live" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/granola-live-e2e.yml" "linux-fuse-live" \
  "        run: tests/run_linux_fuse_ci.sh tests/live_granola_vfs_read.sh"

connector_jobs=(
  "google-docs-live"
  "google-calendar-live"
  "gmail-live"
  "slack-live"
  "linear-live"
)
connector_scripts=(
  "tests/live_google_docs_vfs_roundtrip.sh"
  "tests/live_google_calendar_vfs_roundtrip.sh"
  "tests/live_gmail_vfs_roundtrip.sh"
  "tests/live_slack_vfs_read.sh"
  "tests/live_linear_vfs_roundtrip.sh"
)

for index in "${!connector_jobs[@]}"; do
  job="${connector_jobs[$index]}"
  script="${connector_scripts[$index]}"
  assert_job_line ".github/workflows/connector-live-e2e.yml" "$job" "    runs-on: ubuntu-latest"
  assert_job_line ".github/workflows/connector-live-e2e.yml" "$job" \
    "        run: tests/run_linux_fuse_ci.sh $script"
  assert_job_omits ".github/workflows/connector-live-e2e.yml" "$job" "continue-on-error"
done

for workflow in \
  ".github/workflows/connector-live-e2e.yml" \
  ".github/workflows/granola-live-e2e.yml"; do
  for dependency in \
    '      - "tests/linux-fuse-ci.Dockerfile"' \
    '      - "tests/linux-fuse-ci-entrypoint.sh"' \
    '      - "tests/run_linux_fuse_ci.sh"'; do
    grep -Fqx "$dependency" "$ROOT/$workflow" ||
      fail "$workflow push paths must include $dependency"
  done
done

for dependency in \
  '      - "tests/live_google_docs_mutation_scenario.sh"' \
  '      - "tests/resolve_linear_live_issue.py"' \
  '      - "tests/resolve_linear_live_issue_selftest.sh"'; do
  grep -Fqx "$dependency" "$ROOT/.github/workflows/connector-live-e2e.yml" ||
    fail ".github/workflows/connector-live-e2e.yml push paths must include $dependency"
done

# Slack refresh tokens are single-use. Every live run must force refresh,
# export the replacement, and persist it even if a later live assertion fails.
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-live" \
  '          LOCALITY_LIVE_FORCE_OAUTH_REFRESH: "1"'
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-live" \
  '          LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT: ${{ runner.temp }}/locality-slack-live-credential.json'
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-live" \
  '        if: ${{ always() }}'
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-live" \
  "            gh api \"repos/\$GITHUB_REPOSITORY/environments/connector-live-e2e/secrets/public-key\" \\"

grep -Fqx '  --privileged' "$ROOT/tests/run_linux_fuse_ci.sh" ||
  fail "Linux FUSE CI wrapper must keep Docker privileged mode enabled"
grep -Fqx '  --device /dev/fuse' "$ROOT/tests/run_linux_fuse_ci.sh" ||
  fail "Linux FUSE CI wrapper must pass the real FUSE device"
grep -Fqx '    && chmod 4755 /usr/bin/fusermount3 \' "$ROOT/tests/linux-fuse-ci.Dockerfile" ||
  fail "Linux FUSE CI image must preserve unprivileged fusermount semantics"
grep -Fqx 'ENV CARGO_TARGET_DIR=/tmp/locality-target' "$ROOT/tests/linux-fuse-ci.Dockerfile" ||
  fail "Linux FUSE CI image must not reuse host-built target artifacts"
grep -Fqx '  --env "LOCALITY_BIN=/tmp/locality-target/debug/loc"' "$ROOT/tests/run_linux_fuse_ci.sh" ||
  fail "Linux FUSE CI wrapper must resolve binaries from its isolated target directory"
grep -Fqx '  --volume "$target_root:/tmp/locality-target"' "$ROOT/tests/run_linux_fuse_ci.sh" ||
  fail "Linux FUSE CI wrapper must mount a disposable isolated target directory"

echo "ok: CI workflow contracts are intact"
