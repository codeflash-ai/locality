#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GMAIL_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GMAIL_VFS=1 to run the live Gmail VFS test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

require_linux_fuse
require_live_env \
  LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON \
  LOCALITY_GMAIL_LIVE_TO_EMAIL

if ! command -v curl >/dev/null 2>&1; then
  live_fail "curl is not installed"
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_GMAIL_LIVE_CONNECTION_ID:-gmail-live}"
mount_id="${LOCALITY_GMAIL_LIVE_MOUNT_ID:-gmail-live}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Gmail mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-gmail-vfs.XXXXXX")"
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
drafts_list_report="$tmp_root/gmail-drafts.json"
draft_get_report="$tmp_root/gmail-draft.json"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
message_id=""
raw_message_id=""
draft_id=""
draft_deleted=0
draft_cleanup_needed=0
subject=""
marker=""
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
  local attempts="${LOCALITY_GMAIL_LIVE_MOUNT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$mount_root" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Gmail FUSE mount root did not appear at $mount_root"
}

wait_for_draft_dir() {
  local draft_dir="$mount_root/draft"
  local attempts="${LOCALITY_GMAIL_LIVE_DRAFT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$draft_dir" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Gmail draft directory did not appear under the mount"
}

projected_gmail_draft_matches_message() {
  local path="$1"
  local searched_message_id="$2"

  python3 - "$path" "$searched_message_id" <<'PY'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
searched_message_id = sys.argv[2]
try:
    text = path.read_text(encoding="utf-8")
except OSError:
    raise SystemExit(1)

lines = text.splitlines()
if lines and lines[0].strip() == "---":
    frontmatter = []
    for line in lines[1:]:
        if line.strip() == "---":
            break
        frontmatter.append(line)
else:
    frontmatter = lines

def scalar(value):
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        return value[1:-1]
    return value

def key_value(line, key):
    match = re.match(rf"^\s*{re.escape(key)}:\s*(.*?)\s*$", line)
    if not match:
        return None
    return scalar(match.group(1))

def block_key(line):
    match = re.match(r"^(\s*)([A-Za-z0-9_.-]+):\s*$", line)
    if not match:
        return None
    return (len(match.group(1)), match.group(2))

has_gmail_connector = any(key_value(line, "connector") == "gmail" for line in frontmatter)
has_draft_mailbox = False
has_matching_message_id = False
active_block = None
active_indent = -1

for line in frontmatter:
    stripped = line.strip()
    if not stripped:
        continue

    indent = len(line) - len(line.lstrip(" "))
    if active_block is not None and indent <= active_indent:
        active_block = None
        active_indent = -1

    block = block_key(line)
    if block is not None:
        active_indent, active_block = block
        continue

    if active_block == "gmail":
        if key_value(line, "mailbox") == "draft":
            has_draft_mailbox = True
        if key_value(line, "message_id") == searched_message_id:
            has_matching_message_id = True
    elif active_block == "loc" and key_value(line, "id") == searched_message_id:
        has_matching_message_id = True

if has_gmail_connector and has_draft_mailbox and has_matching_message_id:
    raise SystemExit(0)
raise SystemExit(1)
PY
}

wait_for_marker_under_draft() {
  local marker="$1"
  local searched_message_id="$2"
  local draft_dir="$mount_root/draft"
  local attempts="${LOCALITY_GMAIL_LIVE_MARKER_WAIT_ATTEMPTS:-120}"
  local attempt
  local match_path

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$draft_dir" ]]; then
      while IFS= read -r match_path; do
        [[ -z "$match_path" ]] && continue
        if projected_gmail_draft_matches_message "$match_path" "$searched_message_id"; then
          return 0
        fi
      done < <(grep -R -F -l -- "$marker" "$draft_dir" 2>/dev/null || true)
    fi
    sleep 0.25
  done
  live_fail "created Gmail draft marker was not visible under draft/ after pull"
}

find_gmail_draft_id() {
  local drafts_json_path="$1"
  local searched_message_id="$2"

  python3 - "$drafts_json_path" "$searched_message_id" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
message_id = sys.argv[2]
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
for draft in data.get("drafts") or []:
    if not isinstance(draft, dict):
        continue
    message = draft.get("message")
    if isinstance(message, dict) and message.get("id") == message_id:
        draft_id = draft.get("id")
        if draft_id:
            print(draft_id)
        break
PY
}

gmail_draft_ids() {
  local drafts_json_path="$1"

  python3 - "$drafts_json_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
for draft in data.get("drafts") or []:
    if isinstance(draft, dict) and draft.get("id"):
        print(draft["id"])
PY
}

gmail_drafts_next_page_token() {
  local drafts_json_path="$1"

  python3 - "$drafts_json_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
token = data.get("nextPageToken")
if token:
    print(token)
PY
}

gmail_draft_subject_matches() {
  local draft_json_path="$1"
  local expected_subject="$2"

  python3 - "$draft_json_path" "$expected_subject" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_subject = sys.argv[2]
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
message = data.get("message")
if not isinstance(message, dict):
    raise SystemExit(1)
payload = message.get("payload")
if not isinstance(payload, dict):
    raise SystemExit(1)
for header in payload.get("headers") or []:
    if not isinstance(header, dict):
        continue
    if header.get("name", "").lower() == "subject" and header.get("value") == expected_subject:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

delete_created_gmail_draft() {
  local mode="${1:-best_effort}"
  local access_token

  if [[ "$draft_deleted" == "1" ]]; then
    return 0
  fi
  if [[ -z "$draft_id" && -z "$raw_message_id" && "$draft_cleanup_needed" != "1" ]]; then
    return 0
  fi
  if [[ -z "$credential_path" ]]; then
    credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
  fi

  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Gmail OAuth access token for cleanup"
    fi
    return 1
  fi

  if [[ -z "$draft_id" ]]; then
    local page_token=""
    local candidate_draft_id
    while :; do
      local curl_args=(
        -fsS
        --get
        "https://gmail.googleapis.com/gmail/v1/users/me/drafts"
        -H
        "Authorization: Bearer $access_token"
        --data-urlencode
        "maxResults=100"
      )
      if [[ -n "$page_token" ]]; then
        curl_args+=(--data-urlencode "pageToken=$page_token")
      fi
      if ! curl "${curl_args[@]}" >"$drafts_list_report" 2>>"$command_log"; then
        unset access_token
        if [[ "$mode" == "required" ]]; then
          live_fail "failed to list Gmail drafts during cleanup"
        fi
        return 1
      fi

      if [[ -n "$raw_message_id" ]]; then
        draft_id="$(find_gmail_draft_id "$drafts_list_report" "$raw_message_id" 2>/dev/null || true)"
      fi
      if [[ -z "$draft_id" && -n "${subject:-}" ]]; then
        while IFS= read -r candidate_draft_id; do
          [[ -z "$candidate_draft_id" ]] && continue
          if curl -fsS --get "https://gmail.googleapis.com/gmail/v1/users/me/drafts/$candidate_draft_id" \
            -H "Authorization: Bearer $access_token" \
            --data-urlencode "format=metadata" \
            --data-urlencode "metadataHeaders=Subject" \
            >"$draft_get_report" 2>>"$command_log" \
            && gmail_draft_subject_matches "$draft_get_report" "$subject"; then
            draft_id="$candidate_draft_id"
            break
          fi
        done < <(gmail_draft_ids "$drafts_list_report" 2>/dev/null || true)
      fi
      if [[ -n "$draft_id" ]]; then
        break
      fi
      page_token="$(gmail_drafts_next_page_token "$drafts_list_report" 2>/dev/null || true)"
      if [[ -z "$page_token" ]]; then
        break
      fi
    done

    if [[ -z "$draft_id" ]]; then
      unset access_token
      if [[ "$mode" == "required" ]]; then
        live_fail "could not find Gmail draft id for created message during cleanup"
      fi
      return 1
    fi
  fi

  if curl -fsS -X DELETE "https://gmail.googleapis.com/gmail/v1/users/me/drafts/$draft_id" \
    -H "Authorization: Bearer $access_token" >/dev/null 2>>"$command_log"; then
    draft_deleted=1
    draft_id=""
    raw_message_id=""
    message_id=""
    draft_cleanup_needed=0
    unset access_token
    return 0
  fi

  unset access_token
  if [[ "$mode" == "required" ]]; then
    live_fail "failed to delete created Gmail draft during cleanup"
  fi
  return 1
}

on_error() {
  local code=$?
  echo "live Gmail VFS round trip failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  return "$code"
}

cleanup() {
  set +e
  if [[ -n "${credential_path:-}" && -n "${oauth_refresh_marker:-}" ]]; then
    export_refreshed_oauth_credential_if_requested \
      "$credential_path" \
      "gmail" \
      "$oauth_refresh_marker" \
      "Gmail live credential" >/dev/null 2>&1 || true
  fi
  if [[ "$draft_deleted" != "1" \
    && ( -n "${draft_id:-}" || -n "${raw_message_id:-}" || "$draft_cleanup_needed" == "1" ) ]]; then
    delete_created_gmail_draft best_effort >/dev/null 2>&1 || \
      echo "warning: failed to delete created Gmail draft during cleanup" >&2
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GMAIL_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Gmail VFS temp root: $tmp_root"
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

step="seeding isolated Gmail OAuth credential"
seed_connector_credential \
  "$loc_bin" \
  "$state_root" \
  "gmail" \
  "$connection_id" \
  "$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON"
credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
require_oauth_credential_file "$credential_path" "gmail" "Gmail live credential"
if [[ -n "${LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT:-}" && "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" != "1" ]]; then
  live_fail "LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT requires LOCALITY_LIVE_FORCE_OAUTH_REFRESH=1"
fi
if [[ "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" == "1" ]]; then
  step="forcing Gmail OAuth credential refresh"
  force_oauth_credential_refresh "$credential_path" "gmail" "Gmail live credential"
  oauth_refresh_marker="$(oauth_credential_refresh_marker "$credential_path" "gmail" "Gmail live credential")"
fi
unset LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON

step="registering Gmail Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount gmail "$mount_root" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --view messages \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report" "Gmail mount report"

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="pulling Gmail workspace"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Gmail initial pull report"
if [[ -n "$oauth_refresh_marker" ]]; then
  step="verifying Gmail OAuth credential refresh"
  assert_oauth_credential_refreshed \
    "$credential_path" \
    "gmail" \
    "$oauth_refresh_marker" \
    "Gmail live credential"
  export_refreshed_oauth_credential_if_requested \
    "$credential_path" \
    "gmail" \
    "$oauth_refresh_marker" \
    "Gmail live credential"
fi
wait_for_draft_dir

unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
subject="Locality live Gmail $unique"
marker="Locality live Gmail VFS marker $unique"
draft_path="$mount_root/draft/locality-live-gmail-$unique.md"

step="creating Gmail draft through Linux FUSE"
printf -- '---\nto:\n  - "%s"\nsubject: "%s"\n---\n%s\n' \
  "$LOCALITY_GMAIL_LIVE_TO_EMAIL" \
  "$subject" \
  "$marker" >"$draft_path"

step="diffing created Gmail draft"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$draft_path" \
  >"$diff_report" 2>>"$command_log"
assert_json_ok "$diff_report" "Gmail diff report"
assert_json_field_equals "$diff_report" "action" "confirm_plan" "Gmail diff report"

step="pushing created Gmail draft"
draft_cleanup_needed=1
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$draft_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Gmail push report"
message_id="$(json_field "$push_report" "changed_remote_ids.0" 2>/dev/null || true)"
if [[ -z "$message_id" ]]; then
  live_fail "Gmail push report did not include changed_remote_ids.0"
fi
if [[ "$message_id" == gmail-message:* ]]; then
  raw_message_id="${message_id#gmail-message:}"
else
  raw_message_id="$message_id"
fi
if [[ -z "$raw_message_id" ]]; then
  live_fail "Gmail push report produced an empty message id"
fi

step="pulling Gmail workspace after push"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Gmail pull-after-push report"

step="verifying created Gmail draft marker under draft"
wait_for_marker_under_draft "$marker" "$raw_message_id"

step="deleting created Gmail draft"
delete_created_gmail_draft required

echo "live Gmail API, CLI, daemon, and Linux FUSE draft checks passed"
