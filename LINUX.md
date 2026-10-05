# Linux beta

Ubra supports Ubuntu 22.04 and 24.04 on x86_64 and on 64-bit ARM (aarch64,
Debian's `arm64`) under native Wayland and X11. The desktop renderer draws
through Vulkan, so a Vulkan-capable driver is required. Both architectures are
built natively on Ubuntu 22.04, so both have a glibc 2.35 floor; neither
requires Swift, SwiftPM, Xcode, or a macOS application bundle.

| Architecture | AppImage | Debian package | glibc | CI-tested on |
|---|---|---|---|---|
| x86_64 (Intel, AMD) | `ubra_<version>_x86_64.AppImage` | `ubra_<version>_amd64.deb` | 2.35 or newer | Ubuntu 22.04, 24.04 |
| aarch64 (64-bit ARM) | `ubra_<version>_aarch64.AppImage` | `ubra_<version>_arm64.deb` | 2.35 or newer | Ubuntu 22.04, 24.04 |

`uname -m` prints the name to pick. Other distributions with glibc 2.35 or
newer and a Vulkan-capable driver are expected to run the AppImage but are not
in the tested matrix. The AppImage is the format for distributions without
APT.

`linux-release.json` accompanies each build: it records the version, source
commit, platform, architecture, format, size, and SHA-256 digest of every
artifact, so scripts can pick files from it instead of parsing filenames.

## Install

Download the artifact and `SHA256SUMS` from the same GitHub release, then verify
the download:

```sh
sha256sum --ignore-missing --check SHA256SUMS
```

A checksum only proves the download matches the list next to it. To prove the
files were built by this repository's CI, verify their Sigstore signatures.
Every Linux release file has a `<file>.sigstore.json` bundle beside it. Install
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) (3.x
is tested), download the artifact and its bundle, then run:

```sh
cosign verify-blob \
  --bundle ubra_<version>_x86_64.AppImage.sigstore.json \
  --certificate-identity https://github.com/Ubra-Dev/ubra-app/.github/workflows/nightly.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ubra_<version>_x86_64.AppImage
```

`Verified OK` means the file is byte-for-byte what that workflow on `main`
signed, and that the signature is recorded in the public Sigstore transparency
log. Any other signer, a modified file, or a missing bundle fails. Use the same
command for `ubra_<version>_amd64.deb`, or for the aarch64 files
`ubra_<version>_aarch64.AppImage` and `ubra_<version>_arm64.deb`.

To check every Linux file at once, verify the signed Linux checksum list and
then check against it. CI publishes `SHA256SUMS-linux` beside the release-wide
`SHA256SUMS`; `--ignore-missing` checks the ones you downloaded:

```sh
cosign verify-blob \
  --bundle SHA256SUMS-linux.sigstore.json \
  --certificate-identity https://github.com/Ubra-Dev/ubra-app/.github/workflows/nightly.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS-linux
sha256sum --ignore-missing --check SHA256SUMS-linux
```

There is no long-lived signing key or fingerprint to import: Sigstore issues a
short-lived certificate to the CI job, and the identity above is the trust
anchor. The Debian package is not `dpkg-sig` signed and there is no APT
repository, so `apt` itself does not check a signature; verify the `.deb` with
cosign before installing it.

For Ubuntu or another Debian-based system, install the package with APT so its
runtime dependencies are resolved:

```sh
sudo apt install ./ubra_<version>_amd64.deb    # x86_64
sudo apt install ./ubra_<version>_arm64.deb    # aarch64
```

This installs the desktop entry and the `ubra` command, along with the
`ubra-mcp` MCP frontend; the engine daemon is `ubrad-rs`. Upgrade by installing
the newer package the same way. Remove the program with `sudo apt remove ubra`,
or remove it and its system package metadata with `sudo apt purge ubra`. User
sessions and preferences are not deleted by package removal.

The AppImage needs no installation:

```sh
chmod +x ubra_<version>_x86_64.AppImage    # or ubra_<version>_aarch64.AppImage
./ubra_<version>_x86_64.AppImage
```

Ubra does not replace packages from inside the app on Linux. Settings shows
the installed version and directs you to update through APT or a newer GitHub
release.

## Build from source

On Ubuntu, install the native GPUI dependencies before running Cargo from the
repository root:

```sh
sudo apt update
sudo apt install build-essential clang cmake libasound2-dev libfontconfig-dev \
  libglib2.0-dev libssl-dev libvulkan1 libwayland-dev libx11-xcb-dev \
  libxkbcommon-x11-dev mesa-vulkan-drivers pkg-config
cargo build --workspace
```

Creating distribution artifacts additionally needs `cargo-packager` 0.11.8,
then `scripts/package-linux.sh`. Inspect a built
package tree with `scripts/verify-linux-package.sh dist/linux`.

## User files

Ubra follows the XDG base-directory specification. The defaults are:

| Purpose | Default location |
|---|---|
| Data, PTY holders, injected helpers | `~/.local/share/ubra` |
| Session state, logs and remote session bindings | `~/.local/state/ubra` |
| Host, Agent and account config, manifest overrides | `~/.config/ubra` |
| Cache | `~/.cache/ubra` |
| Control socket and daemon lock | `$XDG_RUNTIME_DIR/ubra` |

When `XDG_RUNTIME_DIR` is unavailable, the runtime directory is
`~/.local/state/ubra/run`. `XDG_DATA_HOME`, `XDG_STATE_HOME`,
`XDG_CONFIG_HOME`, and `XDG_CACHE_HOME` override the corresponding roots.
`UBRA_APP_SUPPORT=/absolute/path` deliberately puts every root beneath one
directory; it is useful for isolated test instances. The daemon creates its
private directories with mode `0700` and its Unix socket with mode `0600`.

The main daemon log is normally
`~/.local/state/ubra/logs/ubrad.log`. Run `ubra doctor` to check the
daemon, agent discovery, state file, and active socket without opening the UI.

## Optional integrations

- Coding-agent executables must be installed separately and visible on the
  login shell's `PATH`. Ubra ships 23 agent definitions and shows the installed
  CLIs it detects. Claude Code and Codex have the deepest status and resume
  integration; every supported CLI still runs in a real terminal.
- Status sounds use the first available command among `pw-play`, `paplay`, and
  `aplay`. Ubra remains fully usable when none is installed.
- SSH password or key-passphrase dialogs use `zenity`, with `kdialog` as a
  fallback. Key-based SSH works without either program.

## Troubleshooting graphics and display startup

Check Vulkan independently with `vulkaninfo` or `vkcube` from your
distribution's Vulkan tools package. On a hybrid-GPU system, the standard
`DRI_PRIME=1` or Mesa device-selection variables can select another GPU.

Ubra follows the active desktop session. To force the X11 path from a Wayland
session, launch it with an empty `WAYLAND_DISPLAY`:

```sh
WAYLAND_DISPLAY= ubra
```

When reporting a Linux launch or rendering bug, include the Ubra version,
package format, distro, kernel, display server, desktop environment, GPU and
driver, plus the privacy-safe diagnostics from Settings.

## Beta limitations

The beta intentionally does not provide native tray or notification actions or
automatic in-package replacement. Approval and status workflows remain
available inside Ubra.

Package smoke tests cover install, upgrade, uninstall, a live shell, engine
restart/holder adoption, CLI hooks, and MCP on clean Ubuntu 22.04 and 24.04
jobs for both x86_64 and aarch64, and the workspace job launches the GUI
against headless Wayland. Those virtual displays do not replace the manual
release matrix for multiple monitors, fractional scaling, suspend/resume, and
native GPU drivers.
