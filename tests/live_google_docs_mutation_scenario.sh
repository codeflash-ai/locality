#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_DOCS_SCENARIO:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_DOCS_SCENARIO=1 to run the live Google Docs mutation scenario"
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
mount_id="${LOCALITY_GOOGLE_DOCS_SCENARIO_MOUNT_ID:-google-docs-scenario}"

if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Google Docs scenario mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-docs-scenario.XXXXXX")"
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
metadata_diff_report="$tmp_root/metadata-diff.json"
metadata_push_report="$tmp_root/metadata-push.json"
pull_after_metadata_report="$tmp_root/pull-after-metadata.json"
rename_diff_report="$tmp_root/rename-diff.json"
rename_push_report="$tmp_root/rename-push.json"
pull_after_rename_report="$tmp_root/pull-after-rename.json"
folder_create_report="$tmp_root/folder-create.json"
folder_get_report="$tmp_root/folder-get.json"
move_pull_report="$tmp_root/move-target-pull.json"
move_diff_report="$tmp_root/move-diff.json"
move_push_report="$tmp_root/move-push.json"
pull_after_move_report="$tmp_root/pull-after-move.json"
delete_diff_report="$tmp_root/delete-diff.json"
delete_push_report="$tmp_root/delete-push.json"
pull_after_delete_report="$tmp_root/pull-after-delete.json"
drive_search_report="$tmp_root/drive-search.json"
drive_get_report="$tmp_root/drive-get.json"
credential_path=""
oauth_refresh_marker=""
daemon_pid=""
fuse_pid=""
doc_id=""
move_folder_id=""
doc_trashed=0
folder_trashed=0
doc_cleanup_needed=0
folder_cleanup_needed=0
page_title=""
single_line=""
blank_line_marker=""
metadata_title=""
renamed_title=""
move_folder_title=""
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

assert_report_confirm_plan() {
  local report_path="$1"
  local label="$2"

  assert_json_ok "$report_path" "$label"
  assert_json_field_equals "$report_path" "action" "confirm_plan" "$label"
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

wait_for_path() {
  local path="$1"
  local label="$2"
  local attempts="${LOCALITY_GOOGLE_DOCS_LIVE_MARKER_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -e "$path" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "$label did not appear at $path"
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
  live_fail "Google Docs marker was not visible under the mount: $marker"
}

assert_marker_absent_under_mount() {
  local marker="$1"
  if grep -R -Fq -- "$marker" "$mount_root" 2>/dev/null; then
    live_fail "Google Docs marker was still visible under the mount after delete: $marker"
  fi
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

markdown_replace_body() {
  local path="$1"
  local body="$2"

  python3 - "$path" "$body" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
body = sys.argv[2]
text = path.read_text(encoding="utf-8")
if not text.startswith("---\n"):
    raise SystemExit(f"{path} did not start with frontmatter")
end = text.find("\n---\n", 4)
if end == -1:
    raise SystemExit(f"{path} frontmatter was not terminated")
prefix = text[: end + len("\n---\n")]
path.write_text(prefix + body, encoding="utf-8")
PY
}

markdown_replace_title() {
  local path="$1"
  local title="$2"

  python3 - "$path" "$title" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
title = sys.argv[2]
lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
if not lines or lines[0] != "---\n":
    raise SystemExit(f"{path} did not start with frontmatter")
for index in range(1, len(lines)):
    if lines[index] == "---\n":
        break
else:
    raise SystemExit(f"{path} frontmatter was not terminated")
for index, line in enumerate(lines[: index]):
    if line.startswith("title:"):
        lines[index] = f"title: {json.dumps(title)}\n"
        path.write_text("".join(lines), encoding="utf-8")
        break
else:
    raise SystemExit(f"{path} frontmatter did not contain title")
PY
}

assert_markdown_body_contains_sequence() {
  local path="$1"
  local expected="$2"
  local label="$3"

  python3 - "$path" "$expected" "$label" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected = sys.argv[2]
label = sys.argv[3]
text = path.read_text(encoding="utf-8")
end = text.find("\n---\n", 4)
if not text.startswith("---\n") or end == -1:
    raise SystemExit(f"{label}: {path} did not contain canonical frontmatter")
body = text[end + len("\n---\n") :]
if expected not in body:
    raise SystemExit(f"{label}: expected body sequence was not present in {path}")
PY
}

google_docs_projected_name() {
  local title="$1"

  python3 - "$title" <<'PY'
import sys

title = sys.argv[1]
slug = []
previous_dash = False
for ch in title.lower():
    if ch.isascii() and ch.isalnum():
        slug.append(ch)
        previous_dash = False
    elif not previous_dash and slug:
        slug.append("-")
        previous_dash = True
while slug and slug[-1] == "-":
    slug.pop()
print("".join(slug) or "untitled")
PY
}

drive_file_by_title() {
  local access_token="$1"
  local title="$2"
  local report_path="$3"

  if ! curl -fsS --get "https://www.googleapis.com/drive/v3/files" \
    -H "Authorization: Bearer $access_token" \
    --data-urlencode "q=name = '$title' and trashed = false" \
    --data-urlencode "fields=files(id,name,mimeType,parents,trashed)" \
    --data-urlencode "pageSize=10" \
    >"$report_path" 2>>"$command_log"; then
    return 1
  fi

  python3 - "$report_path" "$title" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
title = sys.argv[2]
data = json.loads(path.read_text(encoding="utf-8"))
for item in data.get("files") or []:
    if isinstance(item, dict) and item.get("name") == title and item.get("id"):
        print(item["id"])
        raise SystemExit(0)
raise SystemExit(1)
PY
}

drive_get_file() {
  local access_token="$1"
  local file_id="$2"
  local report_path="$3"

  curl -fsS --get "https://www.googleapis.com/drive/v3/files/$file_id" \
    -H "Authorization: Bearer $access_token" \
    --data-urlencode "fields=id,name,mimeType,parents,trashed" \
    >"$report_path" 2>>"$command_log"
}

drive_create_folder() {
  local access_token="$1"
  local parent_id="$2"
  local title="$3"
  local report_path="$4"

  python3 - "$parent_id" "$title" <<'PY' | \
    curl -fsS -X POST "https://www.googleapis.com/drive/v3/files?fields=id,name,mimeType,parents,trashed" \
      -H "Authorization: Bearer $access_token" \
      -H "Content-Type: application/json" \
      --data-binary @- >"$report_path" 2>>"$command_log"
import json
import sys

parent_id = sys.argv[1]
title = sys.argv[2]
print(json.dumps({
    "name": title,
    "mimeType": "application/vnd.google-apps.folder",
    "parents": [parent_id],
}))
PY
}

drive_trash_file() {
  local access_token="$1"
  local file_id="$2"
  local report_path="${3:-/dev/null}"

  curl -fsS -X PATCH "https://www.googleapis.com/drive/v3/files/$file_id" \
    -H "Authorization: Bearer $access_token" \
    -H "Content-Type: application/json" \
    --data-binary '{"trashed":true}' >"$report_path" 2>>"$command_log"
}

assert_drive_name() {
  local report_path="$1"
  local expected="$2"
  local label="$3"

  assert_json_field_equals "$report_path" "name" "$expected" "$label"
}

assert_drive_parent() {
  local report_path="$1"
  local expected_parent="$2"
  local label="$3"

  python3 - "$report_path" "$expected_parent" "$label" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_parent = sys.argv[2]
label = sys.argv[3]
data = json.loads(path.read_text(encoding="utf-8"))
parents = data.get("parents") or []
if expected_parent not in parents:
    raise SystemExit(f"{label} expected parent {expected_parent}, got {parents}")
PY
}

assert_drive_trashed() {
  local report_path="$1"
  local label="$2"

  assert_json_field_equals "$report_path" "trashed" "true" "$label"
}

trash_scratch_drive_file() {
  local file_id="$1"
  local kind="$2"
  local mode="${3:-best_effort}"
  local access_token

  if [[ -z "$file_id" ]]; then
    return 0
  fi
  if [[ -z "$credential_path" ]]; then
    credential_path="$(credential_file_path "$state_root" "connection:$connection_id")"
  fi
  access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
  if [[ -z "$access_token" ]]; then
    if [[ "$mode" == "required" ]]; then
      live_fail "could not read Google Docs OAuth access token for $kind cleanup"
    fi
    return 1
  fi
  if drive_trash_file "$access_token" "$file_id"; then
    unset access_token
    return 0
  fi
  unset access_token
  if [[ "$mode" == "required" ]]; then
    live_fail "failed to trash Google Docs scenario $kind during cleanup"
  fi
  return 1
}

on_error() {
  local code=$?
  echo "live Google Docs mutation scenario failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  emit_live_debug_diagnostics "Google Docs mutation scenario" || true
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
    if [[ -z "${doc_id:-}" && -n "${page_title:-}" && -n "${credential_path:-}" ]]; then
      access_token="$(credential_access_token "$credential_path" 2>/dev/null || true)"
      if [[ -n "$access_token" ]]; then
        doc_id="$(drive_file_by_title "$access_token" "$page_title" "$drive_search_report" 2>/dev/null || true)"
      fi
      unset access_token
    fi
    trash_scratch_drive_file "$doc_id" "document" best_effort >/dev/null 2>&1 || \
      echo "warning: failed to trash Google Docs scenario document during cleanup" >&2
  fi
  if [[ "$folder_trashed" != "1" && ( -n "${move_folder_id:-}" || "$folder_cleanup_needed" == "1" ) ]]; then
    trash_scratch_drive_file "$move_folder_id" "folder" best_effort >/dev/null 2>&1 || \
      echo "warning: failed to trash Google Docs scenario folder during cleanup" >&2
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  unset LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_DOCS_SCENARIO_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Google Docs mutation scenario temp root: $tmp_root"
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
assert_json_ok "$mount_report" "Google Docs scenario mount report"
workspace_folder_id="$(json_field "$mount_report" "remote_root_id")"
if [[ -z "$workspace_folder_id" ]]; then
  live_fail "Google Docs scenario mount report did not include remote_root_id"
fi

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
assert_json_ok "$initial_pull_report" "Google Docs scenario initial pull report"
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
page_title="Locality Google Docs Scenario Seed $unique"
single_line="Locality single-line seed $unique"
blank_line_marker="Locality blank-line insertion marker $unique"
metadata_title="Locality Google Docs Scenario Metadata $unique"
renamed_title="Locality Google Docs Scenario Renamed $unique"
move_folder_title="Locality Google Docs Scenario Folder $unique"
page_dir="$mount_root/$page_title"
page_path="$page_dir/page.md"

step="creating single-line Google Docs page through Linux FUSE"
mkdir "$page_dir"
printf -- '---\ntitle: "%s"\n---\n%s\n' \
  "$page_title" \
  "$single_line" >"$page_path"

step="diffing single-line Google Docs page create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$diff_report" 2>>"$command_log"
assert_report_confirm_plan "$diff_report" "Google Docs scenario create diff report"

step="pushing single-line Google Docs page create"
doc_cleanup_needed=1
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Google Docs scenario create push report"
doc_id="$(json_field "$push_report" "changed_remote_ids.0" 2>/dev/null || true)"
if [[ -z "$doc_id" ]]; then
  live_fail "Google Docs scenario create push report did not include changed_remote_ids.0"
fi

step="pulling Google Docs workspace after single-line create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Google Docs scenario pull-after-create report"

step="locating synced single-line Google Docs page"
wait_for_marker_under_mount "$single_line"
page_path="$(find_marker_path_under_mount "$single_line" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "synced single-line Google Docs page path was not visible under the mount"
fi
page_dir="$(dirname "$page_path")"

step="editing existing single-line page with a blank line and another text line"
markdown_replace_body "$page_path" "$single_line

$blank_line_marker
"

step="diffing blank-line insertion into existing Google Docs page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$edit_diff_report" 2>>"$command_log"
assert_report_confirm_plan "$edit_diff_report" "Google Docs scenario blank-line diff report"

step="pushing blank-line insertion into existing Google Docs page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$edit_push_report" 2>>"$command_log"
assert_json_ok "$edit_push_report" "Google Docs scenario blank-line push report"

step="pulling Google Docs workspace after blank-line insertion"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_edit_report" 2>>"$command_log"
assert_json_ok "$pull_after_edit_report" "Google Docs scenario pull-after-blank-line report"

step="verifying blank-line insertion after pull"
wait_for_marker_under_mount "$blank_line_marker"
page_path="$(find_marker_path_under_mount "$blank_line_marker" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "Google Docs blank-line edit marker was not visible under the mount"
fi
assert_markdown_body_contains_sequence "$page_path" "$single_line

$blank_line_marker" "Google Docs scenario blank-line verification"
page_dir="$(dirname "$page_path")"

step="updating Google Docs title metadata through frontmatter"
markdown_replace_title "$page_path" "$metadata_title"

step="diffing Google Docs title metadata update"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$metadata_diff_report" 2>>"$command_log"
assert_report_confirm_plan "$metadata_diff_report" "Google Docs scenario metadata diff report"

step="pushing Google Docs title metadata update"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$metadata_push_report" 2>>"$command_log"
assert_json_ok "$metadata_push_report" "Google Docs scenario metadata push report"

step="verifying Google Docs title metadata update through Drive"
access_token="$(credential_access_token "$credential_path")"
drive_get_file "$access_token" "$doc_id" "$drive_get_report"
assert_drive_name "$drive_get_report" "$metadata_title" "Google Docs scenario metadata Drive file"
unset access_token

step="pulling Google Docs workspace after metadata update"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_metadata_report" 2>>"$command_log"
assert_json_ok "$pull_after_metadata_report" "Google Docs scenario pull-after-metadata report"
wait_for_marker_under_mount "$blank_line_marker"
page_path="$(find_marker_path_under_mount "$blank_line_marker" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "Google Docs page path was not visible after metadata update pull"
fi
page_dir="$(dirname "$page_path")"

step="renaming Google Docs page directory through Linux FUSE"
renamed_dir="$(dirname "$page_dir")/$renamed_title"
mv "$page_dir" "$renamed_dir"
page_dir="$renamed_dir"
page_path="$page_dir/page.md"
wait_for_path "$page_path" "Google Docs scenario renamed page"

step="diffing Google Docs page directory rename"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$rename_diff_report" 2>>"$command_log"
assert_report_confirm_plan "$rename_diff_report" "Google Docs scenario rename diff report"

step="pushing Google Docs page directory rename"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$rename_push_report" 2>>"$command_log"
assert_json_ok "$rename_push_report" "Google Docs scenario rename push report"

step="verifying Google Docs page directory rename through Drive"
access_token="$(credential_access_token "$credential_path")"
drive_get_file "$access_token" "$doc_id" "$drive_get_report"
assert_drive_name "$drive_get_report" "$renamed_title" "Google Docs scenario renamed Drive file"
unset access_token

step="pulling Google Docs workspace after directory rename"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_rename_report" 2>>"$command_log"
assert_json_ok "$pull_after_rename_report" "Google Docs scenario pull-after-rename report"
wait_for_marker_under_mount "$blank_line_marker"
page_path="$(find_marker_path_under_mount "$blank_line_marker" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "Google Docs page path was not visible after directory rename pull"
fi
page_dir="$(dirname "$page_path")"

step="creating scratch Drive folder move target"
access_token="$(credential_access_token "$credential_path")"
drive_create_folder "$access_token" "$workspace_folder_id" "$move_folder_title" "$folder_create_report"
move_folder_id="$(json_field "$folder_create_report" "id")"
if [[ -z "$move_folder_id" ]]; then
  live_fail "Google Docs scenario folder create report did not include id"
fi
assert_json_field_equals "$folder_create_report" "mimeType" "application/vnd.google-apps.folder" "Google Docs scenario folder create report"
folder_cleanup_needed=1
unset access_token

step="pulling Google Docs workspace after scratch folder create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$move_pull_report" 2>>"$command_log"
assert_json_ok "$move_pull_report" "Google Docs scenario pull-after-folder-create report"
move_folder_path="$mount_root/$(google_docs_projected_name "$move_folder_title")"
wait_for_path "$move_folder_path" "Google Docs scenario scratch folder"

step="moving Google Docs page directory under scratch Drive folder through Linux FUSE"
mv "$page_dir" "$move_folder_path"
page_dir="$move_folder_path/$(basename "$page_dir")"
page_path="$page_dir/page.md"
wait_for_path "$page_path" "Google Docs scenario moved page"

step="diffing Google Docs page move into scratch Drive folder"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$page_path" \
  >"$move_diff_report" 2>>"$command_log"
assert_report_confirm_plan "$move_diff_report" "Google Docs scenario move diff report"

step="pushing Google Docs page move into scratch Drive folder"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$page_path" \
  >"$move_push_report" 2>>"$command_log"
assert_json_ok "$move_push_report" "Google Docs scenario move push report"

step="verifying Google Docs page move through Drive"
access_token="$(credential_access_token "$credential_path")"
drive_get_file "$access_token" "$doc_id" "$drive_get_report"
assert_drive_parent "$drive_get_report" "$move_folder_id" "Google Docs scenario moved Drive file"
unset access_token

step="pulling Google Docs workspace after move"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_move_report" 2>>"$command_log"
assert_json_ok "$pull_after_move_report" "Google Docs scenario pull-after-move report"
wait_for_marker_under_mount "$blank_line_marker"
page_path="$(find_marker_path_under_mount "$blank_line_marker" || true)"
if [[ -z "$page_path" || ! -f "$page_path" ]]; then
  live_fail "Google Docs page path was not visible after move pull"
fi
page_dir="$(dirname "$page_path")"
move_folder_path="$(dirname "$page_dir")"

step="deleting Google Docs page directory through Linux FUSE"
rm -r "$page_dir"

step="diffing deleted Google Docs page archive"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$move_folder_path" \
  >"$delete_diff_report" 2>>"$command_log"
assert_report_confirm_plan "$delete_diff_report" "Google Docs scenario delete diff report"

step="pushing deleted Google Docs page archive"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$move_folder_path" \
  >"$delete_push_report" 2>>"$command_log"
assert_json_ok "$delete_push_report" "Google Docs scenario delete push report"

step="verifying deleted Google Docs page is trashed through Drive"
access_token="$(credential_access_token "$credential_path")"
drive_get_file "$access_token" "$doc_id" "$drive_get_report"
assert_drive_trashed "$drive_get_report" "Google Docs scenario deleted Drive file"
doc_trashed=1
doc_cleanup_needed=0

step="trashing scratch Drive folder"
drive_trash_file "$access_token" "$move_folder_id" "$folder_get_report"
folder_trashed=1
folder_cleanup_needed=0
unset access_token

step="pulling Google Docs workspace after delete"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_delete_report" 2>>"$command_log"
assert_json_ok "$pull_after_delete_report" "Google Docs scenario pull-after-delete report"
assert_marker_absent_under_mount "$blank_line_marker"

echo "live Google Docs mutation scenario passed"
