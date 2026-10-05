#!/usr/bin/env python3
"""Emit cargo-packager's Linux config from the release workspace layout."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


# Installed names: the GUI keeps `ubra` (and `main`, which feeds the desktop
# entry) while the automation CLI ships as `ubra-cli`. The binaries dir given
# to this script must already hold these staged names; see package-linux.sh.
BINARIES = (
    "ubra",
    "ubra-cli",
    "ubra-mcp",
    "ubrad-rs",
    "ubra-holder",
    "ubra-ssh-askpass",
    "ubra-remote",
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", required=True, type=Path)
    parser.add_argument("--binaries", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--license-inventory", required=True, type=Path)
    args = parser.parse_args()

    workspace = args.workspace.resolve()
    # The repository is consolidated at its root: the license files, scripts,
    # and docs referenced below all resolve inside the workspace.
    repository = workspace
    config = {
        "name": "ubra-linux",
        "productName": "ubra",
        "version": args.version,
        "identifier": "com.ubra.ubra",
        "description": "Run and monitor coding agents in local and direct SSH sessions",
        "homepage": "https://github.com/Ubra-Dev/ubra-app",
        "authors": ["Ubra Developers"],
        "licenseFile": str(repository / "LICENSE"),
        "category": "DeveloperTool",
        "formats": ["appimage", "deb"],
        "outDir": str(args.output.resolve()),
        "binariesDir": str(args.binaries.resolve()),
        "binaries": [
            {"path": name, "main": name == "ubra"} for name in BINARIES
        ],
        "icons": [str(workspace / "assets" / "icon.png")],
        "resources": [
            {
                "src": str(workspace / "crates" / "ubra-engine" / "manifests"),
                "target": "manifests",
            },
            {
                "src": str(args.license_inventory.resolve()),
                "target": "licenses/THIRD-PARTY-LICENSES.json",
            },
            {
                "src": str(repository / "LICENSE"),
                "target": "licenses/Apache-2.0.txt",
            },
            {
                "src": str(repository / "NOTICE"),
                "target": "licenses/NOTICE.txt",
            },
            {
                "src": str(repository / "scripts" / "license-policy.json"),
                "target": "licenses/license-policy.json",
            },
            {"src": str(repository / "docs" / "third-party"), "target": "licenses"},
        ],
        "linux": {"generateDesktopEntry": True},
        "deb": {
            "packageName": "ubra",
            "section": "devel",
            "priority": "optional",
            "desktopTemplate": str(
                workspace / "packaging" / "linux" / "ubra.desktop"
            ),
            "depends": [
                "libc6 (>= 2.35)",
                "libasound2",
                "libfontconfig1",
                "libglib2.0-0",
                "libvulkan1",
                "libwayland-client0",
                "libx11-xcb1",
                "libxkbcommon0",
                "libxkbcommon-x11-0",
                "zenity",
            ],
        },
    }
    print(json.dumps(config, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
