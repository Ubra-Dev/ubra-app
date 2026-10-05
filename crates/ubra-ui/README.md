# ubra-ui

Shared GPUI tokens and components ported from Ubra's Swift design system:

- `DesignSystem.swift`: radii, typography, semantic colors, fill/spacing/metric tokens, motion, and floating surfaces.
- `BrandLogos.swift`: the four 24×24 SVG paths and an M/L/H/V/C/S/Q/T/A/Z parser with arc-to-cubic conversion.
- `StatusGlyph.swift`: branded static and animated status marks, shared wall-clock phase math, shell caret, and attention dots.

Run the complete visual state gallery with:

```sh
cargo run -p ubra-ui --example gallery
```

## Adding an icon

Ordinary interface icons are vendored **Lucide** SVGs kept under their semantic
`IconName`. Do not add a text glyph, emoji, or a new SF Symbol call for app UI.
Provider identities such as Claude and Codex are brand marks and remain in
`brand.rs`; provider-brand glyphs (Notion, Google Doc/Sheet/Slides/Drive,
Linear, HubSpot, Figma, Slack, GitHub) stay Ubra-authored because Lucide ships
no brand icons. This workflow is for semantic controls and navigation icons.

1. Copy the Lucide icon verbatim from
   `https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/<name>.svg`
   into `assets/icons/` under the existing kebab-case Ubra filename (see the
   Lucide mapping: `close` <= `x`, `branch` <= `git-branch`,
   `align-left` <= `text-align-start`, and so on). Keep the file byte-identical
   to upstream: `0 0 24 24` view box, `currentColor`, stroke-width `2`, round
   caps and joins. Never restroke or recolor a vendored file; legibility at
   14–20 px is Lucide's job.

2. Register the asset in `src/icon.rs`:

   - add a semantic `IconName` variant;
   - add the variant to `IconName::ALL` and update the array length;
   - map it in `IconName::asset_path`;
   - embed it in `embedded_svg` with `include_bytes!`.

   The `every_icon_has_an_embedded_asset` test catches incomplete registration.

3. Use the semantic icon directly in new code:

   ```rust
   Icon::new(IconName::Folder, IconSize::REGULAR, colors.secondary)
   ```

   Use the shared optical scale (`COMPACT`, `REGULAR`, `LARGE`, or `DISPLAY`)
   instead of adding another one-off icon size.

   If an existing call site still passes an SF Symbol name, also map that name
   in `IconName::from_system_name` and add it to
   `every_legacy_symbol_used_by_the_app_resolves`. The compatibility bridge is
   only for migration; new APIs should accept `IconName` rather than strings.

4. Verify the asset and inspect it alongside the full family from the
   repository root:

   ```sh
   xmllint --noout crates/ubra-ui/assets/icons/*.svg
   cargo test -p ubra-ui
   cargo check -p ubra-app
   cargo run -p ubra-ui --example gallery
   ```

`IconAssets` embeds the SVG bytes in the application binary, so adding an icon
does not require a packaging or runtime resource-copy step.

## Intentional GPUI approximations

- GPUI's rounded rectangles use circular corners, so the SwiftUI continuous-corner squircles are represented with the same radius values but standard rounded corners.
- This pinned GPUI revision has no angular/conic gradient paint. The working mark uses a rotating two-stop linear gradient clipped to the vector path. Rotation direction, 2.4-second period, brand tint, and absolute shared phase match the Swift implementation; only the sweep's gradient geometry differs.

Animated `StatusGlyph` entities tick at 10 fps only while marked visible, motion-enabled, in a loud state, and hosted by an active window. Consumers mounting glyphs in virtualized or hidden panes must call `set_visible` when visibility changes.
