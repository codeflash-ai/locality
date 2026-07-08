#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_NOTION_VFS_PUSH_PULL:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_NOTION_VFS_PUSH_PULL=1 to run the live Notion VFS push/pull test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: live Notion VFS push/pull test requires Linux"
  exit 0
fi

if [[ ! -e /dev/fuse ]]; then
  echo "skip: /dev/fuse is not available on this runner" >&2
  exit 1
fi

if ! command -v fusermount3 >/dev/null 2>&1; then
  echo "skip: fusermount3 is not installed" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "missing python3 for live Notion credential parsing and API setup" >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "missing curl for live Notion API setup" >&2
  exit 1
fi

notion_token="${NOTION_TOKEN:-${NOTION_AT:-}}"
page_id="${LOCALITY_NOTION_PAGE_ID:-${NOTION_PAGE_ID:-}}"
parent_page_id="${LOCALITY_NOTION_LIVE_PARENT_PAGE:-}"
source_connection_id="${LOCALITY_NOTION_LIVE_CONNECTION_ID:-notion-default}"
notion_version="${LOCALITY_NOTION_VERSION:-2026-03-11}"

if [[ -z "$notion_token" ]]; then
  credential_state_root="${LOCALITY_NOTION_LIVE_CREDENTIAL_STATE_DIR:-$HOME/.loc}"
  secret_ref="connection:$source_connection_id"
  secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
  secret_path="$credential_state_root/credentials/$secret_hex"
  if [[ ! -f "$secret_path" ]]; then
    echo "missing NOTION_TOKEN/NOTION_AT and stored Notion credential $secret_ref at $secret_path" >&2
    exit 1
  fi
  notion_token="$(
    python3 - "$secret_path" <<'PY'
import json
import pathlib
import sys

secret = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
if not secret:
    raise SystemExit("stored Notion credential is empty")
if secret.startswith("{"):
    token = (json.loads(secret).get("access_token") or "").strip()
else:
    token = secret
if not token:
    raise SystemExit("stored Notion credential has an empty access token")
print(token)
PY
  )"
fi

normalize_notion_page_id() {
  python3 - "$1" <<'PY'
import re
import sys

value = sys.argv[1].strip()
matches = re.findall(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}|[0-9a-fA-F]{32}",
    value,
)
if not matches:
    raise SystemExit(f"invalid Notion page id or URL: {value}")
raw = matches[-1].replace("-", "").lower()
print(f"{raw[:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:]}")
PY
}

compact_notion_page_id() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -d '-'
}

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
mount_id="${LOCALITY_LIVE_NOTION_VFS_MOUNT_ID:-notion-main}"
connection_id="${LOCALITY_LIVE_NOTION_VFS_CONNECTION_ID:-live-notion-vfs}"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-notion-vfs.XXXXXX")"
state_root="${LOCALITY_LIVE_NOTION_VFS_STATE:-$tmp_root/state}"
LOCALITY_ROOT="${LOCALITY_LIVE_NOTION_VFS_ROOT:-$tmp_root/Locality}"
NOTION_MOUNT="${LOCALITY_LIVE_NOTION_VFS_MOUNT:-$LOCALITY_ROOT/notion-main}"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
original_file="$tmp_root/original-page.md"
create_page_body="$tmp_root/create-page.json"
create_page_response="$tmp_root/create-page-response.json"
remote_blocks_response="$tmp_root/remote-blocks.json"
remote_page_response="$tmp_root/remote-page.json"
parent_page_response="$tmp_root/parent-page.json"
initial_pull="$tmp_root/initial-pull.json"
file_pull="$tmp_root/file-pull.json"
push_output="$tmp_root/push.json"
push_child_output="$tmp_root/push-child.json"
pull_after_child_create="$tmp_root/pull-after-child-create.json"
push_move_parent_output="$tmp_root/push-move-parent.json"
pull_after_move_parent_create="$tmp_root/pull-after-move-parent-create.json"
push_move_out_output="$tmp_root/push-move-out.json"
push_move_back_output="$tmp_root/push-move-back.json"
push_rename_output="$tmp_root/push-rename.json"
push_delete_output="$tmp_root/push-delete.json"
pull_after_push="$tmp_root/pull-after-push.json"
localityd_pid=""
fuse_pid=""
page_file=""
restore_needed=0
scratch_created=0
scratch_page_id=""
created_child_page_id=""
created_move_parent_page_id=""
step="initializing"

notion_api() {
  local method="$1"
  local path="$2"
  local body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -fsS -X "$method" "https://api.notion.com/v1/$path" \
      -H "Authorization: Bearer $notion_token" \
      -H "Notion-Version: $notion_version" \
      -H "Content-Type: application/json" \
      --data-binary "@$body"
  else
    curl -fsS -X "$method" "https://api.notion.com/v1/$path" \
      -H "Authorization: Bearer $notion_token" \
      -H "Notion-Version: $notion_version"
  fi
}

create_scratch_page() {
  local parent_id="$1"
  local title="Locality live FUSE scratch $(date -u +%Y-%m-%dT%H:%M:%SZ)-$$"
  python3 - "$parent_id" "$title" >"$create_page_body" <<'PY'
import json
import sys

parent_id, title = sys.argv[1], sys.argv[2]
body = {
    "parent": {"type": "page_id", "page_id": parent_id},
    "properties": {
        "title": {
            "title": [
                {"type": "text", "text": {"content": title}},
            ],
        },
    },
    "children": [
        {
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    {
                        "type": "text",
                        "text": {
                            "content": "Original paragraph for live Linux FUSE e2e."
                        },
                    }
                ]
            },
        }
    ],
}
print(json.dumps(body))
PY
  notion_api POST pages "$create_page_body" >"$create_page_response"
  python3 - "$create_page_response" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
page_id = page.get("id")
if not page_id:
    raise SystemExit(f"Notion create page response did not include id: {page}")
print(page_id)
PY
}

validate_parent_page() {
  local parent_id="$1"
  notion_api GET "pages/$parent_id" >"$parent_page_response"
  python3 - "$parent_page_response" "$parent_id" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
parent_id = sys.argv[2]
if page.get("object") == "error":
    message = page.get("message") or page
    raise SystemExit(
        f"LOCALITY_NOTION_LIVE_PARENT_PAGE `{parent_id}` must point to an accessible writable Notion page; retrieve failed: {message}"
    )
if page.get("object") != "page":
    raise SystemExit(
        f"LOCALITY_NOTION_LIVE_PARENT_PAGE `{parent_id}` did not resolve to a Notion page: {page}"
    )
if page.get("archived") or page.get("in_trash"):
    raise SystemExit(
        f"LOCALITY_NOTION_LIVE_PARENT_PAGE `{parent_id}` points to a Notion page that is archived or in trash; choose an active writable parent page"
    )
PY
}

assert_remote_contains_marker() {
  local remote_page_id="$1"
  local marker="$2"
  notion_api GET "blocks/$remote_page_id/children?page_size=100" >"$remote_blocks_response"
  python3 - "$remote_blocks_response" "$marker" <<'PY'
import json
import pathlib
import sys

blocks = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
marker = sys.argv[2]

def strings(value):
    if isinstance(value, dict):
        for key, item in value.items():
            if key in {"plain_text", "content"} and isinstance(item, str):
                yield item
            else:
                yield from strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)

text = "\n".join(strings(blocks))
if marker not in text:
    raise SystemExit(f"remote Notion blocks did not contain marker {marker!r}\n{text}")
PY
}

assert_remote_page_title() {
  local remote_page_id="$1"
  local expected_title="$2"
  notion_api GET "pages/$remote_page_id" >"$remote_page_response"
  python3 - "$remote_page_response" "$expected_title" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
expected = sys.argv[2]
properties = page.get("properties") or {}
titles = []
for prop in properties.values():
    if prop.get("type") != "title":
        continue
    for item in prop.get("title") or []:
        text = item.get("plain_text")
        if text:
            titles.append(text)
title = "".join(titles)
if title != expected:
    raise SystemExit(f"remote Notion page title was {title!r}, expected {expected!r}")
PY
}

assert_remote_page_parent() {
  local remote_page_id="$1"
  local expected_parent_id="$2"
  notion_api GET "pages/$remote_page_id" >"$remote_page_response"
  python3 - "$remote_page_response" "$(compact_notion_page_id "$expected_parent_id")" <<'PY'
import json
import pathlib
import re
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
expected = sys.argv[2]
parent = page.get("parent") or {}
parent_id = parent.get("page_id") or parent.get("database_id") or parent.get("data_source_id")
if not parent_id:
    raise SystemExit(f"remote Notion page did not include a page/database parent: {page}")
compact = re.sub(r"-", "", parent_id).lower()
if compact != expected:
    raise SystemExit(f"remote Notion page parent was {parent_id!r}, expected {expected!r}")
PY
}

assert_notion_page_archived() {
  local remote_page_id="$1"
  notion_api GET "pages/$remote_page_id" >"$remote_page_response"
  python3 - "$remote_page_response" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if not (page.get("archived") or page.get("in_trash")):
    raise SystemExit(f"Notion page was not archived: {page.get('id')}")
PY
}

created_remote_id_from_push() {
  local report_path="$1"
  local parent_id="$2"
  python3 - "$report_path" "$(compact_notion_page_id "$parent_id")" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
parent_id = sys.argv[2]
for remote_id in report.get("changed_remote_ids") or []:
    compact = remote_id.replace("-", "").lower()
    if compact != parent_id:
        print(remote_id)
        break
else:
    raise SystemExit(f"push report did not include a created child remote id: {report}")
PY
}

on_error() {
  echo "live Notion VFS push/pull test failed during: $step" >&2
  if [[ -s "$initial_pull" ]]; then
    echo "initial pull output:" >&2
    cat "$initial_pull" >&2
  fi
  if [[ -s "$file_pull" ]]; then
    echo "file pull output:" >&2
    cat "$file_pull" >&2
  fi
  if [[ -s "$push_output" ]]; then
    echo "push output:" >&2
    cat "$push_output" >&2
  fi
  if [[ -s "$push_child_output" ]]; then
    echo "push-child output:" >&2
    cat "$push_child_output" >&2
  fi
  if [[ -s "$pull_after_child_create" ]]; then
    echo "pull-after-child-create output:" >&2
    cat "$pull_after_child_create" >&2
  fi
  if [[ -s "$push_move_parent_output" ]]; then
    echo "push-move-parent output:" >&2
    cat "$push_move_parent_output" >&2
  fi
  if [[ -s "$pull_after_move_parent_create" ]]; then
    echo "pull-after-move-parent-create output:" >&2
    cat "$pull_after_move_parent_create" >&2
  fi
  if [[ -s "$push_move_out_output" ]]; then
    echo "push-move-out output:" >&2
    cat "$push_move_out_output" >&2
  fi
  if [[ -s "$push_move_back_output" ]]; then
    echo "push-move-back output:" >&2
    cat "$push_move_back_output" >&2
  fi
  if [[ -s "$push_rename_output" ]]; then
    echo "push-rename output:" >&2
    cat "$push_rename_output" >&2
  fi
  if [[ -s "$push_delete_output" ]]; then
    echo "push-delete output:" >&2
    cat "$push_delete_output" >&2
  fi
  if [[ -s "$pull_after_push" ]]; then
    echo "pull-after-push output:" >&2
    cat "$pull_after_push" >&2
  fi
  if [[ -s "$parent_page_response" ]]; then
    echo "parent-page response:" >&2
    cat "$parent_page_response" >&2
  fi
  echo "daemon log:" >&2
  cat "$daemon_log" >&2 || true
  echo "FUSE log:" >&2
  cat "$fuse_log" >&2 || true
}

cleanup() {
  set +e
  if [[ "$restore_needed" == "1" && -n "$page_file" && -f "$original_file" && -e "$page_file" ]]; then
    cp "$original_file" "$page_file"
    env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
      "$loc_bin" push "$page_file" -y --json >/dev/null 2>&1
  fi
  if [[ "$scratch_created" == "1" && -n "$scratch_page_id" ]]; then
    archive_body="$tmp_root/archive-page.json"
    printf '{"archived":true}\n' >"$archive_body"
    notion_api PATCH "pages/$scratch_page_id" "$archive_body" >/dev/null 2>&1
  fi
  if [[ -n "$created_child_page_id" ]]; then
    archive_body="$tmp_root/archive-child-page.json"
    printf '{"archived":true}\n' >"$archive_body"
    notion_api PATCH "pages/$created_child_page_id" "$archive_body" >/dev/null 2>&1
  fi
  if [[ -n "$created_move_parent_page_id" ]]; then
    archive_body="$tmp_root/archive-move-parent-page.json"
    printf '{"archived":true}\n' >"$archive_body"
    notion_api PATCH "pages/$created_move_parent_page_id" "$archive_body" >/dev/null 2>&1
  fi
  if mountpoint -q "$LOCALITY_ROOT"; then
    fusermount3 -uz "$LOCALITY_ROOT" >/dev/null 2>&1
  fi
  if [[ -n "$fuse_pid" ]] && kill -0 "$fuse_pid" >/dev/null 2>&1; then
    kill "$fuse_pid" >/dev/null 2>&1
    wait "$fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "$localityd_pid" ]] && kill -0 "$localityd_pid" >/dev/null 2>&1; then
    kill "$localityd_pid" >/dev/null 2>&1
    wait "$localityd_pid" >/dev/null 2>&1
  fi
  if [[ "${LOCALITY_LIVE_NOTION_VFS_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Notion VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

if [[ -z "$page_id" ]]; then
  if [[ -z "$parent_page_id" ]]; then
    echo "missing LOCALITY_NOTION_PAGE_ID/NOTION_PAGE_ID or LOCALITY_NOTION_LIVE_PARENT_PAGE" >&2
    exit 1
  fi
  parent_page_id="$(normalize_notion_page_id "$parent_page_id")"
  step="validating live Notion parent page"
  validate_parent_page "$parent_page_id"
  step="creating scratch Notion page"
  scratch_page_id="$(create_scratch_page "$parent_page_id")"
  scratch_created=1
  page_id="$scratch_page_id"
else
  page_id="$(normalize_notion_page_id "$page_id")"
fi
normalized_page_id="$(compact_notion_page_id "$page_id")"

if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
  cargo build -p localityd -p loc-cli -p locality-fuse
fi

wait_for_daemon() {
  for _ in {1..80}; do
    if LOCALITY_STATE_DIR="$state_root" "$loc_bin" daemon status --state-dir "$state_root" --json \
      | grep -q '"state": "running"'; then
      return 0
    fi
    sleep 0.25
  done
  echo "localityd did not become ready" >&2
  return 1
}

wait_for_mount() {
  for _ in {1..80}; do
    if mountpoint -q "$LOCALITY_ROOT"; then
      return 0
    fi
    if [[ -n "$fuse_pid" ]] && ! kill -0 "$fuse_pid" >/dev/null 2>&1; then
      echo "locality-fuse exited before mount became ready" >&2
      return 1
    fi
    sleep 0.25
  done
  echo "FUSE mount did not become ready" >&2
  return 1
}

find_pulled_page_file() {
  find_page_file_by_remote_id "$normalized_page_id" "$NOTION_MOUNT"
}

find_page_file_by_remote_id() {
  local remote_id="$1"
  local search_root="$2"
  local normalized
  normalized="$(compact_notion_page_id "$remote_id")"
  local file
  while IFS= read -r file; do
    if tr '[:upper:]' '[:lower:]' < "$file" | tr -d '-' | grep -q "$normalized"; then
      printf '%s\n' "$file"
      return 0
    fi
  done < <(find "$search_root" -name page.md -type f -print)
  return 1
}

wait_for_page_file_by_remote_id() {
  local remote_id="$1"
  local search_root="$2"
  local file
  for _ in {1..80}; do
    if file="$(find_page_file_by_remote_id "$remote_id" "$search_root")"; then
      printf '%s\n' "$file"
      return 0
    fi
    sleep 0.25
  done
  echo "could not locate projected page file for Notion page $remote_id under $search_root" >&2
  return 1
}

assert_no_conflict_markers() {
  local file="$1"
  if grep -Eq '^(<<<<<<<|=======|>>>>>>>)' "$file"; then
    echo "unexpected conflict markers in $file" >&2
    sed -n '1,120p' "$file" >&2
    return 1
  fi
}

assert_status_contains() {
  local path="$1"
  local pattern="$2"
  local output
  output="$(
    env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
      "$loc_bin" status "$path" --json
  )"
  if ! grep -q "$pattern" <<<"$output"; then
    echo "status for $path did not contain $pattern" >&2
    echo "$output" >&2
    return 1
  fi
}

assert_path_absent() {
  local path="$1"
  if [[ -e "$path" ]]; then
    echo "expected path to be absent: $path" >&2
    return 1
  fi
}

step="creating temp directories"
mkdir -p "$state_root" "$LOCALITY_ROOT" "$NOTION_MOUNT"

step="creating isolated Notion connection"
printf '%s' "$notion_token" | env -u NOTION_TOKEN -u NOTION_AT \
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" connect notion --name "$connection_id" --token-stdin --json >/dev/null

step="registering Notion Linux FUSE mount"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount notion "$NOTION_MOUNT" \
    --root-page "$page_id" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >/dev/null

step="starting localityd"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"
wait_for_daemon

step="starting locality-fuse"
LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --state-dir "$state_root" \
  --mountpoint "$LOCALITY_ROOT" >"$fuse_log" 2>&1 &
fuse_pid="$!"
wait_for_mount

step="pulling root page"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" pull "$NOTION_MOUNT" --json >"$initial_pull"
step="locating pulled page file"
page_file="$(find_pulled_page_file)"
step="pulling page file"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" pull "$page_file" --json >"$file_pull"

step="saving original content"
cp "$page_file" "$original_file"
if [[ "$scratch_created" != "1" ]]; then
  restore_needed=1
fi

marker="Locality live VFS push-pull block $(date -u +%Y-%m-%dT%H:%M:%SZ)-$$"
step="editing page file"
edit_file="$tmp_root/edited-page.md"
awk -v marker="$marker" '
  BEGIN { fences = 0; inserted = 0 }
  {
    print
    if ($0 == "---") {
      fences++
      next
    }
    if (fences >= 2 && inserted == 0 && $0 == "") {
      print marker
      print ""
      inserted = 1
    }
  }
  END {
    if (inserted == 0) {
      print ""
      print marker
    }
  }
' "$page_file" > "$edit_file"
cp "$edit_file" "$page_file"

step="pushing edited page file through the direct CLI path"
push_exit=0
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$page_file" -y --json >"$push_output" || push_exit=$?
step="pulling immediately after push"
pull_exit=0
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" pull "$page_file" --json >"$pull_after_push" || pull_exit=$?

step="checking for conflict markers"
assert_no_conflict_markers "$page_file"
grep -q "$marker" "$page_file"
step="verifying pushed content through Notion API"
assert_remote_contains_marker "$page_id" "$marker"
if [[ "$push_exit" -ne 0 ]]; then
  echo "push command failed with exit code $push_exit" >&2
  on_error
  exit "$push_exit"
fi
if [[ "$pull_exit" -ne 0 ]]; then
  echo "pull-after-push command failed with exit code $pull_exit" >&2
  on_error
  exit "$pull_exit"
fi

parent_dir="$(dirname "$page_file")"
unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
child_title="Locality live FUSE child $unique"
child_marker="Linux FUSE created child $unique"
child_dir="$parent_dir/$child_title"
child_page="$child_dir/page.md"

step="creating child page directory through FUSE"
mkdir "$child_dir"
printf -- '---\ntitle: "%s"\n---\n# Created child\n\n%s\n' "$child_title" "$child_marker" >"$child_page"
assert_status_contains "$child_page" '"pending_virtual_create"'

step="pushing created child page"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$child_page" -y --json >"$push_child_output"
created_child_page_id="$(created_remote_id_from_push "$push_child_output" "$page_id")"
assert_remote_page_title "$created_child_page_id" "$child_title"
assert_remote_contains_marker "$created_child_page_id" "$child_marker"

step="pulling parent after child page creation"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" pull "$parent_dir" --json >"$pull_after_child_create"
step="locating reconciled child page file"
child_page="$(wait_for_page_file_by_remote_id "$created_child_page_id" "$parent_dir")"
child_dir="$(dirname "$child_page")"

move_parent_title="Locality live FUSE move target $unique"
move_parent_marker="Linux FUSE move target $unique"
move_parent_dir="$parent_dir/$move_parent_title"
move_parent_page="$move_parent_dir/page.md"
step="creating move target parent page directory through FUSE"
mkdir "$move_parent_dir"
printf -- '---\ntitle: "%s"\n---\n# Move target\n\n%s\n' "$move_parent_title" "$move_parent_marker" >"$move_parent_page"
assert_status_contains "$move_parent_page" '"pending_virtual_create"'

step="pushing move target parent page"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$move_parent_page" -y --json >"$push_move_parent_output"
created_move_parent_page_id="$(created_remote_id_from_push "$push_move_parent_output" "$page_id")"
assert_remote_page_title "$created_move_parent_page_id" "$move_parent_title"

step="pulling parent after move target creation"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" pull "$parent_dir" --json >"$pull_after_move_parent_create"
step="locating reconciled move target and child page files"
move_parent_page="$(wait_for_page_file_by_remote_id "$created_move_parent_page_id" "$parent_dir")"
move_parent_dir="$(dirname "$move_parent_page")"
child_page="$(wait_for_page_file_by_remote_id "$created_child_page_id" "$parent_dir")"
child_dir="$(dirname "$child_page")"

step="moving child page directory under another page through FUSE"
mv "$child_dir" "$move_parent_dir/"
moved_child_dir="$move_parent_dir/$child_title"
moved_child_page="$moved_child_dir/page.md"
assert_status_contains "$moved_child_page" '"pending_virtual_rename"'

step="pushing child page move out"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$moved_child_page" -y --json >"$push_move_out_output"
assert_remote_page_parent "$created_child_page_id" "$created_move_parent_page_id"
assert_path_absent "$child_dir"

step="moving child page directory back to its original parent through FUSE"
mv "$moved_child_dir" "$parent_dir/"
child_dir="$parent_dir/$child_title"
child_page="$child_dir/page.md"
assert_status_contains "$child_page" '"pending_virtual_rename"'

step="pushing child page move back"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$child_page" -y --json >"$push_move_back_output"
assert_remote_page_parent "$created_child_page_id" "$page_id"
assert_path_absent "$moved_child_dir"

renamed_child_title="Locality live FUSE renamed child $unique"
renamed_child_dir="$parent_dir/$renamed_child_title"
renamed_child_page="$renamed_child_dir/page.md"
step="renaming child page directory through FUSE"
mv "$child_dir" "$renamed_child_dir"
assert_status_contains "$renamed_child_page" '"pending_virtual_rename"'

step="pushing renamed child page"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$renamed_child_page" -y --json >"$push_rename_output"
assert_remote_page_title "$created_child_page_id" "$renamed_child_title"

step="deleting child page directory through FUSE"
rm -r "$renamed_child_dir"
assert_status_contains "$parent_dir" '"pending_virtual_delete"'

step="pushing deleted child page archive"
env -u NOTION_TOKEN -u NOTION_AT LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" \
  "$loc_bin" push "$parent_dir" -y --json >"$push_delete_output"
assert_notion_page_archived "$created_child_page_id"

if [[ "$scratch_created" != "1" ]]; then
  step="restoring original content"
  cp "$original_file" "$page_file"
  env -u NOTION_TOKEN -u NOTION_AT LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$page_file" -y --json >/dev/null
  restore_needed=0
fi

echo "live Notion VFS push/pull test passed"
