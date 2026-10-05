//! Runtime font-family selection.
//!
//! Font choices are made once from GPUI's discovered catalog. macOS keeps its
//! virtual system family; Linux prefers common desktop and monospace families
//! while retaining fontconfig generic fallbacks for minimal installations.
//!
//! The terminal family is the one user choice here. It is stored by name and
//! only honoured while that family is installed: GPUI substitutes a
//! proportional UI font for a missing family, which would wreck the grid.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use gpui::{App, Font, FontFallbacks, font};

static UI_FAMILY: OnceLock<&'static str> = OnceLock::new();
static MONO_FAMILY: OnceLock<&'static str> = OnceLock::new();
/// Families known to be installed: the catalog at launch plus whatever a
/// later [`monospace_families`] listing found (fonts installed since).
static INSTALLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Call once at startup, after GPUI has discovered the system font catalog.
pub fn init(cx: &App) {
    let names: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    let _ = UI_FAMILY.set(select_ui(&names));
    let _ = MONO_FAMILY.set(select_mono(&names));
    *installed() = Some(names);
}

fn installed() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    INSTALLED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn ui_family() -> &'static str {
    UI_FAMILY.get().copied().unwrap_or(default_ui())
}

pub fn mono_family() -> &'static str {
    MONO_FAMILY.get().copied().unwrap_or(default_mono())
}

/// The family the terminal actually paints with for a stored preference:
/// the preference while it is installed, otherwise the platform default.
pub fn terminal_family(configured: &str) -> &str {
    let usable = !configured.is_empty()
        && installed()
            .as_ref()
            .is_some_and(|names| names.contains(configured));
    if usable { configured } else { mono_family() }
}

/// The terminal font for a stored family preference (empty = default).
/// Called on every terminal frame, so the last answer is reused.
pub fn terminal_font(configured: &str) -> Font {
    thread_local! {
        static LAST: RefCell<Option<(String, Font)>> = const { RefCell::new(None) };
    }
    let family = terminal_family(configured);
    LAST.with_borrow_mut(|last| {
        if let Some((key, font)) = last.as_ref()
            && key == family
        {
            return font.clone();
        }
        let mut mono = font(family.to_owned());
        let mut fallbacks = terminal_fallbacks();
        // A custom family may lack glyphs the default covers.
        if mono.family.as_ref() != mono_family() {
            fallbacks.insert(0, mono_family().to_owned());
        }
        mono.fallbacks = Some(FontFallbacks::from_fonts(fallbacks));
        *last = Some((family.to_owned(), mono.clone()));
        mono
    })
}

/// Installed fixed-pitch families, sorted, without private (`.`) families.
/// Reads the live font catalog, so call it when a picker opens, not per frame.
pub fn monospace_families(cx: &App) -> Vec<String> {
    let mut families = platform_monospace_families(cx);
    families.retain(|family| !family.is_empty() && !family.starts_with('.'));
    families.sort_by_key(|family| family.to_lowercase());
    families.dedup();
    installed()
        .get_or_insert_with(HashSet::new)
        .extend(families.iter().cloned());
    families
}

#[cfg(target_os = "macos")]
fn platform_monospace_families(_cx: &App) -> Vec<String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSFont, NSFontManager, NSFontTraitMask};

    let Some(mtm) = MainThreadMarker::new() else {
        return Vec::new();
    };
    let manager = NSFontManager::sharedFontManager(mtm);
    let Some(faces) = manager.availableFontNamesWithTraits(NSFontTraitMask::FixedPitchFontMask)
    else {
        return Vec::new();
    };
    let mut families = HashSet::new();
    for face in faces.iter() {
        if let Some(family) = NSFont::fontWithName_size(&face, 12.0).and_then(|f| f.familyName()) {
            families.insert(family.to_string());
        }
    }
    families.into_iter().collect()
}

/// Without a fixed-pitch query, offer the well-known coding families that
/// are installed.
#[cfg(not(target_os = "macos"))]
fn platform_monospace_families(cx: &App) -> Vec<String> {
    let names: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    [
        "monospace",
        "JetBrains Mono",
        "Fira Code",
        "Fira Mono",
        "Cascadia Code",
        "Cascadia Mono",
        "Source Code Pro",
        "Hack",
        "Iosevka",
        "IBM Plex Mono",
        "Ubuntu Mono",
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Liberation Mono",
        "Inconsolata",
        "Roboto Mono",
        "Geist Mono",
    ]
    .into_iter()
    .filter(|family| names.contains(*family))
    .map(str::to_owned)
    .collect()
}

#[cfg(target_os = "macos")]
fn default_ui() -> &'static str {
    ".SystemUIFont"
}

#[cfg(not(target_os = "macos"))]
fn default_ui() -> &'static str {
    "sans-serif"
}

#[cfg(target_os = "macos")]
fn default_mono() -> &'static str {
    "Menlo"
}

#[cfg(not(target_os = "macos"))]
fn default_mono() -> &'static str {
    "monospace"
}

#[cfg(target_os = "macos")]
fn select_ui(_names: &HashSet<String>) -> &'static str {
    default_ui()
}

#[cfg(not(target_os = "macos"))]
fn select_ui(names: &HashSet<String>) -> &'static str {
    ["Noto Sans", "DejaVu Sans", "Liberation Sans", "Ubuntu"]
        .into_iter()
        .find(|candidate| names.contains(*candidate))
        .unwrap_or(default_ui())
}

#[cfg(target_os = "macos")]
fn select_mono(names: &HashSet<String>) -> &'static str {
    if names.contains("SF Mono") {
        "SF Mono"
    } else {
        default_mono()
    }
}

#[cfg(not(target_os = "macos"))]
fn select_mono(names: &HashSet<String>) -> &'static str {
    [
        "JetBrains Mono",
        "Cascadia Mono",
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Liberation Mono",
    ]
    .into_iter()
    .find(|candidate| names.contains(*candidate))
    .unwrap_or(default_mono())
}

#[cfg(target_os = "macos")]
fn terminal_fallbacks() -> Vec<String> {
    [
        ".SF NS Mono",
        "Menlo",
        "Apple Symbols",
        "STIX Two Math",
        "Apple Color Emoji",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(not(target_os = "macos"))]
fn terminal_fallbacks() -> Vec<String> {
    [
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Noto Sans Symbols 2",
        "STIX Two Math",
        "Noto Color Emoji",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        default_mono, default_ui, installed, mono_family, select_mono, select_ui, terminal_family,
        terminal_font,
    };
    use std::collections::HashSet;

    #[test]
    fn terminal_family_is_honoured_only_while_installed() {
        installed()
            .get_or_insert_with(HashSet::new)
            .insert("Ubra Test Mono".to_owned());
        assert_eq!(terminal_family(""), mono_family());
        assert_eq!(terminal_family("Ubra Test Mono"), "Ubra Test Mono");
        assert_eq!(terminal_family("Uninstalled Mono"), mono_family());

        let custom = terminal_font("Ubra Test Mono");
        assert_eq!(custom.family.as_ref(), "Ubra Test Mono");
        assert_eq!(
            custom
                .fallbacks
                .as_ref()
                .map(|f| f.fallback_list()[0].as_str()),
            Some(mono_family())
        );
        assert_eq!(terminal_font("").family.as_ref(), mono_family());
    }

    #[test]
    fn font_selection_uses_discovered_families_or_platform_fallbacks() {
        assert_eq!(select_ui(&HashSet::new()), default_ui());
        assert_eq!(select_mono(&HashSet::new()), default_mono());

        #[cfg(target_os = "macos")]
        assert_eq!(
            select_mono(&HashSet::from(["SF Mono".to_owned()])),
            "SF Mono"
        );

        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(
                select_ui(&HashSet::from(["Noto Sans".to_owned()])),
                "Noto Sans"
            );
            assert_eq!(
                select_mono(&HashSet::from(["DejaVu Sans Mono".to_owned()])),
                "DejaVu Sans Mono"
            );
        }
    }
}
