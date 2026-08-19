#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import tempfile

from live_provider_connector_scenario import Runner


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
        runner._initialize_and_seed()
        assert (runner.state / "state.sqlite3").is_file()
        secret = runner._stored_credential()
        assert secret == runner.credential
        sanitized = runner._sanitize(f"secret={runner.credential}")
        assert runner.credential not in sanitized
        assert "<redacted>" in sanitized
        shutil.rmtree(runner.tmp)
finally:
    shutil.rmtree(provider_root, ignore_errors=True)

print("ok: live provider connector scenario self-test passed")
