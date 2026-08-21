#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_LINEAR_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_LINEAR_VFS=1 to run the live Linear VFS test"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=tests/live_connector_common.sh
source "$script_dir/live_connector_common.sh"
assert_live_scenario_contract "linear" "linux-fuse" "tests/live_linear_vfs_roundtrip.sh"

step="checking prerequisites"

on_error() {
  local code=$?
  echo "live Linear VFS round trip failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  emit_live_debug_diagnostics "Linear VFS round trip" || true
  return "$code"
}

trap on_error ERR

require_linux_fuse
require_live_scenario_env "linear"
command -v curl >/dev/null 2>&1 || live_fail "curl is not installed"

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="${LOCALITY_LINEAR_LIVE_CONNECTION_ID:-linear-live}"
mount_id="${LOCALITY_LINEAR_LIVE_MOUNT_ID:-linear-live}"

step="validating Linear live configuration"
if [[ ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ || ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  live_fail "live Linear mount or connection id has an invalid shape"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-linear-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/$mount_id"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
command_log="$tmp_root/commands.err.log"
mount_report="$tmp_root/mount.json"
initial_pull_report="$tmp_root/initial-pull.json"
issue_search_report="$tmp_root/issue-search.json"
diff_report="$tmp_root/diff.json"
push_report="$tmp_root/push.json"
pull_after_push_report="$tmp_root/pull-after-push.json"
status_report="$tmp_root/status.json"
restore_diff_report="$tmp_root/restore-diff.json"
restore_push_report="$tmp_root/restore-push.json"
cleanup_diff_report="$tmp_root/cleanup-diff.json"
cleanup_push_report="$tmp_root/cleanup-push.json"
cleanup_pull_report="$tmp_root/cleanup-pull.json"
metadata_diff_report="$tmp_root/metadata-diff.json"
metadata_push_report="$tmp_root/metadata-push.json"
pull_after_metadata_report="$tmp_root/pull-after-metadata.json"
remote_drift_push_report="$tmp_root/remote-drift-push.json"
remote_drift_pull_report="$tmp_root/remote-drift-pull.json"
remote_drift_restore_report="$tmp_root/remote-drift-restore.json"
linear_api_report="$tmp_root/linear-api.json"
linear_api_request="$tmp_root/linear-api-request.json"
original_copy="$tmp_root/original-page.md"
original_body_copy="$tmp_root/original-body.md"
original_remote_body_copy="$tmp_root/original-remote-body.txt"
restored_copy="$tmp_root/restored-page.md"
daemon_pid=""
fuse_pid=""
issue_path=""
original_saved=0
cleanup_restore_needed=0
remote_mutation_attempted=0
linear_api_key=""
original_title=""

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

linear_graphql() {
  local query="$1"
  local variables_file="$2"
  local output="$3"

  python3 - "$query" "$variables_file" >"$linear_api_request" <<'PY'
import json
import pathlib
import sys

query, variables_path = sys.argv[1:]
variables = json.loads(pathlib.Path(variables_path).read_text(encoding="utf-8"))
print(json.dumps({"query": query, "variables": variables}, separators=(",", ":")))
PY
  curl -fsS --connect-timeout 10 --max-time 30 \
    -H "Authorization: $linear_api_key" \
    -H "Content-Type: application/json" \
    --data-binary "@$linear_api_request" \
    https://api.linear.app/graphql >"$output"
  python3 - "$output" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if payload.get("errors"):
    raise SystemExit("Linear GraphQL verification returned errors")
PY
}

fetch_linear_issue() {
  local variables="$tmp_root/linear-fetch-variables.json"
  python3 - "$LOCALITY_LINEAR_LIVE_ISSUE_ID" >"$variables" <<'PY'
import json
import sys
print(json.dumps({"id": sys.argv[1]}))
PY
  linear_graphql \
    'query LiveIssue($id: String!) { issue(id: $id) { id title description updatedAt } }' \
    "$variables" \
    "$linear_api_report"
}

update_linear_issue_from_files() {
  local title_file="$1"
  local body_file="$2"
  local variables="$tmp_root/linear-update-variables.json"
  python3 - "$LOCALITY_LINEAR_LIVE_ISSUE_ID" "$title_file" "$body_file" >"$variables" <<'PY'
import json
import pathlib
import sys

issue_id, title_path, body_path = sys.argv[1:]
payload = {"id": issue_id, "input": {}}
if title_path:
    payload["input"]["title"] = pathlib.Path(title_path).read_text(encoding="utf-8")
if body_path:
    payload["input"]["description"] = pathlib.Path(body_path).read_text(encoding="utf-8")
print(json.dumps(payload))
PY
  linear_graphql \
    'mutation LiveIssueUpdate($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success issue { id updatedAt } } }' \
    "$variables" \
    "$linear_api_report"
}

assert_linear_remote_field() {
  local field="$1"
  local expected_file="$2"
  fetch_linear_issue
  python3 - "$linear_api_report" "$field" "$expected_file" <<'PY'
import json
import pathlib
import sys

report_path, field, expected_path = sys.argv[1:]
payload = json.loads(pathlib.Path(report_path).read_text(encoding="utf-8"))
actual = ((payload.get("data") or {}).get("issue") or {}).get(field)
expected = pathlib.Path(expected_path).read_text(encoding="utf-8")
if (actual or "") != expected:
    raise SystemExit(f"Linear API readback did not match restored {field}")
PY
}

rewrite_linear_title() {
  local path="$1"
  local title="$2"
  python3 - "$path" "$title" <<'PY'
import json
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
replacement = "title: " + json.dumps(sys.argv[2])
updated, count = re.subn(r"^title:.*$", replacement, text, count=1, flags=re.MULTILINE)
if count != 1:
    raise SystemExit("Linear page frontmatter did not contain exactly one title field")
path.write_text(updated, encoding="utf-8")
PY
}

assert_linear_sidecars_read_only() {
  local issue_dir
  local sidecar
  local path
  local before_hash
  local after_hash
  local write_status

  issue_dir="$(dirname "$issue_path")"
  for sidecar in comments.md attachments.md pull-requests.md history.md; do
    path="$issue_dir/$sidecar"
    [[ -e "$path" ]] || continue
    cat "$path" >/dev/null 2>>"$command_log"
    before_hash="$(sha256sum "$path" | awk '{print $1}')"
    write_status=0
    { printf '\nforbidden live sidecar edit\n' >>"$path"; } 2>>"$command_log" || write_status="$?"
    after_hash="$(sha256sum "$path" | awk '{print $1}')"
    if [[ "$write_status" == "0" || "$before_hash" != "$after_hash" ]]; then
      live_fail "Linear generated sidecar $sidecar accepted a filesystem write"
    fi
  done
}

assert_changed_remote_id_matches_issue() {
  local report_path="$1"
  local label="$2"
  local remote_id

  remote_id="$(json_field "$report_path" "changed_remote_ids.0" 2>/dev/null || true)"
  if [[ -z "$remote_id" ]]; then
    live_fail "$label did not include changed_remote_ids.0"
  fi
  if ! python3 - "$remote_id" "$LOCALITY_LINEAR_LIVE_ISSUE_ID" <<'PY'
import sys

def canonical_issue_id(value):
    value = value.strip()
    compact = "".join(
        character.lower()
        for character in value
        if character in "0123456789abcdefABCDEF"
    )
    return compact if len(compact) == 32 else value

raise SystemExit(
    0 if canonical_issue_id(sys.argv[1]) == canonical_issue_id(sys.argv[2]) else 1
)
PY
  then
    live_fail "$label changed_remote_ids.0 did not match the target Linear issue id"
  fi
}

wait_for_projected_mount_root() {
  local attempts="${LOCALITY_LINEAR_LIVE_MOUNT_WAIT_ATTEMPTS:-80}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -d "$mount_root" ]]; then
      return 0
    fi
    sleep 0.25
  done
  live_fail "Linear FUSE mount root did not appear at $mount_root"
}

extract_page_body() {
  local source_path="$1"
  local body_path="$2"

  python3 - "$source_path" "$body_path" <<'PY' 2>>"$command_log"
import pathlib
import sys

source_path = pathlib.Path(sys.argv[1])
body_path = pathlib.Path(sys.argv[2])

def page_body(text, label):
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].strip() != "---":
        raise SystemExit(f"{label} page.md did not start with frontmatter")
    for index, line in enumerate(lines[1:], start=1):
        if line.strip() == "---":
            return "".join(lines[index + 1 :])
    raise SystemExit(f"{label} page.md frontmatter was not terminated")

body_path.write_text(
    page_body(source_path.read_text(encoding="utf-8"), "Linear original"),
    encoding="utf-8",
)
PY
}

restore_original_body_under_current_frontmatter() {
  local target_path="$1"

  python3 - "$target_path" "$original_body_copy" <<'PY' 2>>"$command_log"
import pathlib
import sys

target_path = pathlib.Path(sys.argv[1])
body_path = pathlib.Path(sys.argv[2])

def current_frontmatter(text):
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].strip() != "---":
        raise SystemExit("Linear target page.md did not start with frontmatter")
    for index, line in enumerate(lines[1:], start=1):
        if line.strip() == "---":
            frontmatter = "".join(lines[: index + 1])
            if not frontmatter.endswith(("\n", "\r")):
                frontmatter += "\n"
            return frontmatter
    raise SystemExit("Linear target page.md frontmatter was not terminated")

frontmatter = current_frontmatter(target_path.read_text(encoding="utf-8"))
body = body_path.read_text(encoding="utf-8")
target_path.write_text(frontmatter + body, encoding="utf-8")
PY
}

find_issue_by_frontmatter() {
  local issue_id="$1"
  local match

  if ! LOCALITY_STATE_DIR="$state_root" "$loc_bin" search "$issue_id" \
    --connector linear \
    --limit 2 \
    --json >"$issue_search_report" 2>>"$command_log"; then
    return 1
  fi

  if match="$(python3 "$script_dir/resolve_linear_live_issue.py" \
    "$issue_search_report" \
    "$mount_root" \
    "$mount_id" \
    "$issue_id" 2>>"$command_log")"; then
    [[ -n "$match" ]] || return 1
    printf '%s\n' "$match"
    return 0
  fi

  return 1
}

wait_for_target_issue() {
  local issue_id="$1"
  local attempts="${LOCALITY_LINEAR_LIVE_ISSUE_WAIT_ATTEMPTS:-160}"
  local attempt
  local match

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    match="$(find_issue_by_frontmatter "$issue_id" 2>/dev/null || true)"
    if [[ -n "$match" ]]; then
      printf '%s\n' "$match"
      return 0
    fi
    sleep 0.25
  done

  live_fail "Linear target issue page.md was not found under the mount"
}

wait_for_marker_in_target_issue() {
  local marker="$1"
  local attempts="${LOCALITY_LINEAR_LIVE_MARKER_WAIT_ATTEMPTS:-120}"
  local attempt
  local match

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ -n "$issue_path" && -f "$issue_path" ]] \
      && grep -Fq -- "$marker" "$issue_path" 2>>"$command_log"; then
      return 0
    fi

    match="$(find_issue_by_frontmatter "$LOCALITY_LINEAR_LIVE_ISSUE_ID" 2>/dev/null || true)"
    if [[ -n "$match" ]]; then
      issue_path="$match"
      if grep -Fq -- "$marker" "$issue_path" 2>>"$command_log"; then
        return 0
      fi
    fi
    sleep 0.25
  done

  live_fail "Linear marker was not visible in the target issue page after pull"
}

best_effort_restore_original() {
  local target="$issue_path"
  local action

  if [[ "$cleanup_restore_needed" != "1" \
    || "$original_saved" != "1" \
    || ! -f "$original_copy" \
    || ! -f "$original_body_copy" ]]; then
    return 0
  fi

  if [[ "$remote_mutation_attempted" == "1" ]]; then
    LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
      >"$cleanup_pull_report" 2>>"$command_log" || return 1
    assert_json_ok "$cleanup_pull_report" "Linear cleanup pull report" || return 1
    target="$(find_issue_by_frontmatter "$LOCALITY_LINEAR_LIVE_ISSUE_ID" 2>/dev/null || true)"
    issue_path="$target"
  elif [[ -z "$target" || ! -f "$target" ]]; then
    target="$(find_issue_by_frontmatter "$LOCALITY_LINEAR_LIVE_ISSUE_ID" 2>/dev/null || true)"
  fi
  if [[ -z "$target" || ! -f "$target" ]]; then
    return 1
  fi

  restore_original_body_under_current_frontmatter "$target" || return 1
  LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$target" \
    >"$cleanup_diff_report" 2>>"$command_log" || return 1
  assert_json_ok "$cleanup_diff_report" "Linear cleanup diff report" || return 1
  action="$(json_field "$cleanup_diff_report" "action" 2>/dev/null || true)"
  if [[ "$action" == "noop" ]]; then
    cleanup_restore_needed=0
    return 0
  fi
  [[ "$action" == "confirm_plan" ]] || return 1

  LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$target" \
    >"$cleanup_push_report" 2>>"$command_log" || return 1
  assert_json_ok "$cleanup_push_report" "Linear cleanup push report" || return 1
  assert_changed_remote_id_matches_issue "$cleanup_push_report" "Linear cleanup push report" || return 1
  cleanup_restore_needed=0
  return 0
}

cleanup() {
  set +e
  if [[ "${cleanup_restore_needed:-0}" == "1" ]]; then
    if [[ -n "${linear_api_key:-}" \
      && -n "${original_title:-}" \
      && -f "${original_remote_body_copy:-}" ]]; then
      printf '%s' "$original_title" >"$tmp_root/cleanup-original-title.txt"
      if update_linear_issue_from_files \
        "$tmp_root/cleanup-original-title.txt" \
        "$original_remote_body_copy" >/dev/null 2>&1; then
        cleanup_restore_needed=0
      fi
    fi
    if [[ "$cleanup_restore_needed" == "1" ]]; then
      best_effort_restore_original >/dev/null 2>&1 || \
        echo "warning: failed to restore Linear issue during cleanup" >&2
    fi
  fi
  stop_live_processes "$locality_root" "$fuse_pid" "$daemon_pid"
  linear_api_key=""
  unset LINEAR_API_KEY
  if [[ "${LOCALITY_LINEAR_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept live Linear VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}

trap cleanup EXIT

step="creating isolated state"
mkdir -p "$state_root" "$locality_root" "$mount_root"

step="building live-test binaries"
build_live_binaries "$loc_bin" "$localityd_bin" "$fuse_bin"

step="seeding isolated Linear API key credential"
linear_api_key="$LINEAR_API_KEY"
seed_connector_credential \
  "$loc_bin" \
  "$state_root" \
  "linear" \
  "$connection_id" \
  "$LINEAR_API_KEY"
unset LINEAR_API_KEY

step="registering Linear Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount linear "$mount_root" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report" "Linear mount report"

step="starting localityd"
daemon_pid="$(start_live_daemon "$localityd_bin" "$state_root" "$daemon_log")"
wait_for_daemon "$loc_bin" "$state_root"

step="starting locality-fuse"
fuse_pid="$(start_live_fuse "$fuse_bin" "$state_root" "$locality_root" "$fuse_log")"
wait_for_fuse "$locality_root" "$fuse_pid"
wait_for_projected_mount_root

step="pulling Linear workspace"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$initial_pull_report" 2>>"$command_log"
assert_json_ok "$initial_pull_report" "Linear initial pull report"

step="finding configured Linear issue page"
issue_path="$(wait_for_target_issue "$LOCALITY_LINEAR_LIVE_ISSUE_ID")"

step="saving original Linear issue page"
cp "$issue_path" "$original_copy"
extract_page_body "$original_copy" "$original_body_copy"
fetch_linear_issue
python3 - "$linear_api_report" "$original_remote_body_copy" >"$tmp_root/original-title.txt" <<'PY'
import json
import pathlib
import sys

report_path, body_path = sys.argv[1:]
payload = json.loads(pathlib.Path(report_path).read_text(encoding="utf-8"))
issue = (payload.get("data") or {}).get("issue") or {}
title = issue.get("title")
if not isinstance(title, str) or not title:
    raise SystemExit("Linear live fixture omitted its title")
pathlib.Path(body_path).write_text(issue.get("description") or "", encoding="utf-8")
print(title, end="")
PY
original_title="$(cat "$tmp_root/original-title.txt")"
original_saved=1
cleanup_restore_needed=1

step="verifying generated Linear sidecars reject Linux FUSE writes"
ls "$(dirname "$issue_path")" >/dev/null 2>>"$command_log"
assert_linear_sidecars_read_only

unique="$(date -u +%Y%m%dT%H%M%SZ)-$$"
marker="Locality live Linear VFS marker $unique"

step="appending Linear issue marker through Linux FUSE"
printf '\n%s\n' "$marker" >>"$issue_path"

step="diffing edited Linear issue page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$issue_path" \
  >"$diff_report" 2>>"$command_log"
assert_json_ok "$diff_report" "Linear diff report"
assert_json_field_equals "$diff_report" "action" "confirm_plan" "Linear diff report"

step="pushing edited Linear issue page"
remote_mutation_attempted=1
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$issue_path" \
  >"$push_report" 2>>"$command_log"
assert_json_ok "$push_report" "Linear push report"
assert_changed_remote_id_matches_issue "$push_report" "Linear push report"

step="pulling Linear workspace after push"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_push_report" 2>>"$command_log"
assert_json_ok "$pull_after_push_report" "Linear pull-after-push report"

step="verifying Linear issue marker after pull"
wait_for_marker_in_target_issue "$marker"

step="restoring original Linear issue page"
restore_original_body_under_current_frontmatter "$issue_path"
cp "$issue_path" "$restored_copy"

step="diffing restored Linear issue page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$issue_path" \
  >"$restore_diff_report" 2>>"$command_log"
assert_json_ok "$restore_diff_report" "Linear restore diff report"
assert_json_field_equals "$restore_diff_report" "action" "confirm_plan" "Linear restore diff report"

step="pushing restored Linear issue page"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$issue_path" \
  >"$restore_push_report" 2>>"$command_log"
assert_json_ok "$restore_push_report" "Linear restore push report"
assert_changed_remote_id_matches_issue "$restore_push_report" "Linear restore push report"

step="editing supported Linear title metadata"
metadata_title="${original_title} [Locality live metadata $unique]"
printf '%s' "$metadata_title" >"$tmp_root/metadata-title.txt"
rewrite_linear_title "$issue_path" "$metadata_title"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff --json "$issue_path" \
  >"$metadata_diff_report" 2>>"$command_log"
assert_json_ok "$metadata_diff_report" "Linear metadata diff report"
assert_json_field_equals "$metadata_diff_report" "action" "confirm_plan" "Linear metadata diff report"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$issue_path" \
  >"$metadata_push_report" 2>>"$command_log"
assert_json_ok "$metadata_push_report" "Linear metadata push report"
assert_changed_remote_id_matches_issue "$metadata_push_report" "Linear metadata push report"
assert_linear_remote_field "title" "$tmp_root/metadata-title.txt"

step="pulling renamed Linear issue after metadata edit"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_metadata_report" 2>>"$command_log"
assert_json_ok "$pull_after_metadata_report" "Linear pull-after-metadata report"
issue_path="$(wait_for_target_issue "$LOCALITY_LINEAR_LIVE_ISSUE_ID")"

step="restoring original Linear title metadata"
printf '%s' "$original_title" >"$tmp_root/original-title.txt"
rewrite_linear_title "$issue_path" "$original_title"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$issue_path" \
  >"$metadata_push_report" 2>>"$command_log"
assert_json_ok "$metadata_push_report" "Linear metadata restore push report"
assert_linear_remote_field "title" "$tmp_root/original-title.txt"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$pull_after_metadata_report" 2>>"$command_log"
assert_json_ok "$pull_after_metadata_report" "Linear pull-after-metadata-restore report"
issue_path="$(wait_for_target_issue "$LOCALITY_LINEAR_LIVE_ISSUE_ID")"

step="verifying Linear remote drift blocks a pending local push"
local_drift_marker="Locality local drift marker $unique"
remote_drift_marker="Locality remote drift marker $unique"
printf '\n%s\n' "$local_drift_marker" >>"$issue_path"
cp "$original_remote_body_copy" "$tmp_root/remote-drift-body.txt"
printf '\n%s\n' "$remote_drift_marker" >>"$tmp_root/remote-drift-body.txt"
update_linear_issue_from_files "" "$tmp_root/remote-drift-body.txt"
if LOCALITY_STATE_DIR="$state_root" "$loc_bin" push --json -y "$issue_path" \
  >"$remote_drift_push_report" 2>>"$command_log"; then
  live_fail "Linear push unexpectedly overwrote remote drift"
fi
python3 - "$remote_drift_push_report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if report.get("ok") is True:
    raise SystemExit("Linear drift push unexpectedly reported ok=true")
text = json.dumps(report).lower()
if not any(word in text for word in ("drift", "conflict", "changed", "concurrency")):
    raise SystemExit("Linear drift push did not report a concurrency-safe failure")
PY
assert_linear_remote_field "description" "$tmp_root/remote-drift-body.txt"

step="restoring Linear remote drift fixture"
update_linear_issue_from_files "" "$original_remote_body_copy"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$remote_drift_pull_report" 2>>"$command_log"
assert_json_ok "$remote_drift_pull_report" "Linear remote-drift restore pull report"
issue_path="$(wait_for_target_issue "$LOCALITY_LINEAR_LIVE_ISSUE_ID")"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" restore --force "$issue_path" --json \
  >"$remote_drift_restore_report" 2>>"$command_log"
assert_json_ok "$remote_drift_restore_report" "Linear remote-drift local restore report"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull --json "$mount_root" \
  >"$remote_drift_pull_report" 2>>"$command_log"
assert_json_ok "$remote_drift_pull_report" "Linear final reconciliation pull report"
issue_path="$(wait_for_target_issue "$LOCALITY_LINEAR_LIVE_ISSUE_ID")"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status --json "$issue_path" \
  >"$status_report" 2>>"$command_log"
assert_json_ok "$status_report" "Linear final status report"
assert_json_field_equals "$status_report" "clean" "true" "Linear final status report"
assert_linear_remote_field "description" "$original_remote_body_copy"
cleanup_restore_needed=0

echo "live Linear API, CLI, daemon, and Linux FUSE body, metadata, sidecar, drift, and restoration checks passed"
