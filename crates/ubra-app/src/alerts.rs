//! Platform-native confirmation alerts.
//!
//! Close prompts return an intent; the caller owns pending-request validation
//! and preference updates. Native presentation failures cancel rather than
//! substituting an in-window dialog.

use gpui::{App, Global, Task, Window};

// Explicit paths keep these children under `alerts/` when this file is also
// included directly by the AppKit smoke harness; bare `mod macos;` there would
// resolve to the unrelated `src/macos/mod.rs`.
#[cfg(any(target_os = "linux", test))]
#[path = "alerts/linux.rs"]
mod linux;
#[cfg(target_os = "macos")]
#[path = "alerts/macos.rs"]
mod macos;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseResponse {
    Cancel,
    Close { suppress: bool },
    Unavailable(String),
}

/// Presents a native close confirmation with an independent suppression choice.
pub(crate) fn close_prompt(
    window: &mut Window,
    title: &str,
    message: &str,
    cx: &mut App,
) -> Task<CloseResponse> {
    // TestWindow deliberately has no raw native handle. Detect its dispatcher
    // before handle extraction; native/headless macOS fixtures still use AppKit.
    #[cfg(test)]
    if cx.foreground_executor().dispatcher().as_test().is_some() {
        let response = window.prompt(
            gpui::PromptLevel::Warning,
            title,
            Some(message),
            &["Close", "Cancel", "Close and don't ask again"],
            cx,
        );
        return cx.foreground_executor().spawn(async move {
            match response.await {
                Ok(0) => CloseResponse::Close { suppress: false },
                Ok(2) => CloseResponse::Close { suppress: true },
                _ => CloseResponse::Cancel,
            }
        });
    }

    #[cfg(target_os = "macos")]
    return macos::close_prompt(window, title, message, cx);

    #[cfg(target_os = "linux")]
    return linux::close_prompt(window, title, message, cx);

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (window, title, message, cx);
        Task::ready(CloseResponse::Cancel)
    }
}

/// Entry point for the opt-in AppKit smoke harness, which includes this module
/// tree directly; the application binary itself never calls it.
#[cfg(all(target_os = "macos", test))]
#[allow(dead_code)]
pub(crate) fn native_close_smoke() {
    macos::native_close_smoke();
}

struct NativeAlerts;

impl Global for NativeAlerts {}

// The running app turns this on where a native alert sheet can carry a paste
// confirmation (macOS); elsewhere the paste review stays Ubra's own panel.
// Close confirmations do not consult it: `close_prompt` dispatches by platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn enable(cx: &mut App) {
    cx.set_global(NativeAlerts);
}

pub(crate) fn enabled(cx: &App) -> bool {
    cx.has_global::<NativeAlerts>()
}
