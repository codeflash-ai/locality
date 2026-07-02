#!/usr/bin/env sh
set -eu

# Debian passes "remove" for uninstall and "upgrade" during package upgrades.
# RPM passes 0 for final erase and 1+ when replacing/upgrading.
case "${1:-}" in
  upgrade|1|2|3|4|5)
    exit 0
    ;;
esac

if [ -x /usr/bin/locality-desktop ]; then
  /usr/bin/locality-desktop --prepare-uninstall >/dev/null 2>&1 || true
elif [ -x /usr/bin/Locality ]; then
  /usr/bin/Locality --prepare-uninstall >/dev/null 2>&1 || true
elif [ -x /usr/bin/locality ]; then
  /usr/bin/locality --prepare-uninstall >/dev/null 2>&1 || true
fi

if [ -x /usr/bin/loc ]; then
  /usr/bin/loc daemon stop >/dev/null 2>&1 || true
fi
