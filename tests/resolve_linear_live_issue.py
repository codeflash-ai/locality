#!/usr/bin/env python3
import json
import os
import pathlib
import re
import sys


def fail(message):
    raise SystemExit(message)


def frontmatter_lines(path):
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"Linear issue page could not be read: {error}")
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        fail("Linear issue page.md did not start with frontmatter")
    frontmatter = []
    for line in lines[1:]:
        if line.strip() == "---":
            return frontmatter
        frontmatter.append(line)
    fail("Linear issue page.md frontmatter was not terminated")


def scalar_value(line, key):
    match = re.match(rf"^\s*{re.escape(key)}:\s*(.*?)\s*$", line)
    if not match:
        return None
    value = match.group(1).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        value = value[1:-1]
    return value


def block_key(line):
    match = re.match(r"^(\s*)([A-Za-z0-9_.-]+):\s*$", line)
    if not match:
        return None
    return (len(match.group(1)), match.group(2))


def verify_frontmatter(path, issue_id):
    has_linear_connector = False
    has_issue_id = False
    active_block = None
    active_indent = -1
    for line in frontmatter_lines(path):
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        if active_block is not None and indent <= active_indent:
            active_block = None
            active_indent = -1
        block = block_key(line)
        if block is not None:
            active_indent, active_block = block
            continue
        if active_block == "loc":
            has_linear_connector |= scalar_value(line, "connector") == "linear"
            has_issue_id |= scalar_value(line, "id") == issue_id
    if not has_linear_connector or not has_issue_id:
        fail("Linear issue page.md frontmatter identity did not match the search result")


def main():
    if len(sys.argv) != 5:
        fail(
            "usage: resolve_linear_live_issue.py "
            "<search-report> <mount-root> <mount-id> <issue-id>"
        )
    report_path = pathlib.Path(sys.argv[1])
    root = pathlib.Path(sys.argv[2])
    mount_id = sys.argv[3]
    issue_id = sys.argv[4]
    report = json.loads(report_path.read_text(encoding="utf-8"))
    if report.get("ok") is not True:
        fail("Linear issue search did not report ok=true")
    matches = [
        result
        for result in report.get("results", [])
        if result.get("mount_id") == mount_id
        and result.get("connector") == "linear"
        and result.get("kind") == "page"
        and result.get("remote_id") == issue_id
    ]
    if len(matches) != 1:
        fail(f"Linear issue search expected one exact remote-id match, got {len(matches)}")
    path = pathlib.Path(matches[0].get("absolute_path", ""))
    root_normalized = os.path.abspath(root)
    path_normalized = os.path.abspath(path)
    if os.path.commonpath((root_normalized, path_normalized)) != root_normalized:
        fail("Linear issue search returned a path outside the mount root")
    if path.name != "page.md":
        fail("Linear issue search did not return a page.md path")
    verify_frontmatter(path, issue_id)
    print(path)


if __name__ == "__main__":
    main()
