#!/usr/bin/env python3
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        print("usage: run-with-timeout.py <seconds> -- <command> [args...]", file=sys.stderr)
        return 2

    try:
        timeout_seconds = float(sys.argv[1])
    except ValueError:
        print(f"invalid timeout: {sys.argv[1]}", file=sys.stderr)
        return 2

    command = sys.argv[3:]
    process = subprocess.Popen(command)
    try:
        return process.wait(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        print(f"command timed out after {timeout_seconds:g}s", file=sys.stderr)
        process.terminate()
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        return 124


if __name__ == "__main__":
    raise SystemExit(main())
