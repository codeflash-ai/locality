#!/usr/bin/env python3
import json
import sys
import time

for line in sys.stdin:
    line = line.rstrip("\n")
    if not line:
        continue
    observed_at_ms = int(time.time() * 1000)
    try:
        event = json.loads(line)
    except json.JSONDecodeError:
        event = {"type": "unparsed", "raw": line}
    print(json.dumps({"observed_at_ms": observed_at_ms, "event": event}, separators=(",", ":")), flush=True)
