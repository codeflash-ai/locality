#!/usr/bin/env python3
import datetime
import json
import os
import re
import sys
import time
from pathlib import Path

try:
    import fcntl
except ImportError:  # pragma: no cover - benchmark harness is POSIX today.
    fcntl = None

SENSITIVE_KEY_RE = re.compile(
    r"(api[_-]?key|access[_-]?token|refresh[_-]?token|token|secret|password|authorization|cookie|bearer)",
    re.IGNORECASE,
)
MAX_TOOL_ARG_STRING_CHARS = 4000
MAX_TOOL_ARG_LIST_ITEMS = 100
MAX_TOOL_ARG_DICT_ITEMS = 200
MAX_TOOL_ARG_DEPTH = 8


def main():
    events_path = os.environ.get("CODEX_HARNESS_HOOK_EVENTS_FILE")
    if not events_path:
        return 0

    try:
        payload = json.load(sys.stdin)
    except Exception as error:  # noqa: BLE001
        append_record(
            Path(events_path),
            hook_record(now_ms(), {"type": "harness.hook_error", "error": str(error)}),
        )
        return 0

    if not isinstance(payload, dict):
        payload = {"hook_event_name": "unknown", "value": payload}

    now = now_ms()
    events_path = Path(events_path)
    state_path = Path(
        os.environ.get("CODEX_HARNESS_HOOK_STATE_FILE")
        or f"{events_path}.state.json"
    )

    with locked_state(state_path) as state_file:
        state = state_file.state
        records = records_for_hook(payload, now, state)
        append_records(events_path, records)
        state_file.write(state)

    return 0


def records_for_hook(payload, now, state):
    hook_name = str(payload.get("hook_event_name") or "unknown")
    session_id = str(payload.get("session_id") or "unknown-session")
    turn_id = str(payload.get("turn_id") or "unknown-turn")
    turn_key = f"{session_id}\0{turn_id}"
    records = [hook_record(now, summarized_hook_payload(payload))]

    if hook_name == "SessionStart":
        state.setdefault("sessions", {})[session_id] = {"started_at_ms": now}
        return records

    if hook_name == "UserPromptSubmit":
        session = state.setdefault("sessions", {}).get(session_id, {})
        start_ms = session.get("started_at_ms", now)
        records.append(
            phase_record(
                "input_query",
                "user_query",
                start_ms,
                now,
                timing_quality="measured",
                source=payload,
                extra={
                    "prompt_length": string_length(payload.get("prompt")),
                },
            )
        )
        state.setdefault("turns", {})[turn_key] = {"model_started_at_ms": now}
        return records

    if hook_name == "PreToolUse":
        turn = state.setdefault("turns", {}).setdefault(
            turn_key, {"model_started_at_ms": now}
        )
        model_started_at_ms = turn.pop("model_started_at_ms", None)
        if model_started_at_ms is not None and now > model_started_at_ms:
            records.append(
                phase_record(
                    "thinking",
                    "reasoning",
                    model_started_at_ms,
                    now,
                    timing_quality="measured",
                    source=payload,
                )
            )

        tool_key = tool_state_key(payload)
        state.setdefault("open_tools", {})[tool_key] = {
            "started_at_ms": now,
            "payload": summarized_tool_payload(payload),
        }
        return records

    if hook_name == "PostToolUse":
        tool_key = tool_state_key(payload)
        open_tool = state.setdefault("open_tools", {}).pop(tool_key, None)
        start_ms = open_tool.get("started_at_ms") if open_tool else now
        tool_payload = open_tool.get("payload") if open_tool else summarized_tool_payload(payload)
        records.append(
            phase_record(
                "tool_call",
                "tool",
                start_ms,
                now,
                timing_quality="measured" if open_tool else "inferred",
                source=payload,
                tool=tool_payload,
            )
        )
        state.setdefault("turns", {})[turn_key] = {"model_started_at_ms": now}
        return records

    if hook_name == "Stop":
        turn = state.setdefault("turns", {}).get(turn_key)
        model_started_at_ms = turn.get("model_started_at_ms") if turn else None
        if model_started_at_ms is not None and now > model_started_at_ms:
            records.append(
                phase_record(
                    "output_response",
                    "agent_response",
                    model_started_at_ms,
                    now,
                    timing_quality="measured",
                    source=payload,
                    extra={
                        "last_assistant_message_length": string_length(
                            payload.get("last_assistant_message")
                        ),
                    },
                )
            )
        if turn_key in state.setdefault("turns", {}):
            del state["turns"][turn_key]
        return records

    return records


def tool_state_key(payload):
    return "\0".join(
        [
            str(payload.get("session_id") or "unknown-session"),
            str(payload.get("turn_id") or "unknown-turn"),
            str(payload.get("tool_use_id") or ""),
            str(payload.get("tool_name") or "unknown-tool"),
        ]
    )


def summarized_hook_payload(payload):
    hook_name = str(payload.get("hook_event_name") or "unknown")
    summary = {
        "type": "harness.hook",
        "harness_source": "codex_hook",
        "hook_event_name": hook_name,
        "session_id": payload.get("session_id"),
        "turn_id": payload.get("turn_id"),
        "cwd": payload.get("cwd"),
        "model": payload.get("model"),
        "permission_mode": payload.get("permission_mode"),
    }
    if hook_name in {"PreToolUse", "PostToolUse", "PermissionRequest"}:
        summary.update(summarized_tool_payload(payload))
    if hook_name == "UserPromptSubmit":
        summary["prompt_length"] = string_length(payload.get("prompt"))
    if hook_name == "Stop":
        summary["last_assistant_message_length"] = string_length(
            payload.get("last_assistant_message")
        )
    return summary


def summarized_tool_payload(payload):
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}

    command = tool_input.get("command")
    tool_name = payload.get("tool_name") or "unknown_tool"
    summary = {
        "tool_name": tool_name,
        "tool_call_id": payload.get("tool_use_id"),
        "tool_command": command if isinstance(command, str) else None,
        "command": command if isinstance(command, str) else None,
    }
    if is_mcp_tool(tool_name):
        tool_args = sanitize_tool_args(tool_input)
        summary.update(
            {
                "tool_args": tool_args,
                "tool_args_json": stable_json(tool_args),
                "tool_args_keys": sorted(str(key) for key in tool_args.keys()),
            }
        )
    return summary


def is_mcp_tool(tool_name):
    return isinstance(tool_name, str) and tool_name.startswith("mcp__")


def sanitize_tool_args(value, depth=0):
    if depth >= MAX_TOOL_ARG_DEPTH:
        return "[MAX_DEPTH]"
    if isinstance(value, dict):
        sanitized = {}
        for index, key in enumerate(sorted(value.keys(), key=str)):
            if index >= MAX_TOOL_ARG_DICT_ITEMS:
                sanitized["__truncated_keys__"] = len(value) - MAX_TOOL_ARG_DICT_ITEMS
                break
            key_text = str(key)
            if SENSITIVE_KEY_RE.search(key_text):
                sanitized[key_text] = "[REDACTED]"
            else:
                sanitized[key_text] = sanitize_tool_args(value[key], depth + 1)
        return sanitized
    if isinstance(value, list):
        items = [sanitize_tool_args(item, depth + 1) for item in value[:MAX_TOOL_ARG_LIST_ITEMS]]
        if len(value) > MAX_TOOL_ARG_LIST_ITEMS:
            items.append({"__truncated_items__": len(value) - MAX_TOOL_ARG_LIST_ITEMS})
        return items
    if isinstance(value, str):
        if len(value) > MAX_TOOL_ARG_STRING_CHARS:
            return value[:MAX_TOOL_ARG_STRING_CHARS] + f"...[truncated {len(value) - MAX_TOOL_ARG_STRING_CHARS} chars]"
        return value
    if value is None or isinstance(value, (bool, int, float)):
        return value
    return str(value)


def stable_json(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def phase_record(
    phase,
    activity,
    start_ms,
    end_ms,
    *,
    timing_quality,
    source,
    tool=None,
    extra=None,
):
    event = {
        "type": "harness.phase",
        "harness_source": "codex_hook",
        "phase": phase,
        "activity": activity,
        "span_kind": "codex_hook",
        "status": "completed",
        "started_at_ms": start_ms,
        "ended_at_ms": end_ms,
        "duration_ms": max(1, end_ms - start_ms),
        "timing_quality": timing_quality,
        "session_id": source.get("session_id"),
        "turn_id": source.get("turn_id"),
        "source_hook_event_name": source.get("hook_event_name"),
    }
    if tool:
        event.update(tool)
    if extra:
        event.update(extra)
    return wrapped_record(start_ms, event)


def hook_record(observed_at_ms, event):
    return wrapped_record(observed_at_ms, event)


def wrapped_record(observed_at_ms, event):
    timestamp = iso_ms(observed_at_ms)
    return {
        "observed_at_ms": observed_at_ms,
        "timestamp": timestamp,
        "created_at": timestamp,
        "event": event,
    }


def append_records(path, records):
    if not records:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, separators=(",", ":")) + "\n")


def append_record(path, record):
    append_records(path, [record])


class locked_state:
    def __init__(self, path):
        self.path = path
        self.handle = None
        self.state = {}

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.handle = self.path.open("a+", encoding="utf-8")
        if fcntl is not None:
            fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX)
        self.handle.seek(0)
        text = self.handle.read()
        try:
            self.state = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError:
            self.state = {}
        return self

    def __exit__(self, exc_type, exc, traceback):
        if self.handle is not None:
            if fcntl is not None:
                fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
            self.handle.close()
        return False

    def write(self, state):
        self.handle.seek(0)
        self.handle.truncate()
        self.handle.write(json.dumps(state, separators=(",", ":")) + "\n")
        self.handle.flush()
        os.fsync(self.handle.fileno())


def now_ms():
    fake = os.environ.get("CODEX_HARNESS_HOOK_FAKE_CLOCK_MS")
    if fake:
        path_text = os.environ.get("CODEX_HARNESS_HOOK_FAKE_CLOCK_STATE", "")
        path = Path(path_text) if path_text else None
        values = [int(value) for value in fake.split(",") if value.strip()]
        index = 0
        if path is not None:
            try:
                index = int(path.read_text(encoding="utf-8").strip() or "0")
            except OSError:
                index = 0
        if values:
            value = values[min(index, len(values) - 1)]
            if path is not None:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(str(index + 1), encoding="utf-8")
            return value
    return int(time.time() * 1000)


def iso_ms(value):
    return (
        datetime.datetime.fromtimestamp(value / 1000, datetime.timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


def string_length(value):
    return len(value) if isinstance(value, str) else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001
        events_path = os.environ.get("CODEX_HARNESS_HOOK_EVENTS_FILE")
        if events_path:
            append_record(
                Path(events_path),
                hook_record(now_ms(), {"type": "harness.hook_error", "error": str(error)}),
            )
        raise SystemExit(0)
