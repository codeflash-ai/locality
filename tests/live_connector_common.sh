#!/usr/bin/env bash

live_connector_common_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
live_connector_repo_root="$(cd "$live_connector_common_dir/.." && pwd)"

live_fail() {
  echo "$*" >&2
  return 1
}

_live_report_retained_log() {
  local label="$1"
  local log_path="${2:-}"
  if [[ -n "$log_path" ]]; then
    echo "$label log retained at: $log_path" >&2
  fi
}

_live_debug_sanitize_file() {
  local path="$1"
  local mode="${2:-text}"
  local max_lines="${3:-${LOCALITY_LIVE_DEBUG_TAIL_LINES:-160}}"

  if [[ ! -s "$path" ]]; then
    return 0
  fi

  python3 - "$path" "$mode" "$max_lines" <<'PY'
import json
import os
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
max_lines = int(sys.argv[3])

SENSITIVE_ENV_NAME = re.compile(
    r"(TOKEN|SECRET|CREDENTIAL|PASSWORD|API_KEY|AUTH|ISSUE_ID|"
    r"CONVERSATION_ID|TO_EMAIL|EMAIL|REFRESH)",
    re.IGNORECASE,
)
SENSITIVE_KEY_FRAGMENTS = (
    "token",
    "secret",
    "credential",
    "authorization",
    "api_key",
    "password",
    "refresh",
    "client_secret",
    "cookie",
)
IDENTIFIER_KEYS = {
    "id",
    "remote_id",
    "remote_ids",
    "changed_remote_ids",
    "message_id",
    "thread_id",
    "draft_id",
    "conversation_id",
    "issue_id",
    "document_id",
    "event_id",
    "channel_id",
}
PII_KEYS = {
    "email",
    "to",
    "from",
    "cc",
    "bcc",
    "recipient",
    "recipients",
    "account_label",
}


def sensitive_env_values():
    values = []
    for name, value in os.environ.items():
        if not value or not SENSITIVE_ENV_NAME.search(name):
            continue
        value = str(value)
        if len(value) < 4 or value in {"true", "false", "none", "null"}:
            continue
        values.append(value)
    values.sort(key=len, reverse=True)
    return values


ENV_VALUES = sensitive_env_values()


def redact_text(text):
    if not text:
        return text
    for value in ENV_VALUES:
        text = text.replace(value, "<redacted>")
    replacements = [
        (r"(?i)(authorization:\s*bearer\s+)[^\s]+", r"\1<redacted>"),
        (r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+", r"\1<redacted>"),
        (r"lin_api_[A-Za-z0-9]+", "<redacted>"),
        (r"ya29\.[A-Za-z0-9._-]+", "<redacted>"),
        (r"xox[a-zA-Z]-[A-Za-z0-9-]+", "<redacted>"),
        (r"gh[pousr]_[A-Za-z0-9_]+", "<redacted>"),
        (r"AIza[0-9A-Za-z_-]{20,}", "<redacted>"),
        (
            r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b",
            "<redacted>",
        ),
        (
            r'(?i)("?(?:access_token|refresh_token_handle|api_key|token|secret|credential|authorization|client_secret|password)"?\s*[:=]\s*)("[^"]*"|[^,\s}]+)',
            r"\1<redacted>",
        ),
    ]
    for pattern, replacement in replacements:
        text = re.sub(pattern, replacement, text)
    return text


def is_sensitive_key(key):
    lowered = str(key).lower()
    return any(fragment in lowered for fragment in SENSITIVE_KEY_FRAGMENTS)


def sanitize_json(value, key=""):
    key_lower = str(key).lower()
    if isinstance(value, dict):
        return {k: sanitize_json(v, k) for k, v in value.items()}
    if isinstance(value, list):
        if key_lower in IDENTIFIER_KEYS or key_lower in PII_KEYS or is_sensitive_key(key):
            return ["<redacted>" for _ in value]
        return [sanitize_json(item, key) for item in value]
    if isinstance(value, str):
        if key_lower in IDENTIFIER_KEYS or key_lower in PII_KEYS or is_sensitive_key(key):
            return "<redacted>"
        return redact_text(value)
    return value


try:
    raw_text = path.read_text(encoding="utf-8", errors="replace")
except OSError as error:
    print(f"could not read {path}: {error}", file=sys.stderr)
    raise SystemExit(0)

if mode == "json":
    try:
        parsed = json.loads(raw_text)
    except Exception:
        text = raw_text
    else:
        text = json.dumps(sanitize_json(parsed), indent=2, sort_keys=True)
        max_bytes = int(os.environ.get("LOCALITY_LIVE_DEBUG_JSON_BYTES", "20000"))
        if len(text.encode("utf-8")) > max_bytes:
            text = text.encode("utf-8")[:max_bytes].decode("utf-8", errors="replace")
            text += "\n... <truncated>"
else:
    lines = raw_text.splitlines()
    if max_lines > 0 and len(lines) > max_lines:
        text = f"... showing last {max_lines} of {len(lines)} lines ...\n"
        text += "\n".join(lines[-max_lines:])
    else:
        text = "\n".join(lines)

print(redact_text(text))
PY
}

_live_debug_emit_file() {
  local label="$1"
  local path="$2"
  local mode="${3:-text}"
  local max_lines="${4:-${LOCALITY_LIVE_DEBUG_TAIL_LINES:-160}}"

  if [[ -z "$path" || ! -s "$path" ]]; then
    return 0
  fi

  {
    echo "::group::privacy-safe diagnostics: $label"
    echo "path: $path"
    _live_debug_sanitize_file "$path" "$mode" "$max_lines"
    echo "::endgroup::"
  } >&2
}

_live_debug_emit_var_file() {
  local var_name="$1"
  local label="$2"
  local mode="${3:-text}"
  local max_lines="${4:-${LOCALITY_LIVE_DEBUG_TAIL_LINES:-160}}"
  local path="${!var_name:-}"

  _live_debug_emit_file "$label" "$path" "$mode" "$max_lines"
}

emit_live_debug_diagnostics() {
  local label="${1:-live connector}"
  local report_var
  local log_var

  echo "privacy-safe diagnostics: $label" >&2
  for report_var in \
    mount_report \
    connect_report \
    initial_pull_report \
    first_pull_report \
    second_pull_report \
    diff_report \
    push_report \
    pull_after_push_report \
    edit_diff_report \
    edit_push_report \
    pull_after_edit_report \
    restore_diff_report \
    restore_push_report \
    cleanup_diff_report \
    cleanup_push_report \
    cleanup_pull_report \
    status_report \
    info_report \
    doctor_report \
    issue_search_report \
    drive_search_report \
    calendar_search_report \
    drafts_list_report \
    draft_get_report; do
    _live_debug_emit_var_file "$report_var" "$report_var" json
  done

  for log_var in command_log daemon_log fuse_log; do
    _live_debug_emit_var_file "$log_var" "$log_var" text
  done
}

require_live_env() {
  local name
  for name in "$@"; do
    if [[ ! "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
      live_fail "invalid environment variable name: $name"
      return 1
    fi
    if [[ -z "${!name:-}" ]]; then
      live_fail "missing $name"
      return 1
    fi
  done
}

require_linux_fuse() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    live_fail "live connector Linux FUSE tests require Linux"
    return 1
  fi
  if [[ ! -e /dev/fuse ]]; then
    live_fail "/dev/fuse is not available on this runner"
    return 1
  fi
  if ! command -v fusermount3 >/dev/null 2>&1; then
    live_fail "fusermount3 is not installed"
    return 1
  fi
  if ! command -v mountpoint >/dev/null 2>&1; then
    live_fail "mountpoint is not installed"
    return 1
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    live_fail "python3 is not installed"
    return 1
  fi
  if ! command -v sqlite3 >/dev/null 2>&1; then
    live_fail "sqlite3 is not installed"
    return 1
  fi
}

sql_text_literal() {
  local hex
  hex="$(printf '%s' "$1" | od -An -tx1 -v | tr -d ' \n')"
  if [[ -z "$hex" ]]; then
    printf "''"
  else
    printf "CAST(X'%s' AS TEXT)" "$hex"
  fi
}

_sql_nullable_text_literal() {
  if [[ $# -eq 0 || -z "${1:-}" ]]; then
    printf "NULL"
  else
    sql_text_literal "$1"
  fi
}

secret_hex_name() {
  printf '%s' "$1" | od -An -tx1 -v | tr -d ' \n'
}

credential_file_path() {
  local state_root="$1"
  local secret_ref="$2"
  printf '%s/credentials/%s\n' "$state_root" "$(secret_hex_name "$secret_ref")"
}

write_file_credential() {
  local state_root="$1"
  local secret_ref="$2"
  local secret="$3"
  local credential_dir
  local secret_name
  local secret_path
  local temp_path

  credential_dir="$state_root/credentials"
  secret_name="$(secret_hex_name "$secret_ref")"
  secret_path="$credential_dir/$secret_name"
  mkdir -p "$credential_dir"
  temp_path="$(mktemp "$credential_dir/.${secret_name}.tmp.XXXXXX")"
  printf '%s' "$secret" >"$temp_path"
  chmod 600 "$temp_path" >/dev/null 2>&1 || true
  mv "$temp_path" "$secret_path"
}

credential_access_token() {
  local secret_path
  if [[ $# -eq 1 ]]; then
    secret_path="$1"
  elif [[ $# -eq 2 ]]; then
    secret_path="$(credential_file_path "$1" "$2")"
  else
    live_fail "credential_access_token requires a credential path or state root plus secret ref"
    return 1
  fi
  python3 - "$secret_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
secret = path.read_text(encoding="utf-8").strip()
if not secret:
    raise SystemExit(f"credential secret at {path} is empty")
try:
    parsed = json.loads(secret)
except json.JSONDecodeError as error:
    if secret.lstrip().startswith(("{", "[")):
        raise SystemExit(f"credential secret at {path} was not valid JSON: {error}") from error
    token = secret
else:
    if not isinstance(parsed, dict):
        raise SystemExit(f"credential secret at {path} must be a JSON object or plain secret")
    token = (
        parsed.get("access_token")
        or parsed.get("token")
        or parsed.get("api_key")
        or ""
    )
token = str(token).strip()
if not token:
    raise SystemExit(f"credential secret at {path} did not contain an access token")
print(token)
PY
}

require_oauth_credential_file() {
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    live_fail "require_oauth_credential_file requires a credential path, connector, and optional label"
    return 1
  fi

  local secret_path="$1"
  local connector="$2"
  local label="${3:-$connector OAuth credential}"

  python3 - "$secret_path" "$connector" "$label" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
connector = sys.argv[2]
label = sys.argv[3]

def fail(message):
    raise SystemExit(f"{label} {message}")

try:
    text = path.read_text(encoding="utf-8").strip()
except OSError as error:
    fail(f"could not be read from {path}: {error}")
if not text:
    fail("is empty")
try:
    credential = json.loads(text)
except json.JSONDecodeError as error:
    fail(f"is not valid JSON: {error}")
if not isinstance(credential, dict):
    fail("must be a JSON object")

if credential.get("kind") != "oauth":
    fail("must have kind=oauth")
if credential.get("connector") != connector:
    fail(f"must have connector={connector}")

for field in ("access_token", "oauth_broker_url", "refresh_token_handle"):
    value = credential.get(field)
    if not isinstance(value, str) or not value.strip():
        fail(f"missing {field}")

expires_at = credential.get("expires_at")
if isinstance(expires_at, bool) or not isinstance(expires_at, (int, float)) or expires_at <= 0:
    fail("missing numeric expires_at")
PY
}

force_oauth_credential_refresh() {
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    live_fail "force_oauth_credential_refresh requires a credential path, connector, and optional label"
    return 1
  fi

  local secret_path="$1"
  local connector="$2"
  local label="${3:-$connector OAuth credential}"

  require_oauth_credential_file "$secret_path" "$connector" "$label"
  python3 - "$secret_path" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
credential = json.loads(path.read_text(encoding="utf-8"))
credential["acquired_at"] = 1
credential["expires_at"] = 1
path.write_text(
    json.dumps(credential, separators=(",", ":"), sort_keys=True),
    encoding="utf-8",
)
PY
  chmod 600 "$secret_path" >/dev/null 2>&1 || true
}

oauth_credential_refresh_marker() {
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    live_fail "oauth_credential_refresh_marker requires a credential path, connector, and optional label"
    return 1
  fi

  local secret_path="$1"
  local connector="$2"
  local label="${3:-$connector OAuth credential}"

  require_oauth_credential_file "$secret_path" "$connector" "$label"
  python3 - "$secret_path" <<'PY'
import hashlib
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
credential = json.loads(path.read_text(encoding="utf-8"))

def digest(value):
    return hashlib.sha256(str(value).encode("utf-8")).hexdigest()

marker = {
    "access_token_sha256": digest(credential.get("access_token", "")),
    "acquired_at": credential.get("acquired_at"),
    "expires_at": credential.get("expires_at"),
    "refresh_token_handle_sha256": digest(credential.get("refresh_token_handle", "")),
}
print(json.dumps(marker, separators=(",", ":"), sort_keys=True))
PY
}

assert_oauth_credential_refreshed() {
  if [[ $# -lt 3 || $# -gt 4 ]]; then
    live_fail "assert_oauth_credential_refreshed requires a credential path, connector, marker JSON, and optional label"
    return 1
  fi

  local secret_path="$1"
  local connector="$2"
  local marker_json="$3"
  local label="${4:-$connector OAuth credential}"

  require_oauth_credential_file "$secret_path" "$connector" "$label"
  python3 - "$secret_path" "$marker_json" "$label" <<'PY'
import hashlib
import json
import pathlib
import sys
import time

path = pathlib.Path(sys.argv[1])
marker_json = sys.argv[2]
label = sys.argv[3]

def fail(message):
    raise SystemExit(f"{label} {message}")

try:
    marker = json.loads(marker_json)
except json.JSONDecodeError as error:
    fail(f"refresh marker was not valid JSON: {error}")
if not isinstance(marker, dict):
    fail("refresh marker must be a JSON object")

credential = json.loads(path.read_text(encoding="utf-8"))

def digest(value):
    return hashlib.sha256(str(value).encode("utf-8")).hexdigest()

old_access_digest = marker.get("access_token_sha256")
new_access_digest = digest(credential.get("access_token", ""))
if old_access_digest and old_access_digest == new_access_digest:
    fail("was not refreshed: access_token did not change")

old_acquired_at = marker.get("acquired_at")
new_acquired_at = credential.get("acquired_at")
if (
    isinstance(old_acquired_at, bool)
    or isinstance(new_acquired_at, bool)
    or not isinstance(old_acquired_at, (int, float))
    or not isinstance(new_acquired_at, (int, float))
    or new_acquired_at <= old_acquired_at
):
    fail("was not refreshed: acquired_at did not advance")

old_expires_at = marker.get("expires_at")
new_expires_at = credential.get("expires_at")
if (
    isinstance(old_expires_at, bool)
    or isinstance(new_expires_at, bool)
    or not isinstance(old_expires_at, (int, float))
    or not isinstance(new_expires_at, (int, float))
    or new_expires_at <= old_expires_at
):
    fail("was not refreshed: expires_at did not advance")

if new_expires_at <= int(time.time()) + 60:
    fail("was refreshed but expires_at is still within the refresh window")
PY
}

export_oauth_credential_file() {
  if [[ $# -lt 3 || $# -gt 4 ]]; then
    live_fail "export_oauth_credential_file requires a source path, destination path, connector, and optional label"
    return 1
  fi

  local source_path="$1"
  local destination_path="$2"
  local connector="$3"
  local label="${4:-$connector OAuth credential}"
  local destination_dir
  local temp_path

  require_oauth_credential_file "$source_path" "$connector" "$label"
  destination_dir="$(dirname "$destination_path")"
  mkdir -p "$destination_dir"
  temp_path="$(mktemp "$destination_dir/.oauth-credential.XXXXXX")"
  cp "$source_path" "$temp_path"
  chmod 600 "$temp_path" >/dev/null 2>&1 || true
  mv "$temp_path" "$destination_path"
}

export_refreshed_oauth_credential_if_requested() {
  if [[ $# -lt 3 || $# -gt 4 ]]; then
    live_fail "export_refreshed_oauth_credential_if_requested requires a source path, connector, marker JSON, and optional label"
    return 1
  fi

  local source_path="$1"
  local connector="$2"
  local marker_json="$3"
  local label="${4:-$connector OAuth credential}"

  if [[ -z "${LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT:-}" || -z "$marker_json" ]]; then
    return 0
  fi
  if [[ -s "$LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT" ]]; then
    return 0
  fi
  assert_oauth_credential_refreshed "$source_path" "$connector" "$marker_json" "$label"
  export_oauth_credential_file \
    "$source_path" \
    "$LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT" \
    "$connector" \
    "$label"
}

json_field() {
  local json_source="$1"
  local field_path="$2"
  python3 - "$json_source" "$field_path" <<'PY'
import json
import os
import pathlib
import sys

source = sys.argv[1]
field_path = sys.argv[2]
if os.path.isfile(source):
    text = pathlib.Path(source).read_text(encoding="utf-8")
else:
    text = source
value = json.loads(text)
if field_path:
    for part in field_path.split("."):
        if isinstance(value, list) and part.isdigit():
            value = value[int(part)]
        elif isinstance(value, dict) and part in value:
            value = value[part]
        else:
            raise SystemExit(f"JSON field `{field_path}` was not found")
if isinstance(value, bool):
    print("true" if value else "false")
elif value is None:
    print("")
elif isinstance(value, (dict, list)):
    print(json.dumps(value, separators=(",", ":"), sort_keys=True))
else:
    print(value)
PY
}

assert_json_ok() {
  local report_path="$1"
  local label="${2:-JSON}"
  python3 - "$report_path" "$label" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
label = sys.argv[2]
try:
    text = path.read_text(encoding="utf-8")
except OSError as error:
    raise SystemExit(f"{label} could not be read from {path}: {error}") from error
try:
    report = json.loads(text)
except Exception as error:
    raise SystemExit(f"{label} was not valid JSON at {path}: {error}") from error
if not isinstance(report, dict):
    raise SystemExit(f"{label} did not report ok=true at {path}: top-level JSON was not an object")
if report.get("ok") is not True:
    details = []
    for key in ("ok", "status", "code", "error_code"):
        value = report.get(key)
        if isinstance(value, (str, int, float, bool)) or value is None:
            details.append(f"{key}={value!r}")
    error = report.get("error")
    if isinstance(error, dict):
        code = error.get("code")
        if isinstance(code, (str, int, float, bool)) or code is None:
            details.append(f"error.code={code!r}")
    suffix = f" ({', '.join(details)})" if details else ""
    raise SystemExit(f"{label} did not report ok=true at {path}{suffix}")
PY
}

_assert_json_text_valid() {
  local json_text="$1"
  local label="${2:-JSON}"
  python3 - "$json_text" "$label" <<'PY'
import json
import sys

text = sys.argv[1]
label = sys.argv[2]
try:
    json.loads(text)
except Exception as error:
    raise SystemExit(f"{label} was not valid JSON: {error}") from error
PY
}

connector_profile_id() {
  case "$1" in
    google-docs) printf '%s\n' "google-docs-oauth-default" ;;
    google-calendar) printf '%s\n' "google-calendar-oauth-default" ;;
    gmail) printf '%s\n' "gmail-oauth-default" ;;
    slack) printf '%s\n' "slack-oauth-default" ;;
    granola) printf '%s\n' "granola-api-key-default" ;;
    linear) printf '%s\n' "linear-api-key-default" ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

_live_is_connector() {
  case "$1" in
    google-docs | google-calendar | gmail | slack | granola | linear)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

connector_auth_kind() {
  case "$1" in
    google-docs | google-calendar | gmail | slack) printf '%s\n' "oauth" ;;
    granola | linear) printf '%s\n' "api_key" ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

connector_scopes_json() {
  case "$1" in
    google-docs)
      printf '%s\n' '["openid","email","profile","https://www.googleapis.com/auth/documents","https://www.googleapis.com/auth/drive.file","https://www.googleapis.com/auth/drive.metadata"]'
      ;;
    google-calendar)
      printf '%s\n' '["openid","email","profile","https://www.googleapis.com/auth/calendar.events"]'
      ;;
    gmail)
      printf '%s\n' '["openid","email","profile","https://www.googleapis.com/auth/gmail.readonly","https://www.googleapis.com/auth/gmail.compose"]'
      ;;
    slack)
      printf '%s\n' '["channels:read","channels:history","groups:read","groups:history","im:read","im:history","mpim:read","mpim:history","users:read","team:read","files:read","channels:join"]'
      ;;
    granola)
      printf '%s\n' '["read"]'
      ;;
    linear)
      printf '%s\n' '["issues:read","issues:write"]'
      ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

_connector_capabilities_default_json() {
  local block_updates="$1"
  local entity_body_updates="$2"
  local databases="$3"
  local oauth="$4"
  local remote_observation="$5"
  local lazy_child_enumeration="$6"
  local media_download="$7"
  local undo="$8"
  local batch_observation="$9"
  printf '{"supports_block_updates":%s,"supports_entity_body_updates":%s,"supports_databases":%s,"supports_oauth":%s,"supports_remote_observation":%s,"supports_lazy_child_enumeration":%s,"supports_media_download":%s,"supports_undo":%s,"supports_batch_observation":%s}\n' \
    "$block_updates" \
    "$entity_body_updates" \
    "$databases" \
    "$oauth" \
    "$remote_observation" \
    "$lazy_child_enumeration" \
    "$media_download" \
    "$undo" \
    "$batch_observation"
}

connector_capabilities_json() {
  case "$1" in
    google-docs)
      _connector_capabilities_default_json true false false true true true false false false
      ;;
    google-calendar | gmail)
      _connector_capabilities_default_json false false false true true true false false false
      ;;
    slack)
      _connector_capabilities_default_json false false false true true true false false false
      ;;
    granola)
      _connector_capabilities_default_json false false false false true true false false false
      ;;
    linear)
      _connector_capabilities_default_json false true false false true true true false true
      ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

connector_enabled_actions_json() {
  case "$1" in
    google-docs) printf '%s\n' '["read","write"]' ;;
    google-calendar) printf '%s\n' '["read","create"]' ;;
    gmail) printf '%s\n' '["read","send"]' ;;
    slack) printf '%s\n' '[]' ;;
    granola) printf '%s\n' '["read"]' ;;
    linear) printf '%s\n' '["read","write"]' ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

connector_version() {
  case "$1" in
    google-docs | google-calendar | gmail | slack | granola | linear)
      printf '%s.v1\n' "$1"
      ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

connector_display_name() {
  case "$1" in
    google-docs) printf '%s\n' "Google Docs OAuth" ;;
    google-calendar) printf '%s\n' "Google Calendar OAuth" ;;
    gmail) printf '%s\n' "Gmail OAuth" ;;
    slack) printf '%s\n' "Slack OAuth" ;;
    granola) printf '%s\n' "Granola API key" ;;
    linear) printf '%s\n' "Linear API key" ;;
    *) live_fail "unsupported live connector: $1" ;;
  esac
}

_live_resolve_bin() {
  local candidate="$1"
  if [[ "$candidate" = /* ]]; then
    printf '%s\n' "$candidate"
  elif [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
  elif [[ -x "$live_connector_repo_root/$candidate" ]]; then
    printf '%s\n' "$live_connector_repo_root/$candidate"
  else
    printf '%s\n' "$candidate"
  fi
}

_live_arg_looks_like_bin() {
  local candidate="$1"
  local expected_name="$2"
  local basename
  basename="${candidate%/}"
  basename="${basename##*/}"
  [[ ! -d "$candidate" && ( -x "$candidate" || "$basename" == "$expected_name" ) ]]
}

_live_loc_bin() {
  _live_resolve_bin "${loc_bin:-${LOCALITY_BIN:-./target/debug/loc}}"
}

_live_localityd_bin() {
  _live_resolve_bin "${localityd_bin:-${LOCALITYD_BIN:-./target/debug/localityd}}"
}

_live_fuse_bin() {
  _live_resolve_bin "${fuse_bin:-${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}}"
}

init_live_state() {
  local state_root
  local resolved_loc_bin
  if [[ $# -eq 1 ]]; then
    state_root="$1"
    resolved_loc_bin="$(_live_loc_bin)"
  elif [[ $# -eq 2 ]]; then
    resolved_loc_bin="$(_live_resolve_bin "$1")"
    state_root="$2"
  else
    live_fail "init_live_state requires a state root or loc binary plus state root"
    return 1
  fi
  if [[ ! -x "$resolved_loc_bin" ]]; then
    live_fail "loc binary is not executable: $resolved_loc_bin"
  fi
  mkdir -p "$state_root"
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
    "$resolved_loc_bin" connections --json >/dev/null
}

seed_connector_credential() {
  local resolved_loc_bin=""
  local state_root
  local connector
  local connection_id
  local credential_secret
  if [[ $# -ge 5 ]] && ! _live_is_connector "${2:-}"; then
    resolved_loc_bin="$(_live_resolve_bin "$1")"
    state_root="$2"
    connector="$3"
    connection_id="$4"
    credential_secret="$5"
    shift 5
  elif [[ $# -ge 4 ]]; then
    state_root="$1"
    connector="$2"
    connection_id="$3"
    credential_secret="$4"
    shift 4
  else
    live_fail "seed_connector_credential requires connector credential details"
    return 1
  fi
  local display_name="${1:-$connection_id}"
  local account_label="${2:-}"
  local workspace_id="${3:-}"
  local workspace_name="${4:-}"
  local expires_at="${5:-}"

  if [[ -n "$resolved_loc_bin" ]]; then
    init_live_state "$resolved_loc_bin" "$state_root"
  else
    init_live_state "$state_root"
  fi

  local db="$state_root/state.sqlite3"
  local secret_ref="connection:$connection_id"
  local profile_id
  local auth_kind
  local scopes_json
  local capabilities_json
  local enabled_actions_json
  local version
  local profile_display_name
  local now

  profile_id="$(connector_profile_id "$connector")"
  auth_kind="$(connector_auth_kind "$connector")"
  scopes_json="$(connector_scopes_json "$connector")"
  capabilities_json="$(connector_capabilities_json "$connector")"
  enabled_actions_json="$(connector_enabled_actions_json "$connector")"
  version="$(connector_version "$connector")"
  profile_display_name="$(connector_display_name "$connector")"
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  _assert_json_text_valid "$scopes_json" "$connector scopes JSON"
  _assert_json_text_valid "$capabilities_json" "$connector capabilities JSON"
  _assert_json_text_valid "$enabled_actions_json" "$connector enabled actions JSON"
  write_file_credential "$state_root" "$secret_ref" "$credential_secret"

  local profile_id_sql
  local connector_sql
  local profile_display_name_sql
  local auth_kind_sql
  local scopes_json_sql
  local capabilities_json_sql
  local enabled_actions_json_sql
  local version_sql
  local active_sql
  local now_sql
  local connection_id_sql
  local display_name_sql
  local account_label_sql
  local workspace_id_sql
  local workspace_name_sql
  local secret_ref_sql
  local expires_at_sql

  profile_id_sql="$(sql_text_literal "$profile_id")"
  connector_sql="$(sql_text_literal "$connector")"
  profile_display_name_sql="$(sql_text_literal "$profile_display_name")"
  auth_kind_sql="$(sql_text_literal "$auth_kind")"
  scopes_json_sql="$(sql_text_literal "$scopes_json")"
  capabilities_json_sql="$(sql_text_literal "$capabilities_json")"
  enabled_actions_json_sql="$(sql_text_literal "$enabled_actions_json")"
  version_sql="$(sql_text_literal "$version")"
  active_sql="$(sql_text_literal "active")"
  now_sql="$(sql_text_literal "$now")"
  connection_id_sql="$(sql_text_literal "$connection_id")"
  display_name_sql="$(sql_text_literal "$display_name")"
  account_label_sql="$(_sql_nullable_text_literal "$account_label")"
  workspace_id_sql="$(_sql_nullable_text_literal "$workspace_id")"
  workspace_name_sql="$(_sql_nullable_text_literal "$workspace_name")"
  secret_ref_sql="$(sql_text_literal "$secret_ref")"
  expires_at_sql="$(_sql_nullable_text_literal "$expires_at")"

  sqlite3 "$db" <<SQL
INSERT INTO connector_profiles (
  profile_id,
  connector,
  display_name,
  auth_kind,
  scopes_json,
  capabilities_json,
  enabled_actions_json,
  connector_version,
  status,
  created_at,
  updated_at
) VALUES (
  $profile_id_sql,
  $connector_sql,
  $profile_display_name_sql,
  $auth_kind_sql,
  $scopes_json_sql,
  $capabilities_json_sql,
  $enabled_actions_json_sql,
  $version_sql,
  $active_sql,
  $now_sql,
  $now_sql
)
ON CONFLICT(profile_id) DO UPDATE SET
  connector = excluded.connector,
  display_name = excluded.display_name,
  auth_kind = excluded.auth_kind,
  scopes_json = excluded.scopes_json,
  capabilities_json = excluded.capabilities_json,
  enabled_actions_json = excluded.enabled_actions_json,
  connector_version = excluded.connector_version,
  status = excluded.status,
  updated_at = excluded.updated_at;

INSERT INTO connections (
  connection_id,
  profile_id,
  connector,
  display_name,
  account_label,
  workspace_id,
  workspace_name,
  auth_kind,
  secret_ref,
  scopes_json,
  capabilities_json,
  status,
  created_at,
  updated_at,
  expires_at
) VALUES (
  $connection_id_sql,
  $profile_id_sql,
  $connector_sql,
  $display_name_sql,
  $account_label_sql,
  $workspace_id_sql,
  $workspace_name_sql,
  $auth_kind_sql,
  $secret_ref_sql,
  $scopes_json_sql,
  $capabilities_json_sql,
  $active_sql,
  $now_sql,
  $now_sql,
  $expires_at_sql
)
ON CONFLICT(connection_id) DO UPDATE SET
  profile_id = excluded.profile_id,
  connector = excluded.connector,
  display_name = excluded.display_name,
  account_label = excluded.account_label,
  workspace_id = excluded.workspace_id,
  workspace_name = excluded.workspace_name,
  auth_kind = excluded.auth_kind,
  secret_ref = excluded.secret_ref,
  scopes_json = excluded.scopes_json,
  capabilities_json = excluded.capabilities_json,
  status = excluded.status,
  updated_at = excluded.updated_at,
  expires_at = excluded.expires_at;
SQL
}

build_live_binaries() {
  local default_loc_bin="$live_connector_repo_root/target/debug/loc"
  local default_localityd_bin="$live_connector_repo_root/target/debug/localityd"
  local default_fuse_bin="$live_connector_repo_root/target/debug/locality-fuse"
  local should_build=0

  if [[ $# -eq 0 ]]; then
    loc_bin="$(_live_loc_bin)"
    localityd_bin="$(_live_localityd_bin)"
    fuse_bin="$(_live_fuse_bin)"
  elif [[ $# -eq 3 ]]; then
    loc_bin="$(_live_resolve_bin "$1")"
    localityd_bin="$(_live_resolve_bin "$2")"
    fuse_bin="$(_live_resolve_bin "$3")"
  else
    live_fail "build_live_binaries requires no arguments or loc, localityd, and FUSE binary paths"
    return 1
  fi

  if [[ "$loc_bin" == "$default_loc_bin" \
      && "$localityd_bin" == "$default_localityd_bin" \
      && "$fuse_bin" == "$default_fuse_bin" ]]; then
    should_build=1
  fi
  if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
    should_build=1
  fi
  if [[ "$should_build" == "1" ]]; then
    (cd "$live_connector_repo_root" && cargo build -p loc-cli -p localityd -p locality-fuse)
  fi

  loc_bin="$(_live_loc_bin)"
  localityd_bin="$(_live_localityd_bin)"
  fuse_bin="$(_live_fuse_bin)"
}

wait_for_daemon() {
  local state_root
  local resolved_loc_bin
  local log_path
  if [[ $# -ge 2 ]] && _live_arg_looks_like_bin "$1" "loc"; then
    resolved_loc_bin="$(_live_resolve_bin "$1")"
    state_root="$2"
    log_path="${3:-${daemon_log:-}}"
  elif [[ $# -ge 1 ]]; then
    state_root="$1"
    resolved_loc_bin="$(_live_resolve_bin "${2:-$(_live_loc_bin)}")"
    log_path="${3:-${daemon_log:-}}"
  else
    live_fail "wait_for_daemon requires a state root or loc binary plus state root"
    return 1
  fi
  local attempts="${LOCALITY_LIVE_DAEMON_WAIT_ATTEMPTS:-120}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if LOCALITY_STATE_DIR="$state_root" "$resolved_loc_bin" daemon status --state-dir "$state_root" --json 2>/dev/null \
      | grep -Eq '"state"[[:space:]]*:[[:space:]]*"running"'; then
      return 0
    fi
    sleep 0.25
  done
  echo "localityd did not become ready" >&2
  _live_report_retained_log "localityd" "$log_path"
  return 1
}

wait_for_fuse() {
  local root="$1"
  local watched_fuse_pid="${2:-${fuse_pid:-}}"
  local log_path="${3:-${fuse_log:-}}"
  local attempts="${LOCALITY_LIVE_FUSE_WAIT_ATTEMPTS:-120}"
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if mountpoint -q "$root"; then
      return 0
    fi
    if [[ -n "$watched_fuse_pid" ]] && ! kill -0 "$watched_fuse_pid" >/dev/null 2>&1; then
      echo "locality-fuse stopped before its mount became ready" >&2
      _live_report_retained_log "locality-fuse" "$log_path"
      return 1
    fi
    sleep 0.25
  done
  echo "locality-fuse did not become ready" >&2
  _live_report_retained_log "locality-fuse" "$log_path"
  return 1
}

start_live_daemon() {
  local state_root
  local log_path
  local resolved_localityd_bin
  if [[ $# -ge 3 ]] && _live_arg_looks_like_bin "$1" "localityd"; then
    resolved_localityd_bin="$(_live_resolve_bin "$1")"
    state_root="$2"
    log_path="$3"
  elif [[ $# -ge 2 ]]; then
    state_root="$1"
    log_path="$2"
    resolved_localityd_bin="$(_live_resolve_bin "${3:-$(_live_localityd_bin)}")"
  else
    live_fail "start_live_daemon requires localityd, state root, and log path"
    return 1
  fi
  mkdir -p "$(dirname "$log_path")"
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off \
    "$resolved_localityd_bin" >"$log_path" 2>&1 &
  localityd_pid="$!"
  printf '%s\n' "$localityd_pid"
}

start_live_fuse() {
  local state_root
  local root
  local log_path
  local resolved_fuse_bin
  if [[ $# -ge 4 ]] && _live_arg_looks_like_bin "$1" "locality-fuse"; then
    resolved_fuse_bin="$(_live_resolve_bin "$1")"
    state_root="$2"
    root="$3"
    log_path="$4"
  elif [[ $# -ge 3 ]]; then
    state_root="$1"
    root="$2"
    log_path="$3"
    resolved_fuse_bin="$(_live_resolve_bin "${4:-$(_live_fuse_bin)}")"
  else
    live_fail "start_live_fuse requires FUSE binary, state root, root, and log path"
    return 1
  fi
  mkdir -p "$(dirname "$log_path")" "$root"
  LOCALITY_STATE_DIR="$state_root" "$resolved_fuse_bin" \
    --state-dir "$state_root" \
    --mountpoint "$root" >"$log_path" 2>&1 &
  fuse_pid="$!"
  printf '%s\n' "$fuse_pid"
}

stop_live_processes() {
  local root="${1:-${locality_root:-${LOCALITY_ROOT:-}}}"
  local watched_fuse_pid="${2:-${fuse_pid:-}}"
  local watched_localityd_pid="${3:-${localityd_pid:-}}"
  local had_errexit=0
  [[ $- == *e* ]] && had_errexit=1
  set +e
  if [[ -n "$root" ]] && command -v mountpoint >/dev/null 2>&1 && mountpoint -q "$root"; then
    fusermount3 -uz "$root" >/dev/null 2>&1
  fi
  if [[ -n "$watched_fuse_pid" ]] && kill -0 "$watched_fuse_pid" >/dev/null 2>&1; then
    kill "$watched_fuse_pid" >/dev/null 2>&1
    wait "$watched_fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "$watched_localityd_pid" ]] && kill -0 "$watched_localityd_pid" >/dev/null 2>&1; then
    kill "$watched_localityd_pid" >/dev/null 2>&1
    wait "$watched_localityd_pid" >/dev/null 2>&1
  fi
  if [[ "$had_errexit" == "1" ]]; then
    set -e
  fi
  return 0
}
