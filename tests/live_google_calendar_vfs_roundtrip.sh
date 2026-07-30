#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_CALENDAR_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_CALENDAR_VFS=1 to run the live Google Calendar VFS test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

require_linux_fuse
require_live_env LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON

if ! command -v curl >/dev/null 2>&1; then
  live_fail "curl is not installed"
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_GOOGLE_CALENDAR_LIVE_CONNECTION_ID:-google-calendar-live}"
mount_id="${LOCALITY_GOOGLE_CALENDAR_LIVE_MOUNT_ID:-google-calendar-live}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Google Calendar mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-calendar-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/$mount_id"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
command_log="$tmp_root/commands.err.log"
mount_report="$tmp_root/mount.json"
initial_pull_report="$tmp_root/initial-pull.json"
diff_report="$tmp_root/diff.json"
push_report="$tmp_root/push.json"
pull_after_push_report="$tmp_root/pull-after-push.json"
calendar_search_report="$tmp_root/calendar-events.json"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
event_id=""
event_deleted=0
event_cleanup_needed=0
summary=""
location=""
marker=""
step="initializing"

start_epoch="$(date -u -d '+2 hours' +%s)"
end_epoch="$((start_epoch + 1800))"
event_start="$(date -u -d "@$start_epoch" +%Y-%m-%dT%H:%M:%SZ)"
event_end="$(date -u -d "@$end_epoch" +%Y-%m-%dT%H:%M:%SZ)"
window_after="$(date -u -d "@$((start_epoch - 86400))" +%Y-%m-%d)"
window_before="$(date -u -d "@$((start_epoch + 172800))" +%Y-%m-%d)"
cleanup_time_min="$(date -u -d "@$((start_epoch - 300))" +%Y-%m-%dT%H:%M:%SZ)"
cleanup_time_max="$(date -u -d "@$((end_epoch + 300))" +%Y-%m-%dT%H:%M:%SZ)"
unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"

assert_json_field_equals() {
  local report_path="$1"
  local field_path="$2"
  local expected="$3"
  local label="${4:-JSON report}"
  local actual

  actual="$(json_field "$report_path" "$field_path")"
  if [[ "$actual" != "$expected" ]]; then
    live_fail "$label expected $field_path=$expected, got $actual"
  fi
}

wait_for_projected_mount_root() {
  local attempts="${LOCALITY_GOOGLE_CALENDAR_LIVE_MOUNT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$mount_root" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Google Calendar FUSE mount root did not appear at $mount_root"
}

wait_for_draft_dir() {
  local draft_dir="$mount_root/draft"
  local attempts="${LOCALITY_GOOGLE_CALENDAR_LIVE_DRAFT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$draft_dir" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Google Calendar draft directory did not appear under the mount"
}

wait_for_marker_under_events() {
  local marker="$1"
  local events_dir="$mount_root/events"
  local attempts="${LOCALITY_GOOGLE_CALENDAR_LIVE_MARKER_WAIT_ATTEMPTS:-120}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$events_dir" ]] && grep -R -Fq -- "$marker" "$events_dir" 2>/dev/null; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "created Google Calendar marker was not visible under events/ after pull"
}

find_created_calendar_event() {
  local access_token="$1"

  if [[ -z "${summary:-}" ]]; then
    return 1
  fi

  if ! curl -fsS --get "https://www.googleapis.com/calendar/v3/calendars/primary/events" \
    -H "Authorization: Bearer $access_token" \
    --data-urlencode "q=$summary" \
    --data-urlencode "timeMin=$cleanup_time_min" \
    --data-urlencode "timeMax=$cleanup_time_max" \
    --data-urlencode "singleEvents=true" \
    --data-urlencode "maxResults=10" \
    --data-urlencode "fields=items(id,summary,location,description)" \
    >"$calendar_search_report" 2>>"$command_log"; then
    return 1
  fi

  python3 - "$calendar_search_report" "$summary" "$location" "$marker" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
summary = sys.argv[2]
location = sys.argv[3]
marker = sys.argv[4]
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
for item in data.get("items") or []:
    if not isinstance(item, dict):
        continue
    if item.get("summary") != summary:
        continue
    if location and item.get("location") != location:
        continue
    description = item.get("description") or ""
    if marker and description and marker not in description:
        continue
    event_id = item.get("id")
    if event_id:
        print(event_id)
        raise SystemExit(0)
raise SystemExit(1)
PY
}

delete_created_calendar_event() {
  local mode="${1:-best_effort}"
  local access_token

  if [[ "$event_deleted" == "1" ]]; then
    return 0
  fi
  if [[ -z "$event_id" && "$event_cleanup_needed" != "1" ]]; then
    return 0
  fi
  if [[ -z "$credential_path" ]]; then
    credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
  fi

  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Google Calendar OAuth access token for cleanup"
    fi
    return 1
  fi

  if [[ -z "$event_id" ]]; then
    event_id="$(find_created_calendar_event "$access_token" 2>/dev/null || true)"
  fi
  if [[ -z "$event_id" ]]; then
    unset access_token
    if [[ "$mode" == "required" ]]; then
      live_fail "could not find created Google Calendar event during cleanup"
    fi
    return 1
  fi

  if curl -fsS -X DELETE "https://www.googleapis.com/calendar/v3/calendars/primary/events/$event_id" \
    -H "Authorization: Bearer $access_token" >/dev/null 2>>"$command_log"; then
    event_deleted=1
    event_id=""
    unset access_token
    return 0
  fi

  unset access_token
  if [[ "$mode" == "required" ]]; then
    live_fail "failed to delete created Google Calendar event during cleanup"
  fi
  return 1
}

on_error() {
  local code=$?
  echo "live Google Calendar VFS round trip failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  return "$code"
}

cleanup() {
  set +e
  if [[ -n "${credential_path:-}" && -n "${oauth_refresh_marker:-}" ]]; then
    export_refreshed_oauth_credential_if_requested \
      "$credential_path" \
      "google-calendar" \
      "$oauth_refresh_marker" \
      "Google Calendar live credential" >/dev/null 2>&1 || true
  fi
  if [[ "$event_deleted" != "1" && ( -n "${event_id:-}" || "$event_cleanup_needed" == "1" ) ]]; then
    delete_created_calendar_event best_effort >/dev/null 2>&1 || \
      echo "warning: failed to delete created Google Calendar event during cleanup" >&2
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_CALENDAR_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Google Calendar VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}

trap on_error ERR
trap cleanup EXIT

step="creating isolated state"
mkdir -p "$state_root" "$locality_root" "$mount_root"

step="building live-test binaries"
build_live_binaries "$loc_bin" "$localityd_bin" "$fuse_bin"

step="seeding isolated Google Calendar OAuth credential"
seed_connector_credential \
  "$loc_bin" \
  "$state_root" \
  "google-calendar" \
  "$connection_id" \
  "$LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON"
credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
require_oauth_credential_file "$credential_path" "google-calendar" "Google Calendar live credential"
if [[ -n "${LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT:-}" && "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" != "1" ]]; then
  live_fail "LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT requires LOCALITY_LIVE_FORCE_OAUTH_REFRESH=1"
fi
if [[ "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" == "1" ]]; then
  step="forcing Google Calendar OAuth credential refresh"
  force_oauth_credential_refresh "$credential_path" "google-calendar" "Google Calendar live credential"
  oauth_refresh_marker="$(oauth_credential_refresh_marker "$credential_path" "google-calendar" "Google Calendar live credential")"
fi
unset LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON

step="registering Google Calendar Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount google-calendar "$mount_root" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --after "$window_after" \
    --before "$window_before" \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report" "Google Calendar mount report"

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="pulling Google Calendar workspace"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Google Calendar initial pull report"
if [[ -n "$oauth_refresh_marker" ]]; then
  step="verifying Google Calendar OAuth credential refresh"
  assert_oauth_credential_refreshed \
    "$credential_path" \
    "google-calendar" \
    "$oauth_refresh_marker" \
    "Google Calendar live credential"
  export_refreshed_oauth_credential_if_requested \
    "$credential_path" \
    "google-calendar" \
    "$oauth_refresh_marker" \
    "Google Calendar live credential"
fi
wait_for_draft_dir

summary="Locality live calendar $unique"
location="Locality live test room $unique"
marker="Locality live Google Calendar VFS marker $unique"
draft_path="$mount_root/draft/locality-live-calendar-$unique.md"

step="creating Google Calendar draft event through Linux FUSE"
printf -- '---\nsummary: "%s"\nlocation: "%s"\nstart:\n  dateTime: "%s"\nend:\n  dateTime: "%s"\n---\n%s\n' \
  "$summary" \
  "$location" \
  "$event_start" \
  "$event_end" \
  "$marker" >"$draft_path"

step="diffing created Google Calendar draft"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$draft_path" \
  >"$diff_report" 2>>"$command_log"
assert_json_ok "$diff_report" "Google Calendar diff report"
assert_json_field_equals "$diff_report" "action" "confirm_plan" "Google Calendar diff report"

step="pushing created Google Calendar draft"
event_cleanup_needed=1
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$draft_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Google Calendar push report"
remote_id="$(json_field "$push_report" "changed_remote_ids.0" 2>/dev/null || true)"
remote_prefix="google-calendar-event:primary:"
if [[ -z "$remote_id" ]]; then
  live_fail "Google Calendar push report did not include changed_remote_ids.0"
fi
if [[ "$remote_id" != "$remote_prefix"* ]]; then
  live_fail "Google Calendar push report changed_remote_ids.0 had an unexpected prefix"
fi
event_id="${remote_id#"$remote_prefix"}"
if [[ -z "$event_id" || ! "$event_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "Google Calendar push report produced an invalid event id shape"
fi

step="pulling Google Calendar workspace after push"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Google Calendar pull-after-push report"

step="verifying created Google Calendar marker under events"
wait_for_marker_under_events "$marker"

step="deleting created Google Calendar event"
delete_created_calendar_event required
event_cleanup_needed=0

echo "live Google Calendar API, CLI, daemon, and Linux FUSE create checks passed"
