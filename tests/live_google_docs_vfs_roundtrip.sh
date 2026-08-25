#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_DOCS_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 to run the live Google Docs VFS test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"

require_linux_fuse
require_live_env \
  LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON \
  LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_FOLDER

if ! command -v curl >/dev/null 2>&1; then
  live_fail "curl is not installed"
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_GOOGLE_DOCS_LIVE_CONNECTION_ID:-google-docs-live}"
mount_id="${LOCALITY_GOOGLE_DOCS_LIVE_MOUNT_ID:-google-docs-live}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Google Docs mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-docs-vfs.XXXXXX")"
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
edit_diff_report="$tmp_root/edit-diff.json"
edit_push_report="$tmp_root/edit-push.json"
pull_after_edit_report="$tmp_root/pull-after-edit.json"
drive_search_report="$tmp_root/drive-search.json"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
doc_id=""
doc_trashed=0
doc_cleanup_needed=0
page_title=""
marker=""
edit_marker=""
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
  local attempts="${LOCALITY_GOOGLE_DOCS_LIVE_MOUNT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$mount_root" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Google Docs FUSE mount root did not appear at $mount_root"
}

wait_for_marker_under_mount() {
  local marker="$1"
  local attempts="${LOCALITY_GOOGLE_DOCS_LIVE_MARKER_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if grep -R -Fq -- "$marker" "$mount_root" 2>/dev/null; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "created Google Docs marker was not visible under the mount after pull"
}

find_marker_path_under_mount() {
  local marker="$1"
  local match

  match="$(grep -R -F -l -- "$marker" "$mount_root" 2>/dev/null | head -n 1 || true)"
  if [[ -z "$match" ]]; then
    return 1
  fi
  printf '%s\n' "$match"
}

find_created_drive_file_by_title() {
  local access_token="$1"

  if [[ -z "${page_title:-}" ]]; then
    return 1
  fi

  if ! curl -fsS --get "https://www.googleapis.com/drive/v3/files" \
    -H "Authorization: Bearer $access_token" \
    --data-urlencode "q=name = '$page_title' and mimeType = 'application/vnd.google-apps.document' and trashed = false" \
    --data-urlencode "fields=files(id,name)" \
    --data-urlencode "pageSize=10" \
    >"$drive_search_report" 2>>"$command_log"; then
    return 1
  fi

  python3 - "$drive_search_report" "$page_title" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
title = sys.argv[2]
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
for item in data.get("files") or []:
    if isinstance(item, dict) and item.get("name") == title and item.get("id"):
        print(item["id"])
        raise SystemExit(0)
raise SystemExit(1)
PY
}

trash_created_drive_file() {
  local mode="${1:-best_effort}"
  local access_token

  if [[ "$doc_trashed" == "1" ]]; then
    return 0
  fi
  if [[ -z "$doc_id" && "$doc_cleanup_needed" != "1" ]]; then
    return 0
  fi
  if [[ -z "$credential_path" ]]; then
    credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
  fi

  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Google Docs OAuth access token for cleanup"
    fi
    return 1
  fi

  if [[ -z "$doc_id" ]]; then
    doc_id="$(find_created_drive_file_by_title "$access_token" 2>/dev/null || true)"
  fi
  if [[ -z "$doc_id" ]]; then
    unset access_token
    if [[ "$mode" == "required" ]]; then
      live_fail "could not find created Google Docs Drive file during cleanup"
    fi
    return 1
  fi

  if curl -fsS -X PATCH "https://www.googleapis.com/drive/v3/files/$doc_id" \
    -H "Authorization: Bearer $access_token" \
    -H "Content-Type: application/json" \
    --data-binary '{"trashed":true}' >/dev/null 2>/dev/null; then
    doc_trashed=1
    doc_id=""
    unset access_token
    return 0
  fi

  unset access_token
  if [[ "$mode" == "required" ]]; then
    live_fail "failed to trash created Google Docs Drive file during cleanup"
  fi
  return 1
}

on_error() {
  local code=$?
  echo "live Google Docs VFS round trip failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  emit_live_debug_diagnostics "Google Docs VFS round trip" || true
  return "$code"
}

cleanup() {
  set +e
  if [[ -n "${credential_path:-}" && -n "${oauth_refresh_marker:-}" ]]; then
    export_refreshed_oauth_credential_if_requested \
      "$credential_path" \
      "google-docs" \
      "$oauth_refresh_marker" \
      "Google Docs live credential" >/dev/null 2>&1 || true
  fi
  if [[ "$doc_trashed" != "1" && ( -n "${doc_id:-}" || "$doc_cleanup_needed" == "1" ) ]]; then
    trash_created_drive_file best_effort >/dev/null 2>&1 || \
      echo "warning: failed to trash created Google Docs Drive file during cleanup" >&2
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_DOCS_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Google Docs VFS temp root: $tmp_root"
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

step="seeding isolated Google Docs OAuth credential"
seed_connector_credential \
  "$loc_bin" \
  "$state_root" \
  "google-docs" \
  "$connection_id" \
  "$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON"
credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
require_oauth_credential_file "$credential_path" "google-docs" "Google Docs live credential"
if [[ -n "${LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT:-}" && "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" != "1" ]]; then
  live_fail "LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT requires LOCALITY_LIVE_FORCE_OAUTH_REFRESH=1"
fi
if [[ "${LOCALITY_LIVE_FORCE_OAUTH_REFRESH:-0}" == "1" ]]; then
  step="forcing Google Docs OAuth credential refresh"
  force_oauth_credential_refresh "$credential_path" "google-docs" "Google Docs live credential"
  oauth_refresh_marker="$(oauth_credential_refresh_marker "$credential_path" "google-docs" "Google Docs live credential")"
fi
unset LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON

step="registering Google Docs Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount google-docs "$mount_root" \
    --workspace-folder "$LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_FOLDER" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report" "Google Docs mount report"

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="pulling Google Docs workspace"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Google Docs initial pull report"
if [[ -n "$oauth_refresh_marker" ]]; then
  step="verifying Google Docs OAuth credential refresh"
  assert_oauth_credential_refreshed \
    "$credential_path" \
    "google-docs" \
    "$oauth_refresh_marker" \
    "Google Docs live credential"
  export_refreshed_oauth_credential_if_requested \
    "$credential_path" \
    "google-docs" \
    "$oauth_refresh_marker" \
    "Google Docs live credential"
fi

unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
page_title="Locality Google Docs Live VFS $unique"
marker="Locality live Google Docs VFS marker $unique"
page_dir="$mount_root/$page_title"
page_path="$page_dir/page.md"

step="creating Google Docs page directory through Linux FUSE"
mkdir "$page_dir"
printf -- '---\ntitle: "%s"\n---\n# %s\n\n%s\n' \
  "$page_title" \
  "$page_title" \
  "$marker" >"$page_path"

step="diffing created Google Docs page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$diff_report" 2>>"$command_log"
assert_json_ok "$diff_report" "Google Docs diff report"
assert_json_field_equals "$diff_report" "action" "confirm_plan" "Google Docs diff report"

step="pushing created Google Docs page"
doc_cleanup_needed=1
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Google Docs push report"
doc_id="$(json_field "$push_report" "changed_remote_ids.0" 2>/dev/null || true)"
if [[ -z "$doc_id" ]]; then
  live_fail "Google Docs push report did not include changed_remote_ids.0"
fi

step="pulling Google Docs workspace after push"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Google Docs pull-after-push report"

step="verifying created Google Docs marker after pull"
wait_for_marker_under_mount "$marker"
page_path="$(find_marker_path_under_mount "$marker" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "created Google Docs page path was not visible under the mount after pull"
fi

edit_marker="Locality live Google Docs VFS edit marker $unique"

step="editing created Google Docs page through Linux FUSE"
printf '\n%s\n' "$edit_marker" >>"$page_path"

step="diffing edited Google Docs page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$edit_diff_report" 2>>"$command_log"
assert_json_ok "$edit_diff_report" "Google Docs edit diff report"
assert_json_field_equals "$edit_diff_report" "action" "confirm_plan" "Google Docs edit diff report"

step="pushing edited Google Docs page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$edit_push_report" 2>>"$command_log"
assert_json_ok "$edit_push_report" "Google Docs edit push report"
edited_doc_id="$(json_field "$edit_push_report" "changed_remote_ids.0" 2>/dev/null || true)"
if [[ -n "$edited_doc_id" && "$edited_doc_id" != "$doc_id" ]]; then
  live_fail "Google Docs edit push changed an unexpected remote id"
fi

step="pulling Google Docs workspace after edit"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_edit_report" 2>>"$command_log"
assert_json_ok "$pull_after_edit_report" "Google Docs pull-after-edit report"

step="verifying edited Google Docs marker after pull"
wait_for_marker_under_mount "$edit_marker"

step="trashing created Google Docs Drive file"
trash_created_drive_file required
doc_cleanup_needed=0

echo "live Google Docs API, CLI, daemon, and Linux FUSE create/edit checks passed"
