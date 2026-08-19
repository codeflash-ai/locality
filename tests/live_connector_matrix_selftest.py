#!/usr/bin/env python3

from __future__ import annotations

import copy
import importlib.util
import pathlib


SCRIPT = pathlib.Path(__file__).with_name("live_connector_matrix.py")
SPEC = importlib.util.spec_from_file_location("live_connector_matrix", SCRIPT)
assert SPEC and SPEC.loader
matrix_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matrix_module)


matrix = matrix_module.load(matrix_module.DEFAULT_MATRIX)
matrix_module.validate(matrix)

assert matrix_module.select(matrix, "gmail", "capabilities.outbound_send") == ["linux-fuse"]
assert matrix_module.select(matrix, "slack", "restoration") == "not-applicable-read-only"
assert matrix_module.select(matrix, "linear", "projection.lookup") == "remote-id"
assert matrix_module.select(matrix, "granola", "capabilities.read_only") == ["entire-mount"]

invalid = copy.deepcopy(matrix)
invalid["connectors"]["slack"]["capabilities"]["edit"] = ["recent.md"]
try:
    matrix_module.validate(invalid)
except SystemExit as error:
    assert "read-only mount advertises edit" in str(error)
else:
    raise AssertionError("matrix validation accepted a writable Slack scenario")

invalid = copy.deepcopy(matrix)
invalid["connectors"]["gmail"]["capabilities"]["outbound_send"] = [
    "linux-fuse",
    "windows-cloud-files",
]
try:
    matrix_module.validate(invalid)
except SystemExit as error:
    assert "Gmail send must remain Linux-only" in str(error)
else:
    raise AssertionError("matrix validation accepted non-Linux Gmail send coverage")

print("ok: live connector matrix self-test passed")
