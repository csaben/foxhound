//! Input delivery that does not move the user's pointer or change the OS foreground window.

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
#[cfg(windows)]
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetWindowThreadProcessId,
};

#[cfg(windows)]
pub mod keys;
#[cfg(windows)]
pub mod pointer;

#[cfg(not(windows))]
pub mod keys {
    pub fn unsupported() -> &'static str {
        "foxhound-input has no Linux or macOS backend yet; implement posted input here"
    }
}

#[cfg(not(windows))]
pub mod pointer {
    pub fn unsupported() -> &'static str {
        "foxhound-input has no Linux or macOS backend yet; implement unfocused pointer input here"
    }
}

/// Resolve the native child that receives keyboard messages without requiring the target to be
/// foreground. Falls back through the target's real focus, known text/render controls, then the
/// top-level window itself.
#[cfg(windows)]
pub fn resolve_input_sink(hwnd_val: isize) -> isize {
    unsafe {
        let h = HWND(hwnd_val as *mut c_void);
        let target_tid = GetWindowThreadProcessId(h, None);
        if target_tid == 0 {
            return hwnd_val;
        }
        let our_tid = GetCurrentThreadId();
        let focus = if our_tid == target_tid {
            focus_hwnd()
        } else if AttachThreadInput(our_tid, target_tid, true).as_bool() {
            let f = focus_hwnd();
            let _ = AttachThreadInput(our_tid, target_tid, false);
            f
        } else {
            None
        };
        if let Some(f) = focus {
            if GetWindowThreadProcessId(HWND(f as *mut c_void), None) == target_tid {
                return f;
            }
        }
        for class in ["Chrome_RenderWidgetHostHWND", "Edit", "RICHEDIT50W"] {
            if let Some(c) = find_descendant_by_class(h, class) {
                return c;
            }
        }
        hwnd_val
    }
}

#[cfg(windows)]
unsafe fn focus_hwnd() -> Option<isize> {
    let f = GetFocus();
    (!f.0.is_null()).then_some(f.0 as isize)
}

#[cfg(windows)]
struct ClassSearch {
    want_lower: Vec<u16>,
    found: Option<isize>,
}

#[cfg(windows)]
fn class_eq_ignore_case(name: &[u16], want_lower: &[u16]) -> bool {
    name.len() == want_lower.len()
        && name.iter().zip(want_lower).all(|(c, w)| {
            let lc = if (b'A' as u16..=b'Z' as u16).contains(c) {
                c + 32
            } else {
                *c
            };
            lc == *w
        })
}

#[cfg(windows)]
unsafe extern "system" fn class_match_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut ClassSearch);
    let mut buf = [0u16; 256];
    let n = GetClassNameW(hwnd, &mut buf);
    if n > 0 && class_eq_ignore_case(&buf[..n as usize], &search.want_lower) {
        search.found = Some(hwnd.0 as isize);
        return BOOL(0);
    }
    BOOL(1)
}

#[cfg(windows)]
unsafe fn find_descendant_by_class(parent: HWND, class: &str) -> Option<isize> {
    let mut search = ClassSearch {
        want_lower: class.to_ascii_lowercase().encode_utf16().collect(),
        found: None,
    };
    let _ = EnumChildWindows(
        parent,
        Some(class_match_proc),
        LPARAM(&mut search as *mut _ as isize),
    );
    search.found
}
