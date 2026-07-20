#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: run-launch-readiness-benchmark.sh [--push] [--compare-mcp]

Runs the Locality vs Notion MCP launch-readiness benchmark:
  1. collect git metadata for the selected window
  2. hydrate target and context through Locality
  3. inventory/search hydrated Locality context
  4. run a Locality-backed Codex agent with timed JSON events
  5. write mounted page.md and run loc diff
  6. optionally run a Notion-MCP-only Codex agent with timed JSON events
  7. write run summary artifacts

Important environment:
  REPO_DIR                 Repository path. Default: /home/amika/workspace/locality
  LOC_BIN                  loc binary. Default: $REPO_DIR/target/debug/loc
  TARGET_URL               Notion page URL for benchmark output parent.
  CONTEXT_URLS             Newline-delimited Notion URLs to hydrate as directories.
  CODEX_MODEL              Model passed to codex exec. Default: gpt-5.6-sol
  CODEX_REASONING_EFFORT   Codex reasoning effort. Default: low
  SINCE                    Git window. Default: 24 hours ago
  BASE_REF                 Git ref. Default: origin/main
  OUT_DIR                  Run artifact directory.

By default this is a dry run. Pass --push to publish the mounted report page.
EOF
}

PUSH=0
COMPARE_MCP=0
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --compare-mcp) COMPARE_MCP=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${REPO_DIR:-/home/amika/workspace/locality}"
LOC_BIN="${LOC_BIN:-$REPO_DIR/target/debug/loc}"
TARGET_URL="${TARGET_URL:-https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef}"
CONTEXT_URLS="${CONTEXT_URLS:-https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba}"
CONTEXT_PULL_PATHS="${CONTEXT_PULL_PATHS:-}"
CONTEXT_SEARCH_QUERY="${CONTEXT_SEARCH_QUERY:-benchmark|launch readiness|safe diff|push|review|Live Mode|File Provider|Windows Cloud Files|distribution|Homebrew|install|connector|standup|blocker|risk}"
SINCE="${SINCE:-24 hours ago}"
BASE_REF="${BASE_REF:-origin/main}"
REPORT_TZ="${REPORT_TZ:-Asia/Kolkata}"
REPORT_DATE="${REPORT_DATE:-$(TZ="$REPORT_TZ" date +%F)}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
REPORT_TITLE="${REPORT_TITLE:-Launch Readiness Benchmark $RUN_ID}"
OUT_DIR="${OUT_DIR:-$REPO_DIR/experiment/runs/$RUN_ID}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
METRICS_TSV="$OUT_DIR/metrics.tsv"
SUMMARY_JSON="$OUT_DIR/summary.json"
CONTEXT_PATHS_FILE="$OUT_DIR/locality-context-paths.txt"
CONTEXT_INVENTORY="$OUT_DIR/locality-context-inventory.txt"
CONTEXT_SEARCH_RESULTS="$OUT_DIR/locality-context-search.txt"

mkdir -p "$OUT_DIR"

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

record_metric() {
  local strategy="$1"
  local phase="$2"
  local start_ms="$3"
  local end_ms="$4"
  local status="$5"
  local detail="${6:-}"
  local duration_ms=$((end_ms - start_ms))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$strategy" "$phase" "$start_ms" "$end_ms" "$duration_ms" "$status" "$detail" >> "$METRICS_TSV"
}

phase_start() {
  PHASE_STARTED_AT="$(now_ms)"
}

phase_end() {
  local strategy="$1"
  local phase="$2"
  local status="${3:-ok}"
  local detail="${4:-}"
  local ended_at
  ended_at="$(now_ms)"
  record_metric "$strategy" "$phase" "$PHASE_STARTED_AT" "$ended_at" "$status" "$detail"
}

run_codex_agent() {
  local strategy="$1"
  local prompt_file="$2"
  local report_file="$3"
  local final_file="$4"
  shift 4
  local add_dirs=("$@")
  local events_file="$OUT_DIR/$strategy-codex-events.jsonl"
  local err_file="$OUT_DIR/$strategy-codex.err"
  local out_file="$OUT_DIR/$strategy-codex.out"
  local summary_file="$OUT_DIR/$strategy-codex-summary.json"
  local events_tsv="$OUT_DIR/$strategy-codex-events.tsv"
  local prompt
  prompt="$(cat "$prompt_file")"

  local cmd=(
    codex exec
    --json
    --model "$CODEX_MODEL"
    -c "model_reasoning_effort=\"$CODEX_REASONING_EFFORT\""
    --dangerously-bypass-approvals-and-sandbox
    -C "$REPO_DIR"
    --add-dir "$OUT_DIR"
    --output-last-message "$final_file"
  )
  local dir
  for dir in "${add_dirs[@]}"; do
    cmd+=(--add-dir "$dir")
  done
  cmd+=("$prompt")

  set +e
  set -o pipefail
  "${cmd[@]}" < /dev/null 2> "$err_file" | python3 "$SCRIPT_DIR/scripts/timestamp-jsonl.py" > "$events_file"
  local pipe_status=("${PIPESTATUS[@]}")
  local rc="${pipe_status[0]}"
  set +o pipefail
  set -e
  : > "$out_file"

  python3 "$SCRIPT_DIR/scripts/summarize-codex-events.py" "$events_file" "$summary_file" "$events_tsv"
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi
  test -s "$report_file"
}

echo -e "strategy\tphase\tstart_ms\tend_ms\tduration_ms\tstatus\tdetail" > "$METRICS_TSV"

phase_start
test -d "$REPO_DIR"
test -x "$LOC_BIN"
git -C "$REPO_DIR" rev-parse --git-dir >/dev/null
phase_end "locality" "validate_environment" "ok" "repo=$REPO_DIR; model=$CODEX_MODEL; effort=$CODEX_REASONING_EFFORT"

phase_start
git -C "$REPO_DIR" fetch --quiet origin || true
phase_end "locality" "git_fetch" "ok" "ref=$BASE_REF"

phase_start
python3 - "$REPO_DIR" "$BASE_REF" "$SINCE" "$OUT_DIR/git-data.json" <<'PY'
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

repo, ref, since, out_path = sys.argv[1:5]

def git(*args):
    return subprocess.check_output(["git", "-C", repo, *args], text=True)

fmt = "%H%x09%h%x09%an%x09%ae%x09%ad%x09%s"
raw = git("log", ref, f"--since={since}", "--date=iso-strict", f"--pretty=format:{fmt}")

commits = []
for line in raw.splitlines():
    if not line.strip():
        continue
    full, short, author, email, date, subject = line.split("\t", 5)
    files = [f for f in git("diff-tree", "--no-commit-id", "--name-only", "-r", full).splitlines() if f.strip()]
    commits.append({"full": full, "short": short, "author": author, "email": email, "date": date, "subject": subject, "files": files})

by_email = defaultdict(list)
for commit in commits:
    by_email[commit["email"]].append(commit)

people = []
for email, items in sorted(by_email.items(), key=lambda kv: (-len(kv[1]), kv[0])):
    files = sorted({f for item in items for f in item["files"]})
    top_dirs = defaultdict(int)
    for path in files:
        top_dirs[path.split("/", 1)[0]] += 1
    names = [item["author"] for item in items if item["author"]]
    display = sorted(names, key=lambda n: (-(" " in n), -len(n), n))[0] if names else "Unknown"
    people.append({"name": display, "email": email, "commit_count": len(items), "commits": items, "top_dirs": sorted(top_dirs.items(), key=lambda kv: (-kv[1], kv[0]))[:8], "file_count": len(files)})

Path(out_path).write_text(json.dumps({"repo": repo, "ref": ref, "since": since, "commit_count": len(commits), "people": people}, indent=2) + "\n")
PY
COMMIT_COUNT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["commit_count"])' "$OUT_DIR/git-data.json")"
phase_end "locality" "git_collect" "ok" "commits=$COMMIT_COUNT"

phase_start
PAGE_PATH="$("$LOC_BIN" locate "$TARGET_URL")"
set +e
"$LOC_BIN" pull "$PAGE_PATH" > "$OUT_DIR/loc-pull.out" 2> "$OUT_DIR/loc-pull.err"
PULL_RC=$?
set -e
PULL_DETAIL="$(tr '\n' ' ' < "$OUT_DIR/loc-pull.out" | sed 's/[[:space:]]*$//')"
if [ "$PULL_RC" -eq 0 ]; then
  phase_end "locality" "notion_locate_and_prehydrate" "ok" "path=$PAGE_PATH; $PULL_DETAIL"
elif grep -qi "dirty" "$OUT_DIR/loc-pull.out" "$OUT_DIR/loc-pull.err"; then
  phase_end "locality" "notion_locate_and_prehydrate" "ok" "path=$PAGE_PATH; pull skipped dirty target"
else
  phase_end "locality" "notion_locate_and_prehydrate" "failed" "exit=$PULL_RC; path=$PAGE_PATH"
  cat "$OUT_DIR/loc-pull.out" >&2
  cat "$OUT_DIR/loc-pull.err" >&2
  exit "$PULL_RC"
fi

phase_start
: > "$CONTEXT_PATHS_FILE"
while IFS= read -r context_url; do
  context_url="${context_url#"${context_url%%[![:space:]]*}"}"
  context_url="${context_url%"${context_url##*[![:space:]]}"}"
  if [ -z "$context_url" ]; then
    continue
  fi
  context_page="$("$LOC_BIN" locate "$context_url")"
  printf '%s\n' "$(dirname "$context_page")" >> "$CONTEXT_PATHS_FILE"
done <<< "$CONTEXT_URLS"
while IFS= read -r context_path; do
  context_path="${context_path#"${context_path%%[![:space:]]*}"}"
  context_path="${context_path%"${context_path##*[![:space:]]}"}"
  if [ -n "$context_path" ]; then
    printf '%s\n' "$context_path" >> "$CONTEXT_PATHS_FILE"
  fi
done <<< "$CONTEXT_PULL_PATHS"
sort -u "$CONTEXT_PATHS_FILE" -o "$CONTEXT_PATHS_FILE"
hydrated_count=0
while IFS= read -r context_path; do
  if [ -z "$context_path" ]; then
    continue
  fi
  safe_name="$(printf '%s' "$context_path" | tr '/[:space:]' '__' | tr -cd '[:alnum:]_.-')"
  set +e
  "$LOC_BIN" pull "$context_path" --json > "$OUT_DIR/loc-context-pull-$safe_name.json" 2> "$OUT_DIR/loc-context-pull-$safe_name.err"
  pull_rc=$?
  set -e
  if [ "$pull_rc" -ne 0 ] && ! grep -qi "dirty" "$OUT_DIR/loc-context-pull-$safe_name.json" "$OUT_DIR/loc-context-pull-$safe_name.err"; then
    phase_end "locality" "notion_context_hydrate" "failed" "exit=$pull_rc; path=$context_path"
    cat "$OUT_DIR/loc-context-pull-$safe_name.json" >&2
    cat "$OUT_DIR/loc-context-pull-$safe_name.err" >&2
    exit "$pull_rc"
  fi
  hydrated_count=$((hydrated_count + 1))
done < "$CONTEXT_PATHS_FILE"
phase_end "locality" "notion_context_hydrate" "ok" "paths=$hydrated_count; list=$CONTEXT_PATHS_FILE"

phase_start
: > "$CONTEXT_INVENTORY"
: > "$CONTEXT_SEARCH_RESULTS"
while IFS= read -r context_path; do
  if [ -z "$context_path" ] || [ ! -d "$context_path" ]; then
    continue
  fi
  {
    echo "## $context_path"
    find "$context_path" -name page.md -type f | sort
    echo
  } >> "$CONTEXT_INVENTORY"
  rg -n --no-heading "$CONTEXT_SEARCH_QUERY" "$context_path" >> "$CONTEXT_SEARCH_RESULTS" 2>/dev/null || true
done < "$CONTEXT_PATHS_FILE"
inventory_count="$(grep -c '/page.md$' "$CONTEXT_INVENTORY" 2>/dev/null || true)"
search_hits="$(wc -l < "$CONTEXT_SEARCH_RESULTS" | tr -d ' ')"
phase_end "locality" "notion_context_search" "ok" "pages=$inventory_count; hits=$search_hits; query=$CONTEXT_SEARCH_QUERY"

phase_start
PARENT_DIR="$(dirname "$PAGE_PATH")"
REPORT_PAGE_PATH="$PARENT_DIR/$REPORT_TITLE/page.md"
if [ ! -f "$REPORT_PAGE_PATH" ]; then
  "$LOC_BIN" create page --title "$REPORT_TITLE" --parent "$PARENT_DIR" --json > "$OUT_DIR/loc-create-page.json"
  REPORT_PAGE_PATH="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["path"])' "$OUT_DIR/loc-create-page.json")"
fi
phase_end "locality" "prepare_report_target" "ok" "path=$REPORT_PAGE_PATH"

phase_start
export REPO_DIR LOC_BIN TARGET_URL PAGE_PATH REPORT_PAGE_PATH OUT_DIR METRICS_TSV SUMMARY_JSON
export CONTEXT_PATHS_FILE CONTEXT_INVENTORY CONTEXT_SEARCH_RESULTS CONTEXT_SEARCH_QUERY
locality_add_dirs=()
while IFS= read -r context_path; do
  if [ -n "$context_path" ]; then
    locality_add_dirs+=("$context_path")
  fi
done < "$CONTEXT_PATHS_FILE"
locality_add_dirs+=("$(dirname "$PAGE_PATH")")
set +e
run_codex_agent "locality" "$SCRIPT_DIR/prompts/locality-agent-prompt.md" "$OUT_DIR/report-body.md" "$OUT_DIR/locality-agent-final.md" "${locality_add_dirs[@]}"
agent_rc=$?
set -e
if [ "$agent_rc" -eq 0 ] && [ -s "$OUT_DIR/report-body.md" ]; then
  phase_end "locality" "codex_exec_wall_time" "ok" "report=$OUT_DIR/report-body.md"
else
  phase_end "locality" "codex_exec_wall_time" "failed" "exit=$agent_rc"
  cat "$OUT_DIR/locality-codex.err" >&2 || true
  exit "$agent_rc"
fi

phase_start
python3 - "$REPORT_PAGE_PATH" "$OUT_DIR/report-body.md" "$REPORT_TITLE" <<'PY'
import sys
from pathlib import Path

page = Path(sys.argv[1])
body = Path(sys.argv[2]).read_text()
title = sys.argv[3]
current = page.read_text() if page.exists() else ""
frontmatter = f'---\ntitle: "{title}"\n---\n'
if current.startswith("---\n"):
    end = current.find("\n---\n", 4)
    if end != -1:
        frontmatter = current[: end + len("\n---\n")]
page.write_text(frontmatter.rstrip() + "\n\n" + body)
PY
phase_end "locality" "write_mounted_page" "ok" "path=$REPORT_PAGE_PATH"

phase_start
"$LOC_BIN" diff "$REPORT_PAGE_PATH" > "$OUT_DIR/loc-diff.out"
DIFF_SUMMARY="$(sed -n '1p' "$OUT_DIR/loc-diff.out")"
phase_end "locality" "loc_diff" "ok" "$DIFF_SUMMARY"

if [ "$PUSH" -eq 1 ]; then
  phase_start
  "$LOC_BIN" push "$REPORT_PAGE_PATH" -y > "$OUT_DIR/loc-push.out"
  PUSH_SUMMARY="$(tr '\n' ' ' < "$OUT_DIR/loc-push.out" | sed 's/[[:space:]]*$//')"
  phase_end "locality" "loc_push" "ok" "$PUSH_SUMMARY"
else
  phase_start
  phase_end "locality" "loc_push" "skipped" "dry-run; pass --push to publish"
fi

if [ "$COMPARE_MCP" -eq 1 ]; then
  phase_start
  set +e
  run_codex_agent "notion-mcp" "$SCRIPT_DIR/prompts/notion-mcp-agent-prompt.md" "$OUT_DIR/notion-mcp-report-body.md" "$OUT_DIR/notion-mcp-agent-final.md"
  mcp_rc=$?
  set -e
  if [ "$mcp_rc" -eq 0 ] && [ -s "$OUT_DIR/notion-mcp-report-body.md" ]; then
    phase_end "notion_mcp" "codex_exec_wall_time" "ok" "report=$OUT_DIR/notion-mcp-report-body.md"
  else
    phase_end "notion_mcp" "codex_exec_wall_time" "failed" "exit=$mcp_rc"
  fi
fi

python3 - "$METRICS_TSV" "$SUMMARY_JSON" "$REPORT_PAGE_PATH" "$OUT_DIR" "$PUSH" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" <<'PY'
import csv
import json
import sys
from pathlib import Path

metrics_path, summary_path, page_path, out_dir, push, model, effort = sys.argv[1:8]
with open(metrics_path) as f:
    metrics = list(csv.DictReader(f, delimiter="\t"))

out = Path(out_dir)
agent_summaries = {}
for name in ("locality", "notion-mcp"):
    p = out / f"{name}-codex-summary.json"
    if p.exists():
        agent_summaries[name] = json.loads(p.read_text())

summary = {
    "ok": True,
    "model": model,
    "reasoning_effort": effort,
    "page_path": page_path,
    "out_dir": out_dir,
    "pushed": push == "1",
    "metrics": [
        {
            "strategy": row["strategy"],
            "phase": row["phase"],
            "duration_ms": int(row["duration_ms"]),
            "status": row["status"],
            "detail": row["detail"],
        }
        for row in metrics
    ],
    "agent_event_summaries": agent_summaries,
}
Path(summary_path).write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
PY
