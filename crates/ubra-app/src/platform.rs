//! User-facing desktop vocabulary that genuinely varies by operating system.

pub fn local_machine_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This computer"
    }
}

/// "your Mac" / "your computer", for sentences about the machine itself.
pub fn your_machine() -> &'static str {
    if cfg!(target_os = "macos") {
        "your Mac"
    } else {
        "your computer"
    }
}
