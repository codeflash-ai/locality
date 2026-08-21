#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_MACOS_CONNECTOR_FILE_PROVIDER_LIVE:-}" != "1" ]]; then
  echo "skip: set LOCALITY_MACOS_CONNECTOR_FILE_PROVIDER_LIVE=1 to run connector File Provider scenarios"
  exit 0
fi

fail() {
  echo "live macOS connector File Provider test: $*" >&2
  exit 1
}

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
[[ "${LOCALITY_MACOS_FILE_PROVIDER_DEDICATED_HOST:-}" == "1" ]] \
  || fail "LOCALITY_MACOS_FILE_PROVIDER_DEDICATED_HOST=1 is required"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
connector="${LOCALITY_LIVE_CONNECTOR:-}"
app_path="${LOCALITY_MACOS_FILE_PROVIDER_APP:-}"
expected_bundle_id="${LOCALITY_MACOS_FILE_PROVIDER_EXPECTED_BUNDLE_ID:-}"

case "$connector" in
  gmail | slack | linear | granola) ;;
  *) fail "LOCALITY_LIVE_CONNECTOR must be gmail, slack, linear, or granola" ;;
esac
[[ -d "$app_path" && "$app_path" == *.app ]] || fail "test app does not exist: $app_path"
[[ -n "$expected_bundle_id" ]] || fail "LOCALITY_MACOS_FILE_PROVIDER_EXPECTED_BUNDLE_ID is required"
case "$expected_bundle_id" in
  *test* | *promptfresh*) ;;
  *) fail "expected bundle id must identify a test app" ;;
esac

for command in codesign pluginkit python3 /usr/libexec/PlistBuddy; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done

loc_bin="$app_path/Contents/MacOS/loc"
localityd_bin="$app_path/Contents/MacOS/localityd"
helper_bin="$app_path/Contents/MacOS/locality-file-providerctl"
appex="$app_path/Contents/PlugIns/LocalityFileProvider.appex"
appex_plist="$appex/Contents/Info.plist"
for binary in "$loc_bin" "$localityd_bin" "$helper_bin"; do
  [[ -x "$binary" ]] || fail "missing packaged executable: $binary"
done
[[ -f "$appex_plist" ]] || fail "missing File Provider extension"

actual_bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist")"
actual_extension_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$appex_plist")"
[[ "$actual_bundle_id" == "$expected_bundle_id" ]] || fail "unexpected test app bundle id"
[[ "$actual_extension_id" == "$expected_bundle_id.FileProvider" ]] || fail "unexpected File Provider extension id"
codesign --verify --deep --strict "$app_path" >/dev/null 2>&1 || fail "test app signature verification failed"

registered_extension="$(pluginkit -m -v -i "$actual_extension_id" 2>&1)" \
  || fail "File Provider extension is not registered"
python3 - "$actual_extension_id" "$appex" "$registered_extension" <<'PY'
import pathlib
import sys

bundle_id, expected_path, output = sys.argv[1:]
matches = [line for line in output.splitlines() if bundle_id in line]
if len(matches) != 1:
    raise SystemExit(f"expected one active File Provider registration, found {len(matches)}")
line = matches[0]
if line.lstrip().startswith("-"):
    raise SystemExit("File Provider registration is disabled")
if str(pathlib.Path(expected_path)) not in line:
    raise SystemExit("active File Provider extension is not from the installed test app")
PY

domain_report="$(mktemp "${TMPDIR:-/tmp}/locality-provider-domain.XXXXXX")"
trap 'rm -f "$domain_report"' EXIT
"$helper_bin" list --json >"$domain_report"
provider_root="$(python3 - "$domain_report" <<'PY'
import json
import pathlib
import sys

report = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
domains = [domain for domain in report.get("domains", []) if domain.get("identifier") == "loc"]
if len(domains) != 1:
    raise SystemExit(f"expected exactly one loc File Provider domain, found {len(domains)}")
domain = domains[0]
if not domain.get("userEnabled") or domain.get("disconnected") or not domain.get("url"):
    raise SystemExit("loc File Provider domain is not enabled and connected")
print(domain["url"])
PY
)"
case "$provider_root" in
  "$HOME/Library/CloudStorage/"*) ;;
  *) fail "File Provider root is outside the current user's CloudStorage directory" ;;
esac

python3 "$script_dir/live_connector_matrix.py" validate >/dev/null
python3 "$script_dir/live_provider_connector_scenario.py" \
  --connector "$connector" \
  --projection macos-file-provider \
  --provider-root "$provider_root" \
  --loc "$loc_bin" \
  --localityd "$localityd_bin" \
  --file-providerctl "$helper_bin"
