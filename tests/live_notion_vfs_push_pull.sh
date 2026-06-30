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

notion_token="${NOTION_TOKEN:-${NOTION_AT:-}}"
page_id="${LOCALITY_NOTION_PAGE_ID:-${NOTION_PAGE_ID:-}}"

if [[ -z "$notion_token" ]]; then
  echo "missing NOTION_TOKEN or NOTION_AT" >&2
  exit 1
fi

if [[ -z "$page_id" ]]; then
  echo "missing LOCALITY_NOTION_PAGE_ID or NOTION_PAGE_ID" >&2
  exit 1
fi
normalized_page_id="$(printf '%s' "$page_id" | tr '[:upper:]' '[:lower:]' | tr -d '-')"

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
mount_id="${LOCALITY_LIVE_NOTION_VFS_MOUNT_ID:-notion-main}"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-notion-vfs.XXXXXX")"
state_root="${LOCALITY_LIVE_NOTION_VFS_STATE:-$tmp_root/state}"
LOCALITY_ROOT="${LOCALITY_LIVE_NOTION_VFS_ROOT:-$tmp_root/Locality}"
NOTION_MOUNT="${LOCALITY_LIVE_NOTION_VFS_MOUNT:-$LOCALITY_ROOT/notion-main}"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
original_file="$tmp_root/original-page.md"
initial_pull="$tmp_root/initial-pull.json"
file_pull="$tmp_root/file-pull.json"
push_output="$tmp_root/push.json"
pull_after_push="$tmp_root/pull-after-push.json"
localityd_pid=""
fuse_pid=""
page_file=""
restore_needed=0
step="initializing"

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
  if [[ -s "$pull_after_push" ]]; then
    echo "pull-after-push output:" >&2
    cat "$pull_after_push" >&2
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
    LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" \
      "$loc_bin" push "$page_file" -y --json >/dev/null 2>&1
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
  local file
  while IFS= read -r file; do
    if tr '[:upper:]' '[:lower:]' < "$file" | tr -d '-' | grep -q "$normalized_page_id"; then
      printf '%s\n' "$file"
      return 0
    fi
  done < <(find "$NOTION_MOUNT" -name page.md -type f -print)
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

step="creating temp directories"
mkdir -p "$state_root" "$LOCALITY_ROOT" "$NOTION_MOUNT"

step="registering Notion Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 NOTION_TOKEN="$notion_token" \
  "$loc_bin" mount notion "$NOTION_MOUNT" \
    --root-page "$page_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >/dev/null

step="starting localityd"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off NOTION_TOKEN="$notion_token" \
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
LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" \
  "$loc_bin" pull "$NOTION_MOUNT" --json >"$initial_pull"
step="locating pulled page file"
page_file="$(find_pulled_page_file)"
step="pulling page file"
LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" \
  "$loc_bin" pull "$page_file" --json >"$file_pull"

step="saving original content"
cp "$page_file" "$original_file"
restore_needed=1

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
LOCALITY_DAEMON_DISABLE=1 LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" \
  "$loc_bin" push "$page_file" -y --json >"$push_output" || push_exit=$?
step="pulling immediately after push"
pull_exit=0
LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" \
  "$loc_bin" pull "$page_file" --json >"$pull_after_push" || pull_exit=$?

step="checking for conflict markers"
assert_no_conflict_markers "$page_file"
grep -q "$marker" "$page_file"
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

step="restoring original content"
cp "$original_file" "$page_file"
LOCALITY_STATE_DIR="$state_root" NOTION_TOKEN="$notion_token" "$loc_bin" push "$page_file" -y --json >/dev/null
restore_needed=0

echo "live Notion VFS push/pull test passed"
