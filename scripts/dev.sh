#!/usr/bin/env bash

set -euo pipefail

# Byte length. sockaddr_un counts bytes, and ${#path} counts characters.
path_bytes() {
    printf '%s' "$1" | wc -c | tr -d '[:space:]'
}

# Support directory for one dev build. daemon.sock has to fit in
# sockaddr_un.sun_path (104 bytes including the trailing NUL). A long
# worktree under target/ overflows that and the Engine never binds.
# ponytail: per-user temp, then /tmp. Both fit this filename.
choose_dev_app_support() {
    local target_dir="$1"
    local short_sha="$2"
    local short_root="${3:-${TMPDIR:-/tmp}}"
    local support="${target_dir}/ubra-dev-${short_sha}-support"
    local socket_path="${support}/daemon.sock"
    if (( $(path_bytes "${socket_path}") >= 104 )); then
        short_root="${short_root%/}"
        support="${short_root}/ubra-dev-${short_sha}-support"
        socket_path="${support}/daemon.sock"
        if (( $(path_bytes "${socket_path}") >= 104 )); then
            support="/tmp/ubra-dev-${short_sha}-support"
        fi
    fi
    printf '%s\n' "${support}"
}

if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd "${script_dir}/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
profile="debug"
settings_preview=""
cargo_args=()

usage() {
    cat <<'USAGE'
Usage: scripts/dev.sh [--release] [--settings TAB] [-- CARGO_BUILD_ARGS...]

Build and launch an unmistakable development copy of ubra.

Options:
  --release       Build with Cargo's release profile.
  --settings TAB  Open Settings on general, appearance, resources, remote, or diagnostics.
  -h, --help      Show this help.

Preview scenarios (fake sessions, no daemon; for visual click-through):
  UBRA_SIDEBAR_PREVIEW=1 UBRA_SIDEBAR_SCENARIO=statusbar scripts/dev.sh
  Scenarios: typical, stress, empty, artifacts, fleet, projects, statusbar.
  The statusbar scenario also reads UBRA_STATUSBAR_ACCESS (active-elsewhere,
  attaching, reconnecting, unavailable, live), UBRA_STATUSBAR_SCROLLED=0, and
  uptodate, off).

Arguments after -- are passed to cargo build. Options that change Cargo's
target directory, target triple, or profile are not supported; set
CARGO_TARGET_DIR or use --release instead.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            profile="release"
            cargo_args+=("$1")
            shift
            ;;
        --settings)
            if [[ $# -lt 2 ]]; then
                echo "error: --settings requires a tab" >&2
                usage >&2
                exit 2
            fi
            settings_preview="$2"
            shift 2
            ;;
        --settings=*)
            settings_preview="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            cargo_args+=("$@")
            break
            ;;
        *)
            echo "error: unknown option: $1 (put cargo build arguments after --)" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "${settings_preview}" in
    ""|general|appearance|terminal|resources|remote|diagnostics) ;;
    *)
        echo "error: unknown Settings tab: ${settings_preview}" >&2
        exit 2
        ;;
esac

if (( ${#cargo_args[@]} > 0 )); then
    for argument in "${cargo_args[@]}"; do
        case "${argument}" in
            --target|--target=*|--target-dir|--target-dir=*|--profile|--profile=*)
                echo "error: ${argument} changes where the app binary is written" >&2
                exit 2
                ;;
        esac
    done
fi

branch="$(git -C "${workspace_dir}" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
branch="${branch:-detached}"
short_sha="$(git -C "${workspace_dir}" rev-parse --short=8 HEAD 2>/dev/null || true)"
short_sha="${short_sha:-nogit}"
dirty=""
if git -C "${workspace_dir}" rev-parse --git-dir >/dev/null 2>&1 \
    && git -C "${workspace_dir}" rev-parse --verify HEAD >/dev/null 2>&1; then
    if ! git -C "${workspace_dir}" diff --quiet --ignore-submodules -- \
        || ! git -C "${workspace_dir}" diff --cached --quiet --ignore-submodules --; then
        dirty="+dirty"
    fi
fi
build_label="${branch}@${short_sha}${dirty}"
bundle_id="com.ubra.ubra.dev.${short_sha}"
display_name="ubra dev ${short_sha}"
preferred_app_support="${target_dir}/ubra-dev-${short_sha}-support"
dev_app_support="$(choose_dev_app_support "${target_dir}" "${short_sha}")"
if [[ "${dev_app_support}" != "${preferred_app_support}" ]]; then
    echo "==> App support exceeds the Unix socket limit; using ${dev_app_support}"
fi

mkdir -p "${target_dir}" "${dev_app_support}"
chmod 700 "${dev_app_support}"

cd "${workspace_dir}"
echo "==> Building ${display_name} (${profile})"
# ubra-app does not pull Engine, Holder, or MCP helpers into target/<profile>/.
# Build them here so a clean checkout cannot launch against stale session
# processes or copy ~/Applications/ubra.app's ubra-mcp (no include tools).
# The GUI builds as `ubra-gui` while the automation CLI keeps `ubra`: the
# daemon resolves its session CLI from exe-relative `ubra`, so both must be
# present and must never share one filename.
if (( ${#cargo_args[@]} > 0 )); then
    cargo build --package ubra-app --bin ubra-gui --package ubra-engine --bin ubrad-rs --bin ubra-holder "${cargo_args[@]}"
    cargo build --package ubra-mcp "${cargo_args[@]}"
else
    cargo build --package ubra-app --bin ubra-gui --package ubra-engine --bin ubrad-rs --bin ubra-holder
    cargo build --package ubra-mcp
fi

# One-sided migration guard: before the GUI had its own binary name, both
# packages emitted `ubra`, so a tree that built the GUI last can hold GUI
# bytes under a fresh CLI fingerprint that cargo will never rewrite. The
# daemon would then install the GUI as its session CLI. Detect that exact
# state via the marker string (which lives only in ubra-mcp's bin source)
# and rebuild the CLI once. Distinct filenames make this unreachable on any
# tree that has built since the rename.
#
# NOTE: no `grep -q` here. Under `pipefail`, grep's early exit SIGPIPEs
# strings and the pipeline spuriously fails on binaries that DO contain the
# marker. Let grep consume the whole stream instead.
cli_binary="${target_dir}/${profile}/ubra"
if [[ -f "${cli_binary}" ]] \
    && ! strings "${cli_binary}" 2>/dev/null | grep "Ubra automation CLI" >/dev/null; then
    echo "==> ${cli_binary} is not the automation CLI; rebuilding it once"
    touch "${workspace_dir}/crates/ubra-mcp/src/bin/ubra.rs"
    if (( ${#cargo_args[@]} > 0 )); then
        cargo build --package ubra-mcp --bin ubra "${cargo_args[@]}"
    else
        cargo build --package ubra-mcp --bin ubra
    fi
fi
if [[ -f "${cli_binary}" ]] \
    && ! strings "${cli_binary}" 2>/dev/null | grep "Ubra automation CLI" >/dev/null; then
    echo "error: ${cli_binary} is not the automation CLI; refusing to launch" >&2
    exit 1
fi

binary="${target_dir}/${profile}/ubra-gui"
if [[ ! -x "${binary}" ]]; then
    echo "error: cargo did not produce ${binary}" >&2
    exit 1
fi

# Stage the Agent catalog beside the dev binaries. The daemon looks for an
# exe-relative `manifests/` before its baked-in source path, so this keeps dev
# launch working even when target/ was copied from another checkout (whose
# baked CARGO_MANIFEST_DIR no longer exists) and picks up manifest edits on
# the next daemon start without a rebuild.
rm -rf "${target_dir}/${profile}/manifests"
ln -s "${workspace_dir}/crates/ubra-engine/manifests" "${target_dir}/${profile}/manifests"

engine_bin="${target_dir}/${profile}/ubrad-rs"
if [[ ! -x "${engine_bin}" ]]; then
    echo "error: cargo did not produce ${engine_bin}" >&2
    exit 1
fi

holder_bin="${target_dir}/${profile}/ubra-holder"
if [[ ! -x "${holder_bin}" ]]; then
    echo "error: cargo did not produce ${holder_bin}" >&2
    exit 1
fi

for helper in ubra ubra-mcp; do
    helper_bin="${target_dir}/${profile}/${helper}"
    if [[ ! -x "${helper_bin}" ]]; then
        echo "error: cargo did not produce ${helper_bin}" >&2
        exit 1
    fi
done

# Every invocation gets a fresh bundle. Replacing a bundle beneath a still-
# running process invalidates its code signature, which is especially easy to
# do when judging two builds side by side.
bundle_root="$(mktemp -d "${target_dir}/ubra-dev-${short_sha}.XXXXXX")"
app_path="${bundle_root}/${display_name}.app"
contents="${app_path}/Contents"
mkdir -p "${contents}/MacOS" "${contents}/Resources"
cp "${binary}" "${contents}/MacOS/ubra"
cp "${workspace_dir}/assets/dev-icon.icns" "${contents}/Resources/dev-icon.icns"
cp "${workspace_dir}/assets/dev-Assets.car" "${contents}/Resources/Assets.car"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "${workspace_dir}/crates/ubra-app/Cargo.toml" | head -1)"
cat > "${contents}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>${display_name}</string>
    <key>CFBundleExecutable</key><string>ubra</string>
    <key>CFBundleIconFile</key><string>dev-icon.icns</string>
    <key>CFBundleIconName</key><string>ubra-dev</string>
    <key>CFBundleIdentifier</key><string>${bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>${display_name}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# A bundle assembled after Cargo's link step must be sealed as a unit. Ad-hoc
# signing gives it coherent bundle metadata without ever resembling a release.
codesign --force --sign - \
    --entitlements "${workspace_dir}/assets/ubra.entitlements" \
    --identifier "${bundle_id}" \
    "${app_path}"
codesign --verify --deep --strict "${app_path}"

launch_environment=(
    "UBRA_DEV=1"
    "UBRA_DEV_BUILD=${build_label}"
    "UBRA_APP_SUPPORT=${dev_app_support}"
)
if [[ -n "${settings_preview}" ]]; then
    launch_environment+=("UBRA_SETTINGS_PREVIEW=${settings_preview}")
fi

# The Rust app fail-closes unless Hello reports engineKind=ubra-rust-engine.
# Prefer a local cargo build of ubrad-rs, then the packaged Rust Engine in
# an installed bundle. Never point at the retired legacy daemon — the client
# rejects it and the UI comes up unable to spawn or list sessions.
if [[ -z "${UBRAD_PATH:-}" ]]; then
    for candidate in \
        "${target_dir}/${profile}/ubrad-rs" \
        "${target_dir}/debug/ubrad-rs" \
        "${target_dir}/release/ubrad-rs" \
        "${HOME}/Applications/ubra.app/Contents/Resources/bin/ubrad-rs" \
        "/Applications/ubra.app/Contents/Resources/bin/ubrad-rs"
    do
        if [[ -x "${candidate}" ]]; then
            launch_environment+=("UBRAD_PATH=${candidate}")
            break
        fi
    done
fi

echo "==> Launching ${display_name} (${build_label})"
echo "    ${app_path}"
if [[ -n "${UBRAD_PATH:-}" ]]; then
    echo "    engine: ${UBRAD_PATH}"
fi
for item in "${launch_environment[@]}"; do
    if [[ "${item}" == UBRAD_PATH=* ]]; then
        echo "    engine: ${item#UBRAD_PATH=}"
    fi
done
exec env \
    -u UBRA_SOCKET \
    -u UBRA_SESSION_ID \
    -u UBRA_CLI \
    -u NO_COLOR \
    -u FORCE_COLOR \
    "${launch_environment[@]}" \
    "${contents}/MacOS/ubra"
