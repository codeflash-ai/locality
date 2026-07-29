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

require_live_env sqlite3 python3

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
if [[ ! -x "$repo_root/$loc_bin" && ! -x "$loc_bin" ]]; then
  (cd "$repo_root" && cargo build -p loc-cli >/dev/null)
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/locality-live-common-selftest.XXXXXX")"
cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

state_root="$tmp_root/state"
credential_json='{"kind":"oauth","connector":"google-docs","access_token":"selftest-token"}'

init_live_state "$state_root"
seed_connector_credential "$state_root" "google-docs" "google-docs-live" "$credential_json"

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

access_token="$(credential_access_token "$state_root" "connection:google-docs-live")"
if [[ "$access_token" != "selftest-token" ]]; then
  live_fail "credential_access_token returned $access_token, expected selftest-token"
fi

echo "live connector helper self-test passed"
