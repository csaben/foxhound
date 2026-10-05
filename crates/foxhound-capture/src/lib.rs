//! Unfocused native-window capture.
//!
//! Foxhound deliberately has two capture primitives: a low-latency WGC stream can be added by
//! clients that need it, while [`snapshot::compose`] provides a deterministic still image with
//! owned dialogs, menus, and popups composited onto the target window's stage.

#[cfg(windows)]
mod dib;

#[cfg(windows)]
pub mod snapshot;

/// Placeholder surface for platforms without a Foxhound capture backend yet.
#[cfg(not(windows))]
pub mod snapshot {
    pub fn unsupported() -> &'static str {
        "foxhound-capture has no Linux or macOS backend yet; implement platform capture here"
    }
}
