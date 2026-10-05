#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
source_app="${UBRA_APP_PATH:-${workspace_dir}/dist/ubra.app}"
install_dir="${HOME}/Applications"
installed_app="${install_dir}/ubra.app"

if [[ ! -d "${source_app}" ]]; then
    echo "error: ${source_app} does not exist; run scripts/package.sh first" >&2
    exit 1
fi

mkdir -p "${install_dir}"
# Installing over a live bundle corrupts the code-signature seal (ditto merges
# without deleting stale files; overwriting mapped pages gets the running app
# SIGKILLed with "Code Signature Invalid"). Copy fresh beside it instead.
next_app="${installed_app}.ubra-next"
previous_app="${installed_app}.ubra-previous"
rm -rf "${next_app}" "${previous_app}"
ditto "${source_app}" "${next_app}"
codesign --verify --deep --strict "${next_app}"

# When ubra exits, macOS reads the bundle at its path to decide whether the
# processes it started may outlive it. A missing or half-copied bundle there
# gets the Engine, every Holder and every Agent terminated, so once ubra is
# gone the path only ever changes by rename.
if pgrep -x ubra >/dev/null 2>&1; then
    pkill -x ubra || true
    for _ in $(seq 1 20); do
        pgrep -x ubra >/dev/null 2>&1 || break
        sleep 0.2
    done
fi
if [[ -e "${installed_app}" ]]; then
    mv "${installed_app}" "${previous_app}"
fi
mv "${next_app}" "${installed_app}"
rm -rf "${previous_app}"

echo "Installed ${installed_app}"

# Put the CLI on PATH. It is a symlink into the bundle rather than a copy so an
# app update moves it too, and so it can never disagree with the daemon it
# talks to. Agent hooks and MCP configs record an absolute path, which is why
# this wants a stable location instead of a dev checkout's .build directory.
cli_source="${installed_app}/Contents/Resources/bin/ubra"
cli_dir="${UBRA_CLI_DIR:-${HOME}/.local/bin}"
if [[ -x "${cli_source}" ]]; then
    mkdir -p "${cli_dir}"
    ln -sf "${cli_source}" "${cli_dir}/ubra"
    echo "Linked ${cli_dir}/ubra -> ${cli_source}"
    case ":${PATH}:" in
        *":${cli_dir}:"*) ;;
        *) echo "note: ${cli_dir} is not on PATH; add it to use \`ubra\` directly" >&2 ;;
    esac
else
    echo "warning: ${cli_source} missing; rebuild with scripts/package.sh to ship the CLI" >&2
fi
