#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_FUSE_SMOKE:-}" != "1" ]]; then
  echo "skip: set LOCALITY_FUSE_SMOKE=1 to run the Linux FUSE smoke test"
  exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "skip: Linux FUSE smoke test requires Linux"
  exit 0
fi

if [[ ! -e /dev/fuse ]]; then
  message="skip: /dev/fuse is not available on this runner"
  if [[ "${LOCALITY_FUSE_SMOKE_REQUIRED:-}" == "1" ]]; then
    echo "$message" >&2
    exit 1
  fi
  echo "$message"
  exit 0
fi

if ! command -v fusermount3 >/dev/null 2>&1; then
  message="skip: fusermount3 is not installed"
  if [[ "${LOCALITY_FUSE_SMOKE_REQUIRED:-}" == "1" ]]; then
    echo "$message" >&2
    exit 1
  fi
  echo "$message"
  exit 0
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
mount_id="${LOCALITY_FUSE_SMOKE_MOUNT_ID:-notion-fuse-smoke}"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/locality-fuse-smoke.XXXXXX")"
state_root="${LOCALITY_FUSE_SMOKE_STATE:-$tmp_root/state}"
loc_root="${LOCALITY_FUSE_SMOKE_ROOT:-$tmp_root/loc}"
mount_root="${LOCALITY_FUSE_SMOKE_MOUNT:-$loc_root/notion}"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
localityd_pid=""
fuse_pid=""
failed=0

on_error() {
  failed=1
  echo "Linux FUSE smoke test failed; daemon log:" >&2
  cat "$daemon_log" >&2 || true
  echo "Linux FUSE smoke test failed; FUSE log:" >&2
  cat "$fuse_log" >&2 || true
}

cleanup() {
  set +e
  if mountpoint -q "$loc_root"; then
    fusermount3 -uz "$loc_root" >/dev/null 2>&1
  fi
  if [[ -n "$fuse_pid" ]] && kill -0 "$fuse_pid" >/dev/null 2>&1; then
    kill "$fuse_pid" >/dev/null 2>&1
    wait "$fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "$localityd_pid" ]] && kill -0 "$localityd_pid" >/dev/null 2>&1; then
    kill "$localityd_pid" >/dev/null 2>&1
    wait "$localityd_pid" >/dev/null 2>&1
  fi
  if [[ "${LOCALITY_FUSE_SMOKE_KEEP_TMP:-}" != "1" ]]; then
    rm -rf "$tmp_root"
  else
    echo "kept FUSE smoke temp root: $tmp_root"
  fi
}
trap on_error ERR
trap cleanup EXIT

if [[ ! -x "$loc_bin" || ! -x "$localityd_bin" || ! -x "$fuse_bin" ]]; then
  cargo build -p localityd -p loc-cli -p locality-fuse
fi

seed_fixture() {
  mkdir -p "$state_root" "$loc_root" "$mount_root"
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 NOTION_TOKEN="ci-fuse-smoke-token" \
    "$loc_bin" mount notion "$mount_root" \
      --workspace \
      --mount-id "$mount_id" \
      --projection linux-fuse \
      --json >/dev/null

  local db="$state_root/state.sqlite3"
  local content_root="$state_root/content/$mount_id/files"
  mkdir -p "$content_root/Teamspace Home/Launch Plan"

  local home_frontmatter
  local home_body
  local child_frontmatter
  local child_body
  home_frontmatter=$'loc:\n  id: page-home\n  type: page\n  synced_at: 2026-06-13T00:00:00Z\n  remote_edited_at: 2026-06-13T00:00:00Z\ntitle: Teamspace Home\n'
  home_body=$'## Teamspace Home\n\nThis page proves root file reads work through a real FUSE mount.\n'
  child_frontmatter=$'loc:\n  id: page-launch\n  type: page\n  parent: page-home\n  synced_at: 2026-06-13T00:00:00Z\n  remote_edited_at: 2026-06-13T00:00:00Z\ntitle: Launch Plan\n'
  child_body=$'## Launch Plan\n\nOriginal launch plan from the seeded FUSE fixture.\n'

  printf -- '---\n%s---\n%s' "$home_frontmatter" "$home_body" \
    > "$content_root/Teamspace Home/page.md"
  printf -- '---\n%s---\n%s' "$child_frontmatter" "$child_body" \
    > "$content_root/Teamspace Home/Launch Plan/page.md"

  sqlite3 "$db" <<SQL
INSERT INTO entities (
  mount_id, remote_id, kind_json, title, path, hydration_json, content_hash, remote_edited_at
) VALUES
  ('$mount_id', 'page-home', '"page"', 'Teamspace Home', 'Teamspace Home/page.md', '"hydrated"', NULL, '2026-06-13T00:00:00Z'),
  ('$mount_id', 'page-launch', '"page"', 'Launch Plan', 'Teamspace Home/Launch Plan/page.md', '"hydrated"', NULL, '2026-06-13T00:00:00Z')
ON CONFLICT(mount_id, remote_id) DO UPDATE SET
  kind_json = excluded.kind_json,
  title = excluded.title,
  path = excluded.path,
  hydration_json = excluded.hydration_json,
  content_hash = excluded.content_hash,
  remote_edited_at = excluded.remote_edited_at;

INSERT INTO shadows (
  mount_id, entity_id, frontmatter, body_hash, rendered_body, blocks_json
) VALUES
  ('$mount_id', 'page-home', '$home_frontmatter', 'ci-home-body', '$home_body', '[]'),
  ('$mount_id', 'page-launch', '$child_frontmatter', 'ci-launch-body', '$child_body', '[]')
ON CONFLICT(mount_id, entity_id) DO UPDATE SET
  frontmatter = excluded.frontmatter,
  body_hash = excluded.body_hash,
  rendered_body = excluded.rendered_body,
  blocks_json = excluded.blocks_json;
SQL
}

wait_for_daemon() {
  for _ in {1..80}; do
    if LOCALITY_STATE_DIR="$state_root" "$loc_bin" daemon status --state-dir "$state_root" --json \
      | grep -q '"state": "running"'; then
      return 0
    fi
    sleep 0.25
  done
  echo "localityd did not become ready" >&2
  cat "$daemon_log" >&2 || true
  return 1
}

wait_for_mount() {
  for _ in {1..80}; do
    if mountpoint -q "$loc_root"; then
      return 0
    fi
    if [[ -n "$fuse_pid" ]] && ! kill -0 "$fuse_pid" >/dev/null 2>&1; then
      echo "locality-fuse exited before mount became ready" >&2
      cat "$fuse_log" >&2 || true
      return 1
    fi
    sleep 0.25
  done
  echo "FUSE mount did not become ready" >&2
  cat "$fuse_log" >&2 || true
  return 1
}

assert_status_contains() {
  local path="$1"
  local pattern="$2"
  local output
  output="$(LOCALITY_STATE_DIR="$state_root" "$loc_bin" status "$path" --json)"
  if ! grep -q "$pattern" <<<"$output"; then
    echo "status for $path did not contain $pattern" >&2
    echo "$output" >&2
    return 1
  fi
}

seed_fixture

LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off LOCALITY_DAEMON_PULL_MODE=disabled NOTION_TOKEN="ci-fuse-smoke-token" \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"
wait_for_daemon

LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --mount-id "$mount_id" \
  --state-dir "$state_root" \
  --mountpoint "$loc_root" >"$fuse_log" 2>&1 &
fuse_pid="$!"
wait_for_mount

findmnt -R "$loc_root" >/dev/null
ls -la "$loc_root" >/dev/null

home_dir="$mount_root/Teamspace Home"
home_file="$home_dir/page.md"
child_dir="$mount_root/Teamspace Home/Launch Plan"
child_file="$child_dir/page.md"

test -d "$home_dir"
test -f "$home_file"
test -d "$child_dir"
test -f "$child_file"
head -n 20 "$home_file" >/dev/null
grep -q "Original launch plan" "$child_file"
assert_status_contains "$child_file" '"state": "clean"'

backup="$(mktemp "$tmp_root/original.XXXXXX")"
cat "$child_file" > "$backup"
printf '\nFUSE smoke edit %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$child_file"
assert_status_contains "$child_file" '"state": "dirty"'
assert_status_contains "$child_file" '"local_body_changed"'
cat "$backup" > "$child_file"
assert_status_contains "$child_file" '"state": "clean"'

draft="$child_dir/locality-fuse-smoke-$$.md"
renamed="$child_dir/locality-fuse-smoke-renamed-$$.md"

printf '# FUSE smoke\n\nCreated by tests/linux_fuse_smoke.sh.\n' > "$draft"
mv "$draft" "$renamed"
assert_status_contains "$renamed" '"pending_virtual_create"'
rm "$renamed"
assert_status_contains "$child_dir" '"clean": true'

echo "ok: Linux FUSE smoke test completed"
