#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_SLACK_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_SLACK_VFS=1 to run the live Slack VFS test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_SLACK_LIVE_CONNECTION_ID:-slack-live}"
mount_id="${LOCALITY_SLACK_LIVE_MOUNT_ID:-slack-live}"
slack_types="${LOCALITY_SLACK_LIVE_TYPES:-private_channel,im,mpim}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Slack mount or connection id has an invalid shape"
fi
IFS=',' read -r -a slack_type_parts <<<"$slack_types"
for slack_type in "${slack_type_parts[@]}"; do
  slack_type="${slack_type//[[:space:]]/}"
  if [[ "$slack_type" == "public_channel" ]]; then
    live_fail "live Slack VFS test refuses public_channel because Slack mounts auto-join public channels before reading"
  fi
done

require_linux_fuse
require_live_env \
  LOCALITY_SLACK_LIVE_CREDENTIAL_JSON \
  LOCALITY_SLACK_LIVE_CONVERSATION_ID

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-slack-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/$mount_id"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
command_log="$tmp_root/commands.err.log"
mount_report="$tmp_root/mount.json"
initial_pull_report="$tmp_root/initial-pull.json"
push_report="$tmp_root/push.json"
status_report="$tmp_root/status.json"
original_copy="$tmp_root/recent-original.md"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
recent_path=""
step="initializing"

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
  local attempts="${LOCALITY_SLACK_LIVE_MOUNT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$mount_root" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Slack FUSE mount root did not appear at $mount_root"
}

state_recent_relative_path() {
  local conversation_id="$1"
  local remote_id="slack-recent:$conversation_id"
  local mount_id_sql
  local remote_id_sql
  local result

  if [[ ! -s "$state_root/state.sqlite3" ]]; then
    return 1
  fi

  mount_id_sql="$(sql_text_literal "$mount_id")"
  remote_id_sql="$(sql_text_literal "$remote_id")"
  if ! result="$(sqlite3 -cmd '.timeout 10000' "$state_root/state.sqlite3" \
    "SELECT path FROM entities WHERE mount_id = $mount_id_sql AND remote_id = $remote_id_sql LIMIT 1;" \
    2>>"$command_log")"; then
    return 1
  fi
  if [[ -z "$result" ]]; then
    return 1
  fi
  printf '%s\n' "$result"
}

recent_path_from_state() {
  local conversation_id="$1"
  local relative_path

  relative_path="$(state_recent_relative_path "$conversation_id" || true)"
  if [[ -z "$relative_path" ]]; then
    return 1
  fi
  if [[ "$relative_path" == /* \
    || "$relative_path" == "." \
    || "$relative_path" == ".." \
    || "$relative_path" == ../* \
    || "$relative_path" == */../* \
    || "$relative_path" == */.. \
    || "$relative_path" != */recent.md ]]; then
    live_fail "Slack state resolved an unsafe or unexpected recent.md path"
  fi

  printf '%s/%s\n' "$mount_root" "$relative_path"
}

find_recent_by_path_suffix() {
  local conversation_id="$1"
  local candidate

  while IFS= read -r -d '' candidate; do
    if [[ "$candidate" == *"$conversation_id/recent.md" \
      || "$candidate" == *"-$conversation_id/recent.md" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done < <(find "$mount_root" -type f -name recent.md -print0 2>>"$command_log" || true)

  return 1
}

wait_for_target_recent() {
  local conversation_id="$1"
  local attempts="${LOCALITY_SLACK_LIVE_RECENT_WAIT_ATTEMPTS:-160}"
  local attempt
  local match

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    match="$(recent_path_from_state "$conversation_id" || true)"
    if [[ -n "$match" && -e "$match" ]]; then
      printf '%s\n' "$match"
      return 0
    fi

    match="$(find_recent_by_path_suffix "$conversation_id" 2>/dev/null || true)"
    if [[ -n "$match" ]]; then
      printf '%s\n' "$match"
      return 0
    fi

    sleep 0.25
  done

  live_fail "Slack recent.md for configured conversation was not found under the mount"
}

validate_recent_markdown() {
  local path="$1"
  local conversation_id="$2"

  python3 - "$path" "$conversation_id" <<'PY'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
conversation_id = sys.argv[2]
text = path.read_text(encoding="utf-8")

if "connector: slack\n" not in text:
    raise SystemExit("Slack recent.md omitted connector identity")
conversation_pattern = re.compile(
    rf"^  conversation_id:\s*\"?{re.escape(conversation_id)}\"?\s*$",
    re.MULTILINE,
)
if not conversation_pattern.search(text):
    raise SystemExit("Slack recent.md omitted the configured conversation id")

try:
    _, _frontmatter, body = text.split("---\n", 2)
except ValueError:
    raise SystemExit("Slack recent.md frontmatter was not terminated")

has_message_heading = re.search(r"^## .+$", body, re.MULTILINE) is not None
has_no_messages = "_No recent Slack messages were returned for this conversation._" in body
if not has_message_heading and not has_no_messages:
    raise SystemExit("Slack recent.md did not contain recent message headings or the no-messages placeholder")
PY
}

assert_push_blocked_as_read_only() {
  local report_path="$1"

  python3 - "$report_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
try:
    report = json.loads(path.read_text(encoding="utf-8"))
except Exception as error:
    raise SystemExit(f"Slack push report was not valid JSON: {error}") from error

if report.get("ok") is True:
    raise SystemExit("Slack push unexpectedly reported ok=true")

def contains_slack_read_only(value):
    if isinstance(value, dict):
        if value.get("code") == "slack_read_only":
            return True
        return any(contains_slack_read_only(child) for child in value.values())
    if isinstance(value, list):
        return any(contains_slack_read_only(child) for child in value)
    return False

if not contains_slack_read_only(report):
    raise SystemExit("Slack push report did not include validation code slack_read_only")
PY
}

assert_status_clean_for_target() {
  local report_path="$1"
  local target_path="$2"
  local relative_path="${target_path#"$mount_root"/}"

  python3 - "$report_path" "$target_path" "$relative_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
target_path = sys.argv[2]
relative_path = sys.argv[3]
try:
    report = json.loads(path.read_text(encoding="utf-8"))
except Exception as error:
    raise SystemExit(f"Slack status report was not valid JSON: {error}") from error

if not isinstance(report, dict):
    raise SystemExit("Slack status report was not a JSON object")
if report.get("ok") is not True:
    raise SystemExit("Slack status report did not include ok=true")
if report.get("clean") is not True:
    raise SystemExit("Slack status report did not include clean=true")

selected = []
for mount in report.get("mounts") or []:
    if not isinstance(mount, dict):
        continue
    for entry in mount.get("entries") or []:
        if not isinstance(entry, dict):
            continue
        if entry.get("absolute_path") == target_path or entry.get("path") == relative_path:
            selected.append(entry)

if not selected:
    raise SystemExit("Slack status report omitted the selected recent.md entry")

allowed_sync_states = {"all_synced", "checking_freshness"}
for entry in selected:
    if entry.get("state") != "clean":
        raise SystemExit("Slack selected recent.md status was not clean")
    if entry.get("sync_state") not in allowed_sync_states:
        raise SystemExit("Slack selected recent.md had an unexpected sync_state")
    if entry.get("pending_journal_count") != 0:
        raise SystemExit("Slack selected recent.md had pending journals")
    if entry.get("failed_journal_count") != 0:
        raise SystemExit("Slack selected recent.md had failed journals")
PY
}

on_error() {
  local code=$?
  echo "live Slack VFS read-only test failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  emit_live_debug_diagnostics "Slack VFS read-only test" || true
  return "$code"
}

cleanup() {
  set +e
  if [[ -n "${credential_path:-}" && -n "${oauth_refresh_marker:-}" ]]; then
    export_refreshed_oauth_credential_if_requested \
      "$credential_path" \
      "slack" \
      "$oauth_refresh_marker" \
      "Slack live credential" >/dev/null 2>&1 || true
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_SLACK_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_SLACK_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Slack VFS temp root: $tmp_root"
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

step="seeding isolated Slack OAuth credential"
seed_connector_credential \
  "$loc_bin" \
  "$state_root" \
  "slack" \
  "$connection_id" \
  "$LOCALITY_SLACK_LIVE_CREDENTIAL_JSON"
credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
require_oauth_credential_file "$credential_path" "slack" "Slack live credential"
if [[ -n "${LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT:-}" && "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" != "1" ]]; then
  live_fail "LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT requires LOCALITY_LIVE_FORCE_OAUTH_REFRESH=1"
fi
if [[ "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" == "1" ]]; then
  step="forcing Slack OAuth credential refresh"
  force_oauth_credential_refresh "$credential_path" "slack" "Slack live credential"
  oauth_refresh_marker="$(oauth_credential_refresh_marker "$credential_path" "slack" "Slack live credential")"
fi
unset LOCALITY_SLACK_LIVE_CREDENTIAL_JSON

step="registering Slack Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount slack "$mount_root" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --history-limit 3 \
    --types "$slack_types" \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report" "Slack mount report"
assert_json_field_equals "$mount_report" "read_only" "true" "Slack mount report"

if [[ -n "$oauth_refresh_marker" ]]; then
  # Slack refresh handles are single-use. Refresh once before starting the
  # daemon and FUSE consumers so the deliberately expired test credential is
  # never presented concurrently by multiple processes.
  step="refreshing Slack OAuth credential before starting live consumers"
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
    "$loc_bin" pull --json "$mount_root" \
    >"$initial_pull_report" 2>>"$command_log"
  assert_json_ok "$initial_pull_report" "Slack credential refresh pull report"

  step="verifying Slack OAuth credential refresh"
  assert_oauth_credential_refreshed \
    "$credential_path" \
    "slack" \
    "$oauth_refresh_marker" \
    "Slack live credential"
  export_refreshed_oauth_credential_if_requested \
    "$credential_path" \
    "slack" \
    "$oauth_refresh_marker" \
    "Slack live credential"
fi

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="pulling Slack workspace"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Slack initial pull report"

step="finding configured Slack conversation recent.md"
recent_path="$(wait_for_target_recent "$LOCALITY_SLACK_LIVE_CONVERSATION_ID")"

step="validating configured Slack conversation recent.md"
validate_recent_markdown "$recent_path" "$LOCALITY_SLACK_LIVE_CONVERSATION_ID"

step="verifying Slack recent.md rejects Linux FUSE writes"
cp "$recent_path" "$original_copy"
original_hash="$(sha256sum "$recent_path" | awk '{print $1}')"
write_status=0
{ printf '\nlive e2e must not write\n' >>"$recent_path"; } 2>>"$command_log" || write_status="$?"
after_write_hash="$(sha256sum "$recent_path" | awk '{print $1}')"
if [[ "$write_status" == "0" ]]; then
  if [[ "$original_hash" != "$after_write_hash" ]]; then
    cp "$original_copy" "$recent_path" 2>>"$command_log" || true
    live_fail "Slack mounted recent.md unexpectedly accepted a filesystem write"
  fi
  live_fail "Slack mounted recent.md write command unexpectedly exited successfully"
fi
if [[ "$original_hash" != "$after_write_hash" ]]; then
  cp "$original_copy" "$recent_path" 2>>"$command_log" || true
  live_fail "Slack mounted recent.md changed after a rejected write"
fi

step="verifying Slack read-only push validation"
if LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$recent_path" \
  >"$push_report" 2>>"$command_log"; then
  push_status=0
else
  push_status="$?"
fi
if [[ "$push_status" == "0" ]]; then
  live_fail "Slack read-only push unexpectedly exited successfully"
fi
assert_push_blocked_as_read_only "$push_report"

step="checking Slack recent.md status"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status --json "$recent_path" \
  >"$status_report" 2>>"$command_log"
assert_status_clean_for_target "$status_report" "$recent_path"

echo "live Slack API, CLI, daemon, and Linux FUSE read-only checks passed"
