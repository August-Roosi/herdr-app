//! SPIKE (throwaway): an embedded CEF (Chromium) browser as a native child
//! window over a browser pane. Only on Windows, only with HERDR_SPIKE_CEF=<url>.
//! Remote debugging on 127.0.0.1:9555, so `playwright-cli attach --cdp` works.

use gpui::{Bounds, Pixels, Window};

#[cfg(windows)]
mod imp {
    use super::*;
    use cef::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::Mutex;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos};

    pub const PORT: i32 = 9555;

    struct State {
        url: String,
        created: bool,
        from_pane: bool,
        rect: (i32, i32, i32, i32),
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    wrap_client! {
        struct SpikeClient;
        impl Client {}
    }

    fn log(msg: &str) {
        let dir = crate::mods::app_dir();
        let _ = std::fs::create_dir_all(&dir);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("spike-cef.log")) {
            let _ = writeln!(f, "{msg}");
        }
    }

    /// CEF starts its helpers (renderer, GPU, ...) as this same program with
    /// `--type=...`; they run CEF and exit before anything else of the app.
    pub fn subprocess() {
        if !std::env::args().any(|a| a.starts_with("--type=")) {
            return;
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = args::Args::new();
        let code = execute_process(Some(args.as_main_args()), None::<&mut App>, std::ptr::null_mut());
        std::process::exit(code.max(0));
    }

    /// Starts CEF's browser process with its own UI thread (multi-threaded
    /// message loop), so GPUI's message loop stays as it is.
    pub fn init() {
        let Ok(url) = std::env::var("HERDR_SPIKE_CEF") else { return };
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = args::Args::new();
        let home = crate::mods::app_dir();
        let settings = Settings {
            no_sandbox: 1,
            multi_threaded_message_loop: 1,
            remote_debugging_port: PORT,
            root_cache_path: CefString::from(home.join("cef-cache").to_string_lossy().as_ref()),
            log_file: CefString::from(home.join("cef.log").to_string_lossy().as_ref()),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        };
        let ok = initialize(Some(args.as_main_args()), Some(&settings), None::<&mut App>, std::ptr::null_mut());
        log(&format!("cef initialize -> {ok} (url {url})"));
        if ok == 1 {
            *STATE.lock().unwrap() = Some(State { url, created: false, from_pane: false, rect: (0, 0, 0, 0) });
        }
    }

    fn hwnd(window: &Window) -> Option<windows_sys::Win32::Foundation::HWND> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as _),
            _ => None,
        }
    }

    /// Puts the browser over `bounds` (window px, logical). `pane`: a browser
    /// pane's page area; otherwise a fallback rect used until a pane shows.
    pub fn place(window: &Window, bounds: Bounds<Pixels>, pane: bool) {
        let mut guard = STATE.lock().unwrap();
        let Some(state) = guard.as_mut() else { return };
        if !pane && state.from_pane {
            return;
        }
        let Some(parent) = hwnd(window) else { return };
        let s = window.scale_factor();
        let rect = (
            (f32::from(bounds.origin.x) * s).round() as i32,
            (f32::from(bounds.origin.y) * s).round() as i32,
            (f32::from(bounds.size.width) * s).round().max(1.0) as i32,
            (f32::from(bounds.size.height) * s).round().max(1.0) as i32,
        );
        state.from_pane |= pane;
        if !state.created {
            state.created = true;
            state.rect = rect;
            let info = WindowInfo::default().set_as_child(
                sys::HWND(parent as _),
                &Rect { x: rect.0, y: rect.1, width: rect.2, height: rect.3 },
            );
            let mut client = SpikeClient::new();
            let r = browser_host_create_browser(
                Some(&info),
                Some(&mut client),
                Some(&CefString::from(state.url.as_str())),
                Some(&BrowserSettings::default()),
                None,
                None,
            );
            log(&format!("create_browser -> {r} at {rect:?} (pane {pane}, scale {s})"));
            return;
        }
        if state.rect == rect {
            return;
        }
        let class: Vec<u16> = "CefBrowserWindow\0".encode_utf16().collect();
        let child = unsafe { FindWindowExW(parent, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) };
        if child.is_null() {
            return; // not created yet: the next paint tries again
        }
        state.rect = rect;
        unsafe { SetWindowPos(child, std::ptr::null_mut(), rect.0, rect.1, rect.2, rect.3, SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS) };
        log(&format!("moved to {rect:?} (pane {pane})"));
    }
}

#[cfg(windows)]
pub use imp::{init, place, subprocess};

#[cfg(not(windows))]
pub fn subprocess() {}
#[cfg(not(windows))]
pub fn init() {}
#[cfg(not(windows))]
pub fn place(_window: &Window, _bounds: Bounds<Pixels>, _pane: bool) {}
