#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

if [[ "${LOCALITY_LIVE_GMAIL_SELFTEST:-}" != "1" ]]; then
  if [[ "${LOCALITY_LIVE_GMAIL_VFS:-}" != "1" ]]; then
    echo "skip: set LOCALITY_LIVE_GMAIL_VFS=1 to run the live Gmail VFS test"
    exit 0
  fi

  require_linux_fuse
  require_live_env \
    LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON \
    LOCALITY_GMAIL_LIVE_TO_EMAIL

  if ! command -v curl >/dev/null 2>&1; then
    live_fail "curl is not installed"
  fi
fi

if ! command -v python3 >/dev/null 2>&1; then
  live_fail "python3 is not installed"
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
send_status_report="$tmp_root/send-status.json"
send_diff_report="$tmp_root/send-diff.json"
send_push_report="$tmp_root/send-push.json"
send_pull_after_push_report="$tmp_root/send-pull-after-push.json"
remote_draft_diff_report="$tmp_root/remote-draft-diff.json"
remote_draft_push_report="$tmp_root/remote-draft-push.json"
remote_draft_get_report="$tmp_root/remote-draft.json"
remote_draft_send_diff_report="$tmp_root/remote-draft-send-diff.json"
remote_draft_send_push_report="$tmp_root/remote-draft-send-push.json"
remote_sent_list_report="$tmp_root/remote-sent-list.json"
remote_sent_get_report="$tmp_root/remote-sent-message.json"
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
remote_sent_message_id=""
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

wait_for_outbound_dirs() {
  local draft_dir="$mount_root/draft"
  local outbox_dir="$mount_root/outbox"
  local attempts="${LOCALITY_GMAIL_LIVE_DRAFT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$draft_dir" && -d "$outbox_dir" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Gmail draft/ and outbox/ directories did not appear under the mount"
}

projected_gmail_draft_matches_message() {
  local path="$1"
  local searched_message_id="${2:-}"
  local searched_draft_id="${3:-}"

  python3 - "$path" "$searched_message_id" "$searched_draft_id" <<'PY'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
searched_message_id = sys.argv[2]
searched_draft_id = sys.argv[3]
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
has_matching_identity = False
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
        if searched_draft_id and key_value(line, "draft_id") == searched_draft_id:
            has_matching_identity = True
        if searched_message_id and key_value(line, "message_id") == searched_message_id:
            has_matching_identity = True
    elif active_block == "loc":
        loc_id = key_value(line, "id")
        if searched_draft_id and loc_id == f"gmail-draft:{searched_draft_id}":
            has_matching_identity = True
        if searched_message_id and loc_id == searched_message_id:
            has_matching_identity = True

if has_gmail_connector and has_draft_mailbox and has_matching_identity:
    raise SystemExit(0)
raise SystemExit(1)
PY
}

find_marker_under_draft() {
  local marker="$1"
  local searched_message_id="${2:-}"
  local searched_draft_id="${3:-}"
  local draft_dir="$mount_root/draft"
  local attempts="${LOCALITY_GMAIL_LIVE_MARKER_WAIT_ATTEMPTS:-120}"
  local attempt
  local match_path

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$draft_dir" ]]; then
      while IFS= read -r match_path; do
        [[ -z "$match_path" ]] && continue
        if projected_gmail_draft_matches_message "$match_path" "$searched_message_id" "$searched_draft_id"; then
          printf '%s\n' "$match_path"
          return 0
        fi
      done < <(grep -R -F -l -- "$marker" "$draft_dir" 2>/dev/null || true)
    fi
    sleep 0.25
  done
  live_fail "created Gmail draft marker was not visible under draft/ after pull"
}

wait_for_marker_under_draft() {
  find_marker_under_draft "$@" >/dev/null
}

wait_for_marker_under_sent() {
  local marker="$1"
  local sent_dir="$mount_root/sent"
  local attempts="${LOCALITY_GMAIL_LIVE_SENT_MARKER_WAIT_ATTEMPTS:-120}"
  local attempt

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$sent_dir" ]] && grep -R -F -q -- "$marker" "$sent_dir" 2>/dev/null; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "direct Gmail send marker was not visible under sent/ after pull"
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

gmail_message_subject_body_matches() {
  local message_json_path="$1"
  local expected_subject="$2"
  local expected_body_marker="$3"
  local wrapper="${4:-message}"

  python3 - "$message_json_path" "$expected_subject" "$expected_body_marker" "$wrapper" <<'PY'
import base64
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_subject = sys.argv[2]
expected_body_marker = sys.argv[3]
wrapper = sys.argv[4]

try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)

message = data.get("message") if wrapper == "draft" else data
if not isinstance(message, dict):
    raise SystemExit(1)
payload = message.get("payload")
if not isinstance(payload, dict):
    raise SystemExit(1)

subject = None
for header in payload.get("headers") or []:
    if isinstance(header, dict) and header.get("name", "").lower() == "subject":
        subject = header.get("value")
        break
if subject != expected_subject:
    raise SystemExit(1)

def decode_body(data):
    if not isinstance(data, str) or not data:
        return ""
    padding = "=" * (-len(data) % 4)
    try:
        return base64.urlsafe_b64decode((data + padding).encode("ascii")).decode("utf-8", "replace")
    except Exception:
        return ""

def collect_text_parts(part):
    if not isinstance(part, dict):
        return []
    texts = []
    body = part.get("body")
    mime_type = part.get("mimeType")
    if isinstance(body, dict) and body.get("data") and (mime_type in (None, "text/plain") or not part.get("parts")):
        texts.append(decode_body(body.get("data")))
    for child in part.get("parts") or []:
        texts.extend(collect_text_parts(child))
    return texts

body_text = "\n".join(collect_text_parts(payload))
if expected_body_marker in body_text:
    raise SystemExit(0)
raise SystemExit(1)
PY
}

gmail_draft_subject_body_matches() {
  local draft_json_path="$1"
  local expected_subject="$2"
  local expected_body_marker="$3"

  gmail_message_subject_body_matches "$draft_json_path" "$expected_subject" "$expected_body_marker" draft
}

rewrite_gmail_markdown_subject_body() {
  local markdown_path="$1"
  local updated_subject="$2"
  local updated_body="$3"

  python3 - "$markdown_path" "$updated_subject" "$updated_body" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
updated_subject = sys.argv[2]
updated_body = sys.argv[3]
text = path.read_text(encoding="utf-8")
lines = text.splitlines()
if not lines or lines[0].strip() != "---":
    raise SystemExit("missing frontmatter")

closing_index = None
for index, line in enumerate(lines[1:], start=1):
    if line.strip() == "---":
        closing_index = index
        break
if closing_index is None:
    raise SystemExit("unterminated frontmatter")

def yaml_scalar(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'

frontmatter = lines[:closing_index]
subject_line = "subject: " + yaml_scalar(updated_subject)
for index, line in enumerate(frontmatter):
    if line.startswith("subject:"):
        frontmatter[index] = subject_line
        break
else:
    frontmatter.append(subject_line)

path.write_text("\n".join(frontmatter + ["---", updated_body, ""]), encoding="utf-8")
PY
}

gmail_access_token() {
  local mode="${1:-required}"
  local access_token

  if [[ -z "$credential_path" ]]; then
    credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
  fi

  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Gmail OAuth access token"
    fi
    return 1
  fi
  printf '%s\n' "$access_token"
}

get_gmail_draft_full() {
  local searched_draft_id="$1"
  local output_path="$2"
  local access_token

  access_token="$(gmail_access_token required)"
  if curl -fsS --get "https://gmail.googleapis.com/gmail/v1/users/me/drafts/$searched_draft_id" \
    -H "Authorization: Bearer $access_token" \
    --data-urlencode "format=full" \
    >"$output_path" 2>>"$command_log"; then
    unset access_token
    return 0
  fi
  unset access_token
  return 1
}

wait_for_gmail_draft_content() {
  local searched_draft_id="$1"
  local expected_subject="$2"
  local expected_body_marker="$3"
  local attempts="${LOCALITY_GMAIL_LIVE_API_WAIT_ATTEMPTS:-120}"
  local attempt

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if get_gmail_draft_full "$searched_draft_id" "$remote_draft_get_report" \
      && gmail_message_subject_body_matches "$remote_draft_get_report" "$expected_subject" "$expected_body_marker" draft; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "updated Gmail draft content was not visible through the drafts API"
}

find_gmail_sent_message_id_for_subject_body() {
  local expected_subject="$1"
  local expected_body_marker="$2"
  local access_token
  local message_id
  local attempts="${LOCALITY_GMAIL_LIVE_API_WAIT_ATTEMPTS:-120}"
  local attempt

  access_token="$(gmail_access_token required)"
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if curl -fsS --get "https://gmail.googleapis.com/gmail/v1/users/me/messages" \
      -H "Authorization: Bearer $access_token" \
      --data-urlencode "maxResults=10" \
      --data-urlencode "q=in:sent subject:\"$expected_subject\"" \
      >"$remote_sent_list_report" 2>>"$command_log"; then
      while IFS= read -r message_id; do
        [[ -z "$message_id" ]] && continue
        if curl -fsS --get "https://gmail.googleapis.com/gmail/v1/users/me/messages/$message_id" \
          -H "Authorization: Bearer $access_token" \
          --data-urlencode "format=full" \
          >"$remote_sent_get_report" 2>>"$command_log" \
          && gmail_message_subject_body_matches "$remote_sent_get_report" "$expected_subject" "$expected_body_marker" message; then
          printf '%s\n' "$message_id"
          unset access_token
          return 0
        fi
      done < <(python3 - "$remote_sent_list_report" <<'PY'
import json
import pathlib
import sys

try:
    data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(0)
for message in data.get("messages") or []:
    if isinstance(message, dict) and message.get("id"):
        print(message["id"])
PY
)
    fi
    sleep 0.25
  done
  unset access_token
  return 1
}

trash_gmail_message() {
  local searched_message_id="$1"
  local mode="${2:-best_effort}"
  local access_token

  [[ -z "$searched_message_id" ]] && return 0

  access_token="$(gmail_access_token "$mode" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    return 1
  fi

  if curl -fsS -X POST "https://gmail.googleapis.com/gmail/v1/users/me/messages/$searched_message_id/trash" \
    -H "Authorization: Bearer $access_token" >/dev/null 2>>"$command_log"; then
    unset access_token
    return 0
  fi

  unset access_token
  if [[ "$mode" == "required" ]]; then
    live_fail "failed to trash sent Gmail scratch message"
  fi
  return 1
}

resolve_created_gmail_draft_id() {
  local mode="${1:-best_effort}"
  local access_token

  if [[ -n "$draft_id" ]]; then
    return 0
  fi
  if [[ -z "$raw_message_id" && "$draft_cleanup_needed" != "1" ]]; then
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
        live_fail "failed to list Gmail drafts"
      fi
      return 1
    fi

    if [[ -n "$raw_message_id" ]]; then
      draft_id="$(find_gmail_draft_id "$drafts_list_report" "$raw_message_id" 2>/dev/null || true)"
    fi
    if [[ -z "$draft_id" && -n "${subject:-}" && -n "${marker:-}" ]]; then
      while IFS= read -r candidate_draft_id; do
        [[ -z "$candidate_draft_id" ]] && continue
        if curl -fsS --get "https://gmail.googleapis.com/gmail/v1/users/me/drafts/$candidate_draft_id" \
          -H "Authorization: Bearer $access_token" \
          --data-urlencode "format=full" \
          >"$draft_get_report" 2>>"$command_log" \
          && gmail_draft_subject_body_matches "$draft_get_report" "$subject" "$marker"; then
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

  unset access_token
  if [[ -z "$draft_id" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not find Gmail draft id for created message"
    fi
    return 1
  fi
  return 0
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

  if [[ -z "$draft_id" ]]; then
    if ! resolve_created_gmail_draft_id "$mode"; then
      return 1
    fi
    if [[ -z "$draft_id" ]]; then
      if [[ "$mode" == "required" ]]; then
        live_fail "could not find Gmail draft id for created message during cleanup"
      fi
      return 1
    fi
  fi

  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Gmail OAuth access token for cleanup"
    fi
    return 1
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
  emit_live_debug_diagnostics "Gmail VFS round trip" || true
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
  if [[ -n "${remote_sent_message_id:-}" ]]; then
    trash_gmail_message "$remote_sent_message_id" best_effort >/dev/null 2>&1 || \
      echo "warning: failed to trash sent Gmail scratch message during cleanup" >&2
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GMAIL_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Gmail VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}

run_gmail_helper_selftest() {
  local selftest_subject="Locality Gmail helper self-test subject"
  local selftest_marker="Locality Gmail helper self-test body marker"
  local encoded_marker
  local draft_json="$tmp_root/selftest-draft.json"
  local message_json="$tmp_root/selftest-message.json"

  encoded_marker="$(python3 - "$selftest_marker" <<'PY'
import base64
import sys

print(base64.urlsafe_b64encode(sys.argv[1].encode("utf-8")).decode("ascii").rstrip("="))
PY
)"

  cat >"$draft_json" <<JSON
{
  "message": {
    "payload": {
      "mimeType": "text/plain",
      "headers": [
        {"name": "Subject", "value": "$selftest_subject"}
      ],
      "body": {"data": "$encoded_marker"}
    }
  }
}
JSON

  cat >"$message_json" <<JSON
{
  "payload": {
    "mimeType": "text/plain",
    "headers": [
      {"name": "Subject", "value": "$selftest_subject"}
    ],
    "body": {"data": "$encoded_marker"}
  }
}
JSON

  if ! gmail_draft_subject_body_matches "$draft_json" "$selftest_subject" "$selftest_marker"; then
    live_fail "Gmail helper self-test rejected a matching draft subject/body marker"
  fi
  if gmail_draft_subject_body_matches "$draft_json" "$selftest_subject" "wrong body marker"; then
    live_fail "Gmail helper self-test accepted a draft with the wrong body marker"
  fi
  if gmail_draft_subject_body_matches "$draft_json" "wrong subject" "$selftest_marker"; then
    live_fail "Gmail helper self-test accepted a draft with the wrong subject"
  fi
  if ! gmail_message_subject_body_matches "$message_json" "$selftest_subject" "$selftest_marker" message; then
    live_fail "Gmail helper self-test rejected a matching message subject/body marker"
  fi
}

if [[ "${LOCALITY_LIVE_GMAIL_SELFTEST:-}" == "1" ]]; then
  trap 'rm -rf "$tmp_root"' EXIT
  run_gmail_helper_selftest
  echo "live Gmail helper self-test passed"
  exit 0
fi

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
wait_for_outbound_dirs

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
if [[ "$message_id" == gmail-draft:* ]]; then
  draft_id="${message_id#gmail-draft:}"
  raw_message_id=""
elif [[ "$message_id" == gmail-message:* ]]; then
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
wait_for_marker_under_draft "$marker" "$raw_message_id" "$draft_id"

step="deleting created Gmail draft"
delete_created_gmail_draft required

if [[ "${LOCALITY_LIVE_GMAIL_SEND:-0}" == "1" ]]; then
  send_subject="Locality live Gmail send $unique"
  send_marker="Locality live Gmail direct send marker $unique"
  send_path="$mount_root/outbox/locality-live-gmail-outbox-$unique.md"

  step="creating Gmail direct send through Linux FUSE"
  printf -- '---\nto:\n  - "%s"\nsubject: "%s"\n---\n%s\n' \
    "$LOCALITY_GMAIL_LIVE_TO_EMAIL" \
    "$send_subject" \
    "$send_marker" >"$send_path"

  step="checking Gmail direct send status"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" status --json "$send_path" \
    >"$send_status_report" 2>>"$command_log"
  assert_json_ok "$send_status_report" "Gmail send status report"

  step="diffing Gmail direct send"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$send_path" \
    >"$send_diff_report" 2>>"$command_log"
  assert_json_ok "$send_diff_report" "Gmail send diff report"
  assert_json_field_equals "$send_diff_report" "action" "confirm_plan" "Gmail send diff report"

  step="pushing Gmail direct send"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$send_path" \
    >"$send_push_report" 2>>"$command_log"
  assert_json_ok "$send_push_report" "Gmail send push report"

  step="pulling Gmail workspace after direct send"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
    >"$send_pull_after_push_report" 2>>"$command_log"
  assert_json_ok "$send_pull_after_push_report" "Gmail send pull-after-push report"

  step="verifying Gmail direct send marker under sent"
  wait_for_marker_under_sent "$send_marker"

  remote_subject="Locality live Gmail remote draft $unique"
  remote_marker="Locality live Gmail remote draft marker $unique"
  remote_updated_subject="Locality live Gmail remote draft updated $unique"
  remote_updated_marker="Locality live Gmail remote draft updated marker $unique"
  remote_draft_path="$mount_root/draft/locality-live-gmail-remote-draft-$unique.md"
  remote_outbox_path="$mount_root/outbox/locality-live-gmail-remote-draft-$unique.md"

  step="creating Gmail remote draft edit/send draft through Linux FUSE"
  draft_deleted=0
  draft_cleanup_needed=1
  subject="$remote_subject"
  marker="$remote_marker"
  printf -- '---\nto:\n  - "%s"\nsubject: "%s"\n---\n%s\n' \
    "$LOCALITY_GMAIL_LIVE_TO_EMAIL" \
    "$remote_subject" \
    "$remote_marker" >"$remote_draft_path"

  step="pushing Gmail remote draft edit/send draft"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$remote_draft_path" \
    >"$remote_draft_push_report" 2>>"$command_log"
  assert_json_ok "$remote_draft_push_report" "Gmail remote draft create push report"
  remote_created_id="$(json_field "$remote_draft_push_report" "changed_remote_ids.0" 2>/dev/null || true)"
  if [[ -z "$remote_created_id" ]]; then
    live_fail "Gmail remote draft create push report did not include changed_remote_ids.0"
  fi
  if [[ "$remote_created_id" == gmail-draft:* ]]; then
    draft_id="${remote_created_id#gmail-draft:}"
    raw_message_id=""
  elif [[ "$remote_created_id" == gmail-message:* ]]; then
    raw_message_id="${remote_created_id#gmail-message:}"
    draft_id=""
  else
    raw_message_id="$remote_created_id"
    draft_id=""
  fi

  step="pulling Gmail workspace after remote draft create"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
    >"$pull_after_push_report" 2>>"$command_log"
  assert_json_ok "$pull_after_push_report" "Gmail pull-after-remote-draft-create report"

  if [[ -z "$draft_id" ]]; then
    resolve_created_gmail_draft_id required
  fi

  step="finding projected Gmail remote draft"
  remote_projected_draft_path="$(find_marker_under_draft "$remote_marker" "$raw_message_id" "$draft_id")"
  if [[ -z "$remote_projected_draft_path" ]]; then
    live_fail "created Gmail remote draft file was not visible under draft/"
  fi

  step="editing projected Gmail remote draft"
  rewrite_gmail_markdown_subject_body "$remote_projected_draft_path" "$remote_updated_subject" "$remote_updated_marker"

  step="diffing edited Gmail remote draft"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$remote_projected_draft_path" \
    >"$remote_draft_diff_report" 2>>"$command_log"
  assert_json_ok "$remote_draft_diff_report" "Gmail remote draft edit diff report"
  assert_json_field_equals "$remote_draft_diff_report" "action" "confirm_plan" "Gmail remote draft edit diff report"

  step="pushing edited Gmail remote draft"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$remote_projected_draft_path" \
    >"$remote_draft_push_report" 2>>"$command_log"
  assert_json_ok "$remote_draft_push_report" "Gmail remote draft edit push report"

  step="verifying edited Gmail remote draft through drafts API"
  wait_for_gmail_draft_content "$draft_id" "$remote_updated_subject" "$remote_updated_marker"

  step="moving edited Gmail remote draft to outbox"
  mv "$remote_projected_draft_path" "$remote_outbox_path"

  step="diffing Gmail remote draft send move"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$remote_outbox_path" \
    >"$remote_draft_send_diff_report" 2>>"$command_log"
  assert_json_ok "$remote_draft_send_diff_report" "Gmail remote draft send diff report"
  assert_json_field_equals "$remote_draft_send_diff_report" "action" "confirm_plan" "Gmail remote draft send diff report"

  step="pushing Gmail remote draft send move"
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$remote_outbox_path" \
    >"$remote_draft_send_push_report" 2>>"$command_log"
  assert_json_ok "$remote_draft_send_push_report" "Gmail remote draft send push report"
  draft_deleted=1
  draft_cleanup_needed=0
  draft_id=""
  raw_message_id=""

  step="verifying Gmail remote draft sent message through Gmail API"
  verified_remote_sent_id="$(find_gmail_sent_message_id_for_subject_body "$remote_updated_subject" "$remote_updated_marker" || true)"
  if [[ -z "$verified_remote_sent_id" ]]; then
    live_fail "sent Gmail remote draft message with updated content was not visible through Gmail API"
  fi
  remote_sent_message_id="$verified_remote_sent_id"

  step="trashing sent Gmail remote draft scratch message"
  trash_gmail_message "$remote_sent_message_id" required
  remote_sent_message_id=""

  echo "live Gmail API, CLI, daemon, and Linux FUSE draft, direct-send, and remote draft edit/send checks passed"
else
  echo "skip: set LOCALITY_LIVE_GMAIL_SEND=1 to run the live Gmail direct-send and remote draft edit/send checks; this sends real email"
  echo "live Gmail API, CLI, daemon, and Linux FUSE draft checks passed"
fi
