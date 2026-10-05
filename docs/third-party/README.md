# License texts

ubra's own source is licensed under the repository's [Apache-2.0 license](../../LICENSE).
Distributed builds also include third-party dependencies governed by their own
terms.

- No GPL-licensed crate is linked into ubra's binaries. The only crate
  manifests in the graph that declare no license are `gpui_util` and
  `gpui_shared_string`; each ships its own Apache-2.0 `LICENSE-APACHE` inside
  the crate directory next to its source (Copyright 2022 - 2025 Zed Industries,
  Inc.), the same marking the sibling `gpui`/`gpui_macos`/`gpui_platform`
  crates use, and crates.io publishes `gpui_util` under Apache-2.0.
  `scripts/license-policy.json` records both with that evidence.
- [GPL-3.0-or-later](GPL-3.0-or-later.txt) is the primary license of the
  upstream Zed repository that GPUI is pinned to. It is retained for reference
  only; nothing in this repository or its binaries is licensed under it, and the
  GPL-marked `zlog`/`ztracing` packages are replaced by local Apache-2.0
  compatibility crates under `vendor/`.
- The vendored forks of `alacritty_terminal` and `vte` ship their own texts in
  `vendor/alacritty_terminal/` and `vendor/vte/`.
- [ISC-lucide](ISC-lucide.txt) covers the vendored interface icons: Lucide
  under the ISC license, with the icons Lucide derives from Feather under MIT.
- [MIT-microsoft-terminal](MIT-microsoft-terminal.txt) covers the
  `microsoft/terminal` gamma-correction adaptation in
  `vendor/gpui/src/platform.rs`, and carries the MIT terms that also govern
  the twelve third-party terminal palettes listed in `NOTICE`.

[`scripts/license-policy.json`](../../scripts/license-policy.json) records the
reviewed exceptions. The [license check](../../scripts/check-licenses.py) fails
CI when the dependency graph changes in a way that needs a new review.
