//! The Foxhound Windows window primitive.
//!
//! A target is more than an `HWND`: real applications open owned dialogs,
//! menus, popups, and tooltips.  This crate resolves a stable target spec to
//! a window group, keeps a stable main-window stage, and exposes coordinates
//! in physical screen pixels. Capture and input clients can consume
//! this contract without owning their own Win32 discovery logic.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub mod unsupported {
    pub fn status() -> &'static str {
        "foxhound-window has no Linux or macOS backend yet; implement discovery here"
    }
}
