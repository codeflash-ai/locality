#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GRANOLA_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GRANOLA_VFS=1 to run the live Granola VFS test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: live Granola VFS test requires Linux"
  exit 0
fi

for command in fusermount3 mountpoint python3 sqlite3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required live Granola VFS dependency: $command" >&2
    exit 1
  fi
done
if [[ ! -e /dev/fuse ]]; then
  echo "/dev/fuse is not available on this runner" >&2
  exit 1
fi

granola_api_key="${GRANOLA_API_KEY:-}"
live_note_id="${LOCALITY_GRANOLA_LIVE_NOTE_ID:-}"
if [[ -z "$granola_api_key" ]]; then
  echo "missing GRANOLA_API_KEY" >&2
  exit 1
fi
if [[ -z "$live_note_id" ]]; then
  echo "missing LOCALITY_GRANOLA_LIVE_NOTE_ID" >&2
  exit 1
fi
if [[ ! "$live_note_id" =~ ^[A-Za-z0-9_-]+$ ]]; then
  echo "LOCALITY_GRANOLA_LIVE_NOTE_ID has an invalid shape" >&2
  exit 1
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
mount_id="${LOCALITY_GRANOLA_LIVE_MOUNT_ID:-granola-live}"
connection_id="${LOCALITY_GRANOLA_LIVE_CONNECTION_ID:-granola-live}"
if [[ ! "$mount_id" =~ ^[A-Za-z0-9._-]+$ || ! "$connection_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "live Granola mount or connection id has an invalid shape" >&2
  exit 1
fi
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-granola.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
granola_mount="$locality_root/granola"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
command_log="$tmp_root/commands.err.log"
connect_report="$tmp_root/connect.json"
mount_report="$tmp_root/mount.json"
first_pull_report="$tmp_root/first-pull.json"
second_pull_report="$tmp_root/second-pull.json"
doctor_report="$tmp_root/doctor.json"
status_report="$tmp_root/status.json"
info_report="$tmp_root/info.json"
summary_copy="$tmp_root/summary.md"
summary_reopen_copy="$tmp_root/summary-reopen.md"
transcript_copy="$tmp_root/transcript.md"
localityd_pid=""
fuse_pid=""
step="initializing"

on_error() {
  local code=$?
  echo "live Granola VFS test failed during: $step" >&2
  echo "privacy-safe diagnostics: exit=$code" >&2
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
  unset granola_api_key GRANOLA_API_KEY
  if [[ "${LOCALITY_GRANOLA_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept private live Granola temp state locally"
  else
    rm -rf "$tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
  step="building Locality live-test binaries"
  cargo build -p loc-cli -p localityd -p locality-fuse >/dev/null
fi

wait_for_daemon() {
  for _ in {1..120}; do
    if LOCALITY_STATE_DIR="$state_root" "$loc_bin" daemon status --state-dir "$state_root" --json 2>/dev/null \
      | grep -q '"state": "running"'; then
      return 0
    fi
    sleep 0.25
  done
  echo "localityd did not become ready" >&2
  return 1
}

wait_for_fuse() {
  for _ in {1..120}; do
    if mountpoint -q "$locality_root"; then
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

assert_json_true() {
  local report="$1"
  local field="$2"
  python3 - "$report" "$field" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
value = report
for part in sys.argv[2].split("."):
    if not isinstance(value, dict) or part not in value:
        raise SystemExit(f"live report omitted expected field {sys.argv[2]}")
    value = value[part]
if value is not True:
    raise SystemExit(f"live report field {sys.argv[2]} was not true")
PY
}

state_query() {
  sqlite3 -cmd '.timeout 10000' "$state_root/state.sqlite3" "$1" 2>>"$command_log"
}

copy_mounted_file() {
  local source="$1"
  local destination="$2"
  for _ in {1..20}; do
    if cp "$source" "$destination" >/dev/null 2>>"$command_log"; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

validate_mounted_documents() {
  python3 - "$summary_copy" "$transcript_copy" <<'PY'
import json
import pathlib
import re
import sys

def parse(path, expected_kind):
    text = pathlib.Path(path).read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        raise SystemExit(f"{expected_kind} did not start with YAML frontmatter")
    try:
        _, frontmatter, body = text.split("---\n", 2)
    except ValueError:
        raise SystemExit(f"{expected_kind} frontmatter was not terminated")
    if "  connector: granola\n" not in frontmatter:
        raise SystemExit(f"{expected_kind} omitted the Granola connector identity")
    if f"  content_kind: {expected_kind}\n" not in frontmatter:
        raise SystemExit(f"{expected_kind} used the wrong content kind")
    match = re.search(r"^  note_id: (.+)$", frontmatter, re.MULTILINE)
    if not match:
        raise SystemExit(f"{expected_kind} omitted the durable note id")
    note_id = json.loads(match.group(1))
    if not note_id:
        raise SystemExit(f"{expected_kind} note id was empty")
    if not body.strip():
        raise SystemExit(f"{expected_kind} body was empty")
    return note_id, body

summary_id, summary = parse(sys.argv[1], "summary")
transcript_id, transcript = parse(sys.argv[2], "transcript")
if summary_id != transcript_id:
    raise SystemExit("summary and transcript used different Granola note ids")
if summary.strip() == "_No summary is available._":
    raise SystemExit("mounted Granola fixture did not contain a real summary")

pattern = re.compile(
    r"^\*\*(?:Me|Them)(?: \(.+\))? · "
    r"\d{2}:\d{2}:\d{2}(?:–\d{2}:\d{2}:\d{2})? UTC\*\*$"
)
headings = [line for line in transcript.splitlines() if pattern.fullmatch(line)]
if not headings:
    raise SystemExit("mounted transcript did not contain canonical speaker turns")
for heading in headings:
    speaker = heading.split(" · ", 1)[0].lower()
    if speaker.endswith(" (microphone)") or speaker.endswith(" (speaker)"):
        raise SystemExit("mounted transcript repeated its capture source")
PY
}

step="creating isolated state"
mkdir -p "$state_root" "$locality_root" "$granola_mount"

step="connecting to the live Granola API"
printf '%s' "$granola_api_key" | env -u GRANOLA_API_KEY \
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" connect granola --name "$connection_id" --api-key-stdin --json \
  >"$connect_report" 2>>"$command_log"
assert_json_true "$connect_report" ok
if grep -aFq "$granola_api_key" "$connect_report" "$state_root/state.sqlite3"; then
  echo "Granola API key leaked into command output or SQLite state" >&2
  exit 1
fi

step="registering the read-only Granola Linux FUSE mount"
env -u GRANOLA_API_KEY LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount granola "$granola_mount" \
    --connection "$connection_id" \
    --mount-id "$mount_id" \
    --projection linux-fuse \
    --json >"$mount_report" 2>>"$command_log"
assert_json_true "$mount_report" ok
assert_json_true "$mount_report" read_only
unset granola_api_key GRANOLA_API_KEY

step="starting localityd"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"
wait_for_daemon

step="starting the Linux FUSE provider"
LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --state-dir "$state_root" \
  --mountpoint "$locality_root" >"$fuse_log" 2>&1 &
fuse_pid="$!"
wait_for_fuse

step="enumerating live Granola meetings through the mounted filesystem"
meeting_count="0"
for _ in {1..120}; do
  if listing_dots="$(find "$granola_mount" -mindepth 1 -maxdepth 1 -printf '.' \
    2>>"$command_log")"; then
    meeting_count="${#listing_dots}"
  else
    sleep 0.25
    continue
  fi
  if [[ "$meeting_count" =~ ^[0-9]+$ ]] && (( meeting_count > 0 )); then
    break
  fi
  sleep 0.25
done
if [[ ! "$meeting_count" =~ ^[0-9]+$ ]] || (( meeting_count < 1 )); then
  echo "live Granola enumeration produced no meeting directories" >&2
  exit 1
fi
checkpoint_count="0"
for _ in {1..120}; do
  checkpoint_count="$(state_query \
    "SELECT count(*) FROM connector_state WHERE connector = 'granola' AND scope_kind = 'mount' AND scope_id = '$mount_id';" \
    || printf '0')"
  if [[ "$checkpoint_count" == "1" ]]; then
    break
  fi
  sleep 0.25
done
if [[ "$checkpoint_count" != "1" ]]; then
  echo "mounted Granola discovery did not record its incremental checkpoint" >&2
  exit 1
fi

step="running an explicit incremental Granola pull"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$granola_mount" --json \
  >"$first_pull_report" 2>>"$command_log"
assert_json_true "$first_pull_report" ok
entity_count_before="$(state_query \
  "SELECT count(*) FROM entities WHERE mount_id = '$mount_id';")"

step="hydrating one retained transcript through the mounted filesystem"
selected_relative_path="$(state_query \
  "SELECT path FROM entities WHERE mount_id = '$mount_id' AND remote_id = '$live_note_id' AND kind_json = '\"directory\"' LIMIT 1;")"
if [[ -z "$selected_relative_path" ]]; then
  echo "configured Granola live fixture was not returned by enumeration" >&2
  exit 1
fi
if [[ "$selected_relative_path" == "." || "$selected_relative_path" == ".." || "$selected_relative_path" == */* ]]; then
  echo "configured Granola fixture resolved outside a single meeting directory" >&2
  exit 1
fi
selected_meeting="$granola_mount/$selected_relative_path"
if ! copy_mounted_file "$selected_meeting/transcript.md" "$transcript_copy"; then
  echo "configured Granola transcript could not be read through FUSE" >&2
  exit 1
fi
if ! grep -Eq '^\*\*(Me|Them)( | ·)' "$transcript_copy"; then
  echo "configured Granola fixture did not contain canonical transcript turns" >&2
  exit 1
fi
if ! copy_mounted_file "$selected_meeting/summary.md" "$summary_copy"; then
  echo "configured Granola summary could not be read through FUSE" >&2
  exit 1
fi
if grep -Fxq '_No summary is available._' "$summary_copy"; then
  echo "configured Granola fixture did not contain a real summary" >&2
  exit 1
fi

step="validating the hydrated summary and transcript"
validate_mounted_documents

step="reopening the materialized summary through the mounted filesystem"
summary_reopened=0
if copy_mounted_file "$selected_meeting/summary.md" "$summary_reopen_copy"; then
  summary_reopened=1
fi
if [[ "$summary_reopened" != "1" ]]; then
  echo "materialized Granola summary could not be reopened through FUSE" >&2
  exit 1
fi
if ! cmp -s "$summary_copy" "$summary_reopen_copy"; then
  echo "reopened Granola summary did not match its first mounted read" >&2
  exit 1
fi

step="verifying the mount remains clean and read-only"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" info "$selected_meeting/summary.md" --json \
  >"$info_report" 2>>"$command_log"
assert_json_true "$info_report" mount.read_only
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status "$selected_meeting/summary.md" --json \
  >"$status_report" 2>>"$command_log"
assert_json_true "$status_report" clean
summary_hash_before="$(sha256sum "$selected_meeting/summary.md" | awk '{print $1}')"
if { printf 'live e2e must not write\n' >"$selected_meeting/summary.md"; } 2>/dev/null; then
  echo "Granola mounted file unexpectedly accepted a filesystem write" >&2
  exit 1
fi
summary_hash_after="$(sha256sum "$selected_meeting/summary.md" | awk '{print $1}')"
if [[ "$summary_hash_before" != "$summary_hash_after" ]]; then
  echo "Granola mounted file changed after a rejected write" >&2
  exit 1
fi

step="repeating live discovery without duplicating state"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" pull "$granola_mount" --json \
  >"$second_pull_report" 2>>"$command_log"
assert_json_true "$second_pull_report" ok
entity_count_after="$(state_query \
  "SELECT count(*) FROM entities WHERE mount_id = '$mount_id';")"
if [[ ! "$entity_count_before" =~ ^[0-9]+$ || ! "$entity_count_after" =~ ^[0-9]+$ ]] \
  || (( entity_count_after < entity_count_before )); then
  echo "repeated incremental discovery discarded existing Granola entities" >&2
  exit 1
fi
duplicate_remote_ids="$(state_query \
  "SELECT count(*) FROM (SELECT remote_id FROM entities WHERE mount_id = '$mount_id' GROUP BY remote_id HAVING count(*) > 1);")"
duplicate_paths="$(state_query \
  "SELECT count(*) FROM (SELECT path FROM entities WHERE mount_id = '$mount_id' GROUP BY path HAVING count(*) > 1);")"
if [[ "$duplicate_remote_ids" != "0" || "$duplicate_paths" != "0" ]]; then
  echo "repeated Granola discovery created duplicate identities or paths" >&2
  exit 1
fi

step="running final diagnostics"
doctor_exit=0
LOCALITY_STATE_DIR="$state_root" "$loc_bin" doctor --json \
  >"$doctor_report" 2>>"$command_log" || doctor_exit="$?"
python3 - "$doctor_report" "$doctor_exit" "$mount_id" "$connection_id" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
exit_code = int(sys.argv[2])
mount_id = sys.argv[3]
connection_id = sys.argv[4]

# The CI test launches locality-fuse directly because GitHub-hosted runners do
# not provide a user systemd session. Doctor must see every other component as
# healthy and report only that intentional lack of persistent registration.
if exit_code != 3 or report.get("ok") is not False or report.get("status") != "error":
    raise SystemExit("doctor did not report the expected direct-launch lifecycle exception")
if (report.get("daemon") or {}).get("state") != "running":
    raise SystemExit("doctor did not observe the live daemon")

connections = {
    connection.get("connection_id"): connection
    for connection in report.get("connections", [])
}
connection = connections.get(connection_id)
if not connection:
    raise SystemExit("doctor omitted the live Granola connection")
if connection.get("status") != "active" or connection.get("credential_status") != "ok":
    raise SystemExit("doctor did not report a healthy live Granola connection")

mounts = {mount.get("mount_id"): mount for mount in report.get("mounts", [])}
mount = mounts.get(mount_id)
if not mount:
    raise SystemExit("doctor omitted the live Granola mount")
expected_mount = {
    "connector": "granola",
    "projection": "linux-fuse",
    "read_only": True,
    "root_exists": True,
    "connection_id": connection_id,
}
for field, expected in expected_mount.items():
    if mount.get(field) != expected:
        raise SystemExit(f"doctor reported an unexpected Granola mount {field}")
provider = mount.get("provider") or {}
if provider.get("state") != "unregistered" or provider.get("registered") is not False:
    raise SystemExit("doctor did not identify the direct-launch FUSE lifecycle exception")
if provider.get("helper_present") is not True:
    raise SystemExit("doctor could not find the built Linux FUSE helper")

errors = {
    finding.get("code")
    for finding in report.get("findings", [])
    if finding.get("severity") == "error"
}
if errors != {"provider_unregistered"}:
    raise SystemExit("doctor reported an unexpected error during the live Granola test")
PY

echo "live Granola API, CLI, daemon, and Linux FUSE read-only checks passed"
