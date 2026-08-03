#!/usr/bin/env bash
set -euo pipefail

CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"

mkdir -p "$CODEX_HOME"

merge_codex_config() {
  local config_path="$1"
  local sandbox_mode="${2:-}"

  python3 - "$config_path" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" \
    "$AZURE_OPENAI_BASE_URL" "$sandbox_mode" <<'PY'
import json
import os
import re
import sys
import tempfile
import tomllib

path, model, reasoning, base_url, sandbox_mode = sys.argv[1:]
try:
    with open(path, "rb") as source:
        existing = source.read()
except FileNotFoundError:
    existing = b""

if existing:
    try:
        tomllib.loads(existing.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise SystemExit(f"refusing to modify invalid Codex config {path}: {error}")

text = existing.decode("utf-8")
lines = text.splitlines(keepends=True)
root_values = {
    "model": model,
    "model_provider": "azure",
    "model_reasoning_effort": reasoning,
}
if sandbox_mode:
    root_values["sandbox_mode"] = sandbox_mode
provider_values = {
    "name": "Azure OpenAI",
    "base_url": base_url,
    "env_key": "AZURE_OPENAI_API_KEY",
    "wire_api": "responses",
}

table_pattern = re.compile(r"^\s*\[\[?\s*([^]]+?)\s*]\]?\s*(?:#.*)?$")
key_pattern = re.compile(r'^\s*([A-Za-z0-9_-]+)\s*=')
azure_tables = {"model_providers.azure", 'model_providers."azure"'}
section = ""
seen_root = set()
seen_provider = set()
output = []
root_inserted = False
provider_found = False
provider_inserted = False

def setting(key, value):
    return f"{key} = {json.dumps(value, ensure_ascii=False)}\n"

def append_missing_root():
    global root_inserted
    if root_inserted:
        return
    for key, value in root_values.items():
        if key not in seen_root:
            output.append(setting(key, value))
    if output and output[-1].strip():
        output.append("\n")
    root_inserted = True

def append_missing_provider():
    global provider_inserted
    if provider_inserted:
        return
    for key, value in provider_values.items():
        if key not in seen_provider:
            output.append(setting(key, value))
    provider_inserted = True

for line in lines:
    table_match = table_pattern.match(line.rstrip("\r\n"))
    if table_match:
        if not root_inserted:
            append_missing_root()
        if section in azure_tables:
            append_missing_provider()
        section = table_match.group(1).strip()
        if section in azure_tables:
            if provider_found:
                raise SystemExit(f"refusing to merge duplicate Azure provider tables in {path}")
            provider_found = True
        output.append(line)
        continue

    key_match = key_pattern.match(line)
    if section == "" and key_match and key_match.group(1) in root_values:
        key = key_match.group(1)
        if key not in seen_root:
            output.append(setting(key, root_values[key]))
            seen_root.add(key)
        continue
    if section in azure_tables and key_match and key_match.group(1) in provider_values:
        key = key_match.group(1)
        if key not in seen_provider:
            output.append(setting(key, provider_values[key]))
            seen_provider.add(key)
        continue
    output.append(line)

if not root_inserted:
    append_missing_root()
if section in azure_tables:
    append_missing_provider()
if not provider_found:
    if output and output[-1].strip():
        output.append("\n")
    output.append("[model_providers.azure]\n")
    append_missing_provider()

rendered = "".join(output)
try:
    parsed = tomllib.loads(rendered)
except tomllib.TOMLDecodeError as error:
    raise SystemExit(f"refusing to write invalid merged Codex config {path}: {error}")

provider = parsed.get("model_providers", {}).get("azure", {})
expected_root = root_values
expected_provider = provider_values
if any(parsed.get(key) != value for key, value in expected_root.items()):
    raise SystemExit(f"merged Codex root settings failed validation for {path}")
if any(provider.get(key) != value for key, value in expected_provider.items()):
    raise SystemExit(f"merged Azure provider settings failed validation for {path}")

directory = os.path.dirname(path)
os.makedirs(directory, mode=0o700, exist_ok=True)
fd, temporary = tempfile.mkstemp(prefix=".config.toml.", dir=directory)
try:
    with os.fdopen(fd, "w", encoding="utf-8", newline="") as destination:
        destination.write(rendered)
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)
finally:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
PY
}

merge_codex_config "$CODEX_HOME/config.toml"

chmod 700 "$CODEX_HOME"
chmod 600 "$CODEX_HOME/config.toml"

if [ -n "${AMIKA_AGENT_CWD:-}" ] && [ -d "$AMIKA_AGENT_CWD" ]; then
  mkdir -p "$AMIKA_AGENT_CWD/.codex"
  merge_codex_config "$AMIKA_AGENT_CWD/.codex/config.toml" "workspace-write"
fi

codex --version || true
