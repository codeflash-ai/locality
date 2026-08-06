#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PROMPT_FILE="${ROOT}/experiment/standup-summary/prompts/locality-standup.md"

fail() {
  printf 'run-amika-standup-summary: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat >&2 <<'USAGE'
usage: run-amika-standup-summary.sh --sandbox <machine-id>
       run-amika-standup-summary.sh <machine-id>
USAGE
}

sandbox=""
while (($#)); do
  case "$1" in
    --sandbox)
      shift
      (($#)) || fail "--sandbox requires a machine id"
      sandbox="$1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      fail "unknown argument: $1"
      ;;
    *)
      if [[ -z "$sandbox" ]]; then
        sandbox="$1"
      else
        fail "unexpected argument: $1"
      fi
      ;;
  esac
  shift
done

[[ -n "$sandbox" ]] || fail "missing sandbox machine id"
[[ -s "$PROMPT_FILE" ]] || fail "missing prompt file: $PROMPT_FILE"

notion_parent_page_id="${NOTION_STANDUP_PARENT_PAGE_ID:-}"
[[ -n "$notion_parent_page_id" ]] || fail "set NOTION_STANDUP_PARENT_PAGE_ID"

: "${RUN_ID:=standup-$(date -u +%Y%m%dT%H%M%SZ)}"
: "${LOC_BIN:=loc}"
: "${CODEX_MODEL:=gpt-5.6-sol}"
: "${CODEX_REASONING_EFFORT:=low}"
: "${CODEX_EXEC_TIMEOUT_SECONDS:=900}"
: "${SLACK_TYPES:=private_channel,im,mpim}"
: "${STANDUP_DATE:=$(date -u +%F)}"

if [[ ! "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  fail "RUN_ID must start with an alphanumeric character and contain only [A-Za-z0-9._-]: $RUN_ID"
fi

command -v amika >/dev/null 2>&1 || fail "missing required tool: amika"

if [[ -z "${STANDUP_SINCE_ISO:-}" || -z "${STANDUP_UNTIL_ISO:-}" ]]; then
  read -r computed_since computed_until < <(python3 - <<'PY'
from datetime import datetime, timedelta, timezone
until = datetime.now(timezone.utc).replace(microsecond=0)
since = until - timedelta(hours=24)
print(since.isoformat().replace("+00:00", "Z"), until.isoformat().replace("+00:00", "Z"))
PY
)
  : "${STANDUP_SINCE_ISO:=$computed_since}"
  : "${STANDUP_UNTIL_ISO:=$computed_until}"
fi

amika_flags=()
if [[ -n "${AMIKA_SANDBOX_FLAGS:-}" ]]; then
  read -r -a amika_flags <<< "$AMIKA_SANDBOX_FLAGS"
fi

b64() {
  base64 | tr -d '\n'
}

shell_quote() {
  printf '%q' "$1"
}

amika_sandbox_ssh() {
  local machine="$1"
  shift
  amika sandbox ssh "${amika_flags[@]}" "$machine" "$@"
}

remote_script_b64="$(b64 <<'REMOTE_WORKER'
#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'standup remote worker: %s\n' "$*" >&2
  exit 1
}

require_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
}

run_id="${1:?run id required}"
loc_bin="${2:?loc bin required}"
code_model="${3:?codex model required}"
code_effort="${4:?codex reasoning effort required}"
code_timeout="${5:?codex timeout required}"
slack_types="${6:?slack types required}"
standup_date="${7:?standup date required}"
standup_since_iso="${8:?standup since required}"
standup_until_iso="${9:?standup until required}"
notion_parent_page_id="${10:?notion parent page id required}"
linear_connection_id_explicit="${11:-}"
slack_connection_id_explicit="${12:-}"
notion_connection_id_explicit="${13:-}"
locality_repo_dir_arg="${14:-}"
locality_internal_repo_dir_arg="${15:-}"
remote_run_root_arg="${16:-}"
prompt_b64_arg="${17:?prompt required}"

require_tool python3
require_tool git
require_tool codex
require_tool "$loc_bin"

remote_run_root="${remote_run_root_arg:-$HOME/standup-summary-runs}"
run_dir="$remote_run_root/$run_id"
mount_root="$run_dir/mounts"
evidence_dir="$run_dir/evidence"
prompt_file="$run_dir/prompt.md"
final_message_file="$run_dir/final-message.md"
artifact_file="$run_dir/standup.md"
trace_file="$run_dir/trace.md"
context_inventory="$evidence_dir/context-inventory.txt"
codex_events_file="$evidence_dir/codex-events.jsonl"
notion_parent_dir="$mount_root/notion"
locality_repo_dir="${locality_repo_dir_arg:-$HOME/workspace/locality}"
locality_internal_repo_dir="${locality_internal_repo_dir_arg:-$HOME/workspace/locality-internal}"

mkdir -p "$mount_root" "$evidence_dir" "$run_dir"
printf '%s' "$prompt_b64_arg" | base64 -d > "$prompt_file"

"$loc_bin" connections --json > "$evidence_dir/connections.json"

resolve_connection() {
  local connector="$1"
  local explicit_id="$2"
  python3 - "$evidence_dir/connections.json" "$connector" "$explicit_id" <<'PY'
import json
import sys

path, connector, explicit_id = sys.argv[1:4]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)

if isinstance(data, dict):
    rows = data.get("connections") or data.get("items") or data.get("data") or []
else:
    rows = data

def field(row, *names):
    for name in names:
        value = row.get(name)
        if value is not None:
            return value
    return None

active = []
for row in rows:
    if not isinstance(row, dict):
        continue
    row_connector = field(row, "connector", "provider", "type", "source")
    if row_connector != connector:
        continue
    state = field(row, "status", "state")
    is_active = row.get("active")
    if state in (None, "active", "connected") or is_active is True:
        active.append(row)

if explicit_id:
    for row in active:
        if field(row, "id", "connection_id", "connectionId") == explicit_id:
            print(explicit_id)
            raise SystemExit(0)
    print(f"{connector}: explicit connection id is not active: {explicit_id}", file=sys.stderr)
    raise SystemExit(2)

if not active:
    print(f"{connector}: no active connection", file=sys.stderr)
    raise SystemExit(2)
if len(active) > 1:
    ids = ", ".join(str(field(row, "id", "connection_id", "connectionId")) for row in active)
    print(f"{connector}: multiple active connections ({ids}); set {connector.upper()}_CONNECTION_ID", file=sys.stderr)
    raise SystemExit(2)

connection_id = field(active[0], "id", "connection_id", "connectionId")
if not connection_id:
    print(f"{connector}: active connection is missing id", file=sys.stderr)
    raise SystemExit(2)
print(connection_id)
PY
}

linear_connection_id="$(resolve_connection linear "$linear_connection_id_explicit")"
slack_connection_id="$(resolve_connection slack "$slack_connection_id_explicit")"
notion_connection_id="$(resolve_connection notion "$notion_connection_id_explicit")"

"$loc_bin" mount linear "$mount_root/linear" --connection "$linear_connection_id" --mount-id "$run_id-linear" --projection plain-files --json > "$evidence_dir/mount-linear.json"
"$loc_bin" mount slack "$mount_root/slack" --connection "$slack_connection_id" --mount-id "$run_id-slack" --projection plain-files --history-limit 15 --types "$slack_types" --json > "$evidence_dir/mount-slack.json"
"$loc_bin" mount notion "$mount_root/notion" --root-page "$notion_parent_page_id" --connection "$notion_connection_id" --mount-id "$run_id-notion" --projection plain-files --json > "$evidence_dir/mount-notion.json"

hydrate_root() {
  local name="$1"
  local root="$2"
  "$loc_bin" pull "$root" --json > "$evidence_dir/pull-$name.json"
  find "$root" -type f \( \
    -name page.md -o \
    -name recent.md -o \
    -name users.md -o \
    -name comments.md -o \
    -name history.md -o \
    -name pull-requests.md -o \
    -name attachments.md \
  \) -print0 | while IFS= read -r -d '' file; do
    rel_path="${file#"$root"/}"
    if output="$("$loc_bin" pull "$file" --json 2>> "$evidence_dir/hydration-failures.log")"; then
      python3 - "$name" "$rel_path" "$file" ok "$output" >> "$evidence_dir/hydration.jsonl" <<'PY'
import json
import sys

connector, rel_path, path, status, output = sys.argv[1:6]
try:
    payload = json.loads(output)
except json.JSONDecodeError:
    payload = output
print(json.dumps({
    "connector": connector,
    "path": path,
    "relative_path": rel_path,
    "status": status,
    "output": payload,
}, sort_keys=True))
PY
      printf 'ok\t%s\n' "$file" >> "$evidence_dir/hydration.log"
    else
      python3 - "$name" "$rel_path" "$file" failed >> "$evidence_dir/hydration.jsonl" <<'PY'
import json
import sys

connector, rel_path, path, status = sys.argv[1:5]
print(json.dumps({
    "connector": connector,
    "path": path,
    "relative_path": rel_path,
    "status": status,
}, sort_keys=True))
PY
      printf 'failed\t%s\n' "$file" >> "$evidence_dir/hydration.log"
    fi
  done
}

hydrate_root linear "$mount_root/linear"
hydrate_root slack "$mount_root/slack"
hydrate_root notion "$mount_root/notion"

find "$mount_root" -type f \( \
  -name '*.md' -o \
  -name '*.txt' -o \
  -name '*.json' -o \
  -name page.md \
\) -print | sort > "$context_inventory"

ensure_repo() {
  local slug="$1"
  local dir="$2"
  local label
  local log_ref
  label="$(basename "$dir")"
  if [[ -d "$dir" ]] && git -C "$dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    :
  else
    if [[ -e "$dir" ]]; then
      if [[ ! -d "$dir" ]]; then
        fail "repo path exists and is not a directory: $dir"
      fi
      if find "$dir" -mindepth 1 -print -quit | grep -q .; then
        fail "repo path exists but is not a git worktree and is not empty: $dir"
      fi
    fi
    mkdir -p "$(dirname "$dir")"
    if command -v gh >/dev/null 2>&1; then
      gh repo clone "$slug" "$dir"
    else
      git clone "https://github.com/$slug.git" "$dir"
    fi
  fi
  git -C "$dir" fetch --prune origin
  if log_ref="$(git -C "$dir" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null)"; then
    printf 'ref\t%s\n' "$log_ref" > "$evidence_dir/git-log-$label-ref.txt"
    git -C "$dir" log "$log_ref" --since="$standup_since_iso" --date=iso-strict --pretty=format:'%H%x09%ad%x09%an%x09%ae%x09%s' > "$evidence_dir/$label-commits.tsv"
    git -C "$dir" log "$log_ref" --since="$standup_since_iso" --stat --date=iso-strict > "$evidence_dir/$label-stat.log"
  else
    printf 'mode\t--remotes=origin\n' > "$evidence_dir/git-log-$label-ref.txt"
    git -C "$dir" log --remotes=origin --since="$standup_since_iso" --date=iso-strict --pretty=format:'%H%x09%ad%x09%an%x09%ae%x09%s' > "$evidence_dir/$label-commits.tsv"
    git -C "$dir" log --remotes=origin --since="$standup_since_iso" --stat --date=iso-strict > "$evidence_dir/$label-stat.log"
  fi
}

ensure_repo codeflash-ai/locality "$locality_repo_dir"
ensure_repo codeflash-ai/locality-internal "$locality_internal_repo_dir"

export STANDUP_MOUNT_ROOT="$mount_root"
export STANDUP_CONTEXT_INVENTORY="$context_inventory"
export STANDUP_EVIDENCE_DIR="$evidence_dir"
export LOCALITY_REPO_DIR="$locality_repo_dir"
export LOCALITY_INTERNAL_REPO_DIR="$locality_internal_repo_dir"
export STANDUP_NOTION_PARENT_DIR="$notion_parent_dir"
export STANDUP_ARTIFACT_FILE="$artifact_file"
export STANDUP_TRACE_FILE="$trace_file"
export STANDUP_DATE="$standup_date"
export STANDUP_SINCE_ISO="$standup_since_iso"
export STANDUP_UNTIL_ISO="$standup_until_iso"
export STANDUP_PAGE_TITLE="standup-$standup_date"
export LOC_BIN="$loc_bin"

codex_cmd=(
  codex exec
  --json
  --model "$code_model"
  -c "model_reasoning_effort=\"$code_effort\""
  --dangerously-bypass-approvals-and-sandbox
  -C "$locality_repo_dir"
  --add-dir "$mount_root"
  --add-dir "$evidence_dir"
  --add-dir "$locality_repo_dir"
  --add-dir "$locality_internal_repo_dir"
  --output-last-message "$final_message_file"
  "$(cat "$prompt_file")"
)

if [[ "$code_timeout" != "0" ]] && command -v timeout >/dev/null 2>&1; then
  timeout "$code_timeout" "${codex_cmd[@]}" > "$codex_events_file"
else
  "${codex_cmd[@]}" > "$codex_events_file"
fi

[[ -s "$artifact_file" ]] || fail "Codex did not write $artifact_file"
[[ -s "$trace_file" ]] || fail "Codex did not write $trace_file"

python3 - "$run_id" "$run_dir" "$mount_root" "$evidence_dir" "$artifact_file" "$trace_file" "$final_message_file" <<'PY'
import json
import sys

keys = ["run_id", "run_dir", "mount_root", "evidence_dir", "artifact_file", "trace_file", "final_message_file"]
print(json.dumps(dict(zip(keys, sys.argv[1:])), sort_keys=True))
PY
REMOTE_WORKER
)"

prompt_b64="$(b64 < "$PROMPT_FILE")"

remote_args=(
  "$RUN_ID"
  "$LOC_BIN"
  "$CODEX_MODEL"
  "$CODEX_REASONING_EFFORT"
  "$CODEX_EXEC_TIMEOUT_SECONDS"
  "$SLACK_TYPES"
  "$STANDUP_DATE"
  "$STANDUP_SINCE_ISO"
  "$STANDUP_UNTIL_ISO"
  "$notion_parent_page_id"
  "${LINEAR_CONNECTION_ID:-}"
  "${SLACK_CONNECTION_ID:-}"
  "${NOTION_CONNECTION_ID:-}"
  "${LOCALITY_REPO_DIR:-}"
  "${LOCALITY_INTERNAL_REPO_DIR:-}"
  "${STANDUP_REMOTE_RUN_ROOT:-}"
  "$prompt_b64"
)

remote_command="printf %s $(shell_quote "$remote_script_b64") | base64 -d | bash -s --"
for arg in "${remote_args[@]}"; do
  remote_command+=" $(shell_quote "$arg")"
done

amika_sandbox_ssh "$sandbox" -- bash -lc "$remote_command"
