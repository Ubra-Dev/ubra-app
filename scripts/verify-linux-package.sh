#!/usr/bin/env bash

set -euo pipefail

dist_dir="${1:?usage: verify-linux-package.sh <dist-directory>}"
dist_dir="$(cd "${dist_dir}" && pwd)"
deb="$(find "${dist_dir}" -maxdepth 1 -type f -name '*.deb' -print -quit)"
appimage="$(find "${dist_dir}" -maxdepth 1 -type f -name '*.AppImage' -print -quit)"

if [[ -z "${deb}" || -z "${appimage}" ]]; then
    echo "error: expected a DEB and AppImage in ${dist_dir}" >&2
    exit 1
fi

stage="$(mktemp -d "${TMPDIR:-/tmp}/ubra-linux-package.XXXXXX")"
cleanup() {
    rm -rf "${stage}"
}
trap cleanup EXIT

deb_root="${stage}/deb"
mkdir -p "${deb_root}"
dpkg-deb --extract "${deb}" "${deb_root}"

# Native close confirmation is a runtime requirement, not an optional GPUI
# fallback. Assert on the Debian control field without opening a toolkit UI.
deb_depends="$(dpkg-deb --field "${deb}" Depends)"
deb_native_dependency_pattern='(^|,[[:space:]]*)zenity([[:space:](,]|$)'
if [[ ! "${deb_depends}" =~ ${deb_native_dependency_pattern} ]]; then
    echo "error: DEB must depend on zenity for native close confirmation" >&2
    exit 1
fi

for path in \
    usr/bin/ubra \
    usr/bin/ubra-cli \
    usr/bin/ubra-mcp \
    usr/bin/ubrad-rs \
    usr/bin/ubra-holder \
    usr/bin/ubra-ssh-askpass \
    usr/bin/ubra-remote \
    usr/lib/ubra/manifests/codex.json \
    usr/lib/ubra/licenses/THIRD-PARTY-LICENSES.json \
    usr/lib/ubra/licenses/Apache-2.0.txt \
    usr/share/applications/ubra.desktop; do
    test -e "${deb_root}/${path}"
done

chmod +x "${appimage}"
(
    cd "${stage}"
    "${appimage}" --appimage-extract >/dev/null
)
app_root="${stage}/squashfs-root"
for path in \
    usr/bin/ubra \
    usr/bin/ubra-cli \
    usr/bin/ubra-mcp \
    usr/bin/ubrad-rs \
    usr/bin/ubra-holder \
    usr/bin/ubra-ssh-askpass \
    usr/bin/ubra-remote \
    usr/lib/ubra/manifests/codex.json \
    usr/lib/ubra/licenses/THIRD-PARTY-LICENSES.json \
    usr/lib/ubra/licenses/Apache-2.0.txt; do
    test -e "${app_root}/${path}"
done

# The GUI probe prints its icon table; assert on the output so a swapped-in
# CLI (which just prints usage) cannot pass vacuously. Capture instead of
# piping to grep -q: under pipefail the early exit SIGPIPEs the writer.
deb_probe="$(UBRA_PROBE_SYMBOLS=1 "${deb_root}/usr/bin/ubra")"
app_probe="$(UBRA_PROBE_SYMBOLS=1 "${app_root}/usr/bin/ubra")"
[[ "${deb_probe}" == *"gearshape"* ]]
[[ "${app_probe}" == *"gearshape"* ]]
# The CLI must be the automation CLI, not the GUI (which would try to open
# windows instead of answering). grep reads the file directly: no pipe, no
# execution, no display needed.
grep -a -q "Ubra automation CLI" "${deb_root}/usr/bin/ubra-cli"
grep -a -q "Ubra automation CLI" "${app_root}/usr/bin/ubra-cli"

echo "Linux package layouts and executable probes passed"
