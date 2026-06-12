#!/usr/bin/env bash
set -euo pipefail

if [[ "${AFS_FUSE_SMOKE:-}" != "1" ]]; then
  echo "skip: set AFS_FUSE_SMOKE=1 to run the Linux FUSE smoke test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: Linux FUSE smoke test requires Linux"
  exit 0
fi

: "${AFS_FUSE_SMOKE_MOUNT:?set AFS_FUSE_SMOKE_MOUNT to an existing linux_fuse mount root}"

afs_bin="${AFS_BIN:-./target/debug/afs}"
afsd_bin="${AFSD_BIN:-./target/debug/afsd}"
mount_root="$AFS_FUSE_SMOKE_MOUNT"

if [[ ! -x "$afs_bin" || ! -x "$afsd_bin" || ! -x "${AFS_FUSE_BIN:-./target/debug/afs-fuse}" ]]; then
  cargo build -p afsd -p afs-cli -p afs-fuse
fi

"$afs_bin" daemon start --session --afsd-bin "$afsd_bin"
"$afs_bin" file-provider register "$mount_root" --json >/dev/null

findmnt -R "$mount_root" >/dev/null
ls -la "$mount_root" >/dev/null

first_markdown="$(find "$mount_root" -maxdepth 2 -type f -name '*.md' | head -n 1 || true)"
if [[ -n "$first_markdown" ]]; then
  head -n 20 "$first_markdown" >/dev/null
  "$afs_bin" status "$first_markdown" --json >/dev/null
fi

parent_dir="${AFS_FUSE_SMOKE_PARENT_DIR:-}"
if [[ -z "$parent_dir" ]]; then
  parent_dir="$(find "$mount_root" -mindepth 1 -maxdepth 1 -type d | head -n 1 || true)"
fi

if [[ -z "$parent_dir" ]]; then
  echo "skip: no page child or database directory found for create/rename/delete smoke"
  exit 0
fi

draft="$parent_dir/afs-fuse-smoke-$$.md"
renamed="$parent_dir/afs-fuse-smoke-renamed-$$.md"

printf '# FUSE smoke\n\nCreated by tests/linux_fuse_smoke.sh.\n' > "$draft"
mv "$draft" "$renamed"
"$afs_bin" status "$renamed" --json >/dev/null
rm "$renamed"
"$afs_bin" status "$parent_dir" --json >/dev/null

echo "ok: Linux FUSE smoke test completed"
