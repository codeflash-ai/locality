#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import sqlite3
import tempfile

from live_provider_connector_scenario import (
    MACOS_FILE_PROVIDER_DAEMON_ADDR,
    Runner,
    ScenarioError,
    linear_restoration_values,
)


ROOT = pathlib.Path(__file__).resolve().parents[1]
loc = pathlib.Path(os.environ.get("LOCALITY_BIN", ROOT / "target/debug/loc")).resolve()
localityd = pathlib.Path(os.environ.get("LOCALITYD_BIN", ROOT / "target/debug/localityd")).resolve()
if not loc.is_file():
    raise SystemExit(f"provider scenario self-test requires a built loc binary: {loc}")

fake_environment = {
    "LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON": json.dumps(
        {
            "kind": "oauth",
            "connector": "gmail",
            "access_token": "selftest-gmail-token",
            "oauth_broker_url": "https://auth.example.test",
            "refresh_token_handle": "selftest-gmail-refresh",
            "expires_at": 4_102_444_800,
        }
    ),
    "LOCALITY_GMAIL_LIVE_TO_EMAIL": "selftest@example.invalid",
    "LOCALITY_SLACK_LIVE_CREDENTIAL_JSON": json.dumps(
        {
            "kind": "oauth",
            "connector": "slack",
            "access_token": "selftest-slack-token",
            "oauth_broker_url": "https://auth.example.test",
            "refresh_token_handle": "selftest-slack-refresh",
            "expires_at": 4_102_444_800,
        }
    ),
    "LOCALITY_SLACK_LIVE_CONVERSATION_ID": "C_SELFTEST",
    "LOCALITY_SLACK_LIVE_TYPES": "private_channel",
    "LINEAR_API_KEY": "lin_api_selftest",
    "LOCALITY_LINEAR_LIVE_ISSUE_ID": "00000000-0000-0000-0000-000000000000",
    "GRANOLA_API_KEY": "granola-selftest",
    "LOCALITY_GRANOLA_LIVE_NOTE_ID": "note_selftest",
}

provider_root = pathlib.Path(tempfile.mkdtemp(prefix="locality-provider-runner-selftest-"))
try:
    nullable_linear = linear_restoration_values(
        "issue-null-description", {"description": None, "title": "Nullable fixture"}
    )
    assert nullable_linear["description"] is None
    for connector in ("gmail", "slack", "linear", "granola"):
        os.environ.update(fake_environment)
        args = argparse.Namespace(
            connector=connector,
            projection="macos-file-provider",
            provider_root=str(provider_root),
            loc=str(loc),
            localityd=str(localityd),
            file_providerctl=None,
            matrix=ROOT / "tests/live_connector_scenarios.json",
            wait_seconds=1,
        )
        runner = Runner(args)
        assert runner.tcp_addr == MACOS_FILE_PROVIDER_DAEMON_ADDR
        runner._initialize_and_seed()
        assert (runner.state / "state.sqlite3").is_file()
        secret = runner._stored_credential()
        assert secret == runner.credential
        sanitized = runner._sanitize(f"secret={runner.credential}")
        assert runner.credential not in sanitized
        assert "<redacted>" in sanitized

        database = sqlite3.connect(runner.state / "state.sqlite3")
        try:
            database.execute(
                """
                INSERT INTO entities (
                    mount_id, remote_id, kind_json, title, path,
                    hydration_json, content_hash, remote_edited_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    "fingerprint-mount",
                    "fingerprint-remote",
                    '"page"',
                    "before",
                    "fingerprint/page.md",
                    '"hydrated"',
                    "content-hash",
                    "2026-08-19T00:00:00Z",
                ),
            )
            database.commit()
            database_before = runner._state_fingerprint()
            database.execute(
                "UPDATE entities SET title = ? WHERE mount_id = ? AND remote_id = ?",
                ("after", "fingerprint-mount", "fingerprint-remote"),
            )
            database.commit()
            assert runner._state_fingerprint() != database_before
            database.execute(
                "DELETE FROM entities WHERE mount_id = ? AND remote_id = ?",
                ("fingerprint-mount", "fingerprint-remote"),
            )
            database.commit()
        finally:
            database.close()

        cache_file = runner.state / "content" / "fingerprint-mount" / "files" / "page.md"
        cache_file.parent.mkdir(parents=True)
        cache_file.write_text("before", encoding="utf-8")
        cache_before = runner._state_fingerprint()
        cache_file.write_text("after!", encoding="utf-8")
        assert runner._state_fingerprint() != cache_before
        runner._require_cleanup_success("provider stop", {"returncode": 0, "ok": True})
        for failed_report in (
            {"returncode": 1, "ok": False, "message": "stop failed"},
            {"returncode": 0, "ok": False, "message": "registration remains"},
        ):
            try:
                runner._require_cleanup_success("provider stop", failed_report)
            except ScenarioError as error:
                assert "provider stop failed" in str(error)
            else:
                raise AssertionError("provider cleanup failure was not propagated")

        if connector == "gmail":
            os.environ.update(fake_environment)
            windows_args = argparse.Namespace(
                connector=connector,
                projection="windows-cloud-files",
                provider_root=str(provider_root),
                loc=str(loc),
                localityd=str(localityd),
                file_providerctl=None,
                matrix=ROOT / "tests/live_connector_scenarios.json",
                wait_seconds=1,
            )
            windows_runner = Runner(windows_args)
            provider_commands: list[tuple[str, ...]] = []

            def malformed_provider_start(*arguments: str, **kwargs: object) -> dict[str, object]:
                provider_commands.append(arguments)
                return {"returncode": 0, "ok": True}

            windows_runner.command = malformed_provider_start
            try:
                windows_runner._start_provider()
            except ScenarioError as error:
                assert "omitted its sync root" in str(error)
                assert windows_runner.provider_started
            else:
                raise AssertionError("malformed provider startup report was accepted")
            finally:
                windows_runner.cleanup()
                shutil.rmtree(windows_runner.tmp, ignore_errors=True)
            assert [arguments[1] for arguments in provider_commands] == [
                "start",
                "stop",
                "unregister",
            ]

            runner.daemon_started = True
            runner.command = lambda *arguments, **kwargs: {
                "returncode": 1,
                "ok": False,
                "message": "daemon refused to stop",
            }
            try:
                runner.cleanup()
            except ScenarioError as error:
                assert "daemon stop failed" in str(error)
            else:
                raise AssertionError("daemon cleanup failure was not propagated")
        shutil.rmtree(runner.tmp)
finally:
    shutil.rmtree(provider_root, ignore_errors=True)

print("ok: live provider connector scenario self-test passed")
