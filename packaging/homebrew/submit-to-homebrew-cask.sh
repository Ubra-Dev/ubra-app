#!/usr/bin/env bash
#
# Submit ubra as a new cask to Homebrew/homebrew-cask.
#
# Renders packaging/homebrew/ubra.rb for the latest (or a given) GitHub
# release, places it in a local homebrew/cask checkout on a fresh branch, runs
# `brew style` and `brew audit --new --online`, and commits it as
# "ubra <version> (new cask)". With --open-pr it also forks
# Homebrew/homebrew-cask, pushes the branch and opens the pull request.
#
# Read packaging/homebrew/README.md first. Homebrew requires the human
# submitter to review the cask (including the zap paths), to disclose AI
# assistance, and to answer review comments personally.
#
# usage: submit-to-homebrew-cask.sh [options] [version]
#   --render-only     print the rendered cask to stdout and exit (no tap, no fork)
#   --install-tested  you ran `HOMEBREW_NO_INSTALL_FROM_API=1 brew install --cask ubra`
#                     and `brew uninstall --cask ubra` from the prepared checkout
#   --reviewed        you reviewed the rendered cask yourself (required for --open-pr)
#   --open-pr         push to your fork and open the pull request
#
# Environment: GH_REPO (default Ubra-Dev/ubra-app), GH_BIN (default gh),
# BREW_BIN (default brew).

set -euo pipefail

render_only=0
install_tested=0
reviewed=0
open_pr=0
version=""

for arg in "$@"; do
    case "${arg}" in
        --render-only) render_only=1 ;;
        --install-tested) install_tested=1 ;;
        --reviewed) reviewed=1 ;;
        --open-pr) open_pr=1 ;;
        -h | --help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*)
            echo "error: unknown option ${arg}" >&2
            exit 2
            ;;
        *)
            if [[ -n "${version}" ]]; then
                echo "error: more than one version given" >&2
                exit 2
            fi
            version="${arg#v}"
            ;;
    esac
done

if [[ "${open_pr}" == 1 && "${reviewed}" != 1 ]]; then
    echo "error: --open-pr requires --reviewed (Homebrew requires a human review of the cask)" >&2
    exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
candidate="${script_dir}/ubra.rb"
gh_repo="${GH_REPO:-Ubra-Dev/ubra-app}"
gh_bin="${GH_BIN:-gh}"
brew_bin="${BREW_BIN:-brew}"
token="ubra"
cask_path="Casks/d/${token}.rb"
branch="${token}-new-cask"

if [[ ! -f "${candidate}" ]]; then
    echo "error: candidate cask is missing: ${candidate}" >&2
    exit 1
fi

if [[ -z "${version}" ]]; then
    version="$("${gh_bin}" release view --repo "${gh_repo}" --json tagName --jq .tagName)"
    version="${version#v}"
fi
if ! [[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: version '${version}' is not X.Y.Z" >&2
    exit 2
fi

# The checksum comes from GitHub's own asset digest, the same source of truth
# scripts/publish-homebrew-cask.sh verifies the custom tap against.
asset_name="ubra-${version}-universal.dmg"
digest="$(
    "${gh_bin}" release view "v${version}" \
        --repo "${gh_repo}" \
        --json assets \
        --jq ".assets[] | select(.name == \"${asset_name}\") | .digest"
)"
sha="${digest#sha256:}"
if ! [[ "${sha}" =~ ^[0-9a-f]{64}$ ]]; then
    echo "error: GitHub release v${version} has no SHA-256 digest for ${asset_name}" >&2
    exit 1
fi

rendered="$(
    /usr/bin/sed -E \
        -e "s|^  version \".*\"$|  version \"${version}\"|" \
        -e "s|^  sha256 \".*\"$|  sha256 \"${sha}\"|" \
        "${candidate}"
)"
if ! grep -q "^  version \"${version}\"$" <<<"${rendered}" \
    || ! grep -q "^  sha256 \"${sha}\"$" <<<"${rendered}"; then
    echo "error: candidate cask did not render cleanly" >&2
    exit 1
fi

if [[ "${render_only}" == 1 ]]; then
    printf '%s\n' "${rendered}"
    exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: run this on macOS; brew audit --new needs the macOS cask checks" >&2
    exit 1
fi

# A previously refused submission must be read before trying again.
refused="$(
    "${gh_bin}" search prs --repo Homebrew/homebrew-cask --state closed \
        --match title --json title,url --jq \
        ".[] | select(.title | startswith(\"${token} \")) | select(.title | endswith(\"(new cask)\")) | .url" \
        "${token}"
)"
if [[ -n "${refused}" ]]; then
    echo "error: a closed '${token} (new cask)' pull request already exists; read it first:" >&2
    echo "${refused}" >&2
    exit 1
fi

cask_repo="$("${brew_bin}" --repository homebrew/cask)"
if [[ ! -d "${cask_repo}/.git" ]]; then
    echo "==> Tapping homebrew/cask (a full clone; this takes a while)"
    "${brew_bin}" tap --force homebrew/cask
fi
if [[ -n "$(git -C "${cask_repo}" status --porcelain --untracked-files=no)" ]]; then
    echo "error: tracked changes in ${cask_repo}; commit or discard them first" >&2
    exit 1
fi

git -C "${cask_repo}" fetch --quiet origin
if git -C "${cask_repo}" cat-file -e "origin/HEAD:${cask_path}" 2>/dev/null; then
    echo "error: ${cask_path} already exists in homebrew/cask; use:" >&2
    echo "  brew bump-cask-pr --version ${version} ${token}" >&2
    exit 1
fi

git -C "${cask_repo}" switch --quiet -C "${branch}" origin/HEAD
mkdir -p "${cask_repo}/Casks/d"
printf '%s\n' "${rendered}" >"${cask_repo}/${cask_path}"

echo "==> brew style --fix --cask ${cask_path}"
"${brew_bin}" style --fix --cask "${cask_repo}/${cask_path}"
echo "==> brew audit --new --cask --online homebrew/cask/${token}"
HOMEBREW_NO_INSTALL_FROM_API=1 "${brew_bin}" audit --new --cask --online "homebrew/cask/${token}"

if ! diff -q <(printf '%s\n' "${rendered}") "${cask_repo}/${cask_path}" >/dev/null; then
    echo "error: brew style --fix changed the cask; copy the fix back into ${candidate}" >&2
    git -C "${cask_repo}" diff -- "${cask_path}" >&2 || true
    exit 1
fi

# Homebrew forbids AI attribution trailers in commits: this message stays plain.
git -C "${cask_repo}" add "${cask_path}"
git -C "${cask_repo}" commit --quiet -m "${token} ${version} (new cask)" -- "${cask_path}"
echo "==> Committed '${token} ${version} (new cask)' on ${branch} in ${cask_repo}"

check() { [[ "$1" == 1 ]] && echo "x" || echo " "; }
body_file="$(mktemp "${TMPDIR:-/tmp}/ubra-cask-pr.XXXXXX")"
cat >"${body_file}" <<EOF
After making any changes to a cask, existing or new, verify:

- [x] The submission is for [a stable version](https://docs.brew.sh/Acceptable-Casks#stable-versions) or [documented exception](https://docs.brew.sh/Acceptable-Casks#but-there-is-no-stable-version).
- [x] \`brew audit --cask --online ${token}\` is error-free.
- [x] \`brew style --fix ${token}\` reports no offenses.

Additionally, if adding a new cask:

- [x] Named the cask according to the [token reference](https://docs.brew.sh/Cask-Cookbook#token-reference).
- [x] Checked the cask was not [already refused](https://github.com/search?q=repo%3AHomebrew%2Fhomebrew-cask+is%3Aclosed+is%3Aunmerged+&type=pullrequests) (add your cask's name to the end of the search field).
- [x] \`brew audit --cask --new ${token}\` worked successfully.
- [$(check "${install_tested}")] \`HOMEBREW_NO_INSTALL_FROM_API=1 brew install --cask ${token}\` worked successfully.
- [$(check "${install_tested}")] \`brew uninstall --cask ${token}\` worked successfully.

-----

- [$(check "${reviewed}")] I did not use AI/LLM to create this PR, or I disclosed the tool/model below and reviewed its output, including [\`zap\` stanza](https://docs.brew.sh/Cask-Cookbook#stanza-zap) paths; I did not attribute commits to AI and will answer maintainer questions and review comments myself without AI/LLM.

I am the upstream author of Ubra (https://github.com/Ubra-Dev/ubra-app), a Developer ID signed and notarized macOS app that updates itself, hence \`auto_updates true\`. The cask has shipped from my own tap (Ubra-Dev/homebrew-ubra) since August 2026; once this is merged that tap will point users here.

AI disclosure: the cask and its \`zap\` paths were drafted with Claude Code (Claude Opus). I reviewed them, checked each zap path against what the app writes under \`~/Library\`, and ran \`brew style\` and \`brew audit --new --online\` above. \`~/Library/Application Support/Ubra\` is left out of \`zap\` on purpose: it holds notes the user wrote and the state of agent sessions that keep running after the app quits (see the comment in the cask).
EOF

if [[ "${open_pr}" != 1 ]]; then
    cat <<EOF

Prepared, not submitted. Review the cask and the PR body, then either re-run
with --reviewed --open-pr (add --install-tested once you have installed and
uninstalled it from this checkout), or submit by hand:

  cask:     ${cask_repo}/${cask_path}
  PR body:  ${body_file}
EOF
    exit 0
fi

login="$("${gh_bin}" api user --jq .login)"
"${gh_bin}" repo fork Homebrew/homebrew-cask --clone=false >/dev/null 2>&1 || true
if ! git -C "${cask_repo}" remote get-url "${login}" >/dev/null 2>&1; then
    git -C "${cask_repo}" remote add "${login}" "https://github.com/${login}/homebrew-cask.git"
fi
git -C "${cask_repo}" push --quiet --force-with-lease --set-upstream "${login}" "${branch}"
"${gh_bin}" pr create \
    --repo Homebrew/homebrew-cask \
    --base main \
    --head "${login}:${branch}" \
    --title "${token} ${version} (new cask)" \
    --body-file "${body_file}"
rm -f "${body_file}"
