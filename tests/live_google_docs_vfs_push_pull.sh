#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_DOCS_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 to run the live Google Docs VFS push/pull test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: live Google Docs VFS push/pull test requires Linux"
  exit 0
fi

for command in fusermount3 mountpoint findmnt python3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required live Google Docs VFS dependency: $command" >&2
    exit 1
  fi
done
if [[ ! -e /dev/fuse ]]; then
  echo "/dev/fuse is not available on this runner" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON:-}" ]]; then
  echo "missing LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" >&2
  exit 1
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="google-docs-live"
mount_id="google-docs-main"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-docs-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/google-docs-main"
logs_root="$tmp_root/logs"
workspace_id_file="$tmp_root/google-docs-workspace-id"
daemon_log="$logs_root/localityd.log"
fuse_log="$logs_root/locality-fuse.log"
command_log="$logs_root/commands.err.log"
mount_report="$logs_root/mount.json"
pull_report="$logs_root/pull.json"
create_diff_report="$logs_root/create-diff.json"
create_push_report="$logs_root/create-push.json"
status_after_create_report="$logs_root/status-after-create.json"
delete_diff_report="$logs_root/delete-diff.json"
delete_push_report="$logs_root/delete-push.json"
final_status_report="$logs_root/final-status.json"
localityd_pid=""
fuse_pid=""
workspace_id=""
step="initializing"

on_error() {
  local code=$?
  echo "live Google Docs VFS push/pull test failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  if [[ -n "$workspace_id" ]]; then
    echo "privacy-safe diagnostics: scratch_workspace_id_recorded=1" >&2
  fi
  if [[ -n "$localityd_pid" ]]; then
    if kill -0 "$localityd_pid" >/dev/null 2>&1; then
      echo "privacy-safe diagnostics: daemon=running" >&2
    else
      echo "privacy-safe diagnostics: daemon=stopped" >&2
    fi
  fi
  if [[ -n "$fuse_pid" ]]; then
    if kill -0 "$fuse_pid" >/dev/null 2>&1; then
      echo "privacy-safe diagnostics: fuse=running" >&2
    else
      echo "privacy-safe diagnostics: fuse=stopped" >&2
    fi
  fi
  return "$code"
}

cleanup() {
  set +e
  if mountpoint -q "$locality_root"; then
    fusermount3 -uz "$locality_root" >/dev/null 2>&1
  fi
  if [[ -n "$fuse_pid" ]] && kill -0 "$fuse_pid" >/dev/null 2>&1; then
    kill "$fuse_pid" >/dev/null 2>&1
    wait "$fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "$localityd_pid" ]] && kill -0 "$localityd_pid" >/dev/null 2>&1; then
    kill "$localityd_pid" >/dev/null 2>&1
    wait "$localityd_pid" >/dev/null 2>&1
  fi
  if [[ -z "$workspace_id" && -s "$workspace_id_file" ]]; then
    workspace_id="$(tr -d '[:space:]' <"$workspace_id_file")"
  fi
  if [[ -n "$workspace_id" ]]; then
    LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT="$state_root" \
      LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID="$workspace_id" \
      cargo test -p loc-cli --test live_google_connectors live_google_docs_trash_vfs_workspace \
        -- --ignored --exact >>"$command_log" 2>&1 \
      || echo "warning: failed to trash scratch Google Docs VFS workspace; see kept temp logs if LOCALITY_GOOGLE_LIVE_KEEP_TMP=1" >&2
  fi
  unset LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept private live Google Docs VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

assert_json_ok_action() {
  local report="$1"
  local action="$2"
  python3 - "$report" "$action" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
expected = sys.argv[2]
if report.get("ok") is not True:
    raise SystemExit("report ok was not true")
if report.get("action") != expected:
    raise SystemExit(f"report action was {report.get('action')!r}, expected {expected!r}")
PY
}

assert_json_ok() {
  local report="$1"
  python3 - "$report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if report.get("ok") is not True:
    raise SystemExit("report ok was not true")
PY
}

assert_create_plan() {
  local report="$1"
  python3 - "$report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
summary = ((report.get("plan") or {}).get("summary") or {})
if report.get("ok") is not True or report.get("action") != "confirm_plan":
    raise SystemExit("create diff did not report a confirmable plan")
if summary.get("entities_created") != 1:
    raise SystemExit(f"create diff created count was not 1: {summary}")
PY
}

assert_archive_or_delete_plan() {
  local report="$1"
  python3 - "$report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
summary = ((report.get("plan") or {}).get("summary") or {})
if report.get("ok") is not True:
    raise SystemExit("delete diff was not ok")
if report.get("action") not in {"confirm_plan", "confirm_dangerous_plan"}:
    raise SystemExit("delete diff did not report a confirmable plan")
archived = int(summary.get("entities_archived") or 0)
deleted = int(summary.get("entities_deleted") or 0)
if archived + deleted < 1:
    raise SystemExit(f"delete diff did not archive or delete an entity: {summary}")
PY
}

assert_status_clean() {
  local report="$1"
  python3 - "$report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if report.get("ok") is not True or report.get("clean") is not True:
    raise SystemExit("status was not clean")
PY
}

wait_for_daemon() {
  for _ in {1..120}; do
    if LOCALITY_STATE_DIR="$state_root" "$loc_bin" daemon status --state-dir "$state_root" --json \
      2>>"$command_log" | grep -q '"state": "running"'; then
      return 0
    fi
    sleep 0.25
  done
  echo "localityd did not become ready" >&2
  return 1
}

wait_for_fuse() {
  for _ in {1..120}; do
    if mountpoint -q "$locality_root" \
      && findmnt -rn --target "$locality_root" >/dev/null 2>&1 \
      && [[ -d "$mount_root" ]]; then
      return 0
    fi
    if [[ -n "$fuse_pid" ]] && ! kill -0 "$fuse_pid" >/dev/null 2>&1; then
      echo "locality-fuse stopped before its mount became ready" >&2
      return 1
    fi
    sleep 0.25
  done
  echo "locality-fuse did not become ready" >&2
  return 1
}

step="creating isolated directories"
mkdir -p "$state_root" "$locality_root" "$mount_root" "$logs_root"

if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
  step="building Locality live-test binaries"
  cargo build -p loc-cli -p localityd -p locality-fuse >/dev/null
fi

step="seeding live Google Docs VFS state"
helper_env=(
  "LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT=$state_root"
  "LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID_FILE=$workspace_id_file"
)
if [[ -n "${LOCALITY_GOOGLE_LIVE_FORCE_REFRESH:-}" ]]; then
  helper_env+=("LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=$LOCALITY_GOOGLE_LIVE_FORCE_REFRESH")
fi
env "${helper_env[@]}" \
  cargo test -p loc-cli --test live_google_connectors live_google_docs_seed_state_for_vfs \
    -- --ignored --exact >>"$command_log" 2>&1
workspace_id="$(tr -d '[:space:]' <"$workspace_id_file")"
if [[ -z "$workspace_id" ]]; then
  echo "Google Docs VFS seed helper did not write a workspace id" >&2
  exit 1
fi
unset LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON

step="registering the Google Docs Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount google-docs "$mount_root" \
    --workspace-folder "$workspace_id" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >"$mount_report" 2>>"$command_log"
assert_json_ok "$mount_report"

step="starting localityd"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"
wait_for_daemon

step="starting locality-fuse"
LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --state-dir "$state_root" \
  --mountpoint "$locality_root" >"$fuse_log" 2>&1 &
fuse_pid="$!"
wait_for_fuse

step="pulling the live Google Docs workspace through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$mount_root" --json \
  >"$pull_report" 2>>"$command_log"
python3 - "$pull_report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if report.get("ok") is not True:
    raise SystemExit("initial pull was not ok")
PY

draft_dir="$mount_root/fuse-draft"
draft_page="$draft_dir/page.md"
marker="Created from live Google Docs Linux FUSE e2e $(date -u +%Y%m%dT%H%M%SZ)-$$"

step="creating a Google Doc page directory through FUSE"
mkdir "$draft_dir"
printf -- '---\ntitle: Fuse Draft\n---\n# Fuse Draft\n\n%s\n' "$marker" >"$draft_page"

step="diffing the created Google Doc through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff "$draft_page" --json \
  >"$create_diff_report" 2>>"$command_log"
assert_create_plan "$create_diff_report"

step="pushing the created Google Doc through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$draft_page" -y --json \
  >"$create_push_report" 2>>"$command_log"
assert_json_ok_action "$create_push_report" "reconciled"

step="verifying clean Google Docs status after create"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status "$mount_root" --json \
  >"$status_after_create_report" 2>>"$command_log"
assert_status_clean "$status_after_create_report"

step="deleting the Google Doc directory through FUSE"
rm -r "$draft_dir"

step="diffing the deleted Google Doc through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff "$mount_root" --json \
  >"$delete_diff_report" 2>>"$command_log"
assert_archive_or_delete_plan "$delete_diff_report"

step="pushing the deleted Google Doc archive through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$mount_root" -y --confirm --json \
  >"$delete_push_report" 2>>"$command_log"
assert_json_ok_action "$delete_push_report" "reconciled"

step="verifying final clean Google Docs status"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status "$mount_root" --json \
  >"$final_status_report" 2>>"$command_log"
assert_status_clean "$final_status_report"

echo "live Google Docs Linux FUSE push/pull test passed"
