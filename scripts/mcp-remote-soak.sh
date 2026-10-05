#!/usr/bin/env bash
set -euo pipefail
if [[ -z "${UBRA_REMOTE_SSH_TARGET:-}" ]]; then
    echo "UBRA_REMOTE_SSH_TARGET must name a disposable SSH account" >&2
    exit 64
fi
cd "$(dirname "${BASH_SOURCE[0]}")/.."
cargo test --locked --release --package ubra-remote --test mcp_reliability \
    real_ssh_mcp_reliability_soak -- --ignored --exact --nocapture
