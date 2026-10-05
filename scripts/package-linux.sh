#!/usr/bin/env bash

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: Linux packages must be built natively on Linux" >&2
    exit 64
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
# The repository is consolidated at its root: license files and scripts all
# resolve inside the workspace directory.
repository_dir="${workspace_dir}"
dist_dir="${UBRA_DIST_DIR:-${workspace_dir}/dist/linux}"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${workspace_dir}/crates/ubra-app/Cargo.toml" | head -1)"
version="${UBRA_VERSION:-${cargo_version}}"
formats="${UBRA_LINUX_FORMATS:-appimage,deb}"
source_commit="${SOURCE_COMMIT:-$(git -C "${repository_dir}" rev-parse HEAD)}"

for tool in cargo cargo-packager npm python3; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "error: ${tool} is required to package ubra for Linux" >&2
        exit 1
    fi
done

mkdir -p "${dist_dir}"
cd "${workspace_dir}"

echo "==> Building Linux release binaries"
cargo build --locked --release --package ubra-app --bin ubra-gui
cargo build --locked --release --package ubra-mcp --bin ubra --bin ubra-mcp
cargo build --locked --release --package ubra-engine \
    --bin ubrad-rs --bin ubra-holder --bin ubra-ssh-askpass
cargo build --locked --release --package ubra-remote --bin ubra-remote

# Installed names differ from build names on Linux: the GUI keeps `ubra`
# (desktop entry, terminal launch) while the automation CLI ships as
# `ubra-cli`. The marker check is the same one-sided migration guard as
# scripts/package.sh: a tree that built the GUI last under the old shared
# `ubra` name can hold GUI bytes under a fresh CLI fingerprint that cargo
# will never rewrite.
#
# NOTE: no `grep -q` in the pipelines below. Under `pipefail`, grep's early
# exit SIGPIPEs strings and the check spuriously fails on binaries that DO
# contain the marker. Let grep consume the whole stream instead.
cli_binary="${target_dir}/release/ubra"
if [[ -f "${cli_binary}" ]] \
    && ! strings "${cli_binary}" 2>/dev/null | grep "Ubra automation CLI" >/dev/null; then
    echo "==> ${cli_binary} is not the CLI; rebuilding it once"
    touch "${workspace_dir}/crates/ubra-mcp/src/bin/ubra.rs"
    cargo build --locked --release --package ubra-mcp --bin ubra
fi
if [[ -f "${cli_binary}" ]] \
    && ! strings "${cli_binary}" 2>/dev/null | grep "Ubra automation CLI" >/dev/null; then
    echo "error: ${cli_binary} is not the automation CLI; refusing to package" >&2
    exit 1
fi
stage="${target_dir}/linux-binaries-stage"
rm -rf "${stage}"
mkdir -p "${stage}"
cp "${target_dir}/release/ubra-gui" "${stage}/ubra"
cp "${target_dir}/release/ubra" "${stage}/ubra-cli"
for name in ubra-mcp ubrad-rs ubra-holder ubra-ssh-askpass ubra-remote; do
    cp "${target_dir}/release/${name}" "${stage}/${name}"
done

license_inventory="${dist_dir}/THIRD-PARTY-LICENSES.json"
echo "==> Generating third-party license inventory"
python3 "${repository_dir}/scripts/check-licenses.py" --output "${license_inventory}"

packager_config="$(python3 "${script_dir}/linux-packager-config.py" \
    --workspace "${workspace_dir}" \
    --binaries "${stage}" \
    --output "${dist_dir}" \
    --version "${version}" \
    --license-inventory "${license_inventory}")"

echo "==> Creating ${formats} packages"
cargo-packager --config "${packager_config}" --formats "${formats}"

if [[ "${formats}" == *appimage* && "${formats}" == *deb* ]]; then
    python3 "${script_dir}/write-linux-release-manifest.py" \
        --directory "${dist_dir}" \
        --version "${version}" \
        --commit "${source_commit}"
fi

echo "==> Linux artifacts are in ${dist_dir}"
