//! Per-window mouse injection: the pointer counterpart of [`inject_key`](crate::inject_key).
//!
//! Everything here posts `WM_MOUSE*` / `WM_*BUTTON*` straight into the target window's queue with
//! client coordinates in `lParam` and button state in `wParam`. The real cursor never moves and the
//! target is never activated, so a human keeps using the desktop while an agent clicks an occluded
//! window. Toolkits that read the pointer from the message (Win32 controls, Qt, SDL) behave; anything
//! that polls `GetCursorPos` (OLE drag-and-drop, hover tooltips, modal size/move loops) will not.
//!
//! Coordinates are **screen pixels**. The caller is expected to be per-monitor DPI aware so screen
//! and client coordinates are physical pixels on both sides.

use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetClassLongPtrW, GetWindowRect, PostMessageW, SendMessageTimeoutW,
    CS_DBLCLKS, CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GCL_STYLE, HTCLIENT,
    HTCLOSE, HTMAXBUTTON, HTMINBUTTON, SC_CLOSE, SC_MAXIMIZE, SC_RESTORE, SMTO_ABORTIFHUNG,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCHITTEST, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSCOMMAND,
};

const MK_LBUTTON: usize = 0x0001;
const MK_RBUTTON: usize = 0x0002;
const MK_MBUTTON: usize = 0x0010;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// pyautogui-style names: `left` / `right` / `middle` (also `primary` / `secondary`).
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "left" | "primary" => MouseButton::Left,
            "right" | "secondary" => MouseButton::Right,
            "middle" => MouseButton::Middle,
            _ => return None,
        })
    }

    /// The `MK_*` bit this button contributes to a mouse message's `wParam`.
    pub fn mk(self) -> usize {
        match self {
            MouseButton::Left => MK_LBUTTON,
            MouseButton::Right => MK_RBUTTON,
            MouseButton::Middle => MK_MBUTTON,
        }
    }

    fn messages(self) -> (u32, u32, u32) {
        match self {
            MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDBLCLK),
            MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_RBUTTONDBLCLK),
            MouseButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MBUTTONDBLCLK),
        }
    }
}

/// Where a screen point lands inside one top-level window: the deepest visible, enabled child
/// under it and the point in that child's client coordinates.
#[derive(Debug, Clone, Copy)]
pub struct PointerTarget {
    pub hwnd: isize,
    pub client: (i32, i32),
}

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

/// Pack two coordinates as `MAKELPARAM` does (signed 16-bit each, so negative client coords survive).
fn make_lparam(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as u16 as u32) << 16) | (x as u16 as u32)) as i32 as isize)
}

/// Descend from `top` to the deepest child under screen point (`sx`, `sy`).
///
/// `ChildWindowFromPointEx` only looks one level down, so walk until it returns the parent itself.
/// Qt paints most widgets without their own HWND, so for Qt this usually stops at the top-level
/// (or a native child like a D3D preview); classic Win32 dialogs resolve to the button/edit.
pub fn target_at(top: isize, sx: i32, sy: i32) -> PointerTarget {
    unsafe {
        let mut cur = hwnd(top);
        for _ in 0..32 {
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(cur, &mut pt);
            let child = ChildWindowFromPointEx(
                cur,
                pt,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            );
            if child.0.is_null() || child == cur {
                break;
            }
            cur = child;
        }
        let mut pt = POINT { x: sx, y: sy };
        let _ = ScreenToClient(cur, &mut pt);
        PointerTarget {
            hwnd: cur.0 as isize,
            client: (pt.x, pt.y),
        }
    }
}

/// A screen point in a *fixed* window's client coordinates. Drags use this: once a button goes
/// down, every later move and the release belong to the window that took the press (as with
/// mouse capture), even when the pointer leaves it.
pub fn client_target(h: isize, sx: i32, sy: i32) -> PointerTarget {
    let mut pt = POINT { x: sx, y: sy };
    unsafe {
        let _ = ScreenToClient(hwnd(h), &mut pt);
    }
    PointerTarget {
        hwnd: h,
        client: (pt.x, pt.y),
    }
}

/// Ask a top-level window what part of it is under a screen point (`WM_NCHITTEST`). Bounded by a
/// short timeout so a hung target can't stall the caller; a hung window reports `HTCLIENT`.
pub fn hit_test(top: isize, sx: i32, sy: i32) -> u32 {
    unsafe {
        let mut result: usize = HTCLIENT as usize;
        let ok = SendMessageTimeoutW(
            hwnd(top),
            WM_NCHITTEST,
            WPARAM(0),
            make_lparam(sx, sy),
            SMTO_ABORTIFHUNG,
            250,
            Some(&mut result),
        );
        if ok.0 == 0 {
            HTCLIENT
        } else {
            result as i32 as u32
        }
    }
}

/// Post a pointer move. `held` is the `MK_*` mask of buttons currently down (drags need it).
pub fn post_move(t: PointerTarget, held: usize) {
    unsafe {
        let _ = PostMessageW(
            hwnd(t.hwnd),
            WM_MOUSEMOVE,
            WPARAM(held),
            make_lparam(t.client.0, t.client.1),
        );
    }
}

/// Post one button edge. `held` is the mask *after* this edge, which is what Windows reports.
/// `double` sends the `*DBLCLK` form for the second press of a double click when the window class
/// asks for it (`CS_DBLCLKS`); classes without it expect a plain second down.
pub fn post_button(t: PointerTarget, button: MouseButton, down: bool, held: usize, double: bool) {
    let (msg_down, msg_up, msg_dbl) = button.messages();
    let msg = if !down {
        msg_up
    } else if double && wants_dblclk(t.hwnd) {
        msg_dbl
    } else {
        msg_down
    };
    unsafe {
        let _ = PostMessageW(
            hwnd(t.hwnd),
            msg,
            WPARAM(held),
            make_lparam(t.client.0, t.client.1),
        );
    }
}

fn wants_dblclk(h: isize) -> bool {
    unsafe { GetClassLongPtrW(hwnd(h), GCL_STYLE) as u32 & CS_DBLCLKS.0 != 0 }
}

/// Post a wheel notch series. Wheel messages carry **screen** coordinates; `delta` is in raw wheel
/// units (120 = one notch), matching `pyautogui.scroll` on Windows. `horizontal` selects the tilt
/// wheel.
pub fn post_wheel(t: PointerTarget, sx: i32, sy: i32, delta: i32, held: usize, horizontal: bool) {
    let msg = if horizontal {
        WM_MOUSEHWHEEL
    } else {
        WM_MOUSEWHEEL
    };
    let wparam = ((delta as i16 as u16 as usize) << 16) | (held & 0xFFFF);
    unsafe {
        let _ = PostMessageW(hwnd(t.hwnd), msg, WPARAM(wparam), make_lparam(sx, sy));
    }
}

/// What a click on a top-level window's non-client area turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// The point is client area: deliver ordinary button messages.
    Client,
    /// Caption button mapped to the equivalent `WM_SYSCOMMAND` (close / maximize / restore).
    SysCommand(u32),
    /// Frame, caption drag or minimize: not injectable without taking the real pointer
    /// (DefWindowProc runs a modal loop that tracks the physical cursor), so it is dropped.
    Ignored(u32),
}

/// Decide how to deliver a left click at a screen point on a top-level window. Caption buttons are
/// translated to their system commands because DefWindowProc's own button tracking follows the
/// physical mouse. Minimize is refused: a minimized window stops rendering, which blinds capture.
pub fn frame_action(top: isize, sx: i32, sy: i32, maximized: bool) -> FrameAction {
    let hit = hit_test(top, sx, sy);
    match hit {
        h if h == HTCLIENT => FrameAction::Client,
        h if h == HTCLOSE => FrameAction::SysCommand(SC_CLOSE),
        h if h == HTMAXBUTTON => {
            FrameAction::SysCommand(if maximized { SC_RESTORE } else { SC_MAXIMIZE })
        }
        h if h == HTMINBUTTON => FrameAction::Ignored(h),
        // HTNOWHERE / HTTRANSPARENT: let the client path decide where it lands.
        0 | 0xFFFF_FFFF => FrameAction::Client,
        h => FrameAction::Ignored(h),
    }
}

/// Post a `WM_SYSCOMMAND` to a top-level window (the caption-button path of [`frame_action`]).
pub fn post_syscommand(top: isize, cmd: u32) {
    unsafe {
        let _ = PostMessageW(hwnd(top), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
    }
}

/// Screen rectangle of a window (`GetWindowRect`), as `(left, top, right, bottom)`.
pub fn window_rect(h: isize) -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let mut r = RECT::default();
        GetWindowRect(hwnd(h), &mut r).ok()?;
        Some((r.left, r.top, r.right, r.bottom))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lparam_packs_signed_coordinates() {
        assert_eq!(make_lparam(10, 20).0, (20 << 16) | 10);
        // Negative client coords (a point left of / above a child) keep their 16-bit two's complement.
        let lp = make_lparam(-1, -2).0 as u32;
        assert_eq!(lp & 0xFFFF, 0xFFFF);
        assert_eq!(lp >> 16, 0xFFFE);
    }

    #[test]
    fn button_names_follow_pyautogui() {
        assert_eq!(MouseButton::parse("LEFT"), Some(MouseButton::Left));
        assert_eq!(MouseButton::parse("secondary"), Some(MouseButton::Right));
        assert_eq!(
            MouseButton::parse("middle").map(|b| b.mk()),
            Some(MK_MBUTTON)
        );
        assert_eq!(MouseButton::parse("x1"), None);
    }
}
