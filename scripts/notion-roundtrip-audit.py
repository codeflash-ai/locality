#!/usr/bin/env python3
"""Live AFS/Notion round-trip audit.

This script is intentionally conservative. By default it runs read-only checks.
Pass --write to append a unique paragraph, push it through AFS, verify it with
the Notion API, restore the exact local Markdown backup, push the restore, and
verify the normalized Notion snapshot matches the pre-test snapshot.
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import json
import os
import re
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


NOTION_VERSION = "2026-03-11"
SKIP_CHILD_TYPES = {"child_page", "child_database"}
VOLATILE_NOTION_KEYS = {"last_edited_time", "last_edited_by", "expiry_time", "request_id"}
RISKY_WRITE_BLOCK_TYPES = {
    "audio",
    "bookmark",
    "breadcrumb",
    "child_database",
    "child_page",
    "column",
    "column_list",
    "embed",
    "file",
    "image",
    "link_preview",
    "pdf",
    "synced_block",
    "table_of_contents",
    "template",
    "unsupported",
    "video",
}


@dataclass(frozen=True)
class Candidate:
    remote_id: str
    title: str
    path: str


class AuditFailure(Exception):
    pass


class NotionClient:
    def __init__(self, token: str, min_interval: float, retries: int) -> None:
        self.token = token
        self.min_interval = min_interval
        self.retries = retries
        self.last_request = 0.0

    def request(self, method: str, path: str, body: Any | None = None) -> Any:
        url = "https://api.notion.com" + path
        data = None
        headers = {
            "Authorization": f"Bearer {self.token}",
            "Notion-Version": NOTION_VERSION,
        }
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"

        for attempt in range(self.retries + 1):
            self._pace()
            request = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    return json.loads(response.read().decode("utf-8"))
            except urllib.error.HTTPError as error:
                text = error.read().decode("utf-8", errors="replace")
                if error.code in {429, 500, 502, 503, 504} and attempt < self.retries:
                    retry_after = error.headers.get("Retry-After")
                    delay = float(retry_after) if retry_after and retry_after.isdigit() else min(2**attempt, 16)
                    time.sleep(delay)
                    continue
                raise AuditFailure(f"notion {method} {path} failed HTTP {error.code}: {text}") from error
            except (urllib.error.URLError, TimeoutError, socket.timeout) as error:
                if attempt < self.retries:
                    time.sleep(min(2**attempt, 16))
                    continue
                raise AuditFailure(f"notion {method} {path} failed: {error}") from error

        raise AuditFailure(f"notion {method} {path} exhausted retries")

    def _pace(self) -> None:
        if self.min_interval <= 0:
            return
        elapsed = time.monotonic() - self.last_request
        if elapsed < self.min_interval:
            time.sleep(self.min_interval - elapsed)
        self.last_request = time.monotonic()


def main() -> int:
    args = parse_args()
    state_root = Path(args.state_root).expanduser()
    mount_root = Path(args.mount_root).expanduser()
    run_root = resolve_run_root(args)
    run_root.mkdir(parents=True, exist_ok=True)
    log_path = run_root / "events.jsonl"

    def log(event: str, **fields: Any) -> None:
        record = {"ts": datetime.now(timezone.utc).isoformat(), "event": event, **fields}
        with log_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
        page = fields.get("path", "")
        detail = fields.get("status") or fields.get("message") or ""
        progress = ""
        if "index" in fields and "total" in fields:
            progress = f"{fields['index']}/{fields['total']}"
        print(" ".join(part for part in [record["ts"], event, progress, page, detail] if part), flush=True)

    afs_bin = Path(args.afs_bin).expanduser()
    token = notion_token(state_root, args.mount_id, args.secret_ref)
    notion = NotionClient(token, args.notion_min_interval_ms / 1000.0, args.notion_retries)
    candidates = load_candidates(state_root, args.mount_id)
    candidates = filter_candidates(candidates, args)
    write_manifest(run_root, args, candidates)
    log("run_start", pages=len(candidates), mode="write" if args.write else "read_only")

    failures = 0
    skipped = 0
    tested = 0
    for index, candidate in enumerate(candidates, start=1):
        page_dir = run_root / f"{index:04d}-{safe_name(candidate.path)}"
        page_dir.mkdir(parents=True, exist_ok=True)
        result_path = page_dir / "result.json"
        if args.resume and result_path.exists():
            previous = json.loads(result_path.read_text(encoding="utf-8"))
            if previous.get("ok") and (previous.get("restored", False) or previous.get("mode") == "read_only"):
                skipped += 1
                log("page_skip_completed", path=candidate.path, index=index, total=len(candidates))
                continue

        try:
            tested += 1
            result = run_page(
                args,
                afs_bin,
                mount_root,
                state_root,
                notion,
                candidate,
                page_dir,
                log,
                index,
                len(candidates),
            )
            if result == "skipped":
                skipped += 1
        except Exception as error:  # noqa: BLE001 - audit must capture exact unexpected failures.
            failures += 1
            write_json(result_path, {
                "path": candidate.path,
                "remote_id": candidate.remote_id,
                "ok": False,
                "restored": False,
                "error": str(error),
            })
            log("page_failed", path=candidate.path, status="failed", message=str(error))
            if args.stop_on_failure:
                break

    final_status = run_cmd([str(afs_bin), "status", str(mount_root), "--json"], run_root / "final-status.json", check=False)
    final_status_problems = final_status_problem_counts(run_root / "final-status.json")
    result_counts = collect_result_counts(run_root)
    summary = {
        "ok": failures == 0 and final_status.returncode == 0 and not any(final_status_problems.values()),
        "tested_this_invocation": tested,
        "skipped_this_invocation": skipped,
        "failures": failures,
        "candidate_count": len(candidates),
        "results": result_counts,
        "run_root": str(run_root),
        "final_status_exit": final_status.returncode,
        "final_status_problems": final_status_problems,
    }
    write_json(run_root / "summary.json", summary)
    log("run_complete", status="ok" if summary["ok"] else "failed", failures=failures, skipped=skipped)
    return 0 if summary["ok"] else 1


def run_page(
    args: argparse.Namespace,
    afs_bin: Path,
    mount_root: Path,
    state_root: Path,
    notion: NotionClient,
    candidate: Candidate,
    page_dir: Path,
    log,
    index: int,
    total: int,
) -> str:
    mounted_path = mount_root / candidate.path
    content_path = virtual_content_path(state_root, args.mount_id, candidate.path)
    marker = f"AFS roundtrip audit marker {int(time.time())}-{os.getpid()}-{hashlib.sha1(candidate.remote_id.encode()).hexdigest()[:8]}"

    log("page_start", path=candidate.path, remote_id=candidate.remote_id, index=index, total=total)
    run_cmd(
        [str(afs_bin), "status", str(mounted_path), "--json"],
        page_dir / "status-before.json",
        retries=args.command_retries,
    )
    run_cmd(
        [str(afs_bin), "pull", str(mounted_path), "--json"],
        page_dir / "pull-before.json",
        retries=args.command_retries,
    )
    if not content_path.exists():
        raise AuditFailure(f"hydrated content cache missing: {content_path}")

    original = content_path.read_bytes()
    (page_dir / "page-before.md").write_bytes(original)
    copy_media_assets(original.decode("utf-8", errors="replace"), content_path, page_dir / "media-before")

    before_snapshot = notion_snapshot(notion, candidate.remote_id)
    write_json(page_dir / "notion-before.json", before_snapshot)
    write_json(page_dir / "notion-before.normalized.json", normalize_notion(before_snapshot))
    write_classification = classify_write_risk(
        before_snapshot,
        original.decode("utf-8", errors="replace"),
        args.max_write_blocks,
    )
    write_json(page_dir / "classification.json", write_classification)

    run_cmd(
        [str(afs_bin), "inspect", str(mounted_path), "--json"],
        page_dir / "inspect-before.json",
        retries=args.command_retries,
    )
    diff_before = run_cmd(
        [str(afs_bin), "diff", str(mounted_path), "--json"],
        page_dir / "diff-before.json",
        retries=args.command_retries,
    )
    diff_before_json = load_json_output(page_dir / "diff-before.json")
    if diff_before.returncode != 0 or not diff_is_noop(diff_before_json):
        raise AuditFailure("pre-test diff is not clean; leaving page untouched")

    if not args.write:
        write_json(page_dir / "result.json", {
            "path": candidate.path,
            "remote_id": candidate.remote_id,
            "ok": True,
            "mode": "read_only",
            "restored": True,
        })
        log("page_read_only_ok", path=candidate.path, status="ok")
        return "ok"

    if args.write_policy == "safe" and write_classification["risk_reasons"]:
        write_json(page_dir / "result.json", {
            "path": candidate.path,
            "remote_id": candidate.remote_id,
            "ok": True,
            "mode": "write_skipped",
            "restored": True,
            "risk_reasons": write_classification["risk_reasons"],
        })
        log(
            "page_write_skipped",
            path=candidate.path,
            status="read_only_ok",
            message="; ".join(write_classification["risk_reasons"]),
        )
        return "skipped"

    remote_changed = False
    try:
        append_marker(content_path, marker)
        run_cmd(
            [str(afs_bin), "diff", str(mounted_path), "--json"],
            page_dir / "diff-edited.json",
            retries=args.command_retries,
        )
        edited_diff = load_json_output(page_dir / "diff-edited.json")
        assert_single_append(edited_diff, marker)

        push_edit = run_cmd(
            [str(afs_bin), "push", str(mounted_path), "-y", "--json"],
            page_dir / "push-edit.json",
            check=False,
        )
        if push_edit.returncode != 0:
            push_edit_json = load_json_output(page_dir / "push-edit.json")
            if stale_remote_push_failure(push_edit_json):
                content_path.write_bytes(original)
                clear_empty_failed_journal(afs_bin, push_edit_json, page_dir)
                run_cmd(
                    [str(afs_bin), "pull", str(mounted_path), "--json"],
                    page_dir / "pull-after-stale-push.json",
                    retries=args.command_retries,
                )
                run_cmd(
                    [str(afs_bin), "diff", str(mounted_path), "--json"],
                    page_dir / "diff-after-stale-push.json",
                    retries=args.command_retries,
                )
                stale_diff = load_json_output(page_dir / "diff-after-stale-push.json")
                if not diff_is_noop(stale_diff):
                    raise AuditFailure("stale push recovery left a non-clean local diff")

                reason = "remote changed before push apply; refreshed and left page unchanged"
                write_json(page_dir / "result.json", {
                    "path": candidate.path,
                    "remote_id": candidate.remote_id,
                    "ok": True,
                    "mode": "write_skipped",
                    "restored": True,
                    "risk_reasons": [reason],
                })
                log("page_write_skipped", path=candidate.path, status="stale_remote", message=reason)
                return "skipped"

            raise AuditFailure(command_failure_message(
                [str(afs_bin), "push", str(mounted_path), "-y", "--json"],
                page_dir / "push-edit.json",
                push_edit,
            ))
        remote_changed = True
        after_edit = notion_snapshot(notion, candidate.remote_id)
        write_json(page_dir / "notion-after-edit.json", after_edit)
        if not snapshot_contains_text(after_edit, marker):
            raise AuditFailure("Notion API snapshot did not contain pushed marker")

        content_path.write_bytes(original)
        run_cmd(
            [str(afs_bin), "diff", str(mounted_path), "--json"],
            page_dir / "diff-restore.json",
            retries=args.command_retries,
        )
        restore_diff = load_json_output(page_dir / "diff-restore.json")
        assert_restore_diff(restore_diff, marker)
        run_cmd([str(afs_bin), "push", str(mounted_path), "-y", "--json"], page_dir / "push-restore.json")
        remote_changed = False

        after_restore = notion_snapshot(notion, candidate.remote_id)
        write_json(page_dir / "notion-after-restore.json", after_restore)
        write_json(page_dir / "notion-after-restore.normalized.json", normalize_notion(after_restore))
        if normalize_notion(before_snapshot) != normalize_notion(after_restore):
            raise AuditFailure("normalized Notion snapshot differs after restore")

        run_cmd(
            [str(afs_bin), "pull", str(mounted_path), "--json"],
            page_dir / "pull-after-restore.json",
            retries=args.command_retries,
        )
        run_cmd(
            [str(afs_bin), "diff", str(mounted_path), "--json"],
            page_dir / "diff-final.json",
            retries=args.command_retries,
        )
        final_diff = load_json_output(page_dir / "diff-final.json")
        if not diff_is_noop(final_diff):
            raise AuditFailure("final diff is not clean after restore")

        write_json(page_dir / "result.json", {
            "path": candidate.path,
            "remote_id": candidate.remote_id,
            "ok": True,
            "mode": "write",
            "restored": True,
            "marker": marker,
        })
        log("page_write_ok", path=candidate.path, status="restored")
        return "ok"
    except Exception:
        content_path.write_bytes(original)
        if remote_changed:
            run_cmd([str(afs_bin), "push", str(mounted_path), "-y", "--json"], page_dir / "push-emergency-restore.json", check=False)
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--afs-bin", default="target/release/afs")
    parser.add_argument("--state-root", default="~/.afs")
    parser.add_argument("--mount-root", default="~/Library/CloudStorage/AFS-AFS/notion")
    parser.add_argument("--mount-id", default="notion-main")
    parser.add_argument("--output", default="target/notion-roundtrip-audit")
    parser.add_argument("--run-root")
    parser.add_argument("--secret-ref")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--match", default="")
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--write-policy", choices=["safe", "all"], default="safe")
    parser.add_argument("--max-write-blocks", type=int, default=200)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--stop-on-failure", dest="stop_on_failure", action="store_true", default=True)
    parser.add_argument("--continue-on-failure", dest="stop_on_failure", action="store_false")
    parser.add_argument("--notion-min-interval-ms", type=int, default=500)
    parser.add_argument("--notion-retries", type=int, default=6)
    parser.add_argument("--command-retries", type=int, default=3)
    return parser.parse_args()


def resolve_run_root(args: argparse.Namespace) -> Path:
    if args.run_root:
        return Path(args.run_root).expanduser()

    output = Path(args.output).expanduser()
    if args.resume:
        runs = [path for path in output.iterdir() if path.is_dir()] if output.exists() else []
        if runs:
            return sorted(runs)[-1]

    return output / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def load_candidates(state_root: Path, mount_id: str) -> list[Candidate]:
    db = sqlite3.connect(state_root / "state.sqlite3")
    rows = db.execute(
        """
        select remote_id, title, path
        from entities
        where mount_id = ?
          and kind_json = '"page"'
          and path like '%page.md'
        order by path
        """,
        (mount_id,),
    ).fetchall()
    return [Candidate(*row) for row in rows]


def filter_candidates(candidates: list[Candidate], args: argparse.Namespace) -> list[Candidate]:
    if args.match:
        candidates = [candidate for candidate in candidates if args.match in candidate.path or args.match in candidate.title]
    if args.offset:
        candidates = candidates[args.offset :]
    if args.limit is not None:
        candidates = candidates[: args.limit]
    return candidates


def notion_token(state_root: Path, mount_id: str, secret_ref: str | None) -> str:
    if os.environ.get("NOTION_TOKEN"):
        return os.environ["NOTION_TOKEN"]
    if secret_ref is None:
        db = sqlite3.connect(state_root / "state.sqlite3")
        row = db.execute(
            """
            select c.secret_ref
            from mounts m
            join connections c on c.connection_id = m.connection_id
            where m.mount_id = ?
            """,
            (mount_id,),
        ).fetchone()
        secret_ref = row[0] if row else "connection:notion-default"
    output = subprocess.check_output(
        ["security", "find-generic-password", "-a", secret_ref, "-s", "afs", "-w"],
        text=True,
    ).strip()
    try:
        return json.loads(output)["access_token"]
    except json.JSONDecodeError:
        return output


def notion_snapshot(notion: NotionClient, page_id: str) -> dict[str, Any]:
    return {
        "page": notion.request("GET", f"/v1/pages/{page_id}"),
        "blocks": fetch_children(notion, page_id),
    }


def fetch_children(notion: NotionClient, block_id: str) -> list[dict[str, Any]]:
    children = []
    cursor = None
    while True:
        query = "?page_size=100"
        if cursor:
            query += "&start_cursor=" + urllib.parse.quote(cursor)
        page = notion.request("GET", f"/v1/blocks/{block_id}/children{query}")
        for block in page.get("results", []):
            block = dict(block)
            block_type = block.get("type")
            if block.get("has_children") and block_type not in SKIP_CHILD_TYPES:
                block["children"] = fetch_children(notion, block["id"])
            children.append(block)
        if not page.get("has_more"):
            return children
        cursor = page.get("next_cursor")


def normalize_notion(value: Any) -> Any:
    if isinstance(value, dict):
        normalized = {}
        for key, child in value.items():
            if key in VOLATILE_NOTION_KEYS:
                continue
            if key == "url" and (value.get("type") == "file" or "expiry_time" in value):
                continue
            normalized[key] = normalize_notion(child)
        return normalized
    if isinstance(value, list):
        return [normalize_notion(item) for item in value]
    return value


def snapshot_contains_text(value: Any, needle: str) -> bool:
    if isinstance(value, dict):
        if value.get("plain_text") == needle or value.get("content") == needle:
            return True
        return any(snapshot_contains_text(child, needle) for child in value.values())
    if isinstance(value, list):
        return any(snapshot_contains_text(child, needle) for child in value)
    return False


def stale_remote_push_failure(push_output: dict[str, Any]) -> bool:
    message = str(push_output.get("message") or "")
    return (
        "changed since the Synced Tree shadow" in message
        and int(push_output.get("apply_effect_count") or 0) == 0
        and not push_output.get("changed_remote_ids")
    )


def clear_empty_failed_journal(afs_bin: Path, push_output: dict[str, Any], page_dir: Path) -> None:
    if push_output.get("journal_status") != "failed":
        return
    if int(push_output.get("apply_effect_count") or 0) != 0:
        return
    push_id = push_output.get("push_id")
    if not push_id:
        return
    run_cmd(
        [str(afs_bin), "undo", str(push_id), "--json"],
        page_dir / "undo-empty-failed-push.json",
        check=False,
    )


def classify_write_risk(
    snapshot: dict[str, Any],
    markdown: str,
    max_write_blocks: int,
) -> dict[str, Any]:
    block_types = sorted(block_type_counts(snapshot).items())
    total_blocks = sum(count for _, count in block_types)
    risky_types = [
        {"type": block_type, "count": count}
        for block_type, count in block_types
        if block_type in RISKY_WRITE_BLOCK_TYPES
    ]
    risk_reasons = []
    if risky_types:
        risk_reasons.append(
            "risky block types: "
            + ", ".join(f"{item['type']}={item['count']}" for item in risky_types)
        )
    if total_blocks > max_write_blocks:
        risk_reasons.append(f"large page: {total_blocks} blocks exceeds --max-write-blocks={max_write_blocks}")
    if ".afs/media/" in markdown:
        risk_reasons.append("page references local media assets")

    return {
        "block_count": total_blocks,
        "block_types": [{"type": block_type, "count": count} for block_type, count in block_types],
        "risk_reasons": risk_reasons,
        "write_policy": "read_only_if_risky",
    }


def block_type_counts(snapshot: dict[str, Any]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for block in snapshot.get("blocks", []):
        collect_block_type_counts(block, counts)
    return counts


def collect_block_type_counts(block: dict[str, Any], counts: dict[str, int]) -> None:
    block_type = str(block.get("type", "unknown"))
    counts[block_type] = counts.get(block_type, 0) + 1
    for child in block.get("children", []):
        if isinstance(child, dict):
            collect_block_type_counts(child, counts)


def run_cmd(
    argv: list[str],
    output_path: Path,
    check: bool = True,
    retries: int = 0,
) -> subprocess.CompletedProcess[str]:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    retries = max(0, retries)
    for attempt in range(retries + 1):
        result = subprocess.run(
            argv,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=180,
        )
        output_path.write_text(result.stdout, encoding="utf-8")
        stderr_path = output_path.with_suffix(output_path.suffix + ".stderr")
        if result.stderr:
            stderr_path.write_text(result.stderr, encoding="utf-8")
        elif stderr_path.exists():
            stderr_path.unlink()

        if result.returncode == 0:
            return result

        if attempt < retries and transient_command_failure(result):
            preserve_attempt_output(output_path, attempt + 1)
            time.sleep(min(2**attempt, 8))
            continue

        if check:
            raise AuditFailure(command_failure_message(argv, output_path, result))
        return result

    raise AssertionError("retry loop should return or raise")


def preserve_attempt_output(output_path: Path, attempt: int) -> None:
    attempt_path = output_path.with_name(f"{output_path.name}.attempt{attempt}")
    shutil.copy2(output_path, attempt_path)
    stderr_path = output_path.with_suffix(output_path.suffix + ".stderr")
    if stderr_path.exists():
        shutil.copy2(stderr_path, attempt_path.with_suffix(attempt_path.suffix + ".stderr"))


def transient_command_failure(result: subprocess.CompletedProcess[str]) -> bool:
    text = f"{result.stdout}\n{result.stderr}".lower()
    transient_markers = [
        "429",
        "502",
        "503",
        "504",
        "bad gateway",
        "gateway timeout",
        "rate limit",
        "rate_limited",
        "remote_fetch_failed",
        "notion request failed",
        "error sending request",
        "temporarily unavailable",
        "timed out",
        "connection reset",
    ]
    return any(marker in text for marker in transient_markers)


def command_failure_message(
    argv: list[str],
    output_path: Path,
    result: subprocess.CompletedProcess[str],
) -> str:
    parts = [
        f"command failed ({result.returncode}): {' '.join(argv)}",
        f"stdout={output_path}",
    ]
    stderr_path = output_path.with_suffix(output_path.suffix + ".stderr")
    if result.stderr:
        parts.append(f"stderr={stderr_path}")
    stdout_tail = tail_text(result.stdout)
    stderr_tail = tail_text(result.stderr)
    if stdout_tail:
        parts.append(f"stdout_tail={stdout_tail!r}")
    if stderr_tail:
        parts.append(f"stderr_tail={stderr_tail!r}")
    return "; ".join(parts)


def tail_text(value: str, max_chars: int = 600) -> str:
    value = value.strip()
    if len(value) <= max_chars:
        return value
    return "..." + value[-max_chars:]


def load_json_output(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def diff_is_noop(diff: dict[str, Any]) -> bool:
    summary = diff.get("plan", {}).get("summary", {})
    return diff.get("ok") is True and all(value == 0 for value in summary.values())


def assert_single_append(diff: dict[str, Any], marker: str) -> None:
    operations = diff.get("plan", {}).get("operations", [])
    if len(operations) != 1:
        raise AuditFailure(f"expected one append operation, got {len(operations)}")
    operation = operations[0]
    text = json.dumps(operation, sort_keys=True)
    if "append" not in text.lower() or marker not in text:
        raise AuditFailure(f"unexpected edit diff operation: {text}")


def assert_restore_diff(diff: dict[str, Any], marker: str) -> None:
    operations = diff.get("plan", {}).get("operations", [])
    text = json.dumps(operations, sort_keys=True)
    if marker in text:
        raise AuditFailure("restore diff still contains marker text")
    if not operations:
        raise AuditFailure("restore diff had no operations after remote edit")


def append_marker(path: Path, marker: str) -> None:
    contents = path.read_text(encoding="utf-8")
    path.write_text(contents + "\n\n" + marker + "\n", encoding="utf-8")


def copy_media_assets(markdown: str, content_path: Path, output_dir: Path) -> None:
    refs = set(re.findall(r"\]\(([^)]+\.afs/media/[^)]+)\)", markdown))
    if not refs:
        return
    output_dir.mkdir(parents=True, exist_ok=True)
    for href in refs:
        source = (content_path.parent / href).resolve()
        if source.exists() and source.is_file():
            target = output_dir / safe_name(str(source))
            shutil.copy2(source, target)


def virtual_content_path(state_root: Path, mount_id: str, relative_path: str) -> Path:
    override = os.environ.get("AFS_VIRTUAL_FS_CONTENT_ROOT")
    if override:
        base = Path(override).expanduser()
    else:
        default_state = Path.home() / ".afs"
        group_base = Path.home() / "Library/Group Containers/group.ai.codeflash.afs/content"
        base = group_base if state_root == default_state and group_base.exists() else state_root / "content"
    return base / mount_id / "files" / relative_path


def write_manifest(run_root: Path, args: argparse.Namespace, candidates: list[Candidate]) -> None:
    with (run_root / "candidates.csv").open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=["remote_id", "title", "path"])
        writer.writeheader()
        for candidate in candidates:
            writer.writerow(candidate.__dict__)
    write_json(run_root / "args.json", vars(args))


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def collect_result_counts(run_root: Path) -> dict[str, Any]:
    counts: dict[str, Any] = {
        "total_result_files": 0,
        "ok": 0,
        "failed": 0,
        "restored": 0,
        "modes": {},
    }
    for result_path in sorted(run_root.glob("*/result.json")):
        try:
            result = json.loads(result_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        counts["total_result_files"] += 1
        if result.get("ok"):
            counts["ok"] += 1
        else:
            counts["failed"] += 1
        if result.get("restored"):
            counts["restored"] += 1
        mode = str(result.get("mode", "failed" if not result.get("ok") else "unknown"))
        counts["modes"][mode] = counts["modes"].get(mode, 0) + 1
    return counts


def final_status_problem_counts(status_path: Path) -> dict[str, int]:
    try:
        status = load_json_output(status_path)
    except (OSError, json.JSONDecodeError):
        return {"unreadable_status": 1}
    summary = status.get("summary", {})
    problem_keys = [
        "dirty",
        "conflicted",
        "missing",
        "error",
        "pending_journals",
        "failed_journals",
        "pending_local_changes",
        "review_needed",
        "sync_conflicted",
    ]
    return {key: int(summary.get(key) or 0) for key in problem_keys}


def safe_name(value: str) -> str:
    safe = re.sub(r"[^A-Za-z0-9._-]+", "-", value).strip("-")
    digest = hashlib.sha1(value.encode("utf-8")).hexdigest()[:8]
    return f"{safe[:80]}-{digest}"


if __name__ == "__main__":
    sys.exit(main())
