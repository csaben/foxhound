use std::ffi::c_void;

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
    IsWindow, IsWindowVisible, IsZoomed, SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_OWNER,
    HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
    WS_EX_TRANSPARENT,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetSpec {
    Process(String),
    Pid(u32),
    Title(String),
    Hwnd(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WindowRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
impl WindowRect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    pub rect: WindowRect,
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

#[derive(Debug, Clone)]
pub struct WindowGroup {
    pub main: WindowInfo,
    pub windows: Vec<WindowInfo>,
}
impl WindowGroup {
    pub fn stage(&self) -> WindowRect {
        self.main.rect
    }
    pub fn to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        (self.main.rect.left + x, self.main.rect.top + y)
    }
    pub fn window_at(&self, x: i32, y: i32) -> &WindowInfo {
        self.windows
            .iter()
            .find(|w| !w.click_through && !w.minimized && w.rect.contains(x, y))
            .unwrap_or(&self.main)
    }
    pub fn key_window(&self, last_clicked: Option<isize>) -> &WindowInfo {
        last_clicked
            .and_then(|h| self.windows.iter().find(|w| w.hwnd == h && w.enabled))
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|w| w.enabled && !w.no_activate && !w.minimized)
            })
            .unwrap_or(&self.main)
    }
}

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

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
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(&want) {
                out.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut b = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut b);
        String::from_utf16_lossy(&b[..got.max(0) as usize])
    }
}
fn class(h: HWND) -> String {
    unsafe {
        let mut b = [0u16; 256];
        let n = GetClassNameW(h, &mut b);
        String::from_utf16_lossy(&b[..n.max(0) as usize])
    }
}

pub fn visible_rect(h: isize) -> Option<WindowRect> {
    unsafe {
        let mut r = RECT::default();
        if DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            || r.right <= r.left
        {
            GetWindowRect(hwnd(h), &mut r).ok()?;
        }
        Some(WindowRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
    }
}

fn describe(h: HWND, pid: u32) -> Option<WindowInfo> {
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return None;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4);
        if cloaked != 0 {
            return None;
        }
        let c = class(h);
        if matches!(c.as_str(), "SysShadow" | "IME" | "MSCTFIME UI") {
            return None;
        }
        let rect = visible_rect(h.0 as isize)?;
        if rect.width() <= 0 || rect.height() <= 0 {
            return None;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        Some(WindowInfo {
            hwnd: h.0 as isize,
            pid,
            title: text(h),
            class: c,
            rect,
            owned: GetWindow(h, GW_OWNER)
                .map(|o| !o.0.is_null())
                .unwrap_or(false),
            enabled: IsWindowEnabled(h).as_bool(),
            hung: IsHungAppWindow(h).as_bool(),
            foreground: GetForegroundWindow() == h,
            minimized: IsIconic(h).as_bool(),
            click_through: ex & WS_EX_TRANSPARENT.0 != 0,
            no_activate: ex & WS_EX_NOACTIVATE.0 != 0,
        })
    }
}

pub fn windows_of(pids: &[u32]) -> Vec<WindowInfo> {
    struct Ctx<'a> {
        pids: &'a [u32],
        out: Vec<WindowInfo>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let c = &mut *(lp.0 as *mut Ctx);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if c.pids.contains(&pid) {
            if let Some(w) = describe(h, pid) {
                c.out.push(w);
            }
        }
        BOOL(1)
    }
    let mut c = Ctx {
        pids,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut c as *mut _ as isize));
    }
    c.out
}

pub fn all_app_windows() -> Vec<WindowInfo> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<WindowInfo>);
        let mut pid = 0;
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

fn pick_main(ws: &[WindowInfo]) -> Option<WindowInfo> {
    let area = |w: &WindowInfo| w.rect.width() as i64 * w.rect.height() as i64;
    ws.iter()
        .filter(|w| !w.owned && !w.title.is_empty())
        .max_by_key(|w| area(w))
        .or_else(|| ws.iter().max_by_key(|w| area(w)))
        .cloned()
}

pub fn resolve(spec: &TargetSpec, sticky: Option<isize>) -> Result<WindowGroup, String> {
    let (pids, preferred) = match spec {
        TargetSpec::Hwnd(h) => {
            if unsafe { !IsWindow(hwnd(*h)).as_bool() } {
                return Err(format!("window {h} no longer exists"));
            }
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd(*h), Some(&mut pid));
            }
            (vec![pid], Some(*h))
        }
        TargetSpec::Process(exe) => {
            let p = pids_for_exe(exe);
            if p.is_empty() {
                return Err(format!("{exe} is not running"));
            }
            (p, sticky)
        }
        TargetSpec::Pid(pid) => {
            let p = vec![*pid];
            (p, sticky)
        }
        TargetSpec::Title(sub) => {
            let h = sticky
                .filter(|h| unsafe { IsWindow(hwnd(*h)).as_bool() })
                .or_else(|| {
                    all_app_windows()
                        .into_iter()
                        .find(|w| w.title.to_lowercase().contains(&sub.to_lowercase()))
                        .map(|w| w.hwnd)
                })
                .ok_or_else(|| format!("no window title contains {sub:?}"))?;
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd(h), Some(&mut pid));
            }
            (vec![pid], Some(h))
        }
    };
    let windows = windows_of(&pids);
    let main = preferred
        .and_then(|h| windows.iter().find(|w| w.hwnd == h).cloned())
        .or_else(|| pick_main(&windows))
        .ok_or_else(|| "the target has no visible window yet".to_string())?;
    Ok(WindowGroup { main, windows })
}

pub fn ensure_rendering(h: isize) -> bool {
    unsafe {
        let w = hwnd(h);
        if !IsIconic(w).as_bool() {
            return false;
        }
        let _ = ShowWindow(w, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            w,
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
pub fn is_maximized(h: isize) -> bool {
    unsafe { IsZoomed(hwnd(h)).as_bool() }
}
