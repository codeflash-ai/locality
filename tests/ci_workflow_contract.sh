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
assert_job_line ".github/workflows/ci.yml" "macos" \
  "          bash -n tests/live_macos_file_provider.sh"
assert_job_line ".github/workflows/ci.yml" "macos" \
  "          bash -n tests/live_macos_connector_file_provider.sh"

assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "file-provider-live" \
  "    runs-on: [self-hosted, macOS, locality-file-provider]"
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "file-provider-live" \
  "            --no-reset-domain \\"
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "file-provider-live" \
  "        run: tests/live_macos_file_provider.sh"
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "connector-file-provider-live" \
  "        run: tests/live_macos_connector_file_provider.sh"
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "slack-file-provider-live" \
  "        run: tests/live_macos_connector_file_provider.sh"
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "slack-file-provider-live" \
  '          gh api "repos/$GITHUB_REPOSITORY/environments/connector-live-e2e/secrets/public-key" >/dev/null'
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "slack-file-provider-live" \
  '        run: echo "LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT=$RUNNER_TEMP/locality-slack-macos-credential.json" >> "$GITHUB_ENV"'
assert_job_line ".github/workflows/macos-file-provider-live-e2e.yml" "granola-file-provider-live" \
  "        run: tests/live_macos_connector_file_provider.sh"
grep -Fqx "  group: locality-live-secret-consumers" "$ROOT/.github/workflows/macos-file-provider-live-e2e.yml" ||
  fail "macOS provider workflow must serialize live secret consumers"
grep -Fqx "  group: locality-live-secret-consumers" "$ROOT/.github/workflows/connector-live-e2e.yml" ||
  fail "connector workflow must serialize live secret consumers"

assert_job_line ".github/workflows/e2e.yml" "linux-fuse" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/e2e.yml" "linux-fuse" \
  "        run: LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 tests/run_linux_fuse_ci.sh tests/linux_fuse_smoke.sh"

assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_connector_common.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_connector_common_selftest.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_google_docs_mutation_scenario.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          bash -n tests/live_gmail_vfs_roundtrip.sh"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          python3 tests/live_connector_matrix.py validate"
assert_job_line ".github/workflows/connector-live-e2e.yml" "harness-selftest" \
  "          python3 tests/live_provider_connector_scenario_selftest.py"

assert_job_line ".github/workflows/notion-live-e2e.yml" "linux-fuse-live" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/notion-live-e2e.yml" "linux-fuse-live" \
  "        run: tests/run_linux_fuse_ci.sh env -u NOTION_TOKEN -u NOTION_AT tests/live_notion_vfs_push_pull.sh"

assert_job_line ".github/workflows/granola-live-e2e.yml" "linux-fuse-live" "    runs-on: ubuntu-latest"
assert_job_line ".github/workflows/granola-live-e2e.yml" "linux-fuse-live" \
  "        run: tests/run_linux_fuse_ci.sh tests/live_granola_vfs_read.sh"
assert_job_line ".github/workflows/granola-live-e2e.yml" "windows-cloud-files-live" \
  "        run: ./tests/windows_connector_cloud_files_live.ps1 -Connector granola"

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
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-windows-cloud-files-live" \
  '          gh api "repos/$GITHUB_REPOSITORY/environments/connector-live-e2e/secrets/public-key" >/dev/null'

windows_connector_jobs=(
  "gmail-windows-cloud-files-live:gmail"
  "slack-windows-cloud-files-live:slack"
  "linear-windows-cloud-files-live:linear"
)
for entry in "${windows_connector_jobs[@]}"; do
  job="${entry%%:*}"
  connector="${entry#*:}"
  assert_job_line ".github/workflows/connector-live-e2e.yml" "$job" "    runs-on: windows-latest"
  assert_job_line ".github/workflows/connector-live-e2e.yml" "$job" \
    "        run: ./tests/windows_connector_cloud_files_live.ps1 -Connector $connector"
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
  '      - "tests/live_connector_scenarios.json"' \
  '      - "tests/live_connector_matrix.py"' \
  '      - "tests/live_provider_connector_scenario.py"' \
  '      - "tests/live_provider_connector_scenario_selftest.py"' \
  '      - "tests/windows_connector_cloud_files_live.ps1"' \
  '      - "tests/resolve_linear_live_issue.py"' \
  '      - "tests/resolve_linear_live_issue_selftest.sh"'; do
  grep -Fqx "$dependency" "$ROOT/.github/workflows/connector-live-e2e.yml" ||
    fail ".github/workflows/connector-live-e2e.yml push paths must include $dependency"
done

python3 "$ROOT/tests/live_connector_matrix.py" validate >/dev/null
python3 "$ROOT/tests/live_connector_matrix_selftest.py" >/dev/null
for connector in gmail slack linear granola; do
  for platform in linux-fuse macos-file-provider windows-cloud-files; do
    python3 "$ROOT/tests/live_connector_matrix.py" get "$connector" "scenarios.$platform" >/dev/null
  done
done
if grep -Eq '"google-(docs|calendar)"[[:space:]]*:' "$ROOT/tests/live_connector_scenarios.json"; then
  fail "shared parity matrix must exclude Google Docs and Google Calendar"
fi
grep -Fq "def cleanup(self)" "$ROOT/tests/live_provider_connector_scenario.py" ||
  fail "provider runner must retain its strict cleanup hook"
grep -Fq "def _sanitize(self" "$ROOT/tests/live_provider_connector_scenario.py" ||
  fail "provider runner must retain privacy-safe diagnostics"

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
assert_job_line ".github/workflows/connector-live-e2e.yml" "slack-windows-cloud-files-live" \
  '        run: '\''"LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT=$env:RUNNER_TEMP\locality-slack-windows-credential.json" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append'\'''

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
