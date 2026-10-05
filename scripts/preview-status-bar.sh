#!/usr/bin/env bash
#
# Launch the dev app on the bottom-status-bar click-through mockup: every
# strip segment lit at once with fake sessions and no daemon.
#
#   ./scripts/preview-status-bar.sh
#
# Extra environment rides along, so variants work the same way:
#
#   UBRA_STATUSBAR_ACCESS=attaching ./scripts/preview-status-bar.sh
#
# Arguments after -- reach scripts/dev.sh (e.g. --release). See
# `scripts/dev.sh --help` for the other preview scenarios and knobs.

set -euo pipefail

if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${UBRA_SIDEBAR_PREVIEW:=1}"
: "${UBRA_SIDEBAR_SCENARIO:=statusbar}"
export UBRA_SIDEBAR_PREVIEW UBRA_SIDEBAR_SCENARIO

exec "${script_dir}/dev.sh" "$@"
