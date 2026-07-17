#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GMAIL_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GMAIL_VFS=1 to run the live Gmail VFS read/send test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: live Gmail VFS read/send test requires Linux"
  exit 0
fi

for command in fusermount3 mountpoint findmnt python3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required live Gmail VFS dependency: $command" >&2
    exit 1
  fi
done
if [[ ! -e /dev/fuse ]]; then
  echo "/dev/fuse is not available on this runner" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON:-}" ]]; then
  echo "missing LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GMAIL_LIVE_TEST_RECIPIENT:-}" ]]; then
  echo "missing LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" >&2
  exit 1
fi

validate_recipient() {
  python3 - "$1" <<'PY'
import sys

value = sys.argv[1]
delimiters = set(",;<>'\"()[]{}")
valid = (
    value
    and value.count("@") == 1
    and not any(ch.isspace() or ord(ch) < 32 or ch in delimiters for ch in value)
)
if valid:
    local, domain = value.split("@", 1)
    valid = (
        bool(local)
        and not local.startswith(".")
        and not local.endswith(".")
        and "." in domain
        and not domain.startswith(".")
        and not domain.endswith(".")
        and ".." not in domain
        and all(part for part in domain.split("."))
    )
if not valid:
    raise SystemExit("LOCALITY_GMAIL_LIVE_TEST_RECIPIENT must be a single email-like recipient")
PY
}

recipient="$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT"
validate_recipient "$recipient"
after="${LOCALITY_GMAIL_LIVE_AFTER:-$(date -u -d yesterday +%F)}"
before="${LOCALITY_GMAIL_LIVE_BEFORE:-$(date -u -d tomorrow +%F)}"
body="This message was sent by the Locality live Gmail e2e suite."

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
connection_id="gmail-live"
messages_mount_id="gmail-main"
threads_mount_id="gmail-threads"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-gmail-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
messages_mount="$locality_root/gmail-main"
threads_mount="$locality_root/gmail-threads"
logs_root="$tmp_root/logs"
daemon_log="$logs_root/localityd.log"
fuse_log="$logs_root/locality-fuse.log"
command_log="$logs_root/commands.err.log"
messages_mount_report="$logs_root/gmail-main-mount.json"
threads_mount_report="$logs_root/gmail-threads-mount.json"
messages_pull_report="$logs_root/gmail-main-pull.json"
threads_pull_report="$logs_root/gmail-threads-pull.json"
hydrate_report="$logs_root/hydrate.json"
thread_hydrate_report="$logs_root/thread-hydrate.json"
draft_diff_report="$logs_root/draft-diff.json"
draft_push_report="$logs_root/draft-push.json"
sent_diff_report="$logs_root/sent-diff.json"
sent_original="$tmp_root/sent-original.md"
sent_edit="$tmp_root/sent-edit.md"
localityd_pid=""
fuse_pid=""
sent_file=""
thread_file=""
step="initializing"

on_error() {
  local code=$?
  echo "live Gmail VFS read/send test failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
  if [[ -n "$sent_file" ]]; then
    echo "privacy-safe diagnostics: sent_marker_found=1" >&2
  fi
  if [[ -n "$thread_file" ]]; then
    echo "privacy-safe diagnostics: thread_marker_found=1" >&2
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
  unset LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON
  if [[ "${LOCALITY_GOOGLE_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept private live Gmail VFS temp root: $tmp_root"
  else
    rm -rf "$tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

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

assert_create_plan() {
  local report="$1"
  python3 - "$report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
summary = ((report.get("plan") or {}).get("summary") or {})
if report.get("ok") is not True or report.get("action") != "confirm_plan":
    raise SystemExit("draft diff did not report a confirmable plan")
if summary.get("entities_created") != 1:
    raise SystemExit(f"draft diff created count was not 1: {summary}")
PY
}

assert_validation_code() {
  local report="$1"
  local code="$2"
  python3 - "$report" "$code" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
expected = sys.argv[2]
codes = {issue.get("code") for issue in report.get("validation") or []}
if expected not in codes:
    raise SystemExit(f"expected validation code {expected!r}, got {sorted(codes)}")
PY
}

find_file_containing_marker() {
  local search_root="$1"
  local marker="$2"
  python3 - "$search_root" "$marker" <<'PY'
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
marker = sys.argv[2]
for directory, _, filenames in os.walk(root):
    for filename in sorted(filenames):
        if not filename.endswith(".md"):
            continue
        path = pathlib.Path(directory) / filename
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        if marker in text:
            print(path)
            raise SystemExit(0)
raise SystemExit(1)
PY
}

assert_message_file() {
  local path="$1"
  local marker="$2"
  local expected_body="$3"
  python3 - "$path" "$marker" "$expected_body" <<'PY'
import pathlib
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
marker = sys.argv[2]
expected_body = sys.argv[3]
if marker not in text:
    raise SystemExit("message file did not contain the unique marker")
if expected_body not in text:
    raise SystemExit("message file did not contain the expected body")
if "<!-- loc:stub" in text:
    raise SystemExit("message file still contained the Locality stub marker")
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
      && [[ -d "$messages_mount" && -d "$threads_mount" ]]; then
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

wait_for_message_mailboxes() {
  for _ in {1..40}; do
    if LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$messages_mount" --json \
      >"$messages_pull_report" 2>>"$command_log"; then
      if [[ -d "$messages_mount/inbox" && -d "$messages_mount/sent" && -d "$messages_mount/draft" ]]; then
        return 0
      fi
    fi
    sleep 0.75
  done
  echo "Gmail messages mount did not expose inbox, sent, and draft directories" >&2
  return 1
}

pull_until_hydrated_marker() {
  local mount_root="$1"
  local search_root="$2"
  local pull_report="$3"
  local hydrate_output="$4"
  local marker="$5"
  local found
  local deadline=$((SECONDS + 30))
  while (( SECONDS <= deadline )); do
    LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$mount_root" --json \
      >"$pull_report" 2>>"$command_log"
    if found="$(find_file_containing_marker "$search_root" "$marker")"; then
      LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$found" --json \
        >"$hydrate_output" 2>>"$command_log"
      if assert_message_file "$found" "$marker" "$body" 2>>"$command_log"; then
        printf '%s\n' "$found"
        return 0
      fi
    fi
    sleep 1
  done
  echo "Gmail marker did not appear hydrated under $search_root before the deadline" >&2
  return 1
}

step="creating isolated directories"
mkdir -p "$state_root" "$locality_root" "$messages_mount" "$threads_mount" "$logs_root"

if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
  step="building Locality live-test binaries"
  cargo build -p loc-cli -p localityd -p locality-fuse >/dev/null
fi

step="seeding live Gmail VFS state"
helper_env=("LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT=$state_root")
if [[ -n "${LOCALITY_GOOGLE_LIVE_FORCE_REFRESH:-}" ]]; then
  helper_env+=("LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=$LOCALITY_GOOGLE_LIVE_FORCE_REFRESH")
fi
env "${helper_env[@]}" \
  cargo test -p loc-cli --test live_google_connectors live_gmail_seed_state_for_vfs \
    -- --ignored --exact >>"$command_log" 2>&1
unset LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON

step="registering the Gmail messages Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount gmail "$messages_mount" \
    --connection "$connection_id" \
    --mount-id "$messages_mount_id" \
    --after "$after" \
    --before "$before" \
    --view messages \
    --projection linux-fuse \
    --json >"$messages_mount_report" 2>>"$command_log"
assert_json_ok "$messages_mount_report"

step="registering the Gmail threads Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount gmail "$threads_mount" \
    --connection "$connection_id" \
    --mount-id "$threads_mount_id" \
    --after "$after" \
    --before "$before" \
    --view threads \
    --projection linux-fuse \
    --json >"$threads_mount_report" 2>>"$command_log"
assert_json_ok "$threads_mount_report"

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

step="pulling Gmail messages until mailbox directories exist"
wait_for_message_mailboxes
assert_json_ok "$messages_pull_report"

marker="loc-live-gmail-vfs-$(date -u +%Y%m%dT%H%M%SZ)-$$"
subject="Locality live Gmail VFS e2e $marker"
draft_file="$messages_mount/draft/$marker.md"

step="writing a Gmail draft through FUSE"
printf -- '---\nto:\n  - "%s"\nsubject: "%s"\n---\n%s\n' "$recipient" "$subject" "$body" >"$draft_file"

step="diffing the Gmail draft through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff "$draft_file" --json \
  >"$draft_diff_report" 2>>"$command_log"
assert_create_plan "$draft_diff_report"

step="pushing the Gmail draft through FUSE"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$draft_file" -y --json \
  >"$draft_push_report" 2>>"$command_log"
assert_json_ok_action "$draft_push_report" "reconciled"

step="waiting for the sent Gmail message to appear hydrated"
sent_file="$(pull_until_hydrated_marker "$messages_mount" "$messages_mount/sent" "$messages_pull_report" "$hydrate_report" "$marker")"

step="checking Gmail sent mailbox read-only behavior"
cp "$sent_file" "$sent_original"
cp "$sent_original" "$sent_edit"
printf '\nLocal edit that must stay read-only.\n' >>"$sent_edit"
if cp "$sent_edit" "$sent_file" 2>>"$command_log"; then
  if ! cmp -s "$sent_file" "$sent_original"; then
    diff_exit=0
    LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff "$sent_file" --json \
      >"$sent_diff_report" 2>>"$command_log" || diff_exit="$?"
    assert_validation_code "$sent_diff_report" "gmail_read_only_mailbox"
  fi
else
  if ! cmp -s "$sent_file" "$sent_original"; then
    echo "rejected sent-mail write changed file content" >&2
    exit 1
  fi
fi

step="waiting for the sent Gmail thread view to appear hydrated"
thread_file="$(pull_until_hydrated_marker "$threads_mount" "$threads_mount/sent" "$threads_pull_report" "$thread_hydrate_report" "$marker")"

echo "live Gmail Linux FUSE read/send test passed"
