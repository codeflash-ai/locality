#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-linear-lookup-selftest.XXXXXX")"
trap 'rm -rf "$tmp_root"' EXIT

mount_root="$tmp_root/mount"
page_path="$mount_root/Teams/Engineering/Issues/Todo/ENG-1 Test/page.md"
report_path="$tmp_root/search.json"
mkdir -p "$(dirname "$page_path")"

cat >"$page_path" <<'MARKDOWN'
---
loc:
  id: issue-1
  type: page
  connector: linear
title: Test
---
Body
MARKDOWN

write_report() {
  local absolute_path="$1"
  local remote_id="${2:-issue-1}"
  python3 - "$report_path" "$absolute_path" "$remote_id" <<'PY'
import json
import pathlib
import sys

pathlib.Path(sys.argv[1]).write_text(
    json.dumps(
        {
            "ok": True,
            "results": [
                {
                    "mount_id": "linear-live",
                    "connector": "linear",
                    "kind": "page",
                    "remote_id": sys.argv[3],
                    "absolute_path": sys.argv[2],
                }
            ],
        }
    ),
    encoding="utf-8",
)
PY
}

write_report "$page_path"
resolved="$(python3 "$script_dir/resolve_linear_live_issue.py" \
  "$report_path" "$mount_root" linear-live issue-1)"
[[ "$resolved" == "$page_path" ]]

write_report "$tmp_root/outside/page.md"
if python3 "$script_dir/resolve_linear_live_issue.py" \
  "$report_path" "$mount_root" linear-live issue-1 >/dev/null 2>&1; then
  echo "resolver accepted a path outside the mount root" >&2
  exit 1
fi

write_report "$page_path" issue-2
if python3 "$script_dir/resolve_linear_live_issue.py" \
  "$report_path" "$mount_root" linear-live issue-1 >/dev/null 2>&1; then
  echo "resolver accepted a non-matching search result" >&2
  exit 1
fi

write_report "$page_path"
sed -i.bak 's/connector: linear/connector: slack/' "$page_path"
rm -f "$page_path.bak"
if python3 "$script_dir/resolve_linear_live_issue.py" \
  "$report_path" "$mount_root" linear-live issue-1 >/dev/null 2>&1; then
  echo "resolver accepted mismatched projected frontmatter" >&2
  exit 1
fi

echo "Linear live issue resolver self-test passed"
