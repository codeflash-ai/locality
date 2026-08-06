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

notion_parent_page_id="${NOTION_STANDUP_PARENT_PAGE_ID:-${NOTION_ROOT_PAGE_ID:-}}"
[[ -n "$notion_parent_page_id" ]] || fail "set NOTION_STANDUP_PARENT_PAGE_ID or NOTION_ROOT_PAGE_ID"

: "${RUN_ID:=standup-$(date -u +%Y%m%dT%H%M%SZ)}"
: "${LOC_BIN:=loc}"
: "${CODEX_MODEL:=gpt-5.6-sol}"
: "${CODEX_REASONING_EFFORT:=low}"
: "${CODEX_EXEC_TIMEOUT_SECONDS:=900}"
: "${SLACK_TYPES:=private_channel,im,mpim}"
: "${STANDUP_DATE:=$(date -u +%F)}"

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

worker_b64="$(b64 <<'REMOTE_WORKER'
#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'standup remote worker: %s\n' "$*" >&2
  exit 1
}

require_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
}

require_tool python3
require_tool git
require_tool codex
require_tool "$LOC_BIN"

run_id="${RUN_ID:?}"
loc_bin="${LOC_BIN:?}"
code_model="${CODEX_MODEL:?}"
code_effort="${CODEX_REASONING_EFFORT:?}"
code_timeout="${CODEX_EXEC_TIMEOUT_SECONDS:?}"
slack_types="${SLACK_TYPES:?}"
standup_date="${STANDUP_DATE:?}"
standup_since_iso="${STANDUP_SINCE_ISO:?}"
standup_until_iso="${STANDUP_UNTIL_ISO:?}"
notion_parent_page_id="${NOTION_STANDUP_PARENT_PAGE_ID:?}"

remote_run_root="${STANDUP_REMOTE_RUN_ROOT:-$HOME/standup-summary-runs}"
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
locality_repo_dir="${LOCALITY_REPO_DIR:-$HOME/workspace/locality}"
locality_internal_repo_dir="${LOCALITY_INTERNAL_REPO_DIR:-$HOME/workspace/locality-internal}"

mkdir -p "$mount_root" "$evidence_dir" "$run_dir"
printf '%s' "${PROMPT_B64:?}" | base64 -d > "$prompt_file"

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

linear_connection_id="$(resolve_connection linear "${LINEAR_CONNECTION_ID:-}")"
slack_connection_id="$(resolve_connection slack "${SLACK_CONNECTION_ID:-}")"
notion_connection_id="$(resolve_connection notion "${NOTION_CONNECTION_ID:-}")"

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
    if "$loc_bin" pull "$file" --json > "$evidence_dir/hydrate-$(basename "$file").json" 2>> "$evidence_dir/hydration-failures.log"; then
      printf 'ok\t%s\n' "$file" >> "$evidence_dir/hydration.log"
    else
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
  mkdir -p "$(dirname "$dir")"
  if ! git -C "$dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    rm -rf "$dir"
    if command -v gh >/dev/null 2>&1; then
      gh repo clone "$slug" "$dir"
    else
      git clone "https://github.com/$slug.git" "$dir"
    fi
  fi
  git -C "$dir" remote get-url origin > "$evidence_dir/$(basename "$dir")-origin.txt"
  git -C "$dir" fetch --prune origin
  git -C "$dir" log --since="$standup_since_iso" --date=iso-strict --pretty=format:'%H%x09%ad%x09%an%x09%ae%x09%s' > "$evidence_dir/$(basename "$dir")-commits.tsv"
  git -C "$dir" log --since="$standup_since_iso" --stat --date=iso-strict > "$evidence_dir/$(basename "$dir")-stat.log"
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
  timeout "$code_timeout" "${codex_cmd[@]}" | tee "$codex_events_file"
else
  "${codex_cmd[@]}" | tee "$codex_events_file"
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

remote_command=""
add_export() {
  local name="$1"
  local value="$2"
  remote_command+="export ${name}=$(shell_quote "$value")"$'\n'
}

add_export RUN_ID "$RUN_ID"
add_export LOC_BIN "$LOC_BIN"
add_export CODEX_MODEL "$CODEX_MODEL"
add_export CODEX_REASONING_EFFORT "$CODEX_REASONING_EFFORT"
add_export CODEX_EXEC_TIMEOUT_SECONDS "$CODEX_EXEC_TIMEOUT_SECONDS"
add_export SLACK_TYPES "$SLACK_TYPES"
add_export STANDUP_DATE "$STANDUP_DATE"
add_export STANDUP_SINCE_ISO "$STANDUP_SINCE_ISO"
add_export STANDUP_UNTIL_ISO "$STANDUP_UNTIL_ISO"
add_export NOTION_STANDUP_PARENT_PAGE_ID "$notion_parent_page_id"
add_export LINEAR_CONNECTION_ID "${LINEAR_CONNECTION_ID:-}"
add_export SLACK_CONNECTION_ID "${SLACK_CONNECTION_ID:-}"
add_export NOTION_CONNECTION_ID "${NOTION_CONNECTION_ID:-}"
add_export LOCALITY_REPO_DIR "${LOCALITY_REPO_DIR:-}"
add_export LOCALITY_INTERNAL_REPO_DIR "${LOCALITY_INTERNAL_REPO_DIR:-}"
add_export STANDUP_REMOTE_RUN_ROOT "${STANDUP_REMOTE_RUN_ROOT:-}"
add_export PROMPT_B64 "$prompt_b64"
add_export WORKER_B64 "$worker_b64"
remote_command+='printf %s "$WORKER_B64" | base64 -d | bash'

exec amika sandbox ssh "${amika_flags[@]}" "$sandbox" "$remote_command"
