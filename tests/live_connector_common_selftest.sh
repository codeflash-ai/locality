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

fake_bin_dir="$tmp_root/bin"
mkdir -p "$fake_bin_dir"
fake_loc="$fake_bin_dir/loc"
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

chmod +x "$fake_loc" "$fake_localityd" "$fake_fuse" "$fake_mountpoint"

build_live_binaries "$fake_loc" "$fake_localityd" "$fake_fuse"

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
