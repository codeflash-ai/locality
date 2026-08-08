#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GMAIL_SCENARIO:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GMAIL_SCENARIO=1 to run the full live Gmail workflow scenario"
  echo "note: this scenario sends real email through Gmail; set LOCALITY_GMAIL_LIVE_TO_EMAIL to a safe recipient"
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export LOCALITY_LIVE_GMAIL_VFS=1
export LOCALITY_LIVE_GMAIL_SEND=1

exec "$script_dir/live_gmail_vfs_roundtrip.sh"
