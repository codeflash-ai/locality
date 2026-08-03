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
import copy
import json
import os
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
parsed_existing = tomllib.loads(text) if text else {}
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

def decoded_key_path(source):
    """Use tomllib itself to decode bare, quoted, and dotted key syntax."""
    try:
        parsed_key = tomllib.loads(f"{source} = 0")
    except tomllib.TOMLDecodeError as error:
        raise SystemExit(f"refusing to modify unrecognized TOML key in {path}: {error}")
    result = []
    cursor = parsed_key
    while isinstance(cursor, dict) and len(cursor) == 1:
        key, cursor = next(iter(cursor.items()))
        result.append(key)
    if cursor != 0:
        raise SystemExit(f"refusing to modify ambiguous TOML key in {path}")
    return tuple(result)


def line_end(start):
    end = text.find("\n", start)
    return len(text) if end < 0 else end + 1


def scan_header(start):
    array_table = text.startswith("[[", start)
    opening = 2 if array_table else 1
    closing = "]]" if array_table else "]"
    index = start + opening
    quote = None
    escaped = False
    while index < len(text):
        character = text[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                quote = None
            index += 1
            continue
        if quote == "'":
            if character == "'":
                quote = None
            index += 1
            continue
        if character in "\"'":
            quote = character
            index += 1
            continue
        if text.startswith(closing, index):
            key_source = text[start + opening:index].strip()
            return decoded_key_path(key_source), array_table, line_end(index + len(closing))
        index += 1
    raise SystemExit(f"refusing to modify unterminated TOML table header in {path}")


def scan_assignment(start):
    index = start
    quote = None
    escaped = False
    while index < len(text):
        character = text[index]
        if quote == '"':
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                quote = None
            index += 1
            continue
        if quote == "'":
            if character == "'":
                quote = None
            index += 1
            continue
        if character in "\"'":
            quote = character
        elif character == "=":
            break
        elif character in "\r\n#":
            raise SystemExit(f"refusing to modify unrecognized TOML assignment in {path}")
        index += 1
    if index == len(text):
        raise SystemExit(f"refusing to modify incomplete TOML assignment in {path}")

    key = decoded_key_path(text[start:index].strip())
    value_start = index + 1
    while value_start < len(text) and text[value_start] in " \t":
        value_start += 1

    index = value_start
    state = None
    square_depth = 0
    brace_depth = 0
    while index < len(text):
        if state == "basic":
            if text[index] == "\\":
                index = min(index + 2, len(text))
            elif text[index] == '"':
                state = None
                index += 1
            else:
                index += 1
            continue
        if state == "literal":
            if text[index] == "'":
                state = None
            index += 1
            continue
        if state in {"multiline-basic", "multiline-literal"}:
            delimiter = '\"\"\"' if state == "multiline-basic" else "'''"
            quote_character = delimiter[0]
            if state == "multiline-basic" and text[index] == "\\":
                index = min(index + 2, len(text))
                continue
            if text.startswith(delimiter, index):
                while index < len(text) and text[index] == quote_character:
                    index += 1
                state = None
            else:
                index += 1
            continue

        if text.startswith('\"\"\"', index):
            state = "multiline-basic"
            index += 3
        elif text.startswith("'''", index):
            state = "multiline-literal"
            index += 3
        elif text[index] == '"':
            state = "basic"
            index += 1
        elif text[index] == "'":
            state = "literal"
            index += 1
        elif text[index] == "[":
            square_depth += 1
            index += 1
        elif text[index] == "]":
            square_depth -= 1
            index += 1
        elif text[index] == "{":
            brace_depth += 1
            index += 1
        elif text[index] == "}":
            brace_depth -= 1
            index += 1
        elif text[index] == "#":
            if square_depth == 0 and brace_depth == 0:
                break
            index = line_end(index)
        elif text[index] in "\r\n" and square_depth == 0 and brace_depth == 0:
            break
        else:
            index += 1

    value_end = index
    while value_end > value_start and text[value_end - 1] in " \t":
        value_end -= 1
    return key, value_start, value_end, line_end(index)


assignments = []
headers = []
current_table = ()
current_array_table = False
index = 0
while index < len(text):
    statement_start = index
    while statement_start < len(text) and text[statement_start] in " \t":
        statement_start += 1
    if statement_start == len(text):
        break
    if text[statement_start] in "\r\n":
        index = line_end(statement_start)
        continue
    if text[statement_start] == "#":
        index = line_end(statement_start)
        continue
    if text[statement_start] == "[":
        if headers:
            headers[-1]["section_end"] = statement_start
        table_path, current_array_table, index = scan_header(statement_start)
        current_table = table_path
        headers.append({
            "path": table_path,
            "array": current_array_table,
            "start": statement_start,
            "section_end": len(text),
        })
        continue
    key_path, value_start, value_end, index = scan_assignment(statement_start)
    assignments.append({
        "path": current_table + key_path,
        "context": current_table,
        "array": current_array_table,
        "value_start": value_start,
        "value_end": value_end,
    })
if headers:
    headers[-1]["section_end"] = len(text)

first_header = headers[0]["start"] if headers else len(text)
azure_path = ("model_providers", "azure")
replacements = []
insertions = {}


def replacement_for(target_path, value):
    matches = [item for item in assignments if item["path"] == target_path]
    if len(matches) > 1 or (matches and matches[0]["array"]):
        raise SystemExit(f"refusing to modify ambiguous TOML setting {'.'.join(target_path)} in {path}")
    if not matches:
        return False
    item = matches[0]
    replacements.append((item["value_start"], item["value_end"], json.dumps(value, ensure_ascii=False)))
    return True


def add_group(position, lines):
    insertions.setdefault(position, []).append(lines)


missing_root = []
for key, value in root_values.items():
    if not replacement_for((key,), value):
        if key in parsed_existing:
            raise SystemExit(f"refusing to modify non-source TOML setting {key} in {path}")
        missing_root.append(f"{key} = {json.dumps(value, ensure_ascii=False)}")
if missing_root:
    add_group(first_header, missing_root)

model_providers = parsed_existing.get("model_providers", {})
if not isinstance(model_providers, dict):
    raise SystemExit(f"refusing to replace non-table model_providers in {path}")
provider = model_providers.get("azure")
if provider is not None and not isinstance(provider, dict):
    raise SystemExit(f"refusing to replace non-table Azure provider in {path}")

missing_provider = []
for key, value in provider_values.items():
    if not replacement_for(azure_path + (key,), value):
        if isinstance(provider, dict) and key in provider:
            raise SystemExit(f"refusing to modify inline Azure provider setting {key} in {path}")
        missing_provider.append(key)

if missing_provider:
    explicit_provider_tables = [
        header for header in headers
        if header["path"] == azure_path and not header["array"]
    ]
    if len(explicit_provider_tables) > 1:
        raise SystemExit(f"refusing to modify duplicate Azure provider tables in {path}")
    if explicit_provider_tables:
        header = explicit_provider_tables[0]
        add_group(header["section_end"], [
            f"{key} = {json.dumps(provider_values[key], ensure_ascii=False)}"
            for key in missing_provider
        ])
    elif provider is None:
        add_group(len(text), [
            "[model_providers.azure]",
            *(
                f"{key} = {json.dumps(provider_values[key], ensure_ascii=False)}"
                for key in missing_provider
            ),
        ])
    else:
        provider_children = [
            item for item in assignments
            if not item["array"]
            and len(item["path"]) > len(azure_path)
            and item["path"][:len(azure_path)] == azure_path
            and azure_path[:len(item["context"])] == item["context"]
        ]
        if not provider_children:
            raise SystemExit(f"refusing to extend inline Azure provider table in {path}")
        context = max((item["context"] for item in provider_children), key=len)
        if context:
            context_headers = [
                header for header in headers
                if header["path"] == context and not header["array"]
            ]
            if len(context_headers) != 1:
                raise SystemExit(f"refusing to extend ambiguous Azure provider context in {path}")
            insertion_position = context_headers[0]["section_end"]
        else:
            insertion_position = first_header
        relative_provider = azure_path[len(context):]
        add_group(insertion_position, [
            f"{'.'.join((*relative_provider, key))} = {json.dumps(provider_values[key], ensure_ascii=False)}"
            for key in missing_provider
        ])

newline = "\r\n" if "\r\n" in text else "\n"
for position, groups in insertions.items():
    payload = (newline * 2).join(newline.join(group) for group in groups)
    if position > 0 and text[position - 1] not in "\r\n":
        payload = newline + payload
    if position < len(text) or text.endswith(("\n", "\r")):
        payload += newline
    replacements.append((position, position, payload))

ordered = sorted(replacements, key=lambda item: (item[0], item[1]))
for previous, current in zip(ordered, ordered[1:]):
    if previous[1] > current[0]:
        raise SystemExit(f"refusing to apply overlapping TOML edits to {path}")
rendered = text
for start, end, replacement in reversed(ordered):
    rendered = rendered[:start] + replacement + rendered[end:]

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

expected = copy.deepcopy(parsed_existing)
expected.update(root_values)
expected.setdefault("model_providers", {}).setdefault("azure", {}).update(provider_values)
if parsed != expected:
    raise SystemExit(f"refusing to write Codex config after unrelated TOML values changed in {path}")

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
