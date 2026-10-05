#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-}" in
    "") ;;
    -h|--help)
        echo "usage: ./scripts/check.sh"
        exit 0
        ;;
    *)
        echo "error: unknown option: $1" >&2
        echo "usage: ./scripts/check.sh" >&2
        exit 2
        ;;
esac

for tool in bash cargo python3; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "error: ${tool} is required; see CONTRIBUTING.md" >&2
        exit 1
    fi
done

echo "==> Shell syntax guards"
bash -n "${root}"/scripts/*.sh

echo "==> Rust workspace"
(
    cd "${root}"
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
)

echo "==> Dependency license policy"
python3 "${root}/scripts/check-licenses.py"

echo "All contributor checks passed."
