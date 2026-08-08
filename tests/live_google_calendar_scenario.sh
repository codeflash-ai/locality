#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_CALENDAR_SCENARIO:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_CALENDAR_SCENARIO=1 to run the live Google Calendar scenario"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

require_linux_fuse
require_live_env LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON

for command in curl python3 sha256sum timeout; do
  if ! command -v "$command" >/dev/null 2>&1; then
    live_fail "$command is not installed"
  fi
done

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_GOOGLE_CALENDAR_LIVE_CONNECTION_ID:-google-calendar-live}"
mount_id="${LOCALITY_GOOGLE_CALENDAR_LIVE_MOUNT_ID:-google-calendar-live-scenario}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Google Calendar mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-calendar-scenario.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/$mount_id"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
command_log="$tmp_root/commands.err.log"
mount_report="$tmp_root/mount.json"
initial_pull_report="$tmp_root/initial-pull.json"
first_pull_report="$tmp_root/pull-after-seed.json"
second_pull_report="$tmp_root/pull-after-remote-update.json"
diff_report="$tmp_root/create-diff.json"
push_report="$tmp_root/create-push.json"
pull_after_push_report="$tmp_root/pull-after-create.json"
edit_diff_report="$tmp_root/local-update-diff.json"
edit_push_report="$tmp_root/local-update-push.json"
timed_event_report="$tmp_root/seed-timed-event.json"
all_day_event_report="$tmp_root/seed-all-day-event.json"
meeting_event_report="$tmp_root/seed-meeting-event.json"
recurring_event_report="$tmp_root/seed-recurring-event.json"
remote_update_report="$tmp_root/remote-update-event.json"
created_event_report="$tmp_root/created-event.json"
provider_after_rejected_update_report="$tmp_root/provider-after-rejected-local-update.json"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
timed_event_id=""
all_day_event_id=""
meeting_event_id=""
recurring_event_id=""
draft_event_id=""
scratch_cleanup_needed=0
step="initializing"

unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
base_epoch="$(date -u -d '+4 hours' +%s)"
window_after="$(date -u -d "@$((base_epoch - 86400))" +%Y-%m-%d)"
window_before="$(date -u -d "@$((base_epoch + 7 * 86400))" +%Y-%m-%d)"

timed_start="$(date -u -d "@$base_epoch" +%Y-%m-%dT%H:%M:%SZ)"
timed_end="$(date -u -d "@$((base_epoch + 1800))" +%Y-%m-%dT%H:%M:%SZ)"
meeting_start="$(date -u -d "@$((base_epoch + 3600))" +%Y-%m-%dT%H:%M:%SZ)"
meeting_end="$(date -u -d "@$((base_epoch + 5400))" +%Y-%m-%dT%H:%M:%SZ)"
recurring_start="$(date -u -d "@$((base_epoch + 7200))" +%Y-%m-%dT%H:%M:%SZ)"
recurring_end="$(date -u -d "@$((base_epoch + 9000))" +%Y-%m-%dT%H:%M:%SZ)"
draft_start="$(date -u -d "@$((base_epoch + 10800))" +%Y-%m-%dT%H:%M:%SZ)"
draft_end="$(date -u -d "@$((base_epoch + 12600))" +%Y-%m-%dT%H:%M:%SZ)"
all_day_start="$(date -u -d "@$((base_epoch + 2 * 86400))" +%Y-%m-%d)"
all_day_end="$(date -u -d "@$((base_epoch + 3 * 86400))" +%Y-%m-%d)"

timed_summary="Locality Calendar Scenario timed $unique"
all_day_summary="Locality Calendar Scenario all-day $unique"
meeting_summary="Locality Calendar Scenario meeting $unique"
recurring_summary="Locality Calendar Scenario recurring $unique"
draft_summary="Locality Calendar Scenario created from draft $unique"
updated_timed_summary="Locality Calendar Scenario timed updated $unique"
timed_marker="Locality calendar scenario timed marker $unique"
all_day_marker="Locality calendar scenario all-day marker $unique"
meeting_marker="Locality calendar scenario meeting marker $unique"
recurring_marker="Locality calendar scenario recurring marker $unique"
draft_marker="Locality calendar scenario draft-create marker $unique"
remote_update_marker="Locality calendar scenario remote-update marker $unique"
local_update_marker="Locality calendar scenario rejected-local-update marker $unique"

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

assert_json_validation_code() {
  local report_path="$1"
  local expected_code="$2"
  local label="${3:-JSON report}"

  python3 - "$report_path" "$expected_code" "$label" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected = sys.argv[2]
label = sys.argv[3]
data = json.loads(path.read_text(encoding="utf-8"))
for issue in data.get("validation") or []:
    if isinstance(issue, dict) and issue.get("code") == expected:
        raise SystemExit(0)
raise SystemExit(f"{label} did not include validation code {expected}")
PY
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

wait_for_events_dir() {
  local events_dir="$mount_root/events"
  local attempts="${LOCALITY_GOOGLE_CALENDAR_LIVE_EVENTS_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$events_dir" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Google Calendar events directory did not appear under the mount"
}

event_marker_count() {
  local marker="$1"

  python3 - "$mount_root/events" "$marker" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
marker = sys.argv[2]
count = 0
if root.is_dir():
    for path in root.rglob("*.md"):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        if marker in text:
            count += 1
print(count)
PY
}

find_marker_path_under_events() {
  local marker="$1"

  python3 - "$mount_root/events" "$marker" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
marker = sys.argv[2]
if root.is_dir():
    for path in sorted(root.rglob("*.md")):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        if marker in text:
            print(path)
            raise SystemExit(0)
raise SystemExit(1)
PY
}

wait_for_marker_count_under_events() {
  local marker="$1"
  local expected_count="$2"
  local label="$3"
  local attempts="${LOCALITY_GOOGLE_CALENDAR_LIVE_MARKER_WAIT_ATTEMPTS:-160}"
  local attempt
  local actual_count

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    actual_count="$(event_marker_count "$marker")"
    if (( actual_count >= expected_count )); then
      return 0
    fi
    sleep 0.25
  done
  live_fail "$label marker appeared in $actual_count event file(s), expected at least $expected_count"
}

assert_file_contains() {
  local path="$1"
  local needle="$2"
  local label="$3"

  if ! grep -Fq -- "$needle" "$path"; then
    live_fail "$label did not contain the expected projection text"
  fi
}

write_seed_event_body() {
  local case_name="$1"
  local output_path="$2"
  local summary="$3"
  local description_marker="$4"
  local location="$5"
  local start_value="$6"
  local end_value="$7"
  local attendee_email="${8:-}"
  local conference_request_id="${9:-}"

  python3 - \
    "$case_name" \
    "$output_path" \
    "$summary" \
    "$description_marker" \
    "$location" \
    "$start_value" \
    "$end_value" \
    "$attendee_email" \
    "$conference_request_id" <<'PY'
import json
import pathlib
import sys

case_name = sys.argv[1]
output_path = pathlib.Path(sys.argv[2])
summary = sys.argv[3]
marker = sys.argv[4]
location = sys.argv[5]
start_value = sys.argv[6]
end_value = sys.argv[7]
attendee_email = sys.argv[8].strip()
conference_request_id = sys.argv[9].strip()

event = {
    "summary": summary,
    "description": f"{marker}\nScratch event created by tests/live_google_calendar_scenario.sh.",
    "location": location,
    "extendedProperties": {
        "private": {
            "locality_live_scenario": marker,
            "locality_live_case": case_name,
        }
    },
}

if case_name == "all-day":
    event["start"] = {"date": start_value}
    event["end"] = {"date": end_value}
    event["transparency"] = "transparent"
    event["visibility"] = "private"
    event["reminders"] = {
        "useDefault": False,
        "overrides": [{"method": "popup", "minutes": 10}],
    }
elif case_name == "recurring":
    event["start"] = {"dateTime": start_value, "timeZone": "UTC"}
    event["end"] = {"dateTime": end_value, "timeZone": "UTC"}
    event["recurrence"] = ["RRULE:FREQ=DAILY;COUNT=2"]
elif case_name == "meeting":
    event["start"] = {"dateTime": start_value, "timeZone": "UTC"}
    event["end"] = {"dateTime": end_value, "timeZone": "UTC"}
    if attendee_email:
        event["attendees"] = [{"email": attendee_email, "responseStatus": "needsAction"}]
    if conference_request_id:
        event["conferenceData"] = {
            "createRequest": {
                "requestId": conference_request_id,
                "conferenceSolutionKey": {"type": "hangoutsMeet"},
            }
        }
else:
    event["start"] = {"dateTime": start_value, "timeZone": "UTC"}
    event["end"] = {"dateTime": end_value, "timeZone": "UTC"}

output_path.write_text(json.dumps(event, separators=(",", ":"), sort_keys=True), encoding="utf-8")
PY
}

write_patch_event_body() {
  local output_path="$1"
  local summary="$2"
  local description_marker="$3"
  local location="$4"

  python3 - "$output_path" "$summary" "$description_marker" "$location" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
summary = sys.argv[2]
marker = sys.argv[3]
location = sys.argv[4]
body = {
    "summary": summary,
    "description": f"{marker}\nScratch event updated remotely by tests/live_google_calendar_scenario.sh.",
    "location": location,
}
path.write_text(json.dumps(body, separators=(",", ":"), sort_keys=True), encoding="utf-8")
PY
}

insert_calendar_event() {
  local access_token="$1"
  local body_path="$2"
  local report_path="$3"
  local create_conference="${4:-0}"
  local query="sendUpdates=none"

  if [[ "$create_conference" == "1" ]]; then
    query="$query&conferenceDataVersion=1"
  fi

  if ! curl -fsS -X POST "https://www.googleapis.com/calendar/v3/calendars/primary/events?$query" \
    -H "Authorization: Bearer $access_token" \
    -H "Content-Type: application/json" \
    --data-binary "@$body_path" \
    >"$report_path" 2>>"$command_log"; then
    live_fail "failed to insert Google Calendar scratch event"
  fi
  json_field "$report_path" "id"
}

patch_calendar_event() {
  local access_token="$1"
  local event_id="$2"
  local body_path="$3"
  local report_path="$4"

  curl -fsS -X PATCH "https://www.googleapis.com/calendar/v3/calendars/primary/events/$event_id?sendUpdates=none" \
    -H "Authorization: Bearer $access_token" \
    -H "Content-Type: application/json" \
    --data-binary "@$body_path" \
    >"$report_path" 2>>"$command_log"
}

get_calendar_event() {
  local access_token="$1"
  local event_id="$2"
  local report_path="$3"

  curl -fsS "https://www.googleapis.com/calendar/v3/calendars/primary/events/$event_id" \
    -H "Authorization: Bearer $access_token" \
    >"$report_path" 2>>"$command_log"
}

assert_event_json() {
  local report_path="$1"
  local expected_summary="$2"
  local required_marker="$3"
  local forbidden_marker="${4:-}"

  python3 - "$report_path" "$expected_summary" "$required_marker" "$forbidden_marker" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_summary = sys.argv[2]
required_marker = sys.argv[3]
forbidden_marker = sys.argv[4]
event = json.loads(path.read_text(encoding="utf-8"))
summary = event.get("summary") or ""
description = event.get("description") or ""
if summary != expected_summary:
    raise SystemExit(f"provider event summary mismatch: expected {expected_summary!r}, got {summary!r}")
if required_marker and required_marker not in description:
    raise SystemExit("provider event description did not include the required marker")
if forbidden_marker and forbidden_marker in description:
    raise SystemExit("provider event description included the forbidden marker")
PY
}

assert_event_json_has_conference() {
  local report_path="$1"
  local label="$2"

  python3 - "$report_path" "$label" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
label = sys.argv[2]
event = json.loads(path.read_text(encoding="utf-8"))
if event.get("hangoutLink") or event.get("conferenceData"):
    raise SystemExit(0)
raise SystemExit(f"{label} did not include Google Meet conference data")
PY
}

delete_calendar_event_id() {
  local access_token="$1"
  local event_id="$2"

  curl -fsS -X DELETE "https://www.googleapis.com/calendar/v3/calendars/primary/events/$event_id?sendUpdates=none" \
    -H "Authorization: Bearer $access_token" \
    >/dev/null 2>>"$command_log"
}

cleanup_scratch_events() {
  local mode="${1:-best_effort}"
  local access_token
  local failed=0
  local event_id

  if [[ "$scratch_cleanup_needed" != "1" ]]; then
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

  for event_id in \
    "$timed_event_id" \
    "$all_day_event_id" \
    "$meeting_event_id" \
    "$recurring_event_id" \
    "$draft_event_id"; do
    [[ -z "$event_id" ]] && continue
    if delete_calendar_event_id "$access_token" "$event_id"; then
      continue
    else
      failed=1
    fi
  done

  unset access_token
  if [[ "$failed" == "1" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "failed to delete one or more Google Calendar scratch events"
    fi
    return 1
  fi

  timed_event_id=""
  all_day_event_id=""
  meeting_event_id=""
  recurring_event_id=""
  draft_event_id=""
  scratch_cleanup_needed=0
}

on_error() {
  local code=$?
  echo "live Google Calendar scenario failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  emit_live_debug_diagnostics "Google Calendar scenario" || true
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
  cleanup_scratch_events best_effort >/dev/null 2>&1 || \
    echo "warning: failed to delete one or more Google Calendar scratch events during cleanup" >&2
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_CALENDAR_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Google Calendar scenario temp root: $tmp_root"
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
assert_json_ok "$mount_report" "Google Calendar scenario mount report"

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="initial Google Calendar pull"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Google Calendar scenario initial pull report"
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
wait_for_events_dir

access_token="$(credential_access_token "$credential_path")"

step="seeding timed Google Calendar event through the provider API"
timed_body="$tmp_root/seed-timed-event.request.json"
write_seed_event_body \
  "timed" \
  "$timed_body" \
  "$timed_summary" \
  "$timed_marker" \
  "Locality scenario room $unique" \
  "$timed_start" \
  "$timed_end"
timed_event_id="$(insert_calendar_event "$access_token" "$timed_body" "$timed_event_report")"
scratch_cleanup_needed=1

step="seeding all-day Google Calendar event through the provider API"
all_day_body="$tmp_root/seed-all-day-event.request.json"
write_seed_event_body \
  "all-day" \
  "$all_day_body" \
  "$all_day_summary" \
  "$all_day_marker" \
  "Locality scenario all-day $unique" \
  "$all_day_start" \
  "$all_day_end"
all_day_event_id="$(insert_calendar_event "$access_token" "$all_day_body" "$all_day_event_report")"

step="seeding meeting Google Calendar event through the provider API"
meeting_body="$tmp_root/seed-meeting-event.request.json"
write_seed_event_body \
  "meeting" \
  "$meeting_body" \
  "$meeting_summary" \
  "$meeting_marker" \
  "Locality scenario meeting room $unique" \
  "$meeting_start" \
  "$meeting_end" \
  "${LOCALITY_GOOGLE_CALENDAR_LIVE_ATTENDEE_EMAIL:-}" \
  "locality-scenario-meeting-$unique"
meeting_event_id="$(insert_calendar_event "$access_token" "$meeting_body" "$meeting_event_report" 1)"
assert_event_json_has_conference "$meeting_event_report" "seeded meeting event"

step="seeding recurring Google Calendar event through the provider API"
recurring_body="$tmp_root/seed-recurring-event.request.json"
write_seed_event_body \
  "recurring" \
  "$recurring_body" \
  "$recurring_summary" \
  "$recurring_marker" \
  "Locality scenario recurring room $unique" \
  "$recurring_start" \
  "$recurring_end"
recurring_event_id="$(insert_calendar_event "$access_token" "$recurring_body" "$recurring_event_report")"
unset access_token

step="pulling Google Calendar after provider-seeded events"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root/events" \
  >"$first_pull_report" 2>>"$command_log"
assert_json_ok "$first_pull_report" "Google Calendar scenario pull-after-seed report"

step="verifying provider-seeded event projections"
wait_for_marker_count_under_events "$timed_marker" 1 "timed event"
wait_for_marker_count_under_events "$all_day_marker" 1 "all-day event"
wait_for_marker_count_under_events "$meeting_marker" 1 "meeting event"
wait_for_marker_count_under_events "$recurring_marker" 2 "recurring event instances"

all_day_path="$(find_marker_path_under_events "$all_day_marker")"
meeting_path="$(find_marker_path_under_events "$meeting_marker")"
assert_file_contains "$all_day_path" "date: \"$all_day_start\"" "all-day event projection"
assert_file_contains "$meeting_path" "conferenceData:" "meeting event projection"
if [[ -n "${LOCALITY_GOOGLE_CALENDAR_LIVE_ATTENDEE_EMAIL:-}" ]]; then
  assert_file_contains "$meeting_path" "${LOCALITY_GOOGLE_CALENDAR_LIVE_ATTENDEE_EMAIL}" "meeting attendee projection"
fi

draft_path="$mount_root/draft/locality-calendar-scenario-$unique.md"

step="creating Google Calendar draft event through Linux FUSE"
printf -- '---\nsummary: "%s"\nlocation: "Locality scenario created room %s"\nstart:\n  dateTime: "%s"\nend:\n  dateTime: "%s"\ntransparency: opaque\nvisibility: private\ngoogle_calendar:\n  conference: google_meet\n---\n%s\n' \
  "$draft_summary" \
  "$unique" \
  "$draft_start" \
  "$draft_end" \
  "$draft_marker" >"$draft_path"

step="diffing Google Calendar draft create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$draft_path" \
  >"$diff_report" 2>>"$command_log"
assert_json_ok "$diff_report" "Google Calendar scenario create diff report"
assert_json_field_equals "$diff_report" "action" "confirm_plan" "Google Calendar scenario create diff report"

step="pushing Google Calendar draft create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$draft_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Google Calendar scenario create push report"
remote_id="$(json_field "$push_report" "changed_remote_ids.0" 2>/dev/null || true)"
remote_prefix="google-calendar-event:primary:"
if [[ -z "$remote_id" || "$remote_id" != "$remote_prefix"* ]]; then
  live_fail "Google Calendar scenario push report did not include a primary calendar event id"
fi
draft_event_id="${remote_id#"$remote_prefix"}"
if [[ -z "$draft_event_id" || ! "$draft_event_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "Google Calendar scenario push report produced an invalid event id shape"
fi

access_token="$(credential_access_token "$credential_path")"

step="verifying provider event created by Locality"
get_calendar_event "$access_token" "$draft_event_id" "$created_event_report"
assert_event_json "$created_event_report" "$draft_summary" "$draft_marker"
assert_event_json_has_conference "$created_event_report" "Locality-created event"

step="pulling Google Calendar after Locality create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root/events" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Google Calendar scenario pull-after-create report"
wait_for_marker_count_under_events "$draft_marker" 1 "Locality-created event"

step="updating seeded Google Calendar event through the provider API"
remote_update_body="$tmp_root/remote-update-event.request.json"
write_patch_event_body \
  "$remote_update_body" \
  "$updated_timed_summary" \
  "$remote_update_marker" \
  "Locality scenario updated room $unique"
patch_calendar_event "$access_token" "$timed_event_id" "$remote_update_body" "$remote_update_report"
assert_event_json "$remote_update_report" "$updated_timed_summary" "$remote_update_marker" "$timed_marker"

step="pulling Google Calendar after provider-side update"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root/events" \
  >"$second_pull_report" 2>>"$command_log"
assert_json_ok "$second_pull_report" "Google Calendar scenario pull-after-remote-update report"
wait_for_marker_count_under_events "$remote_update_marker" 1 "provider-updated event"

updated_event_path="$(find_marker_path_under_events "$remote_update_marker")"
if [[ -z "$updated_event_path" || ! -f "$updated_event_path" ]]; then
  live_fail "provider-updated event path was not visible under events/"
fi
assert_file_contains "$updated_event_path" "summary: \"$updated_timed_summary\"" "provider-updated event projection"

step="attempting unsupported local update to projected event"
original_event_hash="$(sha256sum "$updated_event_path" | awk '{print $1}')"
write_status=0
# shellcheck disable=SC2016
if timeout "${LOCALITY_GOOGLE_CALENDAR_LIVE_WRITE_TIMEOUT_SECONDS:-10}s" \
  bash -c 'printf "\n%s\n" "$1" >>"$2"' \
  _ "$local_update_marker" "$updated_event_path" 2>>"$command_log"; then
  write_status=0
else
  write_status="$?"
fi
after_write_hash="$(sha256sum "$updated_event_path" | awk '{print $1}')"
if [[ "$write_status" == "124" ]]; then
  live_fail "Google Calendar projected event write hung instead of rejecting within the timeout"
fi
if [[ "$write_status" != "0" && "$original_event_hash" != "$after_write_hash" ]]; then
  live_fail "Google Calendar projected event changed after a rejected filesystem write"
fi

if [[ "$write_status" == "0" ]]; then
  step="diffing unsupported local Google Calendar event update"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$updated_event_path" \
    >"$edit_diff_report" 2>>"$command_log"
  assert_json_ok "$edit_diff_report" "Google Calendar scenario local-update diff report"
  assert_json_field_equals "$edit_diff_report" "action" "fix_validation" "Google Calendar scenario local-update diff report"
  assert_json_validation_code "$edit_diff_report" "google_calendar_events_read_only" "Google Calendar scenario local-update diff report"

  step="pushing unsupported local Google Calendar event update"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$updated_event_path" \
    >"$edit_push_report" 2>>"$command_log"
  assert_json_ok "$edit_push_report" "Google Calendar scenario local-update push report"
  assert_json_field_equals "$edit_push_report" "action" "fix_validation" "Google Calendar scenario local-update push report"
  assert_json_validation_code "$edit_push_report" "google_calendar_events_read_only" "Google Calendar scenario local-update push report"
fi

step="verifying unsupported local update did not mutate provider event"
get_calendar_event "$access_token" "$timed_event_id" "$provider_after_rejected_update_report"
assert_event_json \
  "$provider_after_rejected_update_report" \
  "$updated_timed_summary" \
  "$remote_update_marker" \
  "$local_update_marker"
unset access_token

step="deleting Google Calendar scenario scratch events"
cleanup_scratch_events required

echo "live Google Calendar retrieval, create, remote-update read-back, and read-only guardrail scenario passed"
