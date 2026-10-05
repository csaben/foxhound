//! Which windows the helper drives: one application, identified by process name, a title
//! substring, or an explicit HWND, and seen as a *group* — its main window plus every other visible
//! top-level window it owns (dialogs, Qt menus and combo popups, tooltips, native file pickers).
//!
//! The main window's visible frame is the **stage**: screenshots are exactly the stage, and every
//! coordinate the harness sends is stage-relative. Popups that fall inside the stage are composited
//! into it, and clicks on them are routed to them, so the agent sees and uses the app like a user
//! looking at that one window.

use std::ffi::c_void;

use foxhound_capture::snapshot::{Layer, ScreenRect};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic,
    IsWindow, IsWindowVisible, IsZoomed, SetForegroundWindow, SetWindowPos, ShowWindow,
    GWL_EXSTYLE, GW_HWNDPREV, GW_OWNER, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
};

// Target identity is shared with the standalone window primitive. The helper
// keeps its compositor-facing group types local for now.
pub use foxhound_window::TargetSpec;

/// A visible top-level window belonging to the target app.
#[derive(Debug, Clone, Serialize)]
pub struct AppWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    /// Visible frame in screen coordinates (DWM extended frame bounds).
    #[serde(skip)]
    pub bounds: ScreenRect,
    pub owned: bool,
    pub enabled: bool,
    pub hung: bool,
    pub foreground: bool,
    pub minimized: bool,
    #[serde(skip)]
    pub click_through: bool,
    #[serde(skip)]
    pub no_activate: bool,
}

/// The resolved group at one instant: windows ordered **top to bottom** in z-order.
#[derive(Debug, Clone)]
pub struct Group {
    pub main: AppWindow,
    pub windows: Vec<AppWindow>,
}

impl Group {
    pub fn stage(&self) -> ScreenRect {
        self.main.bounds
    }

    /// Stage coordinate → screen coordinate.
    pub fn to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        (self.main.bounds.left + x, self.main.bounds.top + y)
    }

    /// Layers for the compositor, bottom to top.
    pub fn layers(&self) -> Vec<Layer> {
        self.windows
            .iter()
            .rev()
            .map(|w| Layer {
                hwnd: w.hwnd,
                visible: w.bounds,
            })
            .collect()
    }

    /// The top-level window a click at a screen point lands on: the topmost group window under it,
    /// else the main window (points inside the stage but outside every window, e.g. its shadow).
    pub fn window_at(&self, sx: i32, sy: i32) -> &AppWindow {
        self.windows
            .iter()
            .find(|w| !w.click_through && !w.minimized && w.bounds.contains(sx, sy))
            .unwrap_or(&self.main)
    }

    /// Where keyboard input should go: the window the agent last clicked if it is still up and
    /// accepting input, else the topmost window that can take focus. A modal dialog disables its
    /// owner, so this naturally picks the dialog.
    pub fn key_window(&self, last_clicked: Option<isize>) -> &AppWindow {
        if let Some(w) =
            last_clicked.and_then(|h| self.windows.iter().find(|w| w.hwnd == h && w.enabled))
        {
            return w;
        }
        self.windows
            .iter()
            .find(|w| w.enabled && !w.no_activate && !w.minimized)
            .unwrap_or(&self.main)
    }
}

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

/// Process IDs whose executable name matches `exe` (case-insensitive, `.exe` optional).
pub fn pids_for_exe(exe: &str) -> Vec<u32> {
    let want = exe.to_ascii_lowercase();
    let want = if want.ends_with(".exe") {
        want
    } else {
        format!("{want}.exe")
    };
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase() == want {
                out.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn window_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn class_name(h: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(h, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Visible frame of a window: DWM extended frame bounds, falling back to the window rect.
pub fn visible_bounds(h: isize) -> Option<ScreenRect> {
    unsafe {
        let mut r = RECT::default();
        let dwm = DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        );
        if dwm.is_err() || r.right <= r.left {
            GetWindowRect(hwnd(h), &mut r).ok()?;
        }
        Some(ScreenRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
    }
}

fn describe(h: HWND, pid: u32) -> Option<AppWindow> {
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return None;
        }
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4);
        if cloaked != 0 {
            return None;
        }
        let class = class_name(h);
        // Drop-shadow and IME helper windows are not UI anyone clicks.
        if matches!(class.as_str(), "SysShadow" | "IME" | "MSCTFIME UI") {
            return None;
        }
        let bounds = visible_bounds(h.0 as isize)?;
        if bounds.width() <= 0 || bounds.height() <= 0 {
            return None;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let foreground = GetForegroundWindow();
        Some(AppWindow {
            hwnd: h.0 as isize,
            pid,
            title: window_text(h),
            class,
            bounds,
            owned: GetWindow(h, GW_OWNER)
                .map(|o| !o.0.is_null())
                .unwrap_or(false),
            enabled: IsWindowEnabled(h).as_bool(),
            hung: IsHungAppWindow(h).as_bool(),
            foreground: foreground == h,
            minimized: IsIconic(h).as_bool(),
            click_through: ex & WS_EX_TRANSPARENT.0 != 0,
            no_activate: ex & WS_EX_NOACTIVATE.0 != 0,
        })
    }
}

/// Every visible top-level window of the given processes, top to bottom.
pub fn windows_of(pids: &[u32]) -> Vec<AppWindow> {
    struct Ctx<'a> {
        pids: &'a [u32],
        out: Vec<AppWindow>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if ctx.pids.contains(&pid) {
            if let Some(w) = describe(h, pid) {
                ctx.out.push(w);
            }
        }
        BOOL(1)
    }
    let mut ctx = Ctx {
        pids,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out
}

/// All visible, titled, unowned top-level windows on the desktop (for picking a target).
pub fn all_app_windows() -> Vec<AppWindow> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<AppWindow>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if let Some(w) = describe(h, pid) {
            if !w.owned && !w.title.is_empty() {
                out.push(w);
            }
        }
        BOOL(1)
    }
    let mut out = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// The main window among a process group's windows: an unowned, titled window, largest first.
/// Owned windows (dialogs, popups) are never main while an unowned one exists.
fn pick_main(windows: &[AppWindow]) -> Option<AppWindow> {
    let area = |w: &AppWindow| w.bounds.width() as i64 * w.bounds.height() as i64;
    windows
        .iter()
        .filter(|w| !w.owned && !w.title.is_empty())
        .max_by_key(|w| area(w))
        .or_else(|| windows.iter().max_by_key(|w| area(w)))
        .cloned()
}

/// Resolve a spec to a live group. `sticky` is the main window chosen last time: while it is alive
/// it stays main, so a big dialog can never steal the stage.
pub fn resolve(spec: &TargetSpec, sticky: Option<isize>) -> Result<Group, String> {
    let (pids, preferred): (Vec<u32>, Option<isize>) = match spec {
        TargetSpec::Hwnd(h) => {
            if unsafe { !IsWindow(hwnd(*h)).as_bool() } {
                return Err(format!("window {h} no longer exists"));
            }
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd(*h), Some(&mut pid)) };
            (vec![pid], Some(*h))
        }
        TargetSpec::Process(exe) => {
            let pids = pids_for_exe(exe);
            if pids.is_empty() {
                return Err(format!("{exe} is not running"));
            }
            (pids, sticky)
        }
        TargetSpec::Pid(pid) => (vec![*pid], sticky),
        TargetSpec::Title(sub) => {
            let needle = sub.to_lowercase();
            let hit = sticky
                .filter(|h| unsafe { IsWindow(hwnd(*h)).as_bool() })
                .or_else(|| {
                    all_app_windows()
                        .into_iter()
                        .find(|w| w.title.to_lowercase().contains(&needle))
                        .map(|w| w.hwnd)
                })
                .ok_or_else(|| format!("no window title contains {sub:?}"))?;
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd(hit), Some(&mut pid)) };
            (vec![pid], Some(hit))
        }
    };
    let windows = windows_of(&pids);
    let main = preferred
        .and_then(|h| windows.iter().find(|w| w.hwnd == h).cloned())
        .or_else(|| pick_main(&windows))
        .ok_or_else(|| "the target has no visible window yet".to_string())?;
    Ok(Group { main, windows })
}

/// A minimized window renders nothing, so the helper un-minimizes the main window without
/// activating it and sends it to the back of the z-order — it keeps rendering behind whatever the
/// human is doing. Returns true if it had to.
pub fn ensure_rendering(main: isize) -> bool {
    unsafe {
        let h = hwnd(main);
        if !IsIconic(h).as_bool() {
            return false;
        }
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            h,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
        true
    }
}

/// Current desktop foreground window, if it is still a valid window.
pub fn foreground_window() -> Option<isize> {
    unsafe {
        let h = GetForegroundWindow();
        (!h.0.is_null() && IsWindow(h).as_bool()).then_some(h.0 as isize)
    }
}

/// Restore a foreground window that the target application displaced. This is never used to
/// activate the target; it only returns keyboard ownership to the last observed non-target HWND.
pub fn restore_foreground(window: isize) -> bool {
    unsafe { IsWindow(hwnd(window)).as_bool() && SetForegroundWindow(hwnd(window)).as_bool() }
}

/// Keep the complete target group immediately behind the human's foreground window without
/// activating either side. `Group::windows` is top-to-bottom, so chaining each target after the
/// previous one preserves popup/dialog/main ordering while moving the group as a unit.
pub fn place_behind(group: &Group, anchor: isize) -> usize {
    if group.windows.iter().any(|w| w.hwnd == anchor) {
        return 0;
    }
    unsafe {
        if !IsWindow(hwnd(anchor)).as_bool() {
            return 0;
        }
        // If the group's highest window is already below the anchor, every remaining target
        // window is too. Avoid generating a continuous stream of no-op WM_WINDOWPOSCHANGING.
        if let Some(top) = group.windows.first() {
            let mut previous = GetWindow(hwnd(top.hwnd), GW_HWNDPREV).ok();
            while let Some(window) = previous {
                if window == hwnd(anchor) {
                    return 0;
                }
                previous = GetWindow(window, GW_HWNDPREV).ok();
            }
        }
        let mut after = anchor;
        let mut moved = 0;
        for window in &group.windows {
            if SetWindowPos(
                hwnd(window.hwnd),
                hwnd(after),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            )
            .is_ok()
            {
                moved += 1;
                after = window.hwnd;
            }
        }
        moved
    }
}

pub fn is_maximized(h: isize) -> bool {
    unsafe { IsZoomed(hwnd(h)).as_bool() }
}
