#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
publisher="${script_dir}/publish-github-release.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/ubra-github-publish-test.XXXXXX")"

cleanup() {
    rm -rf "${fixture_root}"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

state_dir="${fixture_root}/gh-state"
fake_gh="${fixture_root}/gh"
notes="${fixture_root}/notes.md"
dmg="${fixture_root}/ubra-0.4.6-universal.dmg"
zip="${fixture_root}/ubra-0.4.6-universal.zip"
feed="${fixture_root}/appcast.json"
checksums="${fixture_root}/SHA256SUMS"
inventory="${fixture_root}/THIRD-PARTY-LICENSES.json"
mkdir -p "${state_dir}"
printf 'notes\n' > "${notes}"
printf 'dmg bytes\n' > "${dmg}"
printf 'zip bytes\n' > "${zip}"
printf '{"feed_version":1}\n' > "${feed}"
printf 'checksums\n' > "${checksums}"
printf '{"schema":1}\n' > "${inventory}"

dmg_sha="$(shasum -a 256 "${dmg}" | awk '{print $1}')"
zip_sha="$(shasum -a 256 "${zip}" | awk '{print $1}')"
feed_sha="$(shasum -a 256 "${feed}" | awk '{print $1}')"
checksums_sha="$(shasum -a 256 "${checksums}" | awk '{print $1}')"
inventory_sha="$(shasum -a 256 "${inventory}" | awk '{print $1}')"

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'printf "%q " "$@" >> "${TEST_GH_STATE:?}/calls.log"' \
    'printf "\n" >> "${TEST_GH_STATE:?}/calls.log"' \
    'if [[ "$1 $2" == "release view" ]]; then' \
    '    [[ -f "${TEST_GH_STATE}/exists" ]] || exit 1' \
    '    if [[ " $* " == *" --json assets "* ]]; then' \
    '        case "$*" in' \
    '            *ubra-0.4.6-universal.dmg*) printf "sha256:%s\n" "${TEST_DMG_SHA:?}" ;;' \
    '            *ubra-0.4.6-universal.zip*) printf "sha256:%s\n" "${TEST_ZIP_SHA:?}" ;;' \
    '            *appcast.json*) printf "sha256:%s\n" "${TEST_FEED_SHA:?}" ;;' \
    '            *SHA256SUMS*) printf "sha256:%s\n" "${TEST_CHECKSUMS_SHA:?}" ;;' \
    '            *THIRD-PARTY-LICENSES.json*) printf "sha256:%s\n" "${TEST_INVENTORY_SHA:?}" ;;' \
    '            *ubra_0.4.6_amd64.deb*) if [[ -f "${TEST_GH_STATE}/uploaded-deb" ]]; then printf "sha256:%s\n" "${TEST_DEB_SHA:?}"; fi ;;' \
    '            *) exit 1 ;;' \
    '        esac' \
    '    fi' \
    'elif [[ "$1 $2" == "release create" ]]; then' \
    '    touch "${TEST_GH_STATE}/exists"' \
    'elif [[ "$1 $2" == "release upload" ]]; then' \
    '    if [[ -z "${TEST_ALLOW_UPLOAD:-}" ]]; then echo "release upload must never be used" >&2; exit 99; fi' \
    '    if [[ " $* " == *"ubra_0.4.6_amd64.deb"* ]]; then touch "${TEST_GH_STATE}/uploaded-deb"; fi' \
    'else' \
    '    exit 1' \
    'fi' > "${fake_gh}"
chmod +x "${fake_gh}"

run_publisher() {
    TEST_GH_STATE="${state_dir}" \
    TEST_DMG_SHA="${dmg_sha}" \
    TEST_ZIP_SHA="${zip_sha}" \
    TEST_FEED_SHA="${feed_sha}" \
    TEST_CHECKSUMS_SHA="${checksums_sha}" \
    TEST_INVENTORY_SHA="${inventory_sha}" \
    GH_BIN="${fake_gh}" \
    GH_REPO="example/ubra" \
    SOURCE_COMMIT="0123456789abcdef0123456789abcdef01234567" \
        "${publisher}" 0.4.6 "${notes}" "${dmg}" "${zip}" "${feed}" "${checksums}" "${inventory}"
}

# An exact rerun may recover a failed downstream tap push without modifying the
# already-public release.
touch "${state_dir}/exists"
run_publisher
if grep -qE 'release (create|upload)' "${state_dir}/calls.log"; then
    fail "identical existing release was mutated"
fi

# Rebuilding the same version produces different signed/notarized bytes. Those
# bytes must be rejected, never uploaded with --clobber.
printf 'replacement dmg bytes\n' > "${dmg}"
if run_publisher >"${fixture_root}/immutable.log" 2>&1; then
    fail "publisher replaced an existing release with different bytes"
fi
grep -q "refusing to replace immutable release asset" "${fixture_root}/immutable.log" \
    || fail "immutable-asset failure did not explain the unsafe replacement"
if grep -q 'release upload' "${state_dir}/calls.log"; then
    fail "publisher attempted release upload for an immutable version"
fi

# A brand-new version is created once, then every uploaded digest is verified.
printf 'dmg bytes\n' > "${dmg}"
rm -f "${state_dir}/exists" "${state_dir}/calls.log"
run_publisher
grep -q 'release create' "${state_dir}/calls.log" \
    || fail "new release was not created"
grep -q -- '--target 0123456789abcdef0123456789abcdef01234567' "${state_dir}/calls.log" \
    || fail "new release was not pinned to the reviewed source commit"
if grep -q -- '--clobber' "${state_dir}/calls.log"; then
    fail "publisher exposed a clobber path"
fi

# Attach mode adds a missing asset (the Linux packages arrive after the macOS
# release is public), leaves existing identical ones alone, and never clobbers.
deb="${fixture_root}/ubra_0.4.6_amd64.deb"
printf 'deb bytes\n' > "${deb}"
deb_sha="$(shasum -a 256 "${deb}" | awk '{print $1}')"
rm -f "${state_dir}/calls.log"
run_attach() {
    PUBLISH_ATTACH=1 TEST_ALLOW_UPLOAD=1 \
    TEST_GH_STATE="${state_dir}" \
    TEST_DMG_SHA="${dmg_sha}" TEST_ZIP_SHA="${zip_sha}" TEST_FEED_SHA="${feed_sha}" \
    TEST_CHECKSUMS_SHA="${checksums_sha}" TEST_INVENTORY_SHA="${inventory_sha}" \
    TEST_DEB_SHA="${deb_sha}" \
    GH_BIN="${fake_gh}" GH_REPO="example/ubra" \
        "${publisher}" 0.4.6 "${notes}" "${dmg}" "$@"
}
run_attach "${deb}"
grep -q 'release upload v0.4.6 .*ubra_0.4.6_amd64.deb' "${state_dir}/calls.log" \
    || fail "attach did not upload the missing asset"
if grep 'release upload' "${state_dir}/calls.log" | grep -q 'universal.dmg'; then
    fail "attach re-uploaded an asset the release already has"
fi
if grep -q -- '--clobber' "${state_dir}/calls.log"; then
    fail "attach exposed a clobber path"
fi
# Attach still refuses bytes that differ from what is published.
printf 'other deb bytes\n' > "${deb}"
if run_attach "${deb}" >"${fixture_root}/attach-immutable.log" 2>&1; then
    fail "attach accepted different bytes for a published asset"
fi
grep -q "refusing to replace immutable release asset" "${fixture_root}/attach-immutable.log" \
    || fail "attach did not refuse a changed asset"
# Attaching to a release that does not exist fails rather than creating one.
rm -f "${state_dir}/exists"
if run_attach "${deb}" >/dev/null 2>&1; then
    fail "attach created a release"
fi
if grep -q 'release create' "${state_dir}/calls.log"; then
    fail "attach created a release"
fi

echo "publish-github-release regression test passed"
