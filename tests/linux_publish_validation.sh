#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/afs-linux-publish-validation.XXXXXX")"

cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

source "${ROOT}/scripts/publish-linux.sh"

if [[ "$(uname -s)" == "Linux" ]]; then
  mkdir -p "${tmp_root}/deb/control" "${tmp_root}/deb/data/usr/bin"
  printf 'Package: afs\nVersion: 0.1.0\nArchitecture: amd64\nMaintainer: AFS\nDescription: test\n' \
    > "${tmp_root}/deb/control/control"
  touch \
    "${tmp_root}/deb/data/usr/bin/afs" \
    "${tmp_root}/deb/data/usr/bin/afsd" \
    "${tmp_root}/deb/data/usr/bin/afs-fuse"

  printf '2.0\n' > "${tmp_root}/deb/debian-binary"
  tar -czf "${tmp_root}/deb/control.tar.gz" -C "${tmp_root}/deb/control" .
  tar -czf "${tmp_root}/deb/data.tar.gz" -C "${tmp_root}/deb/data" usr
  (
    cd "${tmp_root}/deb"
    ar r "${tmp_root}/afs.deb" debian-binary control.tar.gz data.tar.gz >/dev/null
  )

  validate_deb "${tmp_root}/afs.deb"
else
  printf 'linux publish validation: skipping Debian archive fixture on non-Linux host\n'
fi

touch "${tmp_root}/libappindicator3.so.1"
write_appindicator_pc "appindicator3-0.1" "${tmp_root}/libappindicator3.so.1"
grep -qx "Libs: -L\${libdir} ${tmp_root}/libappindicator3.so.1" \
  "${APPINDICATOR_PKG_CONFIG_TMP}/appindicator3-0.1.pc"
