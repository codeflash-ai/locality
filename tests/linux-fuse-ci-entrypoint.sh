#!/usr/bin/env bash
set -euo pipefail

uid="${LOCALITY_CI_UID:?LOCALITY_CI_UID is required}"
gid="${LOCALITY_CI_GID:?LOCALITY_CI_GID is required}"

if group_name="$(getent group "$gid" | cut -d: -f1)" && [[ -n "$group_name" ]]; then
  :
else
  group_name="locality-ci"
  groupadd --gid "$gid" "$group_name"
fi

if ! getent passwd "$uid" >/dev/null; then
  useradd \
    --uid "$uid" \
    --gid "$gid" \
    --home-dir /tmp/locality-home \
    --no-create-home \
    --shell /bin/bash \
    locality-ci
fi

exec setpriv \
  --reuid "$uid" \
  --regid "$gid" \
  --init-groups \
  "$@"
