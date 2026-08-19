#!/usr/bin/env python3
"""Validate and query the private live-connector E2E scenario contract."""

from __future__ import annotations

import argparse
import json
import pathlib
import shlex
import sys
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_MATRIX = pathlib.Path(__file__).with_name("live_connector_scenarios.json")
CONNECTORS = ("gmail", "slack", "linear", "granola")
PLATFORMS = ("linux-fuse", "macos-file-provider", "windows-cloud-files")


def load(path: pathlib.Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"could not read live connector matrix {path}: {error}") from error
    if not isinstance(value, dict):
        raise SystemExit("live connector matrix must be a JSON object")
    return value


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"invalid live connector matrix: {message}")


def validate(matrix: dict[str, Any]) -> None:
    require(matrix.get("schema_version") == 1, "schema_version must equal 1")
    connectors = matrix.get("connectors")
    require(isinstance(connectors, dict), "connectors must be an object")
    require(tuple(connectors) == CONNECTORS, f"connectors must be exactly {CONNECTORS}")

    for connector, scenario in connectors.items():
        require(isinstance(scenario, dict), f"{connector} must be an object")
        credential = scenario.get("credential")
        fixtures = scenario.get("fixtures")
        capabilities = scenario.get("capabilities")
        projection = scenario.get("projection")
        scenarios = scenario.get("scenarios")
        cleanup = scenario.get("cleanup")
        require(isinstance(credential, dict), f"{connector}.credential must be an object")
        require(credential.get("kind") in {"oauth", "api_key"}, f"{connector} credential kind")
        require(_env_name(credential.get("environment")), f"{connector} credential environment")
        require(str(credential.get("secret_ref", "")).startswith("connection:"), f"{connector} secret_ref")
        require(isinstance(fixtures, dict) and fixtures, f"{connector}.fixtures must not be empty")
        require(all(_env_name(value) for value in fixtures.values()), f"{connector} fixture environment")
        require(isinstance(capabilities, dict), f"{connector}.capabilities must be an object")
        require(capabilities.get("read") is True, f"{connector} must support reads")
        for name in ("create", "edit", "delete", "read_only", "remote_drift", "outbound_send"):
            require(isinstance(capabilities.get(name), list), f"{connector}.capabilities.{name} must be a list")
        require(isinstance(projection, dict), f"{connector}.projection must be an object")
        require(projection.get("lookup") in {"mailbox", "remote-id"}, f"{connector} projection lookup")
        require(isinstance(cleanup, list) and cleanup, f"{connector}.cleanup must not be empty")
        require(isinstance(scenarios, dict), f"{connector}.scenarios must be an object")
        require(tuple(scenarios) == PLATFORMS, f"{connector} must define all provider surfaces")
        for platform, script in scenarios.items():
            require(isinstance(script, str) and script.startswith("tests/"), f"{connector}/{platform} script")
            require((ROOT / script).is_file(), f"{connector}/{platform} script does not exist: {script}")

        read_only = capabilities["read_only"] == ["entire-mount"]
        if read_only:
            require(not capabilities["create"], f"{connector} read-only mount advertises create")
            require(not capabilities["edit"], f"{connector} read-only mount advertises edit")
            require(not capabilities["delete"], f"{connector} read-only mount advertises delete")
            require(not capabilities["remote_drift"], f"{connector} read-only mount advertises write drift")
            require(scenario.get("restoration") == "not-applicable-read-only", f"{connector} restoration")

    require(connectors["gmail"]["capabilities"]["outbound_send"] == ["linux-fuse"], "Gmail send must remain Linux-only")
    require(connectors["slack"]["credential"]["rotation"] == "required-after-every-consumer", "Slack rotation policy")
    require(connectors["slack"]["capabilities"]["read_only"] == ["entire-mount"], "Slack must be read-only")
    require(connectors["granola"]["capabilities"]["read_only"] == ["entire-mount"], "Granola must be read-only")


def _env_name(value: object) -> bool:
    return isinstance(value, str) and value.isidentifier() and value.upper() == value


def select(matrix: dict[str, Any], connector: str, dotted: str) -> Any:
    value: Any = matrix["connectors"][connector]
    for part in dotted.split(".") if dotted else ():
        if not isinstance(value, dict) or part not in value:
            raise SystemExit(f"unknown matrix field for {connector}: {dotted}")
        value = value[part]
    return value


def shell_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    if isinstance(value, (dict, list)):
        return json.dumps(value, separators=(",", ":"), sort_keys=True)
    return str(value)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=pathlib.Path, default=DEFAULT_MATRIX)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("validate")
    subparsers.add_parser("github-matrix")
    get_parser = subparsers.add_parser("get")
    get_parser.add_argument("connector", choices=CONNECTORS)
    get_parser.add_argument("field")
    shell_parser = subparsers.add_parser("shell")
    shell_parser.add_argument("connector", choices=CONNECTORS)

    args = parser.parse_args()
    matrix = load(args.matrix)
    validate(matrix)
    if args.command == "validate":
        print("ok: live connector scenario matrix is valid")
    elif args.command == "github-matrix":
        include = [
            {"connector": connector, "display_name": matrix["connectors"][connector]["display_name"]}
            for connector in CONNECTORS
        ]
        print(json.dumps({"include": include}, separators=(",", ":")))
    elif args.command == "get":
        print(shell_value(select(matrix, args.connector, args.field)))
    elif args.command == "shell":
        scenario = matrix["connectors"][args.connector]
        values = {
            "LIVE_CONNECTOR": args.connector,
            "LIVE_DISPLAY_NAME": scenario["display_name"],
            "LIVE_CREDENTIAL_ENV": scenario["credential"]["environment"],
            "LIVE_CREDENTIAL_KIND": scenario["credential"]["kind"],
            "LIVE_SECRET_REF": scenario["credential"]["secret_ref"],
            "LIVE_FIXTURES_JSON": scenario["fixtures"],
            "LIVE_CAPABILITIES_JSON": scenario["capabilities"],
            "LIVE_PROJECTION_JSON": scenario["projection"],
            "LIVE_CLEANUP_JSON": scenario["cleanup"],
        }
        for name, value in values.items():
            print(f"{name}={shlex.quote(shell_value(value))}")


if __name__ == "__main__":
    main()
