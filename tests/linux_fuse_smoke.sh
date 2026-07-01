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
mount_id="notion-main"
google_mount_id="google-docs-main"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/locality-fuse-smoke.XXXXXX")"
state_root="${LOCALITY_FUSE_SMOKE_STATE:-$tmp_root/state}"
LOCALITY_ROOT="${LOCALITY_FUSE_SMOKE_ROOT:-$tmp_root/Locality}"
NOTION_MOUNT="${LOCALITY_FUSE_SMOKE_NOTION_MOUNT:-$LOCALITY_ROOT/notion-main}"
GOOGLE_MOUNT="${LOCALITY_FUSE_SMOKE_GOOGLE_MOUNT:-$LOCALITY_ROOT/google-docs-main}"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
localityd_pid=""
fuse_pid=""
failed=0

on_error() {
  failed=1
  echo "Linux FUSE smoke test failed; daemon log:" >&2
  if [[ -f "$daemon_log" ]]; then
    cat "$daemon_log" >&2 || true
  else
    echo "(daemon log was not created)" >&2
  fi
  echo "Linux FUSE smoke test failed; FUSE log:" >&2
  if [[ -f "$fuse_log" ]]; then
    cat "$fuse_log" >&2 || true
  else
    echo "(FUSE log was not created)" >&2
  fi
}

cleanup() {
  set +e
  if mountpoint -q "$LOCALITY_ROOT"; then
    fusermount3 -uz "$LOCALITY_ROOT" >/dev/null 2>&1
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

sql_text_literal() {
  local hex
  hex="$(printf '%s' "$1" | od -An -tx1 -v | tr -d ' \n')"
  if [[ -z "$hex" ]]; then
    printf "''"
  else
    printf "CAST(X'%s' AS TEXT)" "$hex"
  fi
}

seed_fixture() {
  mkdir -p "$state_root" "$LOCALITY_ROOT"
  LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 NOTION_TOKEN="ci-fuse-smoke-token" \
    "$loc_bin" mount notion "$NOTION_MOUNT" \
      --workspace \
      --mount-id "$mount_id" \
      --projection linux-fuse \
      --json >/dev/null

  local db="$state_root/state.sqlite3"
  local content_root="$state_root/content/$mount_id/files"
  mkdir -p "$content_root/Teamspace Home/Launch Plan"

  local google_mount_id_sql
  local google_mount_root_sql
  local google_connector_sql
  local linux_fuse_projection_sql
  google_mount_id_sql="$(sql_text_literal "$google_mount_id")"
  google_mount_root_sql="$(sql_text_literal "$GOOGLE_MOUNT")"
  google_connector_sql="$(sql_text_literal "google-docs")"
  linux_fuse_projection_sql="$(sql_text_literal '"linux_fuse"')"

  sqlite3 "$db" <<SQL
INSERT INTO mounts (
  mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
) VALUES (
  $google_mount_id_sql, $google_connector_sql, $google_mount_root_sql, NULL, 0, $linux_fuse_projection_sql, NULL
)
ON CONFLICT(mount_id) DO UPDATE SET
  connector = excluded.connector,
  root = excluded.root,
  remote_root_id = excluded.remote_root_id,
  read_only = excluded.read_only,
  projection_json = excluded.projection_json,
  connection_id = excluded.connection_id;
SQL

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

  local mount_id_sql
  local page_home_sql
  local page_launch_sql
  local page_kind_sql
  local title_home_sql
  local title_launch_sql
  local path_home_sql
  local path_launch_sql
  local hydration_sql
  local remote_edited_sql
  local home_frontmatter_sql
  local child_frontmatter_sql
  local home_body_hash_sql
  local child_body_hash_sql
  local home_body_sql
  local child_body_sql
  local blocks_sql
  mount_id_sql="$(sql_text_literal "$mount_id")"
  page_home_sql="$(sql_text_literal "page-home")"
  page_launch_sql="$(sql_text_literal "page-launch")"
  page_kind_sql="$(sql_text_literal '"page"')"
  title_home_sql="$(sql_text_literal "Teamspace Home")"
  title_launch_sql="$(sql_text_literal "Launch Plan")"
  path_home_sql="$(sql_text_literal "Teamspace Home/page.md")"
  path_launch_sql="$(sql_text_literal "Teamspace Home/Launch Plan/page.md")"
  hydration_sql="$(sql_text_literal '"hydrated"')"
  remote_edited_sql="$(sql_text_literal "2026-06-13T00:00:00Z")"
  home_frontmatter_sql="$(sql_text_literal "$home_frontmatter")"
  child_frontmatter_sql="$(sql_text_literal "$child_frontmatter")"
  home_body_hash_sql="$(sql_text_literal "ci-home-body")"
  child_body_hash_sql="$(sql_text_literal "ci-launch-body")"
  home_body_sql="$(sql_text_literal "$home_body")"
  child_body_sql="$(sql_text_literal "$child_body")"
  blocks_sql="$(sql_text_literal "[]")"

  sqlite3 "$db" <<SQL
INSERT INTO entities (
  mount_id, remote_id, kind_json, title, path, hydration_json, content_hash, remote_edited_at
) VALUES
  ($mount_id_sql, $page_home_sql, $page_kind_sql, $title_home_sql, $path_home_sql, $hydration_sql, NULL, $remote_edited_sql),
  ($mount_id_sql, $page_launch_sql, $page_kind_sql, $title_launch_sql, $path_launch_sql, $hydration_sql, NULL, $remote_edited_sql)
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
  ($mount_id_sql, $page_home_sql, $home_frontmatter_sql, $home_body_hash_sql, $home_body_sql, $blocks_sql),
  ($mount_id_sql, $page_launch_sql, $child_frontmatter_sql, $child_body_hash_sql, $child_body_sql, $blocks_sql)
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
    if mountpoint -q "$LOCALITY_ROOT"; then
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

LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off LOCALITY_DAEMON_PULL_MODE=disabled LOCALITY_DAEMON_BACKGROUND_CONNECTOR_SYNC=disabled NOTION_TOKEN="ci-fuse-smoke-token" \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"
wait_for_daemon

LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --state-dir "$state_root" \
  --mountpoint "$LOCALITY_ROOT" >"$fuse_log" 2>&1 &
fuse_pid="$!"
wait_for_mount

findmnt -R "$LOCALITY_ROOT" >/dev/null
ls -la "$LOCALITY_ROOT" >/dev/null
test -d "$NOTION_MOUNT"
test -d "$GOOGLE_MOUNT"

home_dir="$NOTION_MOUNT/Teamspace Home"
home_file="$home_dir/page.md"
child_dir="$NOTION_MOUNT/Teamspace Home/Launch Plan"
child_file="$child_dir/page.md"

test -d "$home_dir"
test -f "$home_file"
test -d "$child_dir"
test -f "$child_file"
head -n 20 "$home_file" >/dev/null
grep -q "Original launch plan" "$child_file"
assert_status_contains "$child_file" '"mount_id": "notion-main"'
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

draft_dir="$child_dir/locality-fuse-smoke-dir-$$"
renamed_dir="$child_dir/locality-fuse-smoke-dir-renamed-$$"
mkdir "$draft_dir"
mv "$draft_dir" "$renamed_dir"
assert_status_contains "$renamed_dir" '"pending_virtual_create"'
rm -r "$renamed_dir"
assert_status_contains "$child_dir" '"clean": true'

echo "ok: Linux FUSE smoke test completed"
