#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_MACOS_FILE_PROVIDER_LIVE:-}" != "1" ]]; then
  echo "skip: set LOCALITY_MACOS_FILE_PROVIDER_LIVE=1 to run the live macOS File Provider test"
  exit 0
fi

fail() {
  printf 'live macOS File Provider test: %s\n' "$*" >&2
  exit 1
}

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
[[ "${LOCALITY_MACOS_FILE_PROVIDER_DEDICATED_HOST:-}" == "1" ]] \
  || fail "refusing to run outside a dedicated macOS test user; set LOCALITY_MACOS_FILE_PROVIDER_DEDICATED_HOST=1"

for command in codesign curl pluginkit python3 /usr/libexec/PlistBuddy; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done

app_path="${LOCALITY_MACOS_FILE_PROVIDER_APP:-}"
expected_bundle_id="${LOCALITY_MACOS_FILE_PROVIDER_EXPECTED_BUNDLE_ID:-}"
notion_token="${NOTION_TOKEN:-${NOTION_AT:-}}"
parent_page_id="${LOCALITY_NOTION_LIVE_PARENT_PAGE:-}"

[[ -n "$app_path" ]] || fail "LOCALITY_MACOS_FILE_PROVIDER_APP is required"
[[ -n "$expected_bundle_id" ]] || fail "LOCALITY_MACOS_FILE_PROVIDER_EXPECTED_BUNDLE_ID is required"
[[ -n "$notion_token" ]] || fail "NOTION_TOKEN or NOTION_AT is required"
[[ -n "$parent_page_id" ]] || fail "LOCALITY_NOTION_LIVE_PARENT_PAGE is required"
[[ -d "$app_path" && "$app_path" == *.app ]] || fail "test app does not exist: $app_path"
case "$expected_bundle_id" in
  *test*|*promptfresh*) ;;
  *) fail "expected bundle id must identify a test app, not a production app: $expected_bundle_id" ;;
esac

loc_bin="$app_path/Contents/MacOS/loc"
localityd_bin="$app_path/Contents/MacOS/localityd"
helper_bin="$app_path/Contents/MacOS/locality-file-providerctl"
appex="$app_path/Contents/PlugIns/LocalityFileProvider.appex"
appex_plist="$appex/Contents/Info.plist"

for binary in "$loc_bin" "$localityd_bin" "$helper_bin"; do
  [[ -x "$binary" ]] || fail "missing executable in test app: $binary"
done
[[ -f "$appex_plist" ]] || fail "missing File Provider extension: $appex"

actual_bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist")"
actual_extension_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$appex_plist")"
[[ "$actual_bundle_id" == "$expected_bundle_id" ]] \
  || fail "test app bundle id was '$actual_bundle_id', expected '$expected_bundle_id'"
[[ "$actual_extension_id" == "$expected_bundle_id.FileProvider" ]] \
  || fail "File Provider extension id was '$actual_extension_id', expected '$expected_bundle_id.FileProvider'"

codesign --verify --deep --strict "$app_path" >/dev/null 2>&1 \
  || fail "test app signature verification failed: $app_path"
pluginkit -m -i "$actual_extension_id" | grep -Fq "$actual_extension_id" \
  || fail "File Provider extension is not registered with pluginkit: $actual_extension_id"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-macos-file-provider-live.XXXXXX")"
state_root="$tmp_root/state"
retired_state_root="$tmp_root/retired-state"
domain_report="$tmp_root/domain.json"
pull_report="$tmp_root/pull.json"
status_report="$tmp_root/status.json"
push_report="$tmp_root/push.json"
child_push_report="$tmp_root/child-push.json"
rename_push_report="$tmp_root/rename-push.json"
delete_push_report="$tmp_root/delete-push.json"
notion_response="$tmp_root/notion-response.json"
tcp_addr="127.0.0.1:38567"
connection_id="macos-file-provider-live"
unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
mount_id="fp-e2e-$unique"
mount_name="notion-fp-e2e-$unique"
scratch_title="Locality macOS File Provider scratch $unique"
child_title="Locality macOS File Provider child $unique"
renamed_child_title="Locality macOS File Provider renamed $unique"
scratch_page_id=""
created_child_page_id=""
domain_url=""
mount_root=""
page_dir=""
page_file=""
daemon_started=0
mount_registered=0
step="preflight"

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

notion_api() {
  local method="$1"
  local path="$2"
  local body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -fsS --retry 2 --connect-timeout 10 --max-time 30 \
      -X "$method" "https://api.notion.com/v1/$path" \
      -H "Authorization: Bearer $notion_token" \
      -H "Notion-Version: ${LOCALITY_NOTION_VERSION:-2026-03-11}" \
      -H "Content-Type: application/json" \
      --data-binary "@$body"
  else
    curl -fsS --retry 2 --connect-timeout 10 --max-time 30 \
      -X "$method" "https://api.notion.com/v1/$path" \
      -H "Authorization: Bearer $notion_token" \
      -H "Notion-Version: ${LOCALITY_NOTION_VERSION:-2026-03-11}"
  fi
}

run_with_timeout() {
  local seconds="$1"
  shift
  python3 - "$seconds" "$@" <<'PY'
import subprocess
import sys

seconds = float(sys.argv[1])
command = sys.argv[2:]
try:
    result = subprocess.run(command, timeout=seconds)
except subprocess.TimeoutExpired:
    print(f"timed out after {seconds:g}s: {' '.join(command)}", file=sys.stderr)
    raise SystemExit(124)
raise SystemExit(result.returncode)
PY
}

run_loc() {
  env -u NOTION_TOKEN -u NOTION_AT \
    LOCALITY_STATE_DIR="$state_root" \
    LOCALITY_DAEMON_TCP_ADDR="$tcp_addr" \
    LOCALITY_FILE_PROVIDERCTL="$helper_bin" \
    "$loc_bin" "$@"
}

wait_for_command() {
  local description="$1"
  shift
  local attempts="${LOCALITY_MACOS_FILE_PROVIDER_WAIT_ATTEMPTS:-120}"
  local index
  for ((index = 1; index <= attempts; index++)); do
    if run_with_timeout 10 "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  printf 'timed out waiting for %s\n' "$description" >&2
  return 1
}

wait_for_status() {
  local path="$1"
  local needle="$2"
  local attempts="${LOCALITY_MACOS_FILE_PROVIDER_WAIT_ATTEMPTS:-120}"
  local index
  for ((index = 1; index <= attempts; index++)); do
    if run_with_timeout 20 env -u NOTION_TOKEN -u NOTION_AT \
      LOCALITY_STATE_DIR="$state_root" \
      LOCALITY_DAEMON_TCP_ADDR="$tcp_addr" \
      LOCALITY_FILE_PROVIDERCTL="$helper_bin" \
      "$loc_bin" status "$path" --json >"$status_report" 2>/dev/null \
      && grep -Fq "$needle" "$status_report"; then
      return 0
    fi
    sleep 0.5
  done
  printf 'timed out waiting for status of %s to contain %s\n' "$path" "$needle" >&2
  [[ ! -s "$status_report" ]] || cat "$status_report" >&2
  return 1
}

start_daemon() {
  run_loc daemon start --session --state-dir "$state_root" --tcp-addr "$tcp_addr" \
    --localityd-bin "$localityd_bin" --json >/dev/null
  daemon_started=1
  local attempts="${LOCALITY_MACOS_FILE_PROVIDER_WAIT_ATTEMPTS:-120}"
  local index
  for ((index = 1; index <= attempts; index++)); do
    if run_with_timeout 10 env -u NOTION_TOKEN -u NOTION_AT \
      LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR="$tcp_addr" \
      "$loc_bin" daemon status --state-dir "$state_root" --tcp-addr "$tcp_addr" --json \
      2>/dev/null | grep -Fq '"state": "running"'; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

stop_daemon() {
  if [[ "$daemon_started" == "1" ]]; then
    run_with_timeout 20 env -u NOTION_TOKEN -u NOTION_AT \
      LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR="$tcp_addr" \
      "$loc_bin" daemon stop --state-dir "$state_root" --tcp-addr "$tcp_addr" --json \
      >/dev/null 2>&1 || true
    daemon_started=0
  fi
}

domain_url_from_report() {
  python3 - "$domain_report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
domains = [domain for domain in report.get("domains", []) if domain.get("identifier") == "loc"]
if len(domains) != 1:
    raise SystemExit(f"expected exactly one loc domain, found {len(domains)}")
domain = domains[0]
if not domain.get("userEnabled"):
    raise SystemExit("the loc File Provider domain is registered but not user-enabled")
if domain.get("disconnected"):
    raise SystemExit("the loc File Provider domain is disconnected")
url = domain.get("url")
if not url:
    raise SystemExit("the loc File Provider domain has no user-visible URL")
print(url)
PY
}

assert_cloud_storage_url() {
  python3 - "$1" <<'PY'
import os
import pathlib
import pwd
import sys

path = pathlib.Path(sys.argv[1])
home = pathlib.Path(pwd.getpwuid(os.getuid()).pw_dir)
expected = home / "Library" / "CloudStorage"
try:
    path.relative_to(expected)
except ValueError:
    raise SystemExit(f"File Provider URL {path} is outside {expected}")
PY
}

create_scratch_page() {
  local body="$tmp_root/create-scratch.json"
  python3 - "$parent_page_id" "$scratch_title" >"$body" <<'PY'
import json
import sys

parent_id, title = sys.argv[1:]
print(json.dumps({
    "parent": {"type": "page_id", "page_id": parent_id},
    "properties": {"title": {"title": [{"type": "text", "text": {"content": title}}]}},
    "children": [{
        "object": "block",
        "type": "paragraph",
        "paragraph": {"rich_text": [{
            "type": "text",
            "text": {"content": "Initial paragraph for the live macOS File Provider e2e."},
        }]},
    }],
}))
PY
  notion_api POST pages "$body" >"$notion_response"
  python3 - "$notion_response" <<'PY'
import json
import pathlib
import sys

response = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
page_id = response.get("id")
if not page_id:
    raise SystemExit(f"Notion create response did not include an id: {response}")
print(page_id)
PY
}

remote_page_text() {
  notion_api GET "blocks/$1/children?page_size=100" >"$notion_response"
  python3 - "$notion_response" <<'PY'
import json
import pathlib
import sys

value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))

def strings(item):
    if isinstance(item, dict):
        for key, child in item.items():
            if key in {"plain_text", "content"} and isinstance(child, str):
                yield child
            else:
                yield from strings(child)
    elif isinstance(item, list):
        for child in item:
            yield from strings(child)

print("\n".join(strings(value)))
PY
}

remote_page_title() {
  notion_api GET "pages/$1" >"$notion_response"
  python3 - "$notion_response" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
for prop in (page.get("properties") or {}).values():
    if prop.get("type") == "title":
        print("".join(item.get("plain_text", "") for item in prop.get("title") or []))
        break
PY
}

archive_page() {
  local page_id="$1"
  [[ -n "$page_id" ]] || return 0
  local body="$tmp_root/archive-${page_id//-/}.json"
  printf '{"archived":true}\n' >"$body"
  notion_api PATCH "pages/$page_id" "$body" >/dev/null 2>&1 || true
}

created_remote_id_from_push() {
  local report="$1"
  local excluded_id="$2"
  python3 - "$report" "$excluded_id" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
excluded = sys.argv[2].replace("-", "").lower()
for remote_id in report.get("changed_remote_ids") or []:
    if remote_id.replace("-", "").lower() != excluded:
        print(remote_id)
        break
else:
    raise SystemExit("push report did not include a created remote id")
PY
}

archive_status_is_true() {
  notion_api GET "pages/$1" >"$notion_response"
  python3 - "$notion_response" <<'PY'
import json
import pathlib
import sys

page = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
raise SystemExit(0 if page.get("archived") or page.get("in_trash") else 1)
PY
}

emit_diagnostics() {
  printf 'live macOS File Provider test failed during: %s\n' "$step" >&2
  "$helper_bin" list --json >&2 || true
  [[ ! -s "$status_report" ]] || { echo "last status report:" >&2; cat "$status_report" >&2; }
  local log
  while IFS= read -r log; do
    echo "log tail: $log" >&2
    LOCALITY_E2E_REDACT_TOKEN="$notion_token" python3 - "$log" <<'PY' >&2
import os
import pathlib
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
token = os.environ.get("LOCALITY_E2E_REDACT_TOKEN", "")
if token:
    text = text.replace(token, "[REDACTED]")
print("\n".join(text.splitlines()[-160:]))
PY
  done < <(find "$state_root/logs" -type f -name '*.log' -print 2>/dev/null | sort)
}

on_error() {
  local status="$?"
  (set +e; emit_diagnostics || true)
  return "$status"
}

remove_visible_test_mount() {
  [[ "$mount_registered" == "1" && -n "$mount_root" ]] || return 0
  stop_daemon
  if [[ -d "$state_root" ]]; then
    mv "$state_root" "$retired_state_root"
  fi
  mkdir -p "$state_root"
  if start_daemon; then
    "$helper_bin" reimport --mount-id loc --identifier root --json >/dev/null 2>&1 || true
    "$helper_bin" signal --mount-id loc --identifier working-set --json >/dev/null 2>&1 || true
    local index
    for ((index = 1; index <= 60; index++)); do
      [[ ! -e "$mount_root" ]] && break
      /bin/ls "$domain_url" >/dev/null 2>&1 || true
      sleep 0.5
    done
    [[ ! -e "$mount_root" ]] \
      || printf 'warning: test mount is still visible after cleanup: %s\n' "$mount_root" >&2
  fi
  stop_daemon
}

cleanup() {
  trap - ERR
  set +e
  archive_page "$created_child_page_id"
  archive_page "$scratch_page_id"
  remove_visible_test_mount
  if [[ "${LOCALITY_MACOS_FILE_PROVIDER_KEEP_TMP:-}" == "1" ]]; then
    printf 'kept live macOS File Provider temp root (contains credentials): %s\n' "$tmp_root" >&2
  else
    rm -rf "$tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

step="checking the dedicated File Provider domain"
"$helper_bin" list --json >"$domain_report"
domain_url="$(domain_url_from_report)"
assert_cloud_storage_url "$domain_url"
[[ -d "$domain_url" ]] || fail "File Provider URL has not materialized: $domain_url"
mount_root="$domain_url/$mount_name"
[[ ! -e "$mount_root" ]] || fail "test mount path already exists: $mount_root"

step="checking the daemon TCP port"
if python3 - "$tcp_addr" <<'PY'
import socket
import sys

host, port = sys.argv[1].rsplit(":", 1)
with socket.socket() as sock:
    sock.settimeout(0.5)
    raise SystemExit(0 if sock.connect_ex((host, int(port))) == 0 else 1)
PY
then
  fail "$tcp_addr is already in use; close Locality before running on the dedicated test host"
fi

parent_page_id="$(normalize_notion_page_id "$parent_page_id")"
step="creating a scratch Notion page"
scratch_page_id="$(create_scratch_page)"

step="creating isolated Locality state"
mkdir -p "$state_root"
printf '%s' "$notion_token" | env -u NOTION_TOKEN -u NOTION_AT \
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" connect notion --name "$connection_id" --token-stdin --json >/dev/null

step="registering the isolated macOS File Provider mount"
env -u NOTION_TOKEN -u NOTION_AT \
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount notion "$mount_root" \
    --root-page "$scratch_page_id" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection macos-file-provider \
    --json >/dev/null
mount_registered=1

step="starting the isolated packaged daemon"
start_daemon || fail "packaged localityd did not become ready"

step="discovering the scratch page"
run_loc pull "$mount_root" --json >"$pull_report"
"$helper_bin" reimport --mount-id loc --identifier root --json >/dev/null
"$helper_bin" signal --mount-id loc --identifier working-set --json >/dev/null
wait_for_command "File Provider mount point" /bin/ls "$mount_root"
page_dir="$mount_root/$scratch_title"
wait_for_command "scratch page directory" /bin/ls "$page_dir"
page_file="$page_dir/page.md"
wait_for_command "scratch page.md placeholder" /usr/bin/stat "$page_file"
cache_file="$state_root/content/$mount_id/files/$scratch_title/page.md"
[[ ! -e "$cache_file" ]] \
  || fail "File Provider enumeration unexpectedly hydrated page.md before it was opened"

step="hydrating page.md through File Provider"
hydrated_file="$tmp_root/hydrated-page.md"
run_with_timeout 60 /bin/cat "$page_file" >"$hydrated_file"
grep -Fq "Initial paragraph for the live macOS File Provider e2e." "$hydrated_file" \
  || fail "hydrated page.md did not contain the expected Notion paragraph"
grep -Fq "Initial paragraph for the live macOS File Provider e2e." "$cache_file" \
  || fail "opening the CloudStorage placeholder did not hydrate the daemon content cache"

step="committing an atomic editor-style page.md replacement"
edit_marker="macOS File Provider atomic edit $unique"
edited_file="$tmp_root/edited-page.md"
cp "$hydrated_file" "$edited_file"
printf '\n%s\n' "$edit_marker" >>"$edited_file"
atomic_temp="$page_dir/page.md.tmp.$unique"
run_with_timeout 30 /bin/cp "$edited_file" "$atomic_temp"
run_with_timeout 30 /bin/mv -f "$atomic_temp" "$page_file"
wait_for_status "$page_file" 'local_body_changed'

step="pushing the File Provider edit"
run_loc push "$page_file" -y --json >"$push_report"
wait_for_status "$page_file" '"state": "clean"'
remote_text="$(remote_page_text "$scratch_page_id")"
grep -Fq "$edit_marker" <<<"$remote_text" \
  || fail "remote Notion page did not contain the File Provider edit marker"

step="creating a child page through File Provider"
child_dir="$page_dir/$child_title"
child_page="$child_dir/page.md"
child_marker="macOS File Provider created child $unique"
run_with_timeout 30 /bin/mkdir "$child_dir"
child_source="$tmp_root/child-page.md"
printf -- '---\ntitle: "%s"\n---\n# Created child\n\n%s\n' "$child_title" "$child_marker" >"$child_source"
run_with_timeout 30 /bin/cp "$child_source" "$child_page"
wait_for_status "$child_page" 'pending_virtual_create'
run_loc push "$child_page" -y --json >"$child_push_report"
created_child_page_id="$(created_remote_id_from_push "$child_push_report" "$scratch_page_id")"
grep -Fq "$child_marker" <<<"$(remote_page_text "$created_child_page_id")" \
  || fail "created child Notion page did not contain the expected marker"

step="renaming the child page through File Provider"
renamed_child_dir="$page_dir/$renamed_child_title"
renamed_child_page="$renamed_child_dir/page.md"
wait_for_command "reconciled child page" /usr/bin/stat "$child_page"
run_with_timeout 30 /bin/mv "$child_dir" "$renamed_child_dir"
wait_for_status "$renamed_child_page" 'pending_virtual_rename'
run_loc push "$renamed_child_page" -y --json >"$rename_push_report"
[[ "$(remote_page_title "$created_child_page_id")" == "$renamed_child_title" ]] \
  || fail "remote child title did not reflect the File Provider rename"

step="deleting the child page through File Provider"
run_with_timeout 30 /bin/rm -r "$renamed_child_dir"
wait_for_status "$page_dir" 'pending_virtual_delete'
run_loc push "$page_dir" -y --json >"$delete_push_report"
archive_status_is_true "$created_child_page_id" \
  || fail "remote child page was not archived after File Provider deletion"
created_child_page_id=""

step="cleaning the scratch page"
archive_page "$scratch_page_id"
scratch_page_id=""

echo "ok: live macOS File Provider enumerate, hydrate, atomic edit, create, rename, and delete passed"
