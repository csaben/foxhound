//! Per-window typing and key chords, beyond the gameplay [`KeySink`](crate::sink::KeySink).
//!
//! - [`type_text`] posts `WM_CHAR` per UTF-16 unit, so any Unicode (accents, CJK, emoji surrogate
//!   pairs) arrives without depending on the keyboard layout. Newline and tab go as real keys.
//! - [`press_chord`] presses `ctrl+s`-style chords. Posted `WM_KEYDOWN(VK_CONTROL)` alone is not
//!   enough: toolkits (Qt, WinForms, `TranslateAccelerator`) ask `GetKeyState` whether Ctrl is down,
//!   and the target thread's key state never saw a real Ctrl. So the chord borrows the target's
//!   input queue with `AttachThreadInput`, marks the modifiers down in the *shared* key-state array
//!   with `SetKeyboardState`, delivers the key synchronously, and restores the state. Only the
//!   target's queue is touched; the human's foreground app never sees any of it. (AutoHotkey's
//!   `ControlSend` uses the same technique.)
//!
//! Key names follow pyautogui (`enter`, `pgdn`, `ctrlleft`, `f5`, `win`, single characters, …).

use std::ffi::c_void;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardState, MapVirtualKeyW, SetKeyboardState, VkKeyScanW, MAPVK_VK_TO_VSC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CHAR,
    WM_KEYDOWN, WM_KEYUP, WM_SETFOCUS, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_RETURN: u32 = 0x0D;
const VK_TAB: u32 = 0x09;

/// How long a synchronous key delivery may take before we stop waiting. A shortcut that opens a
/// modal dialog never "returns"; the key was still delivered, we just stop blocking on it.
const SEND_TIMEOUT_MS: u32 = 1000;

/// Pause around an emoji so a toolkit's queued key events land before and after it, in order.
const ASTRAL_SETTLE: Duration = Duration::from_millis(40);

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

/// One key of a chord: its virtual-key code, plus whether the character needs Shift on the current
/// layout (`"+"` is Shift+`=` on US) so `ctrl` + `+` becomes Ctrl+Shift+`=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    pub vk: u32,
    pub shift: bool,
}

/// Map a pyautogui key name to a virtual-key code. Single characters resolve through the active
/// keyboard layout (`VkKeyScanW`), so punctuation works; `None` for names we don't know.
pub fn key_for_name(name: &str) -> Option<KeyStroke> {
    let n = name.to_ascii_lowercase();
    let plain = |vk| Some(KeyStroke { vk, shift: false });
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return plain(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return plain(c as u32);
        }
        let mut buf = [0u16; 2];
        if c.encode_utf16(&mut buf).len() == 1 {
            let r = unsafe { VkKeyScanW(buf[0]) };
            if r != -1 {
                let (vk, mods) = ((r as u16 & 0xFF) as u32, (r as u16 >> 8) as u8);
                return Some(KeyStroke {
                    vk,
                    shift: mods & 1 != 0,
                });
            }
        }
        return None;
    }
    if let Some(num) = n.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        if (1..=24).contains(&num) {
            return plain(0x70 + num - 1);
        }
    }
    if let Some(num) = n.strip_prefix("num").and_then(|d| d.parse::<u32>().ok()) {
        if num <= 9 {
            return plain(0x60 + num);
        }
    }
    plain(match n.as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" | "\t" => VK_TAB,
        "space" | " " => 0x20,
        "backspace" | "\x08" => 0x08,
        "esc" | "escape" => 0x1B,
        "delete" | "del" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "shift" => VK_SHIFT,
        "shiftleft" => VK_LSHIFT,
        "shiftright" => VK_RSHIFT,
        "ctrl" | "control" => VK_CONTROL,
        "ctrlleft" => VK_LCONTROL,
        "ctrlright" => VK_RCONTROL,
        "alt" | "option" => VK_MENU,
        "altleft" | "optionleft" => VK_LMENU,
        "altright" | "optionright" => VK_RMENU,
        "win" | "winleft" | "command" => VK_LWIN,
        "winright" => VK_RWIN,
        "apps" => 0x5D,
        "capslock" => 0x14,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        "pause" => 0x13,
        "printscreen" | "prtsc" | "prtscr" | "prntscrn" | "print" => 0x2C,
        "add" => 0x6B,
        "subtract" => 0x6D,
        "multiply" => 0x6A,
        "divide" => 0x6F,
        "decimal" => 0x6E,
        "separator" => 0x6C,
        _ => return None,
    })
}

fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        VK_SHIFT
            | VK_CONTROL
            | VK_MENU
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
    )
}

/// Keys whose scan code carries the extended (E0) prefix; lParam bit 24 must say so or apps see the
/// numpad twin (`home` arriving as keypad 7).
fn is_extended(vk: u32) -> bool {
    matches!(
        vk,
        0x21..=0x28
            | 0x2D
            | 0x2E
            | 0x6F
            | VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | 0x5D
            | 0x90
            | 0x2C
    )
}

/// The generic + sided key-state slots a modifier occupies (`ctrl` sets VK_CONTROL and VK_LCONTROL).
fn state_slots(vk: u32) -> &'static [u32] {
    match vk {
        VK_SHIFT | VK_LSHIFT => &[VK_SHIFT, VK_LSHIFT],
        VK_RSHIFT => &[VK_SHIFT, VK_RSHIFT],
        VK_CONTROL | VK_LCONTROL => &[VK_CONTROL, VK_LCONTROL],
        VK_RCONTROL => &[VK_CONTROL, VK_RCONTROL],
        VK_MENU | VK_LMENU => &[VK_MENU, VK_LMENU],
        VK_RMENU => &[VK_MENU, VK_RMENU],
        VK_LWIN => &[VK_LWIN],
        VK_RWIN => &[VK_RWIN],
        _ => &[],
    }
}

/// `lParam` for a key message: repeat 1, scan code, extended bit, Alt context bit (29), and for
/// key-up the previous-state (30) and transition (31) bits.
fn key_lparam(vk: u32, up: bool, alt_down: bool) -> LPARAM {
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) } & 0xFF;
    let mut lp: u32 = 1 | (scan << 16);
    if is_extended(vk) {
        lp |= 1 << 24;
    }
    if alt_down {
        lp |= 1 << 29;
    }
    if up {
        lp |= (1 << 30) | (1 << 31);
    }
    LPARAM(lp as i32 as isize)
}

/// With Alt held, Windows delivers keys as `WM_SYSKEY*` (that is what menus and Alt+F4 listen to).
fn key_message(up: bool, alt_down: bool, vk: u32) -> u32 {
    let sys = alt_down || vk == VK_MENU || vk == VK_LMENU || vk == VK_RMENU || vk == 0x79; // F10
    match (sys, up) {
        (true, false) => WM_SYSKEYDOWN,
        (true, true) => WM_SYSKEYUP,
        (false, false) => WM_KEYDOWN,
        (false, true) => WM_KEYUP,
    }
}

/// Tell a toolkit window it has keyboard focus, without Windows changing the foreground window.
///
/// Qt only routes some keyboard input while it believes one of its windows is focused:
/// `QShortcut` / menu accelerators fire only for the active window, and a `WM_CHAR` surrogate pair
/// (emoji) is delivered to `QGuiApplication::focusObject()`, which is null while the app is
/// inactive. Qt derives "focused" from `WM_SETFOCUS`, so sending one makes it behave as the active
/// app while the human's foreground window is untouched. It lasts until the app sees a real focus
/// change. Harmless to repeat (Qt ignores a focus change to the window it already has).
pub fn assume_focus(target: isize) {
    unsafe {
        let mut out = 0usize;
        let _ = SendMessageTimeoutW(
            hwnd(target),
            WM_SETFOCUS,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            SEND_TIMEOUT_MS,
            Some(&mut out),
        );
    }
}

/// Post a plain key press (down + up) with no modifiers. Posted (not sent) on purpose: the target's
/// own message loop runs `TranslateMessage` on it, which produces the `WM_CHAR` that text fields
/// need, such as Enter in a line edit or Space on a focused button.
pub fn tap_key(target: isize, vk: u32) {
    unsafe {
        let h = hwnd(target);
        let _ = PostMessageW(
            h,
            key_message(false, false, vk),
            WPARAM(vk as usize),
            key_lparam(vk, false, false),
        );
        let _ = PostMessageW(
            h,
            key_message(true, false, vk),
            WPARAM(vk as usize),
            key_lparam(vk, true, false),
        );
    }
}

/// One UTF-16 unit as `WM_KEYDOWN(VK_PACKET)`, `WM_CHAR(unit)`, `WM_KEYUP(VK_PACKET)`, the shape
/// SendInput's `KEYEVENTF_UNICODE` produces. Qt needs it: it only joins an emoji's surrogate pair
/// when each half is the WM_CHAR following a key-down, and drops bare halves. Win32 controls ignore
/// the `VK_PACKET` key messages and take the WM_CHAR.
fn post_packet_char(target: isize, unit: u16) {
    const VK_PACKET: usize = 0xE7;
    let lp = 1u32 | ((unit as u32) << 16);
    unsafe {
        let h = hwnd(target);
        let _ = PostMessageW(h, WM_KEYDOWN, WPARAM(VK_PACKET), LPARAM(lp as i32 as isize));
        let _ = PostMessageW(
            h,
            WM_CHAR,
            WPARAM(unit as usize),
            LPARAM(lp as i32 as isize),
        );
        let up = lp | (1 << 30) | (1 << 31);
        let _ = PostMessageW(h, WM_KEYUP, WPARAM(VK_PACKET), LPARAM(up as i32 as isize));
    }
}

/// Type Unicode text into `target` (its focused control, per [`resolve_input_sink`]). `\n` and `\t`
/// are sent as Enter/Tab key presses; everything else as `VK_PACKET`-framed `WM_CHAR`s, one per
/// UTF-16 unit (see [`post_packet_char`]). `interval` spaces characters out for apps that debounce input.
///
/// [`resolve_input_sink`]: crate::resolve_input_sink
pub fn type_text(target: isize, text: &str, interval: Duration) {
    for ch in text.chars() {
        match ch {
            '\n' => tap_key(target, VK_RETURN),
            '\r' => {}
            '\t' => tap_key(target, VK_TAB),
            _ => {
                let mut buf = [0u16; 2];
                let units = ch.encode_utf16(&mut buf);
                // Qt queues ordinary characters as asynchronous key events but commits a surrogate
                // pair immediately, so an emoji overtakes letters typed just before it. Let the
                // queue drain on both sides of one.
                let astral = units.len() == 2;
                if astral {
                    std::thread::sleep(ASTRAL_SETTLE);
                }
                for unit in units.iter() {
                    post_packet_char(target, *unit);
                }
                if astral {
                    std::thread::sleep(ASTRAL_SETTLE);
                }
            }
        }
        if !interval.is_zero() {
            std::thread::sleep(interval);
        }
    }
}

/// Press a chord like pyautogui's `hotkey`: every key goes down in order, then up in reverse.
///
/// A chord without modifiers (`enter`, `f5`, `a`) is posted like [`tap_key`]. A chord with
/// modifiers (`ctrl`+`s`, `alt`+`f4`, `ctrl`+`shift`+`n`) is delivered synchronously while the
/// target thread's key state shows the modifiers held (see the module docs), then that state is put
/// back exactly as it was.
pub fn press_chord(target: isize, strokes: &[KeyStroke]) -> Result<(), String> {
    let mut keys: Vec<u32> = Vec::new();
    for s in strokes {
        if s.shift && !keys.contains(&VK_SHIFT) {
            keys.insert(0, VK_SHIFT);
        }
        keys.push(s.vk);
    }
    if keys.is_empty() {
        return Ok(());
    }
    if !keys.iter().any(|&k| is_modifier(k)) {
        for vk in keys {
            tap_key(target, vk);
        }
        return Ok(());
    }

    unsafe {
        let h = hwnd(target);
        let target_tid = GetWindowThreadProcessId(h, None);
        if target_tid == 0 {
            return Err("target window is gone".into());
        }
        let our_tid = GetCurrentThreadId();
        let attached =
            our_tid != target_tid && AttachThreadInput(our_tid, target_tid, true).as_bool();
        if !attached && our_tid != target_tid {
            return Err("could not attach to the target's input queue".into());
        }

        let mut original = [0u8; 256];
        let _ = GetKeyboardState(&mut original);
        let mut state = original;

        let send = |msg: u32, vk: u32, lp: LPARAM| {
            let mut out = 0usize;
            let _ = SendMessageTimeoutW(
                h,
                msg,
                WPARAM(vk as usize),
                lp,
                SMTO_ABORTIFHUNG,
                SEND_TIMEOUT_MS,
                Some(&mut out),
            );
        };
        let alt_held = |state: &[u8; 256]| state[VK_MENU as usize] & 0x80 != 0;

        for &vk in &keys {
            let alt = alt_held(&state);
            if is_modifier(vk) {
                for &slot in state_slots(vk) {
                    state[slot as usize] |= 0x80;
                }
                let _ = SetKeyboardState(&state);
            } else {
                state[vk as usize] |= 0x80;
                let _ = SetKeyboardState(&state);
            }
            send(key_message(false, alt, vk), vk, key_lparam(vk, false, alt));
        }
        for &vk in keys.iter().rev() {
            let alt = alt_held(&state) && !matches!(vk, VK_MENU | VK_LMENU | VK_RMENU);
            send(key_message(true, alt, vk), vk, key_lparam(vk, true, alt));
            for &slot in state_slots(vk) {
                state[slot as usize] &= !0x80;
            }
            state[vk as usize] &= !0x80;
            let _ = SetKeyboardState(&state);
        }

        let _ = SetKeyboardState(&original);
        if attached {
            let _ = AttachThreadInput(our_tid, target_tid, false);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyautogui_names_resolve() {
        assert_eq!(key_for_name("enter").map(|k| k.vk), Some(VK_RETURN));
        assert_eq!(key_for_name("Ctrl").map(|k| k.vk), Some(VK_CONTROL));
        assert_eq!(key_for_name("pgdn").map(|k| k.vk), Some(0x22));
        assert_eq!(key_for_name("f12").map(|k| k.vk), Some(0x7B));
        assert_eq!(
            key_for_name("s"),
            Some(KeyStroke {
                vk: b'S' as u32,
                shift: false
            })
        );
        assert_eq!(key_for_name("S").map(|k| k.vk), Some(b'S' as u32));
        assert_eq!(key_for_name("num7").map(|k| k.vk), Some(0x67));
        assert_eq!(key_for_name("definitely-not-a-key"), None);
    }

    #[test]
    fn syskey_messages_when_alt_is_involved() {
        assert_eq!(key_message(false, true, 0x73), WM_SYSKEYDOWN); // alt held + F4
        assert_eq!(key_message(false, false, VK_MENU), WM_SYSKEYDOWN);
        assert_eq!(key_message(true, false, b'S' as u32), WM_KEYUP);
    }

    #[test]
    fn keyup_lparam_sets_transition_bits() {
        let lp = key_lparam(0x26, true, false).0 as u32; // up arrow
        assert_ne!(lp & (1 << 24), 0, "arrows are extended keys");
        assert_ne!(lp & (1 << 31), 0);
        assert_ne!(lp & (1 << 30), 0);
    }
}
