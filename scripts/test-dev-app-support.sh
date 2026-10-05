#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dev.sh
source "${script_dir}/dev.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

socket_bytes() {
    path_bytes "$1/daemon.sock"
}

short_sha="37047201"
short_target="/tmp/ubra-target"
short_support="$(choose_dev_app_support "${short_target}" "${short_sha}" "/var/folders/short/T")"
[[ "${short_support}" == "${short_target}/ubra-dev-${short_sha}-support" ]] \
    || fail "short target should stay under target/, got ${short_support}"
(( $(socket_bytes "${short_support}") < 104 )) \
    || fail "short socket is still too long"

# The worktree that prompted this: target/ + daemon.sock is 107 bytes.
long_target="/Users/alex/GitHub/ubra-fix-close-confirmation-enter-race/ubra/target"
long_socket="${long_target}/ubra-dev-${short_sha}-support/daemon.sock"
(( $(path_bytes "${long_socket}") >= 104 )) \
    || fail "fixture no longer exceeds SUN_LEN (${long_socket})"
temp_root="/var/folders/d_/46cq7fw53_v5z03x7_ft8hv80000gn/T"
long_support="$(choose_dev_app_support "${long_target}" "${short_sha}" "${temp_root}")"
[[ "${long_support}" == "${temp_root}/ubra-dev-${short_sha}-support" ]] \
    || fail "long target should use the temp root, got ${long_support}"
(( $(socket_bytes "${long_support}") < 104 )) \
    || fail "relocated socket is still too long: ${long_support}/daemon.sock"

huge_temp="/var/$(printf 'y%.0s' {1..120})"
fallback="$(choose_dev_app_support "${long_target}" "${short_sha}" "${huge_temp}/")"
[[ "${fallback}" == "/tmp/ubra-dev-${short_sha}-support" ]] \
    || fail "oversized temp root should fall back to /tmp, got ${fallback}"
(( $(socket_bytes "${fallback}") < 104 )) \
    || fail "/tmp socket is still too long"

echo "dev app support path ok"
