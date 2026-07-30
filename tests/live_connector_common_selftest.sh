#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_COMMON_SELFTEST:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_COMMON_SELFTEST=1 to run the live connector helper self-test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

for command in sqlite3 python3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    live_fail "missing required live connector dependency: $command"
  fi
done

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
if [[ ! -x "$repo_root/$loc_bin" && ! -x "$loc_bin" ]]; then
  (cd "$repo_root" && cargo build -p loc-cli >/dev/null)
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/locality-live-common-selftest.XXXXXX")"
fake_daemon_pid=""
fake_fuse_pid=""
cleanup() {
  set +e
  if [[ -n "${fake_fuse_pid:-}" ]] && kill -0 "$fake_fuse_pid" >/dev/null 2>&1; then
    kill "$fake_fuse_pid" >/dev/null 2>&1
    wait "$fake_fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "${fake_daemon_pid:-}" ]] && kill -0 "$fake_daemon_pid" >/dev/null 2>&1; then
    kill "$fake_daemon_pid" >/dev/null 2>&1
    wait "$fake_daemon_pid" >/dev/null 2>&1
  fi
  rm -rf "$tmp_root"
}
trap cleanup EXIT

assert_require_linux_fuse_fails_with_path() {
  local label="$1"
  local expected="$2"
  local fake_path="$3"
  local error_path="$tmp_root/require-linux-fuse-$label.err"
  local actual

  if (PATH="$fake_path" require_linux_fuse) 2>"$error_path"; then
    live_fail "require_linux_fuse accepted $label"
  fi
  actual="$(cat "$error_path")"
  if [[ "$actual" != "$expected" ]]; then
    live_fail "require_linux_fuse reported an unexpected $label error: $actual"
  fi
}

assert_wait_failure_keeps_log_private() {
  local label="$1"
  local error_path="$2"
  local expected_message="$3"
  local expected_log_line="$4"
  local sentinel="$5"
  local actual

  if grep -Fq "$sentinel" "$error_path"; then
    live_fail "$label leaked private log content"
  fi
  if ! grep -Fqx "$expected_message" "$error_path"; then
    actual="$(cat "$error_path")"
    live_fail "$label did not report the expected failure message: $actual"
  fi
  if ! grep -Fqx "$expected_log_line" "$error_path"; then
    actual="$(cat "$error_path")"
    live_fail "$label did not report the retained log path: $actual"
  fi
}

non_linux_fake_path="$tmp_root/require-fuse-non-linux-bin"
mkdir -p "$non_linux_fake_path"
cat >"$non_linux_fake_path/uname" <<'SH'
#!/bin/sh
printf '%s\n' Darwin
SH
chmod +x "$non_linux_fake_path/uname"
assert_require_linux_fuse_fails_with_path \
  "non-linux" \
  "live connector Linux FUSE tests require Linux" \
  "$non_linux_fake_path"

if [[ "$(uname -s)" == "Linux" && ! -e /dev/fuse ]]; then
  missing_fuse_error="$tmp_root/require-linux-fuse-missing-dev-fuse.err"
  if require_linux_fuse 2>"$missing_fuse_error"; then
    live_fail "require_linux_fuse accepted a missing /dev/fuse"
  fi
  if [[ "$(cat "$missing_fuse_error")" != "/dev/fuse is not available on this runner" ]]; then
    live_fail "require_linux_fuse reported an unexpected missing-/dev/fuse error"
  fi
fi

if [[ "$(uname -s)" == "Linux" && -e /dev/fuse ]]; then
  require_fuse_fake_path="$tmp_root/require-fuse-bin"
  mkdir -p "$require_fuse_fake_path"
  cat >"$require_fuse_fake_path/uname" <<'SH'
#!/bin/sh
printf '%s\n' Linux
SH
  cat >"$require_fuse_fake_path/fusermount3" <<'SH'
#!/bin/sh
exit 0
SH
  cat >"$require_fuse_fake_path/mountpoint" <<'SH'
#!/bin/sh
exit 0
SH
  cat >"$require_fuse_fake_path/python3" <<'SH'
#!/bin/sh
exit 0
SH
  cat >"$require_fuse_fake_path/sqlite3" <<'SH'
#!/bin/sh
exit 0
SH
  chmod +x \
    "$require_fuse_fake_path/uname" \
    "$require_fuse_fake_path/fusermount3" \
    "$require_fuse_fake_path/mountpoint" \
    "$require_fuse_fake_path/python3" \
    "$require_fuse_fake_path/sqlite3"

  rm -f "$require_fuse_fake_path/fusermount3"
  assert_require_linux_fuse_fails_with_path \
    "missing-fusermount3" \
    "fusermount3 is not installed" \
    "$require_fuse_fake_path"
  cat >"$require_fuse_fake_path/fusermount3" <<'SH'
#!/bin/sh
exit 0
SH
  chmod +x "$require_fuse_fake_path/fusermount3"

  rm -f "$require_fuse_fake_path/mountpoint"
  assert_require_linux_fuse_fails_with_path \
    "missing-mountpoint" \
    "mountpoint is not installed" \
    "$require_fuse_fake_path"
  cat >"$require_fuse_fake_path/mountpoint" <<'SH'
#!/bin/sh
exit 0
SH
  chmod +x "$require_fuse_fake_path/mountpoint"

  rm -f "$require_fuse_fake_path/python3"
  assert_require_linux_fuse_fails_with_path \
    "missing-python3" \
    "python3 is not installed" \
    "$require_fuse_fake_path"
  cat >"$require_fuse_fake_path/python3" <<'SH'
#!/bin/sh
exit 0
SH
  chmod +x "$require_fuse_fake_path/python3"

  rm -f "$require_fuse_fake_path/sqlite3"
  assert_require_linux_fuse_fails_with_path \
    "missing-sqlite3" \
    "sqlite3 is not installed" \
    "$require_fuse_fake_path"
fi

state_root="$tmp_root/state"
credential_json='{"kind":"oauth","connector":"google-docs","access_token":"selftest-token"}'

export LOCALITY_LIVE_COMMON_SELFTEST_ENV="present"
require_live_env LOCALITY_LIVE_COMMON_SELFTEST_ENV
missing_env_error="$tmp_root/missing-env.err"
if require_live_env LOCALITY_LIVE_COMMON_SELFTEST_MISSING 2>"$missing_env_error"; then
  live_fail "require_live_env accepted an unset variable"
fi
if [[ "$(cat "$missing_env_error")" != "missing LOCALITY_LIVE_COMMON_SELFTEST_MISSING" ]]; then
  live_fail "require_live_env reported an unexpected missing-variable error"
fi

ok_report="$tmp_root/ok.json"
not_ok_report="$tmp_root/not-ok.json"
printf '%s\n' '{"ok":true,"status":"ok"}' >"$ok_report"
printf '%s\n' '{"ok":false,"status":"error","code":"selftest_not_ok"}' >"$not_ok_report"
assert_json_ok "$ok_report" "self-test ok report"
not_ok_error="$tmp_root/not-ok.err"
if assert_json_ok "$not_ok_report" "self-test not-ok report" 2>"$not_ok_error"; then
  live_fail "assert_json_ok accepted ok=false"
fi
if ! grep -Fq "self-test not-ok report did not report ok=true" "$not_ok_error"; then
  live_fail "assert_json_ok did not explain the ok=false failure"
fi

export LINEAR_API_KEY="lin_api_selftestsecret"
export LOCALITY_LINEAR_LIVE_ISSUE_ID="lin_selftest_secret_issue"
export LOCALITY_GMAIL_LIVE_TO_EMAIL="private-selftest@example.com"
debug_report="$tmp_root/debug-report.json"
debug_command_log="$tmp_root/debug-commands.err.log"
debug_daemon_log="$tmp_root/debug-localityd.log"
debug_fuse_log="$tmp_root/debug-locality-fuse.log"
debug_output="$tmp_root/debug-output.err"
cat >"$debug_report" <<'JSON'
{
  "ok": false,
  "status": "error",
  "code": "selftest_permission_denied",
  "error": {
    "code": "linear_push_failed",
    "message": "failed to update lin_selftest_secret_issue for private-selftest@example.com with lin_api_selftestsecret"
  },
  "changed_remote_ids": ["lin_selftest_secret_issue"],
  "access_token": "ya29.selftest-secret-access-token"
}
JSON
cat >"$debug_command_log" <<'LOG'
curl failed with Authorization: Bearer ya29.selftest-secret-access-token
linear key was lin_api_selftestsecret
LOG
cat >"$debug_daemon_log" <<'LOG'
localityd is running with private-selftest@example.com
LOG
cat >"$debug_fuse_log" <<'LOG'
fuse saw lin_selftest_secret_issue
LOG

push_report="$debug_report" \
  command_log="$debug_command_log" \
  daemon_log="$debug_daemon_log" \
  fuse_log="$debug_fuse_log" \
  emit_live_debug_diagnostics "Self-test connector" 2>"$debug_output"
if ! grep -Fq "privacy-safe diagnostics: Self-test connector" "$debug_output"; then
  live_fail "debug diagnostics did not include the connector label"
fi
if ! grep -Fq "selftest_permission_denied" "$debug_output"; then
  live_fail "debug diagnostics did not include the report error code"
fi
if ! grep -Fq "linear_push_failed" "$debug_output"; then
  live_fail "debug diagnostics did not include the nested error code"
fi
if ! grep -Fq "<redacted>" "$debug_output"; then
  live_fail "debug diagnostics did not show redaction placeholders"
fi
for leaked_debug_value in \
  "lin_api_selftestsecret" \
  "lin_selftest_secret_issue" \
  "private-selftest@example.com" \
  "ya29.selftest-secret-access-token"; do
  if grep -Fq "$leaked_debug_value" "$debug_output"; then
    live_fail "debug diagnostics leaked $leaked_debug_value"
  fi
done
unset LINEAR_API_KEY LOCALITY_LINEAR_LIVE_ISSUE_ID LOCALITY_GMAIL_LIVE_TO_EMAIL

init_live_state "$loc_bin" "$state_root"
seed_connector_credential "$loc_bin" "$state_root" "google-docs" "google-docs-live" "$credential_json"

db="$state_root/state.sqlite3"

connection_count="$(
  sqlite3 "$db" \
    "SELECT count(*) FROM connections WHERE connection_id = 'google-docs-live' AND connector = 'google-docs' AND auth_kind = 'oauth';"
)"
if [[ "$connection_count" != "1" ]]; then
  live_fail "expected one seeded google-docs OAuth connection, found $connection_count"
fi

profile_count="$(
  sqlite3 "$db" \
    "SELECT count(*) FROM connector_profiles WHERE profile_id = 'google-docs-oauth-default' AND connector = 'google-docs';"
)"
if [[ "$profile_count" != "1" ]]; then
  live_fail "expected one seeded google-docs OAuth profile, found $profile_count"
fi

secret_path="$(credential_file_path "$state_root" "connection:google-docs-live")"
stored_secret="$(cat "$secret_path")"
if [[ "$stored_secret" != "$credential_json" ]]; then
  live_fail "stored credential JSON did not match the seeded value"
fi

access_token="$(credential_access_token "$secret_path")"
if [[ "$access_token" != "selftest-token" ]]; then
  live_fail "credential_access_token did not return the seeded OAuth token"
fi

plain_secret_ref="connection:granola-live"
plain_secret_path="$(credential_file_path "$state_root" "$plain_secret_ref")"
write_file_credential "$state_root" "$plain_secret_ref" "selftest-api-key"
plain_secret="$(credential_access_token "$plain_secret_path")"
if [[ "$plain_secret" != "selftest-api-key" ]]; then
  live_fail "credential_access_token did not return the seeded plain API key"
fi

refreshable_secret_ref="connection:gmail-live"
refreshable_secret_path="$(credential_file_path "$state_root" "$refreshable_secret_ref")"
refreshable_credential_json='{"kind":"oauth","connector":"gmail","access_token":"gmail-access-token-1","token_type":"Bearer","oauth_client_id":"client","oauth_broker_url":"https://auth.example.test","account_id":"acct-1","account_label":"selftest@example.com","workspace_id":"gmail","workspace_name":"Gmail","scopes":["https://www.googleapis.com/auth/gmail.readonly","https://www.googleapis.com/auth/gmail.compose"],"refresh_token_handle":"refresh-handle-1","acquired_at":100,"expires_at":4102444800}'
write_file_credential "$state_root" "$refreshable_secret_ref" "$refreshable_credential_json"
require_oauth_credential_file "$refreshable_secret_path" "gmail" "self-test Gmail credential"

missing_refresh_secret_ref="connection:gmail-missing-refresh"
missing_refresh_secret_path="$(credential_file_path "$state_root" "$missing_refresh_secret_ref")"
write_file_credential \
  "$state_root" \
  "$missing_refresh_secret_ref" \
  '{"kind":"oauth","connector":"gmail","access_token":"gmail-access-token-1","oauth_broker_url":"https://auth.example.test","acquired_at":100,"expires_at":4102444800}'
missing_refresh_error="$tmp_root/missing-refresh.err"
if require_oauth_credential_file "$missing_refresh_secret_path" "gmail" "self-test missing-refresh credential" 2>"$missing_refresh_error"; then
  live_fail "require_oauth_credential_file accepted a credential without a refresh handle"
fi
if ! grep -Fq "self-test missing-refresh credential missing refresh_token_handle" "$missing_refresh_error"; then
  live_fail "require_oauth_credential_file did not explain the missing refresh handle"
fi
if grep -Fq "gmail-access-token-1" "$missing_refresh_error"; then
  live_fail "require_oauth_credential_file leaked the access token in its error"
fi

force_oauth_credential_refresh "$refreshable_secret_path" "gmail" "self-test Gmail credential"
forced_expires_at="$(json_field "$refreshable_secret_path" "expires_at")"
forced_acquired_at="$(json_field "$refreshable_secret_path" "acquired_at")"
if [[ "$forced_expires_at" != "1" || "$forced_acquired_at" != "1" ]]; then
  live_fail "force_oauth_credential_refresh did not force the credential expiry"
fi
refresh_marker="$(oauth_credential_refresh_marker "$refreshable_secret_path" "gmail" "self-test Gmail credential")"
refreshed_acquired_at="$(( $(date -u +%s) + 1 ))"
refreshed_expires_at="$(( $(date -u +%s) + 3600 ))"
write_file_credential \
  "$state_root" \
  "$refreshable_secret_ref" \
  "{\"kind\":\"oauth\",\"connector\":\"gmail\",\"access_token\":\"gmail-access-token-2\",\"token_type\":\"Bearer\",\"oauth_client_id\":\"client\",\"oauth_broker_url\":\"https://auth.example.test\",\"account_id\":\"acct-1\",\"account_label\":\"selftest@example.com\",\"workspace_id\":\"gmail\",\"workspace_name\":\"Gmail\",\"scopes\":[\"https://www.googleapis.com/auth/gmail.readonly\",\"https://www.googleapis.com/auth/gmail.compose\"],\"refresh_token_handle\":\"refresh-handle-2\",\"acquired_at\":$refreshed_acquired_at,\"expires_at\":$refreshed_expires_at}"
assert_oauth_credential_refreshed "$refreshable_secret_path" "gmail" "$refresh_marker" "self-test Gmail credential"

helper_exported_refreshable_secret_path="$tmp_root/helper-exported/gmail-credential.json"
LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT="$helper_exported_refreshable_secret_path" \
  export_refreshed_oauth_credential_if_requested \
    "$refreshable_secret_path" \
    "gmail" \
    "$refresh_marker" \
    "self-test Gmail credential"
if [[ "$(cat "$helper_exported_refreshable_secret_path")" != "$(cat "$refreshable_secret_path")" ]]; then
  live_fail "export_refreshed_oauth_credential_if_requested did not copy the refreshed credential"
fi

exported_refreshable_secret_path="$tmp_root/exported/gmail-credential.json"
export_oauth_credential_file \
  "$refreshable_secret_path" \
  "$exported_refreshable_secret_path" \
  "gmail" \
  "self-test Gmail credential"
if [[ "$(cat "$exported_refreshable_secret_path")" != "$(cat "$refreshable_secret_path")" ]]; then
  live_fail "export_oauth_credential_file did not copy the refreshed credential"
fi

fake_bin_dir="$tmp_root/bin"
mkdir -p "$fake_bin_dir"
fake_loc="$fake_bin_dir/loc"
fake_not_ready_loc="$fake_bin_dir/loc-not-ready"
fake_localityd="$fake_bin_dir/localityd"
fake_fuse="$fake_bin_dir/locality-fuse"
fake_mountpoint="$fake_bin_dir/mountpoint"

cat >"$fake_loc" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == "daemon" && "${2:-}" == "status" ]]; then
  printf '%s\n' '{"state":"running"}'
  exit 0
fi
printf '%s\n' '{"ok":true}'
SH

cat >"$fake_not_ready_loc" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == "daemon" && "${2:-}" == "status" ]]; then
  printf '%s\n' '{"state":"starting"}'
  exit 0
fi
exit 1
SH

cat >"$fake_localityd" <<'SH'
#!/usr/bin/env bash
if [[ "${LOCALITY_DAEMON_TCP_ADDR:-}" != "off" ]]; then
  exit 2
fi
if [[ -z "${LOCALITY_STATE_DIR:-}" ]]; then
  exit 3
fi
sleep 60
SH

cat >"$fake_fuse" <<'SH'
#!/usr/bin/env bash
state_dir=""
mountpoint=""
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --state-dir)
      shift
      state_dir="${1:-}"
      ;;
    --mountpoint)
      shift
      mountpoint="${1:-}"
      ;;
  esac
  shift || true
done
if [[ -z "$state_dir" || -z "$mountpoint" || -z "${LOCALITY_STATE_DIR:-}" ]]; then
  exit 2
fi
sleep 60
SH

cat >"$fake_mountpoint" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == "-q" && -n "${2:-}" ]]; then
  exit 0
fi
exit 1
SH

chmod +x "$fake_loc" "$fake_not_ready_loc" "$fake_localityd" "$fake_fuse" "$fake_mountpoint"

build_live_binaries "$fake_loc" "$fake_localityd" "$fake_fuse"

private_log_sentinel="selftest-secret-provider-payload"
daemon_failure_log="$tmp_root/localityd-failure.log"
daemon_failure_error="$tmp_root/localityd-failure.err"
printf '%s\n' "$private_log_sentinel" >"$daemon_failure_log"
if LOCALITY_LIVE_DAEMON_WAIT_ATTEMPTS=1 wait_for_daemon "$fake_not_ready_loc" "$state_root" "$daemon_failure_log" 2>"$daemon_failure_error"; then
  live_fail "wait_for_daemon accepted a daemon that never became ready"
fi
assert_wait_failure_keeps_log_private \
  "wait_for_daemon" \
  "$daemon_failure_error" \
  "localityd did not become ready" \
  "localityd log retained at: $daemon_failure_log" \
  "$private_log_sentinel"

fake_unmounted_bin_dir="$tmp_root/unmounted-bin"
mkdir -p "$fake_unmounted_bin_dir"
cat >"$fake_unmounted_bin_dir/mountpoint" <<'SH'
#!/usr/bin/env bash
exit 1
SH
chmod +x "$fake_unmounted_bin_dir/mountpoint"
fuse_failure_log="$tmp_root/locality-fuse-failure.log"
fuse_failure_error="$tmp_root/locality-fuse-failure.err"
printf '%s\n' "$private_log_sentinel" >"$fuse_failure_log"
old_path="$PATH"
PATH="$fake_unmounted_bin_dir:$PATH"
if LOCALITY_LIVE_FUSE_WAIT_ATTEMPTS=1 wait_for_fuse "$tmp_root/unmounted-root" "" "$fuse_failure_log" 2>"$fuse_failure_error"; then
  PATH="$old_path"
  live_fail "wait_for_fuse accepted a mount that never became ready"
fi
PATH="$old_path"
assert_wait_failure_keeps_log_private \
  "wait_for_fuse" \
  "$fuse_failure_error" \
  "locality-fuse did not become ready" \
  "locality-fuse log retained at: $fuse_failure_log" \
  "$private_log_sentinel"

daemon_log="$tmp_root/localityd.log"
fake_daemon_pid="$(start_live_daemon "$fake_localityd" "$state_root" "$daemon_log")"
if [[ ! "$fake_daemon_pid" =~ ^[0-9]+$ ]] || ! kill -0 "$fake_daemon_pid" >/dev/null 2>&1; then
  live_fail "start_live_daemon did not return a running daemon pid"
fi
wait_for_daemon "$fake_loc" "$state_root"

locality_root="$tmp_root/Locality"
fuse_log="$tmp_root/locality-fuse.log"
fake_fuse_pid="$(start_live_fuse "$fake_fuse" "$state_root" "$locality_root" "$fuse_log")"
if [[ ! "$fake_fuse_pid" =~ ^[0-9]+$ ]] || ! kill -0 "$fake_fuse_pid" >/dev/null 2>&1; then
  live_fail "start_live_fuse did not return a running FUSE pid"
fi
old_path="$PATH"
PATH="$fake_bin_dir:$PATH"
wait_for_fuse "$locality_root" "$fake_fuse_pid"
PATH="$old_path"

echo "live connector helper self-test passed"
