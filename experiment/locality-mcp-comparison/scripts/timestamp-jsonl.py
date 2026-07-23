#!/usr/bin/env python3
import datetime
import json
import os
import sys
import time

FAKE_CLOCK = [
    int(value)
    for value in os.environ.get("TIMESTAMP_JSONL_FAKE_CLOCK_MS", "").split(",")
    if value.strip()
]
FAKE_CLOCK_INDEX = 0
LAST_FAKE_CLOCK_MS = None


def now_ms():
    global FAKE_CLOCK_INDEX, LAST_FAKE_CLOCK_MS
    if FAKE_CLOCK:
        if FAKE_CLOCK_INDEX < len(FAKE_CLOCK):
            value = FAKE_CLOCK[FAKE_CLOCK_INDEX]
            FAKE_CLOCK_INDEX += 1
        else:
            value = (LAST_FAKE_CLOCK_MS or FAKE_CLOCK[-1]) + 1
        LAST_FAKE_CLOCK_MS = value
        return value
    return int(time.time() * 1000)


def iso_ms(value):
    return (
        datetime.datetime.fromtimestamp(value / 1000, datetime.timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


for line in sys.stdin:
    line = line.rstrip("\n")
    if not line:
        continue
    observed_at_ms = now_ms()
    timestamp = iso_ms(observed_at_ms)
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        event = {"type": "unparsed", "raw": line}
    if not isinstance(event, dict):
        event = {"type": "non_object", "value": event}
    print(
        json.dumps(
            {
                "observed_at_ms": observed_at_ms,
                "timestamp": timestamp,
                "created_at": timestamp,
                "event": event,
            },
            separators=(",", ":"),
        ),
        flush=True,
    )
