//! foxhound-helper — a small `agent_helper.py`-compatible HTTP API, served on the host and aimed at
//! one application's windows instead of a VM's whole desktop.
//!
//! Screenshots come from `foxhound-capture` (PrintWindow of the app's windows, composited, so they
//! work while the app is covered by other windows) and input goes through `foxhound-input` (posted
//! messages), so the real cursor and keyboard stay with the human. A background keeper also
//! demotes delayed app-created windows and restores the user's window if the target self-activates.
//!
//! Every coordinate is relative to the target's main window: `/health`'s `screen` is that window's
//! size, and `(0, 0)` is its top-left visible pixel. A harness written for the VM helper therefore
//! works unchanged — the "screen" is just the app.
//!
//! ```text
//! foxhound-helper [--process app.exe | --title TEXT | --hwnd N] [--bind 127.0.0.1] [--port 8770]
//!                   [--token T]   (or FOXHOUND_TOKEN)
//! ```

#[path = "target.rs"]
mod target;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use foxhound_input::keys::{key_for_name, press_chord, type_text};
use foxhound_input::pointer::{
    client_target, frame_action, post_button, post_move, post_syscommand, post_wheel, target_at,
    FrameAction, MouseButton,
};
use serde::Deserialize;
use serde_json::{json, Value};

use target::{
    all_app_windows, ensure_rendering, foreground_window, is_maximized, place_behind, resolve,
    restore_foreground, Group, TargetSpec,
};

/// What one request failed with, mapped to the helper's `{"error": ...}` JSON.
enum Fail {
    NoTarget(String),
    Bad(String),
    Internal(String),
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            Fail::NoTarget(m) => (StatusCode::CONFLICT, m),
            Fail::Bad(m) => (StatusCode::BAD_REQUEST, m),
            Fail::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

type Reply = Result<Response, Fail>;

fn ok_json(v: Value) -> Reply {
    Ok(Json(v).into_response())
}

/// Mutable driving state. One lock for all input so concurrent requests can't interleave a drag
/// with a click; screenshots take it too, since they may un-minimize the window.
struct Driver {
    spec: TargetSpec,
    /// Main window chosen last time; kept while alive so a large dialog can't become the stage.
    sticky_main: Option<isize>,
    /// The top-level window the agent last clicked: where typing goes next.
    last_clicked: Option<isize>,
    /// Virtual pointer in stage coordinates (the real cursor is never moved).
    cursor: (i32, i32),
    /// Last foreground window not owned by the target. Target windows are kept behind this anchor.
    background_anchor: Option<isize>,
}

impl Driver {
    fn group(&mut self) -> Result<Group, Fail> {
        let g = resolve(&self.spec, self.sticky_main).map_err(Fail::NoTarget)?;
        if ensure_rendering(g.main.hwnd) {
            // Restored from minimized: give it a beat to paint, then re-read positions.
            std::thread::sleep(Duration::from_millis(250));
            let g = resolve(&self.spec, Some(g.main.hwnd)).map_err(Fail::NoTarget)?;
            self.sticky_main = Some(g.main.hwnd);
            self.keep_background(&g);
            return Ok(g);
        }
        self.sticky_main = Some(g.main.hwnd);
        self.keep_background(&g);
        Ok(g)
    }

    fn keep_background(&mut self, group: &Group) -> usize {
        if self
            .background_anchor
            .is_some_and(|anchor| group.windows.iter().any(|w| w.hwnd == anchor))
        {
            self.background_anchor = None;
        }
        if let Some(current) = foreground_window() {
            if !group.windows.iter().any(|w| w.hwnd == current) {
                self.background_anchor = Some(current);
            }
        }
        // The target may have raised itself before the helper started. Desktop enumeration is
        // top-to-bottom, so the first non-target application is the best available displaced-user
        // anchor; never retain a target HWND as its own background anchor.
        if self.background_anchor.is_none() {
            self.background_anchor = all_app_windows()
                .into_iter()
                .find(|candidate| !group.windows.iter().any(|w| w.hwnd == candidate.hwnd))
                .map(|candidate| candidate.hwnd);
        }
        self.background_anchor
            .map(|anchor| place_behind(group, anchor))
            .unwrap_or(0)
    }

    /// Toolkits may create a dialog or popup after an injected message returns. Re-resolve a few
    /// times across that short window so those late HWNDs join the background group too.
    fn settle_background(&mut self) {
        for delay in [10, 25, 50] {
            std::thread::sleep(Duration::from_millis(delay));
            let Ok(group) = resolve(&self.spec, self.sticky_main) else {
                continue;
            };
            self.sticky_main = Some(group.main.hwnd);
            self.keep_background(&group);
        }
    }

    fn maintain_background(&mut self) {
        let Ok(group) = resolve(&self.spec, self.sticky_main) else {
            return;
        };
        self.sticky_main = Some(group.main.hwnd);
        let target_is_foreground = foreground_window()
            .is_some_and(|current| group.windows.iter().any(|w| w.hwnd == current));
        self.keep_background(&group);
        if target_is_foreground {
            if let Some(anchor) = self.background_anchor {
                restore_foreground(anchor);
            }
        }
    }

    /// Keyboard destination: the key window's focused native child if it has one (Win32 edits,
    /// Chromium render widgets), else the key window itself (Qt routes keys internally). Qt windows
    /// are first told they have focus, or shortcuts and emoji are silently dropped while inactive.
    fn key_sink(&mut self) -> Result<isize, Fail> {
        let g = self.group()?;
        let key_window = g.key_window(self.last_clicked);
        let kw = key_window.hwnd;
        if key_window.class.starts_with("Qt") {
            foxhound_input::keys::assume_focus(kw);
        }
        let sink = foxhound_input::resolve_input_sink(kw);
        let inside = sink == kw
            || unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsChild(hwnd(kw), hwnd(sink)).as_bool()
            };
        Ok(if inside { sink } else { kw })
    }
}

fn hwnd(v: isize) -> windows::Win32::Foundation::HWND {
    windows::Win32::Foundation::HWND(v as *mut std::ffi::c_void)
}

struct App {
    driver: Mutex<Driver>,
    token: Option<String>,
}

type Shared = Arc<App>;

/// Run Win32 work off the async runtime (PrintWindow, SendMessageTimeout and sleeps block).
async fn blocking<F>(app: Shared, f: F) -> Reply
where
    F: FnOnce(&App) -> Reply + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&app))
        .await
        .unwrap_or_else(|e| Err(Fail::Internal(format!("worker panicked: {e}"))))
}

async fn auth(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if let Some(want) = &app.token {
        let got = req
            .headers()
            .get("X-Agent-Token")
            .and_then(|v| v.to_str().ok());
        if got != Some(want.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

// ------------------------------------------------------------------ seeing

async fn health(State(app): State<Shared>) -> Reply {
    blocking(app, |app| {
        let mut d = app.driver.lock().unwrap();
        let (screen, target) = match d.group() {
            Ok(g) => {
                let s = g.stage();
                (
                    [s.width(), s.height()],
                    json!({ "hwnd": g.main.hwnd, "title": g.main.title, "pid": g.main.pid }),
                )
            }
            Err(_) => ([0, 0], Value::Null),
        };
        ok_json(json!({ "ok": true, "screen": screen, "target": target, "spec": d.spec }))
    })
    .await
}

async fn screenshot(State(app): State<Shared>) -> Reply {
    blocking(app, |app| {
        let group = app.driver.lock().unwrap().group()?;
        let shot = foxhound_capture::snapshot::compose(group.stage(), &group.layers())
            .map_err(Fail::Internal)?;
        let png = shot.to_png().map_err(Fail::Internal)?;
        let stage = group.stage();
        let mut resp = Response::new(Body::from(png));
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        for (k, v) in [
            ("X-Width", shot.width as i32),
            ("X-Height", shot.height as i32),
            ("X-Screen-Left", stage.left),
            ("X-Screen-Top", stage.top),
        ] {
            h.insert(k, HeaderValue::from(v));
        }
        Ok(resp)
    })
    .await
}

async fn cursor(State(app): State<Shared>) -> Reply {
    let (x, y) = app.driver.lock().unwrap().cursor;
    ok_json(json!({ "x": x, "y": y }))
}

#[derive(Deserialize)]
struct WindowsQuery {
    all: Option<String>,
}

/// The target app's windows, `rect` in stage coordinates (so the agent can click a dialog it sees
/// listed) and `screen_rect` in screen coordinates. `?all=1` lists every app on the desktop instead,
/// for choosing a target.
async fn windows_route(State(app): State<Shared>, Query(q): Query<WindowsQuery>) -> Reply {
    blocking(app, move |app| {
        if q.all.is_some_and(|v| v != "0") {
            let list: Vec<Value> = all_app_windows()
                .into_iter()
                .map(
                    |w| json!({ "hwnd": w.hwnd, "title": w.title, "pid": w.pid, "class": w.class }),
                )
                .collect();
            return ok_json(Value::Array(list));
        }
        let g = app.driver.lock().unwrap().group()?;
        let s = g.stage();
        let list: Vec<Value> = g
            .windows
            .iter()
            .map(|w| {
                let b = w.bounds;
                let mut v = serde_json::to_value(w).unwrap_or_default();
                v["rect"] = json!([
                    b.left - s.left,
                    b.top - s.top,
                    b.right - s.left,
                    b.bottom - s.top
                ]);
                v["screen_rect"] = json!([b.left, b.top, b.right, b.bottom]);
                v["main"] = json!(w.hwnd == g.main.hwnd);
                v
            })
            .collect();
        ok_json(Value::Array(list))
    })
    .await
}

#[derive(Deserialize, Default)]
struct TargetBody {
    process: Option<String>,
    pid: Option<u32>,
    title: Option<String>,
    hwnd: Option<i64>,
}

/// Choose what to drive: `{"process": "app.exe"}`, `{"title": "Untitled - Notepad"}` or
/// `{"hwnd": 132456}`.
async fn set_target(State(app): State<Shared>, Json(b): Json<TargetBody>) -> Reply {
    let spec = match (b.hwnd, b.title, b.process, b.pid) {
        (Some(h), _, _, _) => TargetSpec::Hwnd(h as isize),
        (None, Some(t), _, _) => TargetSpec::Title(t),
        (None, None, Some(p), _) => TargetSpec::Process(p),
        (None, None, None, Some(pid)) => TargetSpec::Pid(pid),
        _ => return Err(Fail::Bad("give one of process, pid, title or hwnd".into())),
    };
    {
        let mut d = app.driver.lock().unwrap();
        d.spec = spec;
        d.sticky_main = None;
        d.last_clicked = None;
        d.background_anchor = None;
    }
    health(State(app)).await
}

// ------------------------------------------------------------------ acting

#[derive(Deserialize)]
struct ClickBody {
    x: i32,
    y: i32,
    #[serde(default = "left")]
    button: String,
    #[serde(default = "one")]
    clicks: u32,
}
fn left() -> String {
    "left".into()
}
fn one() -> u32 {
    1
}

fn button(name: &str) -> Result<MouseButton, Fail> {
    MouseButton::parse(name).ok_or_else(|| Fail::Bad(format!("unknown button {name:?}")))
}

async fn click(State(app): State<Shared>, Json(b): Json<ClickBody>) -> Reply {
    blocking(app, move |app| {
        let btn = button(&b.button)?;
        let mut d = app.driver.lock().unwrap();
        let g = d.group()?;
        let (sx, sy) = g.to_screen(b.x, b.y);
        let top = g.window_at(sx, sy).clone();
        d.cursor = (b.x, b.y);

        match frame_action(top.hwnd, sx, sy, is_maximized(top.hwnd)) {
            FrameAction::Client => {}
            FrameAction::SysCommand(cmd) if btn == MouseButton::Left => {
                post_syscommand(top.hwnd, cmd);
                d.settle_background();
                return ok_json(json!({ "ok": true, "syscommand": cmd }));
            }
            FrameAction::SysCommand(hit) | FrameAction::Ignored(hit) => {
                // Title-bar drags, borders and minimize would need the real pointer; say so.
                return ok_json(json!({ "ok": true, "dropped": format!("window frame (hit-test {hit}) is not clickable without the real mouse") }));
            }
        }

        let t = target_at(top.hwnd, sx, sy);
        post_move(t, 0);
        for i in 0..b.clicks.max(1) {
            post_button(t, btn, true, btn.mk(), i % 2 == 1);
            post_button(t, btn, false, 0, false);
            std::thread::sleep(Duration::from_millis(15));
        }
        d.last_clicked = Some(top.hwnd);
        d.settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
struct MoveBody {
    x: i32,
    y: i32,
    #[serde(default)]
    duration: f64,
}

/// Straight-line path from `from` to `to`, one point per ~16 ms of `duration` (at least the end).
fn path(from: (i32, i32), to: (i32, i32), duration: f64) -> Vec<(i32, i32)> {
    let steps = ((duration * 60.0).round() as usize).clamp(1, 600);
    (1..=steps)
        .map(|i| {
            let f = i as f64 / steps as f64;
            (
                (from.0 as f64 + (to.0 - from.0) as f64 * f).round() as i32,
                (from.1 as f64 + (to.1 - from.1) as f64 * f).round() as i32,
            )
        })
        .collect()
}

fn step_delay(duration: f64, steps: usize) -> Duration {
    Duration::from_secs_f64((duration.max(0.0) / steps.max(1) as f64).min(1.0))
}

async fn move_route(State(app): State<Shared>, Json(b): Json<MoveBody>) -> Reply {
    blocking(app, move |app| {
        let mut d = app.driver.lock().unwrap();
        let g = d.group()?;
        let pts = path(d.cursor, (b.x, b.y), b.duration);
        let delay = step_delay(b.duration, pts.len());
        for (x, y) in &pts {
            let (sx, sy) = g.to_screen(*x, *y);
            post_move(target_at(g.window_at(sx, sy).hwnd, sx, sy), 0);
            std::thread::sleep(delay);
        }
        d.cursor = (b.x, b.y);
        d.settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
struct DragBody {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    #[serde(default = "drag_duration")]
    duration: f64,
    #[serde(default = "left")]
    button: String,
}
fn drag_duration() -> f64 {
    0.3
}

async fn drag(State(app): State<Shared>, Json(b): Json<DragBody>) -> Reply {
    blocking(app, move |app| {
        let btn = button(&b.button)?;
        let mut d = app.driver.lock().unwrap();
        let g = d.group()?;
        let (sx, sy) = g.to_screen(b.x1, b.y1);
        let top = g.window_at(sx, sy).hwnd;
        let start = target_at(top, sx, sy);
        post_move(start, 0);
        post_button(start, btn, true, btn.mk(), false);
        // Everything until release belongs to the window that took the press.
        let pts = path((b.x1, b.y1), (b.x2, b.y2), b.duration);
        let delay = step_delay(b.duration, pts.len());
        for (x, y) in &pts {
            let (px, py) = g.to_screen(*x, *y);
            post_move(client_target(start.hwnd, px, py), btn.mk());
            std::thread::sleep(delay);
        }
        let (ex, ey) = g.to_screen(b.x2, b.y2);
        post_button(client_target(start.hwnd, ex, ey), btn, false, 0, false);
        d.cursor = (b.x2, b.y2);
        d.last_clicked = Some(top);
        d.settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
struct ScrollBody {
    amount: i32,
    x: Option<i32>,
    y: Option<i32>,
    #[serde(default)]
    horizontal: bool,
}

/// `amount` is raw wheel units like `pyautogui.scroll` on Windows (120 = one notch; negative scrolls
/// down). Large values are split into notch-sized messages, as a real wheel would send them.
async fn scroll(State(app): State<Shared>, Json(b): Json<ScrollBody>) -> Reply {
    blocking(app, move |app| {
        let mut d = app.driver.lock().unwrap();
        let g = d.group()?;
        let (x, y) = match (b.x, b.y) {
            (Some(x), Some(y)) => (x, y),
            _ => d.cursor,
        };
        d.cursor = (x, y);
        let (sx, sy) = g.to_screen(x, y);
        let t = target_at(g.window_at(sx, sy).hwnd, sx, sy);
        post_move(t, 0);
        let mut left = b.amount;
        while left != 0 {
            let chunk = left.clamp(-120, 120);
            post_wheel(t, sx, sy, chunk, 0, b.horizontal);
            left -= chunk;
        }
        d.settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
struct TypeBody {
    text: String,
    #[serde(default = "type_interval")]
    interval: f64,
}
fn type_interval() -> f64 {
    0.01
}

async fn type_route(State(app): State<Shared>, Json(b): Json<TypeBody>) -> Reply {
    blocking(app, move |app| {
        let sink = app.driver.lock().unwrap().key_sink()?;
        type_text(
            sink,
            &b.text,
            Duration::from_secs_f64(b.interval.clamp(0.0, 1.0)),
        );
        app.driver.lock().unwrap().settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Keys {
    Many(Vec<String>),
    One(String),
}

#[derive(Deserialize)]
struct KeyBody {
    keys: Keys,
}

async fn key(State(app): State<Shared>, Json(b): Json<KeyBody>) -> Reply {
    blocking(app, move |app| {
        let names = match b.keys {
            Keys::Many(v) => v,
            Keys::One(s) => vec![s],
        };
        let strokes = names
            .iter()
            .map(|n| key_for_name(n).ok_or_else(|| Fail::Bad(format!("unknown key {n:?}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let sink = app.driver.lock().unwrap().key_sink()?;
        press_chord(sink, &strokes).map_err(Fail::Internal)?;
        app.driver.lock().unwrap().settle_background();
        ok_json(json!({ "ok": true }))
    })
    .await
}

// ------------------------------------------------------------------ main

struct Args {
    bind: String,
    port: u16,
    token: Option<String>,
    spec: Option<TargetSpec>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        bind: "127.0.0.1".into(),
        port: std::env::var("FOXHOUND_PORT")
            .or_else(|_| std::env::var("AGENT_HELPER_PORT"))
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8770),
        token: std::env::var("FOXHOUND_TOKEN")
            .or_else(|_| std::env::var("AGENT_HELPER_TOKEN"))
            .ok()
            .filter(|t| !t.is_empty()),
        spec: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--bind" => a.bind = val()?,
            "--port" => a.port = val()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--token" => a.token = Some(val()?),
            "--process" => a.spec = Some(TargetSpec::Process(val()?)),
            "--pid" => {
                a.spec = Some(TargetSpec::Pid(
                    val()?.parse().map_err(|e| format!("--pid: {e}"))?,
                ))
            }
            "--title" => a.spec = Some(TargetSpec::Title(val()?)),
            "--hwnd" => {
                a.spec = Some(TargetSpec::Hwnd(
                    val()?.parse().map_err(|e| format!("--hwnd: {e}"))?,
                ))
            }
            "-h" | "--help" => {
                println!("{}", include_str!("usage.txt"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(a)
}

pub async fn run() {
    // Physical pixels everywhere: screenshots, window rects and posted client coordinates must agree.
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("foxhound-helper: {e}");
            std::process::exit(2);
        }
    };
    let spec = match args.spec {
        Some(spec) => spec,
        None => {
            eprintln!("foxhound-helper: give --process, --pid, --title, or --hwnd");
            std::process::exit(2);
        }
    };
    if args.bind != "127.0.0.1" && args.bind != "localhost" && args.token.is_none() {
        eprintln!("foxhound-helper: refusing to listen on {} without --token (anyone could drive this desktop)", args.bind);
        std::process::exit(2);
    }
    let app: Shared = Arc::new(App {
        driver: Mutex::new(Driver {
            spec: spec.clone(),
            sticky_main: None,
            last_clicked: None,
            cursor: (0, 0),
            background_anchor: foreground_window(),
        }),
        token: args.token,
    });

    // Some GUI toolkits raise or activate a dialog seconds after the input that created it. Keep
    // enforcing the background lease for the helper's whole lifetime, while `try_lock` ensures the
    // keeper never delays capture or input work.
    let keeper = Arc::downgrade(&app);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(25));
        let Some(app) = keeper.upgrade() else {
            break;
        };
        if let Ok(mut driver) = app.driver.try_lock() {
            driver.maintain_background();
        };
    });

    let router = Router::new()
        .route("/health", get(health))
        .route("/screenshot", get(screenshot))
        .route("/cursor", get(cursor))
        .route("/windows", get(windows_route))
        .route("/target", post(set_target))
        .route("/click", post(click))
        .route("/move", post(move_route))
        .route("/drag", post(drag))
        .route("/scroll", post(scroll))
        .route("/type", post(type_route))
        .route("/key", post(key))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app);

    let addr: SocketAddr = match format!("{}:{}", args.bind, args.port).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("foxhound-helper: bad bind address: {e}");
            std::process::exit(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("foxhound-helper: cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("foxhound-helper: driving {spec:?} on http://{addr}");
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("foxhound-helper: server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_ends_on_target_and_scales_with_duration() {
        assert_eq!(path((0, 0), (10, 20), 0.0), vec![(10, 20)]);
        let p = path((0, 0), (100, 0), 0.5);
        assert_eq!(p.len(), 30);
        assert_eq!(*p.last().unwrap(), (100, 0));
        assert!(p.windows(2).all(|w| w[1].0 >= w[0].0));
    }

    #[test]
    fn keys_accept_list_or_single_string() {
        let many: KeyBody = serde_json::from_str(r#"{"keys":["ctrl","s"]}"#).unwrap();
        assert!(matches!(many.keys, Keys::Many(ref v) if v.len() == 2));
        let one: KeyBody = serde_json::from_str(r#"{"keys":"enter"}"#).unwrap();
        assert!(matches!(one.keys, Keys::One(ref s) if s == "enter"));
    }
}
