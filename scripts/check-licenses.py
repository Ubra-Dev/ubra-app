#!/usr/bin/env python3
"""Validate dependency license metadata and optionally write an inventory."""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
POLICY_PATH = ROOT / "scripts" / "license-policy.json"
# The Rust workspace is the repository root.
RUST_WORKSPACE = ROOT


def cargo_metadata() -> dict:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=RUST_WORKSPACE,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        print(result.stderr, file=sys.stderr, end="")
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)


def is_unacceptable_expression(expression: str) -> bool:
    upper = expression.upper()
    if any(token in upper for token in ("AGPL", "SSPL", "BUSL", "COMMONS-CLAUSE")):
        return True
    if "GPL" not in upper:
        return False
    # SPDX OR expressions let the distributor choose a permissive branch.
    permissive = ("APACHE-2.0", "MIT", "BSD-", "ISC", "ZLIB", "MPL-2.0")
    return " OR " not in upper or not any(token in upper for token in permissive)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, help="write the reviewed inventory as JSON")
    args = parser.parse_args()

    policy = json.loads(POLICY_PATH.read_text())
    metadata = cargo_metadata()
    missing_policy = {
        (entry["name"], entry["version"]): entry
        for entry in policy["rust_missing_metadata"]
    }
    seen_exceptions: set[tuple[str, str]] = set()
    failures: list[str] = []
    inventory: list[dict[str, str]] = []

    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if package["source"] is None:
            continue
        license_expression = package.get("license") or ""
        key = (package["name"], package["version"])
        if not license_expression:
            exception = missing_policy.get(key)
            source = package["source"] or ""
            if exception is None or exception["source_contains"] not in source:
                failures.append(
                    f"{package['name']} {package['version']} has no license metadata ({source})"
                )
                effective_license = "UNKNOWN"
            else:
                seen_exceptions.add(key)
                effective_license = exception["conservative_license"]
        else:
            effective_license = license_expression
            if is_unacceptable_expression(license_expression):
                failures.append(
                    f"{package['name']} {package['version']} uses {license_expression}"
                )

        inventory.append(
            {
                "name": package["name"],
                "version": package["version"],
                "license": effective_license,
                "source": package["source"],
            }
        )

    stale = set(missing_policy) - seen_exceptions
    for name, version in sorted(stale):
        failures.append(
            f"stale missing-license exception for {name} {version}; review and remove it"
        )

    rust_names = {package["name"] for package in metadata["packages"]}
    if {"zlog"} & rust_names:
        failures.append("GPL profiling package zlog re-entered the Rust dependency graph")

    # The telemetry Worker and CLI are not part of this repository. When a
    # `telemetry/` tree is present its devDependencies are build and test
    # tooling that is never distributed, so they are not inventoried; a runtime
    # dependency would be, and would need a review mechanism of its own.
    for manifest in sorted((ROOT / "telemetry").glob("*/package.json")):
        runtime = json.loads(manifest.read_text()).get("dependencies") or {}
        for name in sorted(runtime):
            failures.append(
                f"{manifest.relative_to(ROOT)} adds runtime npm dependency {name}; "
                "review its license and record the review in license-policy.json"
            )

    if failures:
        print("dependency license policy failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        report = {
            "schema": 1,
            "project_license": policy["project_license"],
            "rust_dependencies": inventory,
            "notes": policy["notes"],
        }
        args.output.write_text(json.dumps(report, indent=2) + "\n")

    print(
        f"dependency license policy passed: {len(inventory)} Rust packages, "
        f"{len(seen_exceptions)} conservative metadata exceptions"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
