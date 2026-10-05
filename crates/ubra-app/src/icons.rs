//! Compatibility layer for call sites that still use former SF Symbol names.
//!
//! The actual glyphs are ubra's embedded, platform-independent SVG icon
//! family. Keeping this small bridge makes the visual-system migration atomic:
//! every existing control gets the same semantic SVG on every desktop.

use gpui::{AnyElement, Rgba};
use ubra_ui::{IconName, icon_from_system_name};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum SymbolWeight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

pub fn sf_symbol(name: &str, size: f32, color: Rgba) -> AnyElement {
    icon_from_system_name(name, size, color)
}

pub fn sf_symbol_weighted(name: &str, size: f32, _weight: SymbolWeight, color: Rgba) -> AnyElement {
    icon_from_system_name(name, size, color)
}

/// Verifies semantic SVG mappings without touching a platform UI toolkit.
pub fn probe() {
    for name in [
        "gearshape",
        "sidebar.left",
        "square.and.pencil",
        "folder.fill",
        "person.crop.circle",
        "xmark",
        "plus",
    ] {
        println!("{name}: {:?}", IconName::from_system_name(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn former_platform_symbols_resolve_to_svg_icons() {
        for name in [
            "terminal",
            "bell",
            "magnifyingglass",
            "folder",
            "gearshape",
            "sidebar.left",
            "square.grid.2x2",
            "square.stack.3d.up",
            "arrow.triangle.2.circlepath",
            "sparkle",
            "cube",
            "server.rack",
            "waveform.circle.fill",
            // Right-rail destinations: every icon must resolve, never blank.
            "sidebar.right",
            "network",
            "arrow.branch",
            "arrow.triangle.pull",
            "info.circle",
            "chart.bar",
            "note.text",
            "checklist",
            "link",
        ] {
            assert!(IconName::from_system_name(name).is_some(), "{name}");
        }
    }
}
