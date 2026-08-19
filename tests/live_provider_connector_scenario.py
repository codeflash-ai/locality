#!/usr/bin/env python3
"""Real macOS File Provider / Windows Cloud Files connector scenario runner.

The platform wrappers prove installation and provider registration, then hand
the visible root to this runner. Provider content is always accessed through
that visible root; SQLite is used only for durable-id path resolution.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from typing import Any, Callable

import live_connector_matrix


MACOS_FILE_PROVIDER_DAEMON_ADDR = "127.0.0.1:38567"


class ScenarioError(RuntimeError):
    pass


def linear_restoration_values(issue_id: str, issue: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": issue_id,
        "description": issue.get("description"),
        "title": issue.get("title"),
    }


class Runner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.matrix = live_connector_matrix.load(args.matrix)
        live_connector_matrix.validate(self.matrix)
        self.scenario = self.matrix["connectors"][args.connector]
        self.connector = args.connector
        self.platform = args.projection
        expected = self.scenario["scenarios"][self.platform]
        expected_wrapper = {
            "macos-file-provider": "tests/live_macos_connector_file_provider.sh",
            "windows-cloud-files": "tests/windows_connector_cloud_files_live.ps1",
        }[self.platform]
        if expected != expected_wrapper:
            raise ScenarioError(f"matrix routes {self.connector}/{self.platform} to {expected}")

        credential_env = self.scenario["credential"]["environment"]
        self.credential = required_env(credential_env)
        self.oauth_refresh_marker = ""
        if self.scenario["credential"]["kind"] == "oauth" and os.environ.get("LOCALITY_LIVE_FORCE_OAUTH_REFRESH") == "1":
            try:
                credential_value = json.loads(self.credential)
            except json.JSONDecodeError as error:
                raise ScenarioError("refreshable OAuth credential was not JSON") from error
            old_token = credential_value.get("access_token", "")
            self.oauth_refresh_marker = hashlib.sha256(str(old_token).encode()).hexdigest()
            credential_value["acquired_at"] = 1
            credential_value["expires_at"] = 1
            self.credential = json.dumps(credential_value, separators=(",", ":"), sort_keys=True)
        self.fixtures = {
            name: os.environ.get(env_name, "")
            for name, env_name in self.scenario["fixtures"].items()
        }
        for name, value in self.fixtures.items():
            if not value and not name.endswith("types"):
                raise ScenarioError(f"missing fixture environment for {self.connector}: {name}")

        self.secret_values = [self.credential, *[v for v in self.fixtures.values() if v]]
        if self.scenario["credential"]["kind"] == "oauth":
            credential_value = json.loads(self.credential)
            for key in ("access_token", "refresh_token_handle", "account_id", "account_label"):
                value = credential_value.get(key)
                if isinstance(value, str) and value:
                    self.secret_values.append(value)
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix=f"locality-{self.platform}-{self.connector}-"))
        self.state = self.tmp / "state"
        self.root = pathlib.Path(args.provider_root).resolve()
        self.connection_id = f"{self.connector}-provider-live"
        self.mount_id = f"{self.connector}-provider-{os.getpid()}"
        self.mount = self.root / self.mount_id
        self.tcp_addr = (
            MACOS_FILE_PROVIDER_DAEMON_ADDR
            if self.platform == "macos-file-provider"
            else f"127.0.0.1:{free_port()}"
        )
        self.env = os.environ.copy()
        self.env.update(
            {
                "LOCALITY_STATE_DIR": str(self.state),
                "LOCALITY_DAEMON_TCP_ADDR": self.tcp_addr,
                "LOCALITY_CREDENTIAL_STORE": "file",
            }
        )
        if args.file_providerctl:
            self.env["LOCALITY_FILE_PROVIDERCTL"] = str(pathlib.Path(args.file_providerctl).resolve())
        for env_name in self._sensitive_environment_names():
            self.env.pop(env_name, None)
            os.environ.pop(env_name, None)
        self.daemon_started = False
        self.provider_started = False
        self.created_gmail_draft = ""
        self.linear_original: dict[str, Any] | None = None

    def _sensitive_environment_names(self) -> set[str]:
        names = {self.scenario["credential"]["environment"]}
        names.update(self.scenario["fixtures"].values())
        names.update({"NOTION_TOKEN", "NOTION_AT", "LINEAR_API_KEY", "GRANOLA_API_KEY"})
        return names

    def run(self) -> None:
        self.state.mkdir(parents=True)
        self.root.mkdir(parents=True, exist_ok=True)
        self._initialize_and_seed()
        self._mount()
        self._refresh_oauth_before_consumers()
        self._start_daemon()
        self._start_provider()
        self._wait(lambda: self.mount.is_dir(), "visible provider mount")
        self.command("pull", str(self.mount), "--json")
        self._refresh_provider_root()
        self._doctor()
        if self.connector == "gmail":
            self._gmail_scenario()
        elif self.connector == "linear":
            self._linear_scenario()
        elif self.connector == "slack":
            self._read_only_scenario(f"slack-recent:{self.fixtures['conversation_id']}", "recent.md")
        elif self.connector == "granola":
            self._granola_scenario()
        self._status_clean(self.mount)
        self._export_rotated_credential()
        print(f"ok: {self.scenario['display_name']} {self.platform} live scenario passed")

    def cleanup(self) -> None:
        failures: list[str] = []
        try:
            self._export_rotated_credential()
        except Exception as error:
            failures.append(f"OAuth credential export: {error}")
        try:
            if self.created_gmail_draft:
                self._gmail_delete_draft(self.created_gmail_draft)
        except Exception as error:  # cleanup must continue
            failures.append(f"Gmail draft cleanup: {error}")
        try:
            if self.linear_original is not None:
                self._restore_linear_original()
        except Exception as error:
            failures.append(f"Linear restoration: {error}")
        if self.provider_started and self.platform == "windows-cloud-files":
            for action in ("stop", "unregister"):
                try:
                    report = self.command(
                        "file-provider", action, str(self.mount), "--json", check=False
                    )
                    self._require_cleanup_success(f"provider {action}", report)
                except Exception as error:
                    failures.append(f"provider {action}: {error}")
        if self.daemon_started:
            try:
                self.command(
                    "daemon", "stop", "--state-dir", str(self.state), "--tcp-addr", self.tcp_addr, "--json", check=False
                )
            except Exception as error:
                failures.append(f"daemon stop: {error}")
        if self.platform == "macos-file-provider" and self.args.file_providerctl:
            retired = self.tmp / "retired-state"
            try:
                if self.state.exists():
                    self.state.rename(retired)
                self.state.mkdir()
                self._start_daemon()
                self._refresh_provider_root()
                self._wait(lambda: not self.mount.exists(), "strict File Provider mount removal")
                self.command(
                    "daemon", "stop", "--state-dir", str(self.state), "--tcp-addr", self.tcp_addr, "--json", check=False
                )
            except Exception as error:
                failures.append(f"File Provider mount removal: {error}")
        if os.environ.get("LOCALITY_LIVE_PROVIDER_KEEP_TMP") == "1" or failures:
            print(f"retained privacy-sensitive provider test state at {self.tmp}", file=sys.stderr)
        else:
            shutil.rmtree(self.tmp, ignore_errors=True)
        if failures:
            raise ScenarioError("; ".join(failures))

    def command(self, *arguments: str, check: bool = True, input_text: str | None = None) -> dict[str, Any]:
        command = [self.args.loc, *arguments]
        result = subprocess.run(
            command,
            env=self.env,
            input=input_text,
            text=True,
            capture_output=True,
            timeout=180,
            check=False,
        )
        if check and result.returncode:
            raise ScenarioError(
                f"command failed ({result.returncode}): {' '.join(arguments[:2])}: "
                f"{self._sanitize(result.stderr or result.stdout)}"
            )
        output = result.stdout.strip()
        if not output:
            return {"returncode": result.returncode}
        try:
            report = json.loads(output)
        except json.JSONDecodeError as error:
            if check:
                raise ScenarioError(f"command returned invalid JSON: {' '.join(arguments[:2])}") from error
            return {"returncode": result.returncode, "text": self._sanitize(output)}
        report["returncode"] = result.returncode
        return report

    def _sanitize(self, text: str) -> str:
        for value in sorted(self.secret_values, key=len, reverse=True):
            if len(value) >= 4:
                text = text.replace(value, "<redacted>")
        return "\n".join(text.splitlines()[-40:])[:8000]

    def _require_cleanup_success(self, label: str, report: dict[str, Any]) -> None:
        if report.get("returncode") == 0 and report.get("ok") is True:
            return
        detail = report.get("message") or report.get("text") or "ok was not true"
        raise ScenarioError(
            f"{label} failed with exit {report.get('returncode', 'unknown')}: "
            f"{self._sanitize(str(detail))}"
        )

    def _initialize_and_seed(self) -> None:
        disabled = self.env.copy()
        disabled["LOCALITY_DAEMON_DISABLE"] = "1"
        subprocess.run([self.args.loc, "connections", "--json"], env=disabled, check=True, capture_output=True, timeout=60)
        secret_ref = f"connection:{self.connection_id}"
        credentials = self.state / "credentials"
        credentials.mkdir(exist_ok=True)
        secret_path = credentials / secret_ref.encode().hex()
        secret_path.write_text(self.credential, encoding="utf-8")
        try:
            secret_path.chmod(0o600)
        except OSError:
            pass

        profile = connection_profile(self.connector)
        now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        db = sqlite3.connect(self.state / "state.sqlite3")
        try:
            db.execute(
                """INSERT OR REPLACE INTO connector_profiles
                (profile_id,connector,display_name,auth_kind,scopes_json,capabilities_json,
                 enabled_actions_json,connector_version,status,created_at,updated_at)
                VALUES (?,?,?,?,?,?,?,?,?,?,?)""",
                (
                    profile["profile_id"], self.connector, self.scenario["display_name"],
                    self.scenario["credential"]["kind"], json.dumps(profile["scopes"]),
                    json.dumps(profile["descriptor_capabilities"], separators=(",", ":")),
                    json.dumps(profile["actions"]), f"{self.connector}.v1", "active", now, now,
                ),
            )
            db.execute(
                """INSERT OR REPLACE INTO connections
                (connection_id,profile_id,connector,display_name,account_label,workspace_id,
                 workspace_name,auth_kind,secret_ref,scopes_json,capabilities_json,status,
                 created_at,updated_at,expires_at)
                VALUES (?,?,?,?,NULL,NULL,NULL,?,?,?,?,?,?,?,NULL)""",
                (
                    self.connection_id, profile["profile_id"], self.connector,
                    f"{self.scenario['display_name']} provider live", self.scenario["credential"]["kind"],
                    secret_ref, json.dumps(profile["scopes"]),
                    json.dumps(profile["descriptor_capabilities"], separators=(",", ":")),
                    "active", now, now,
                ),
            )
            db.commit()
        finally:
            db.close()

    def _mount(self) -> None:
        args = ["mount", self.connector, str(self.mount), "--connection", self.connection_id,
                "--mount-id", self.mount_id, "--projection", self.platform]
        if self.connector == "gmail":
            args += ["--view", "messages"]
        if self.connector == "slack":
            types = self.fixtures.get("conversation_types") or "private_channel,im,mpim"
            if "public_channel" in {item.strip() for item in types.split(",")}:
                raise ScenarioError("Slack provider E2E refuses auto-joining public channels")
            args += ["--history-limit", "3", "--types", types]
        self.env["LOCALITY_DAEMON_DISABLE"] = "1"
        try:
            self.command(*args, "--json")
        finally:
            self.env.pop("LOCALITY_DAEMON_DISABLE", None)

    def _refresh_oauth_before_consumers(self) -> None:
        if not self.oauth_refresh_marker:
            return
        self.env["LOCALITY_DAEMON_DISABLE"] = "1"
        try:
            self.command("pull", str(self.mount), "--json")
        finally:
            self.env.pop("LOCALITY_DAEMON_DISABLE", None)
        self._assert_oauth_refreshed()
        self._export_rotated_credential()

    def _start_daemon(self) -> None:
        self.command(
            "daemon", "start", "--session", "--state-dir", str(self.state), "--tcp-addr", self.tcp_addr,
            "--localityd-bin", self.args.localityd, "--json",
        )
        self.daemon_started = True
        self._wait(
            lambda: self.command("daemon", "status", "--state-dir", str(self.state), "--tcp-addr", self.tcp_addr, "--json", check=False).get("state") == "running",
            "daemon health",
        )

    def _start_provider(self) -> None:
        if self.platform == "windows-cloud-files":
            report = self.command("file-provider", "start", str(self.mount), "--json")
            sync_root = pathlib.Path(report["helper_report"]["sync_root"]).resolve()
            if sync_root != self.root:
                raise ScenarioError("Cloud Files registered an unexpected sync root")
            self.provider_started = True
        else:
            self.provider_started = True
            self._refresh_provider_root()

    def _refresh_provider_root(self) -> None:
        if self.platform != "macos-file-provider" or not self.args.file_providerctl:
            return
        for action, identifier in (("reimport", "root"), ("signal", "working-set")):
            subprocess.run(
                [self.args.file_providerctl, action, "--mount-id", "loc", "--identifier", identifier, "--json"],
                env=self.env, capture_output=True, timeout=60, check=False,
            )

    def _doctor(self) -> None:
        report = self.command("doctor", "--json")
        if report.get("ok") is not True:
            raise ScenarioError("loc doctor did not report ok=true with the provider running")

    def _entity_path(self, remote_id: str, leaf: str = "") -> pathlib.Path:
        def resolve() -> pathlib.Path | None:
            db = sqlite3.connect(self.state / "state.sqlite3")
            try:
                row = db.execute(
                    "SELECT path FROM entities WHERE mount_id=? AND remote_id=? ORDER BY path LIMIT 1",
                    (self.mount_id, remote_id),
                ).fetchone()
            finally:
                db.close()
            if not row:
                return None
            path = self.mount / row[0]
            if leaf and path.name != leaf:
                path /= leaf
            return path

        def projected_fixture_exists() -> bool:
            candidate = resolve()
            return candidate is not None and candidate.exists()

        self._wait(projected_fixture_exists, "projected fixture")
        found = resolve()
        if found is None or not found.exists():
            raise ScenarioError("projected fixture disappeared during resolution")
        return found

    def _read_only_scenario(self, remote_id: str, leaf: str = "") -> None:
        target = self._entity_path(remote_id, leaf)
        # The first provider read is allowed to hydrate the durable content
        # cache. Snapshot only after that expected state transition.
        original = target.read_bytes()
        before = self._state_fingerprint()
        self._assert_file_read_only(target, original)
        push = self.command("push", str(target), "-y", "--json", check=False)
        if push.get("returncode") == 0 or push.get("ok") is True:
            raise ScenarioError("read-only connector push unexpectedly succeeded")
        self._status_clean(target)
        if self._state_fingerprint() != before:
            raise ScenarioError("read-only operations changed cache, journal, or virtual-mutation state")
        self.command("pull", str(self.mount), "--json")
        if target.read_bytes() != original:
            raise ScenarioError("read-only provider content changed after repeat observation")

    def _assert_file_read_only(self, target: pathlib.Path, original: bytes | None = None) -> bytes:
        if original is None:
            original = target.read_bytes()
        self._expect_filesystem_rejection("write", lambda: target.write_bytes(original + b"\nforbidden\n"))
        self._expect_filesystem_rejection("rename", lambda: target.rename(target.with_name(target.name + ".forbidden")))
        self._expect_filesystem_rejection("delete", target.unlink)
        self._expect_filesystem_rejection("create", lambda: (target.parent / "forbidden-create.md").write_text("forbidden", encoding="utf-8"))
        if target.read_bytes() != original:
            raise ScenarioError("read-only target bytes changed after rejected filesystem operations")
        return original

    def _granola_scenario(self) -> None:
        note_id = self.fixtures["note_id"]
        summary = self._entity_path(note_id, "summary.md")
        transcript = summary.with_name("transcript.md")
        self._wait(transcript.exists, "Granola transcript placeholder")
        summary_bytes = summary.read_bytes()
        transcript_bytes = transcript.read_bytes()
        if b"connector: granola" not in summary_bytes or b"content_kind: summary" not in summary_bytes:
            raise ScenarioError("Granola summary was not canonical")
        if b"content_kind: transcript" not in transcript_bytes:
            raise ScenarioError("Granola transcript was not canonical")
        self._read_only_scenario(note_id, "summary.md")
        if transcript.read_bytes() != transcript_bytes:
            raise ScenarioError("Granola transcript identity/content changed on repeated discovery")

    def _gmail_scenario(self) -> None:
        for mailbox in self.scenario["projection"]["roots"]:
            self._wait((self.mount / mailbox).is_dir, f"Gmail {mailbox} mailbox")
        unique = f"{int(time.time())}-{os.getpid()}"
        marker = f"Locality provider Gmail draft marker {unique}"
        draft = self.mount / "draft" / f"locality-provider-{unique}.md"
        draft.write_text(
            f'---\nto:\n  - "{self.fixtures["recipient"]}"\nsubject: "Locality provider draft {unique}"\n---\n{marker}\n',
            encoding="utf-8",
        )
        self._assert_reviewable_diff(draft)
        push = self.command("push", str(draft), "-y", "--json")
        remote_ids = push.get("changed_remote_ids") or []
        draft_id = next((value.split(":", 1)[1] for value in remote_ids if str(value).startswith("gmail-draft:")), "")
        if not draft_id:
            draft_id = self._gmail_find_draft(marker)
        if not draft_id:
            raise ScenarioError("Gmail draft push could not be confirmed through the drafts API")
        self.created_gmail_draft = draft_id
        self._gmail_assert_marker(draft_id, marker)
        self.command("pull", str(self.mount), "--json")
        projected = self._entity_path(f"gmail-draft:{draft_id}")
        updated_marker = f"{marker} updated"
        projected.write_text(projected.read_text(encoding="utf-8").replace(marker, updated_marker), encoding="utf-8")
        self._assert_reviewable_diff(projected)
        self.command("push", str(projected), "-y", "--json")
        self._gmail_assert_marker(draft_id, updated_marker)
        self.command("pull", str(self.mount), "--json")
        self._status_clean(projected)
        self._gmail_delete_draft(draft_id)
        self.created_gmail_draft = ""
        self.command("pull", str(self.mount / "draft"), "--json")
        self._wait(lambda: not projected.exists(), "Gmail deleted-draft reconciliation")
        for folder in ("inbox", "sent"):
            candidate = next((path for path in (self.mount / folder).glob("*.md") if path.is_file()), None)
            if candidate:
                self._assert_file_read_only(candidate)

    def _linear_scenario(self) -> None:
        issue_id = self.fixtures["issue_id"]
        issue = self._linear_get()
        self.linear_original = linear_restoration_values(issue_id, issue)
        page = self._entity_path(issue_id, "page.md")
        original_page = page.read_text(encoding="utf-8")
        marker = f"Locality provider Linear marker {int(time.time())}-{os.getpid()}"
        page.write_text(original_page.rstrip() + f"\n\n{marker}\n", encoding="utf-8")
        self._assert_reviewable_diff(page)
        self.command("push", str(page), "-y", "--json")
        if marker not in (self._linear_get().get("description") or ""):
            raise ScenarioError("Linear GraphQL readback did not contain the pushed body marker")
        self.command("pull", str(self.mount), "--json")
        self._status_clean(page)
        for sidecar in ("comments.md", "attachments.md", "pull-requests.md", "history.md"):
            path = page.with_name(sidecar)
            if path.exists():
                self._assert_file_read_only(path)
        current = page.read_text(encoding="utf-8")
        body = original_page.split("---\n", 2)[-1] if original_page.startswith("---\n") else original_page
        if current.startswith("---\n"):
            _, frontmatter, _ = current.split("---\n", 2)
            page.write_text(f"---\n{frontmatter}---\n{body}", encoding="utf-8")
        else:
            page.write_text(original_page, encoding="utf-8")
        self.command("push", str(page), "-y", "--json")
        # Product restoration exercises the mounted write path first. Always
        # finish with an exact API restoration so a nullable description is
        # not collapsed to an empty string in the shared fixture.
        self._restore_linear_original()
        self.linear_original = None
        self.command("pull", str(self.mount), "--json")
        self._status_clean(page)

    def _assert_reviewable_diff(self, path: pathlib.Path) -> None:
        status = self.command("status", str(path), "--json")
        if status.get("clean") is True:
            raise ScenarioError("writable provider mutation did not become dirty")
        diff = self.command("diff", str(path), "--json")
        if diff.get("action") != "confirm_plan":
            raise ScenarioError("writable provider mutation did not produce a reviewable diff")

    def _status_clean(self, path: pathlib.Path) -> None:
        report = self.command("status", str(path), "--json")
        if report.get("ok") is not True or report.get("clean") is not True:
            raise ScenarioError("provider scenario did not reconcile to clean status")

    def _state_fingerprint(self) -> str:
        db = sqlite3.connect(self.state / "state.sqlite3")
        try:
            tables: dict[str, Any] = {}
            for table in ("entities", "shadows", "journals", "virtual_mutations", "content_cache"):
                exists = db.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
                ).fetchone()
                if not exists:
                    tables[table] = None
                    continue
                columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
                rows = [
                    [self._fingerprint_sql_value(value) for value in row]
                    for row in db.execute(f'SELECT * FROM "{table}"')
                ]
                rows.sort(key=lambda row: json.dumps(row, sort_keys=True, separators=(",", ":")))
                tables[table] = {"columns": columns, "rows": rows}
        finally:
            db.close()

        cache_files = []
        content_root = pathlib.Path(
            self.env.get("LOCALITY_VIRTUAL_FS_CONTENT_ROOT", str(self.state / "content"))
        )
        if content_root.exists():
            for path in sorted(content_root.rglob("*")):
                relative = path.relative_to(content_root).as_posix()
                if path.is_symlink():
                    cache_files.append((relative, "symlink", os.readlink(path)))
                elif path.is_file():
                    cache_files.append((relative, "file", hashlib.sha256(path.read_bytes()).hexdigest()))
        payload = {"tables": tables, "content_cache": cache_files}
        return hashlib.sha256(
            json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()

    @staticmethod
    def _fingerprint_sql_value(value: Any) -> Any:
        if isinstance(value, bytes):
            return {"blob_sha256": hashlib.sha256(value).hexdigest(), "length": len(value)}
        return value

    def _expect_filesystem_rejection(self, label: str, action: Callable[[], object]) -> None:
        try:
            action()
        except OSError:
            return
        raise ScenarioError(f"read-only provider unexpectedly allowed {label}")

    def _gmail_request(self, path: str, method: str = "GET") -> Any:
        token = oauth_access_token(self._stored_credential())
        request = urllib.request.Request(
            f"https://gmail.googleapis.com/gmail/v1/users/me/{path}",
            method=method,
            headers={"Authorization": f"Bearer {token}"},
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read()
        except urllib.error.HTTPError as error:
            if method == "DELETE" and error.code == 404:
                return None
            raise ScenarioError(f"Gmail verification returned HTTP {error.code}") from error
        return json.loads(body) if body else None

    def _gmail_find_draft(self, marker: str) -> str:
        listing = self._gmail_request("drafts?maxResults=100") or {}
        for item in listing.get("drafts") or []:
            draft_id = item.get("id")
            if draft_id and self._gmail_payload_contains(self._gmail_request(f"drafts/{draft_id}?format=full"), marker):
                return draft_id
        return ""

    def _gmail_assert_marker(self, draft_id: str, marker: str) -> None:
        self._wait(lambda: self._gmail_payload_contains(self._gmail_request(f"drafts/{draft_id}?format=full"), marker), "Gmail draft API readback")

    def _gmail_payload_contains(self, value: Any, marker: str) -> bool:
        if isinstance(value, dict):
            return any(self._gmail_payload_contains(child, marker) for child in value.values())
        if isinstance(value, list):
            return any(self._gmail_payload_contains(child, marker) for child in value)
        if not isinstance(value, str):
            return False
        if marker in value:
            return True
        try:
            decoded = base64.urlsafe_b64decode(value + "=" * (-len(value) % 4)).decode("utf-8", "ignore")
        except Exception:
            return False
        return marker in decoded

    def _gmail_delete_draft(self, draft_id: str) -> None:
        self._gmail_request(f"drafts/{draft_id}", "DELETE")

    def _linear_get(self) -> dict[str, Any]:
        data = self._linear_graphql(
            "query ProviderLiveIssue($id: String!) { issue(id: $id) { id title description updatedAt } }",
            {"id": self.fixtures["issue_id"]},
        )
        issue = data.get("issue")
        if not isinstance(issue, dict):
            raise ScenarioError("Linear fixture issue was not returned")
        return issue

    def _linear_update(self, values: dict[str, Any]) -> None:
        result = self._linear_graphql(
            "mutation ProviderLiveRestore($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success issue { id } } }",
            {"id": values["id"], "input": {key: values[key] for key in ("description", "title") if key in values}},
        )
        if (result.get("issueUpdate") or {}).get("success") is not True:
            raise ScenarioError("Linear scratch issue restoration mutation did not succeed")

    def _restore_linear_original(self) -> None:
        if self.linear_original is None:
            return
        self._linear_update(self.linear_original)
        restored = self._linear_get()
        for field in ("description", "title"):
            if restored.get(field) != self.linear_original[field]:
                raise ScenarioError(
                    f"Linear scratch issue {field} was not deterministically restored"
                )

    def _linear_graphql(self, query: str, variables: dict[str, Any]) -> dict[str, Any]:
        body = json.dumps({"query": query, "variables": variables}).encode()
        request = urllib.request.Request(
            "https://api.linear.app/graphql", data=body,
            headers={"Authorization": self._stored_credential(), "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as error:
            raise ScenarioError(f"Linear verification returned HTTP {error.code}") from error
        if payload.get("errors"):
            raise ScenarioError("Linear verification GraphQL returned errors")
        return payload.get("data") or {}

    def _stored_credential(self) -> str:
        secret_ref = f"connection:{self.connection_id}"
        return (self.state / "credentials" / secret_ref.encode().hex()).read_text(encoding="utf-8").strip()

    def _export_rotated_credential(self) -> None:
        output = os.environ.get("LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT", "")
        if not output or self.scenario["credential"]["kind"] != "oauth":
            return
        if self.oauth_refresh_marker:
            self._assert_oauth_refreshed()
        destination = pathlib.Path(output)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(self._stored_credential(), encoding="utf-8")
        try:
            destination.chmod(0o600)
        except OSError:
            pass

    def _assert_oauth_refreshed(self) -> None:
        if not self.oauth_refresh_marker:
            return
        credential = json.loads(self._stored_credential())
        new_marker = hashlib.sha256(str(credential.get("access_token", "")).encode()).hexdigest()
        if new_marker == self.oauth_refresh_marker:
            raise ScenarioError("forced OAuth refresh did not replace the access token")
        if int(credential.get("expires_at") or 0) <= int(time.time()):
            raise ScenarioError("forced OAuth refresh did not persist a future expiry")

    def _wait(self, predicate: Callable[[], bool], label: str) -> None:
        deadline = time.monotonic() + self.args.wait_seconds
        last_error: Exception | None = None
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except Exception as error:
                last_error = error
            time.sleep(0.5)
        suffix = f": {last_error}" if last_error else ""
        raise ScenarioError(f"timed out waiting for {label}{suffix}")


def connection_profile(connector: str) -> dict[str, Any]:
    oauth = connector in {"gmail", "slack"}
    scopes = {
        "gmail": ["openid", "email", "profile", "https://www.googleapis.com/auth/gmail.readonly", "https://www.googleapis.com/auth/gmail.compose"],
        "slack": ["channels:read", "channels:history", "groups:read", "groups:history", "im:read", "im:history", "mpim:read", "mpim:history", "users:read", "team:read", "files:read", "channels:join"],
        "linear": ["issues:read", "issues:write"],
        "granola": ["read"],
    }[connector]
    return {
        "profile_id": f"{connector}-{'oauth' if oauth else 'api-key'}-default",
        "scopes": scopes,
        "actions": {"gmail": ["read", "send"], "slack": [], "linear": ["read", "write"], "granola": ["read"]}[connector],
        "descriptor_capabilities": {
            "supports_block_updates": False,
            "supports_entity_body_updates": connector == "linear",
            "supports_databases": False,
            "supports_oauth": oauth,
            "supports_remote_observation": True,
            "supports_lazy_child_enumeration": True,
            "supports_media_download": connector == "linear",
            "supports_undo": False,
            "supports_batch_observation": connector == "linear",
        },
    }


def oauth_access_token(secret: str) -> str:
    try:
        value = json.loads(secret)
    except json.JSONDecodeError as error:
        raise ScenarioError("stored OAuth credential was not JSON") from error
    token = value.get("access_token") if isinstance(value, dict) else None
    if not isinstance(token, str) or not token:
        raise ScenarioError("stored OAuth credential omitted access_token")
    return token


def required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise ScenarioError(f"missing {name}")
    return value


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--connector", choices=live_connector_matrix.CONNECTORS, required=True)
    parser.add_argument("--projection", choices=("macos-file-provider", "windows-cloud-files"), required=True)
    parser.add_argument("--provider-root", required=True)
    parser.add_argument("--loc", required=True)
    parser.add_argument("--localityd", required=True)
    parser.add_argument("--file-providerctl")
    parser.add_argument("--matrix", type=pathlib.Path, default=live_connector_matrix.DEFAULT_MATRIX)
    parser.add_argument("--wait-seconds", type=int, default=120)
    args = parser.parse_args()
    runner: Runner | None = None
    failure: Exception | None = None
    try:
        runner = Runner(args)
        runner.run()
    except Exception as error:
        failure = error
        print(f"live provider connector scenario failed: {error}", file=sys.stderr)
    finally:
        if runner is not None:
            try:
                runner.cleanup()
            except Exception as error:
                print(f"live provider connector cleanup failed: {error}", file=sys.stderr)
                failure = failure or error
    raise SystemExit(1 if failure else 0)


if __name__ == "__main__":
    main()
