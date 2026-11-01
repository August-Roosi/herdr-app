//! Frame sources for the browser pane: `CdpScreencast` shows a target of
//! an agent's Chromium through `Page.startScreencast` and sends user input
//! back as CDP `Input.*` events.

use super::cdp::{Cdp, Event, TargetInfo, Targets, block_on, pick_target};
use super::input;
use serde_json::{Value, json};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

/// A decoded frame (BGRA) and the page size it shows, in CSS px.
#[derive(Clone)]
pub struct Frame {
    pub image: Arc<gpui::RenderImage>,
    pub page: (u32, u32),
}

pub enum SourceEvent {
    Frame(Frame),
    /// All targets and the one shown.
    Targets(Vec<TargetInfo>, Option<String>),
    /// The shown target's main frame navigated.
    Url(String),
    /// The connection ended or stopped answering; the source is done.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    None,
    Left,
    Middle,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nav {
    Back,
    Forward,
    Reload,
    Go(String),
}

/// User input; points are page CSS px (see `input::map_point`).
#[derive(Clone, Debug)]
pub enum PageInput {
    Mouse { kind: MouseKind, x: f64, y: f64, button: MouseButton, clicks: u32 },
    Wheel { x: f64, y: f64, dx: f64, dy: f64 },
    Key { keystroke: gpui::Keystroke, down: bool },
    Text(String),
    Nav(Nav),
}

pub trait FrameSource {
    fn frames(&self) -> async_channel::Receiver<SourceEvent>;
    fn input(&self, ev: PageInput);
    /// Pins a target (`None`: follow the newest page).
    fn select(&self, target: Option<String>);
    /// The pane's size in device px: the screencast's max frame size.
    fn resize(&self, px: (u32, u32));
    fn stop(&self);
}

enum Cmd {
    Input(PageInput),
    Select(Option<String>),
    Resize((u32, u32)),
    Stop,
}

/// Shows a target of a browser-level CDP connection. Runs on its own
/// thread (JPEG decode included); ends with `SourceEvent::Ended` when the
/// connection closes or a call goes unanswered for `CALL_TIMEOUT`.
pub struct CdpScreencast {
    cmds: async_channel::Sender<Cmd>,
    frames: async_channel::Receiver<SourceEvent>,
}

impl CdpScreencast {
    pub fn start(cdp: Cdp, px: (u32, u32)) -> Self {
        let (cmds, cmd_rx) = async_channel::unbounded();
        let (tx, frames) = async_channel::bounded(OUT_CAP);
        let out = Out { tx, rx: frames.clone() };
        let spawned = std::thread::Builder::new().name("cdp-screencast".into()).spawn(move || {
            let mut w = Worker { cdp, out, px, targets: Targets::default(), pinned: None, shown: None, listed: None, buttons: 0 };
            if w.run(cmd_rx).is_err() {
                w.out.push(SourceEvent::Ended);
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("browser: cannot start screencast thread: {e}");
            // the closure (and its sender) is gone: `frames` reports the end
            let (tx, frames) = async_channel::bounded(OUT_CAP);
            let _ = tx.try_send(SourceEvent::Ended);
            return CdpScreencast { cmds, frames };
        }
        CdpScreencast { cmds, frames }
    }
}

impl FrameSource for CdpScreencast {
    fn frames(&self) -> async_channel::Receiver<SourceEvent> {
        self.frames.clone()
    }
    fn input(&self, ev: PageInput) {
        let _ = self.cmds.try_send(Cmd::Input(ev));
    }
    fn select(&self, target: Option<String>) {
        let _ = self.cmds.try_send(Cmd::Select(target));
    }
    fn resize(&self, px: (u32, u32)) {
        let _ = self.cmds.try_send(Cmd::Resize(px));
    }
    fn stop(&self) {
        let _ = self.cmds.try_send(Cmd::Stop);
    }
}

/// How far a call may go unanswered before the source gives up.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Events waiting for the view: a slow view sees the latest frames.
const OUT_CAP: usize = 2;

/// The view's channel. Never blocks: when full, the oldest pending frame
/// goes (other events are kept unless only events are pending).
struct Out {
    tx: async_channel::Sender<SourceEvent>,
    rx: async_channel::Receiver<SourceEvent>,
}

impl Out {
    fn push(&self, ev: SourceEvent) {
        let ev = match self.tx.try_send(ev) {
            Err(async_channel::TrySendError::Full(ev)) => ev,
            _ => return,
        };
        let mut q: Vec<SourceEvent> = std::iter::from_fn(|| self.rx.try_recv().ok()).collect();
        q.push(ev);
        while q.len() > OUT_CAP {
            let i = q.iter().position(|e| matches!(e, SourceEvent::Frame(_))).unwrap_or(0);
            q.remove(i);
        }
        for e in q {
            let _ = self.tx.try_send(e);
        }
    }
}

/// The source is done: the connection closed or stopped answering.
struct Gone;

struct Shown {
    target: String,
    session: String,
}

struct Worker {
    cdp: Cdp,
    out: Out,
    px: (u32, u32),
    targets: Targets,
    pinned: Option<String>,
    shown: Option<Shown>,
    /// The last `SourceEvent::Targets` sent.
    listed: Option<(Vec<TargetInfo>, Option<String>)>,
    /// Mouse buttons held (CDP `buttons` bitmask).
    buttons: u32,
}

enum Next {
    Event(Event),
    EventsClosed,
    Cmd(Cmd),
    CmdsClosed,
}

impl Worker {
    fn run(&mut self, cmds: async_channel::Receiver<Cmd>) -> Result<(), Gone> {
        let events = self.cdp.events();
        self.call("Target.setDiscoverTargets", json!({"discover": true}), None)?.ok();
        if let Ok(r) = self.call("Target.getTargets", json!({}), None)? {
            for info in r["targetInfos"].as_array().into_iter().flatten() {
                self.targets.apply("Target.targetCreated", &json!({"targetInfo": info}));
            }
        }
        self.follow()?;
        loop {
            match next(&events, &cmds) {
                Next::Cmd(Cmd::Stop) | Next::CmdsClosed => return Ok(()),
                Next::Cmd(c) => self.command(c)?,
                Next::Event(e) => self.event(e)?,
                Next::EventsClosed => return Err(Gone),
            }
        }
    }

    /// A call's result or CDP error; `Gone` on timeout or a closed connection.
    fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Result<Value, anyhow::Error>, Gone> {
        match block_on_deadline(self.cdp.call(method, params, session), Instant::now() + CALL_TIMEOUT) {
            None => {
                tracing::warn!("browser: {method} unanswered for {CALL_TIMEOUT:?}");
                Err(Gone)
            }
            Some(Err(_)) if self.cdp.is_closed() => Err(Gone),
            Some(r) => {
                if let Err(e) = &r {
                    tracing::debug!("browser: {e}");
                }
                Ok(r)
            }
        }
    }

    /// Shows what `pick_target` picks, switching sessions if it changed.
    fn follow(&mut self) -> Result<(), Gone> {
        let want = pick_target(&self.targets.list, self.pinned.as_deref());
        if want.as_deref() != self.shown.as_ref().map(|s| s.target.as_str()) {
            if let Some(old) = self.shown.take() {
                self.call("Page.stopScreencast", json!({}), Some(&old.session))?.ok();
                self.call("Target.detachFromTarget", json!({"sessionId": old.session}), None)?.ok();
            }
            if let Some(target) = want {
                self.attach(target)?;
            }
        }
        self.list();
        Ok(())
    }

    fn attach(&mut self, target: String) -> Result<(), Gone> {
        let Ok(r) = self.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None)? else { return Ok(()) };
        let Some(session) = r["sessionId"].as_str().map(str::to_string) else { return Ok(()) };
        self.call("Page.enable", json!({}), Some(&session))?.ok();
        self.shown = Some(Shown { target: target.clone(), session });
        self.start_screencast()?;
        if let Some(t) = self.targets.list.iter().find(|t| t.id == target) {
            self.out.push(SourceEvent::Url(t.url.clone()));
        }
        Ok(())
    }

    /// The viewport is the agent's: only the frames' max size follows the pane.
    fn start_screencast(&self) -> Result<(), Gone> {
        let Some(s) = &self.shown else { return Ok(()) };
        let params = json!({"format": "jpeg", "quality": 70, "maxWidth": self.px.0, "maxHeight": self.px.1});
        self.call("Page.startScreencast", params, Some(&s.session))?.ok();
        Ok(())
    }

    /// Sends `Targets` if the list or the shown target changed.
    fn list(&mut self) {
        let now = (self.targets.list.clone(), self.shown.as_ref().map(|s| s.target.clone()));
        if self.listed.as_ref() != Some(&now) {
            self.listed = Some(now.clone());
            self.out.push(SourceEvent::Targets(now.0, now.1));
        }
    }

    fn event(&mut self, e: Event) -> Result<(), Gone> {
        let ours = e.session.is_some() && e.session.as_deref() == self.shown.as_ref().map(|s| s.session.as_str());
        let p = &e.params;
        match e.method.as_str() {
            m if m.starts_with("Target.target") => {
                if self.targets.apply(m, p) {
                    self.follow()?;
                } else {
                    self.list();
                }
            }
            "Target.detachedFromTarget" => {
                if p["sessionId"].as_str() == self.shown.as_ref().map(|s| s.session.as_str()) {
                    self.shown = None;
                    self.follow()?;
                }
            }
            "Page.screencastFrame" if ours => {
                match decode(p) {
                    Some(f) => self.out.push(SourceEvent::Frame(f)),
                    None => tracing::debug!("browser: undecodable screencast frame"),
                }
                // acked once handed on: the browser sends the next one
                self.call("Page.screencastFrameAck", json!({"sessionId": p["sessionId"]}), e.session.as_deref())?.ok();
            }
            "Page.frameNavigated" if ours && p["frame"]["parentId"].is_null() => {
                if let Some(url) = p["frame"]["url"].as_str() {
                    self.out.push(SourceEvent::Url(url.to_string()));
                }
            }
            "Page.navigatedWithinDocument" if ours && p["frameId"].as_str() == self.shown.as_ref().map(|s| s.target.as_str()) => {
                if let Some(url) = p["url"].as_str() {
                    self.out.push(SourceEvent::Url(url.to_string()));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn command(&mut self, c: Cmd) -> Result<(), Gone> {
        match c {
            Cmd::Select(t) => {
                self.pinned = t;
                self.follow()
            }
            Cmd::Resize(px) => {
                if px == self.px {
                    return Ok(());
                }
                self.px = px;
                if let Some(s) = &self.shown {
                    self.call("Page.stopScreencast", json!({}), Some(&s.session))?.ok();
                }
                self.start_screencast()
            }
            Cmd::Input(i) => self.input(i),
            Cmd::Stop => Ok(()),
        }
    }

    fn input(&mut self, i: PageInput) -> Result<(), Gone> {
        let Some(session) = self.shown.as_ref().map(|s| s.session.clone()) else { return Ok(()) };
        let session = Some(session.as_str());
        let calls: Vec<(&str, Value)> = match i {
            PageInput::Mouse { kind, x, y, button, clicks } => {
                let bit = match button {
                    MouseButton::None => 0,
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 4,
                };
                let kind = match kind {
                    MouseKind::Down => {
                        self.buttons |= bit;
                        "mousePressed"
                    }
                    MouseKind::Up => {
                        self.buttons &= !bit;
                        "mouseReleased"
                    }
                    MouseKind::Move => "mouseMoved",
                };
                let button = match button {
                    MouseButton::None => "none",
                    MouseButton::Left => "left",
                    MouseButton::Middle => "middle",
                    MouseButton::Right => "right",
                };
                vec![("Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": button, "buttons": self.buttons, "clickCount": clicks}))]
            }
            PageInput::Wheel { x, y, dx, dy } => vec![("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}))],
            PageInput::Key { keystroke, down } => input::key_event(&keystroke, down).into_iter().map(|p| ("Input.dispatchKeyEvent", p)).collect(),
            PageInput::Text(text) => vec![("Input.insertText", json!({"text": text}))],
            PageInput::Nav(Nav::Go(url)) => vec![("Page.navigate", json!({"url": url}))],
            PageInput::Nav(Nav::Reload) => vec![("Page.reload", json!({}))],
            PageInput::Nav(nav) => {
                let Ok(h) = self.call("Page.getNavigationHistory", json!({}), session)? else { return Ok(()) };
                let step = if nav == Nav::Back { -1 } else { 1 };
                let i = h["currentIndex"].as_i64().unwrap_or(-1) + step;
                let entry = usize::try_from(i).ok().and_then(|i| h["entries"].get(i));
                match entry.and_then(|e| e["id"].as_i64()) {
                    Some(id) => vec![("Page.navigateToHistoryEntry", json!({"entryId": id}))],
                    None => vec![],
                }
            }
        };
        for (method, params) in calls {
            self.call(method, params, session)?.ok();
        }
        Ok(())
    }
}

/// The next command or CDP event, commands first.
fn next(events: &async_channel::Receiver<Event>, cmds: &async_channel::Receiver<Cmd>) -> Next {
    let mut e = std::pin::pin!(events.recv());
    let mut c = std::pin::pin!(cmds.recv());
    block_on(std::future::poll_fn(|cx| {
        if let Poll::Ready(r) = c.as_mut().poll(cx) {
            return Poll::Ready(r.map_or(Next::CmdsClosed, Next::Cmd));
        }
        if let Poll::Ready(r) = e.as_mut().poll(cx) {
            return Poll::Ready(r.map_or(Next::EventsClosed, Next::Event));
        }
        Poll::Pending
    }))
}

/// A `Page.screencastFrame`'s JPEG as a BGRA image, with the page's size.
fn decode(params: &Value) -> Option<Frame> {
    use base64::Engine as _;
    let jpeg = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str()?).ok()?;
    let mut img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.into_rgba8();
    for px in img.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let m = &params["metadata"];
    let page = (m["deviceWidth"].as_f64()?.round() as u32, m["deviceHeight"].as_f64()?.round() as u32);
    Some(Frame { image: Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(img)])), page })
}

/// Drives `f` on this thread until it ends or `deadline` passes (`None`).
fn block_on_deadline<F: Future>(f: F, deadline: Instant) -> Option<F::Output> {
    use std::task::{Context, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return Some(v);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::park_timeout(deadline - now);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Chromium, chromium_bin};
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for an event `want` picks, up to `secs`; panics on timeout.
    fn wait<T>(rx: &async_channel::Receiver<SourceEvent>, secs: f64, what: &str, mut want: impl FnMut(SourceEvent) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            match rx.try_recv() {
                Ok(e) => {
                    if let Some(t) = want(e) {
                        return t;
                    }
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "timed out after {secs} s waiting for {what}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("source events ended while waiting for {what}: {e}"),
            }
        }
    }

    fn frame(e: SourceEvent) -> Option<Frame> {
        match e {
            SourceEvent::Frame(f) => Some(f),
            _ => None,
        }
    }

    /// BGRA at page CSS px `(x, y)`.
    fn pixel(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let size = f.image.size(0);
        let (w, h) = (size.width.0 as u32, size.height.0 as u32);
        let (x, y) = (x * w / f.page.0, y * h / f.page.1);
        let i = ((y * w + x) * 4) as usize;
        f.image.as_bytes(0).unwrap()[i..i + 4].try_into().unwrap()
    }

    fn center(f: &Frame) -> [u8; 4] {
        pixel(f, f.page.0 / 2, f.page.1 / 2)
    }

    /// Near `bgr` (JPEG is lossy).
    fn near(p: [u8; 4], bgr: [u8; 3]) -> bool {
        (0..3).all(|i| p[i].abs_diff(bgr[i]) < 50)
    }

    fn tiny(w: u32) -> SourceEvent {
        let img = image::RgbaImage::new(1, 1);
        SourceEvent::Frame(Frame { image: Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(img)])), page: (w, 1) })
    }

    fn kinds(rx: &async_channel::Receiver<SourceEvent>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match e {
                SourceEvent::Frame(f) => format!("f{}", f.page.0),
                SourceEvent::Url(u) => u,
                SourceEvent::Targets(..) => "targets".into(),
                SourceEvent::Ended => "ended".into(),
            })
            .collect()
    }

    #[test]
    fn out_drops_oldest_frame_never_blocks() {
        let (tx, rx) = async_channel::bounded(OUT_CAP);
        let out = Out { tx, rx: rx.clone() };
        for w in 1..=3 {
            out.push(tiny(w));
        }
        assert_eq!(kinds(&rx), ["f2", "f3"]);
        // events push frames out, oldest first; only events left: oldest goes
        out.push(tiny(1));
        out.push(SourceEvent::Url("u".into()));
        out.push(SourceEvent::Targets(vec![], None));
        assert_eq!(kinds(&rx), ["u", "targets"]);
        out.push(SourceEvent::Url("u".into()));
        out.push(tiny(1));
        out.push(SourceEvent::Ended);
        assert_eq!(kinds(&rx), ["u", "ended"]);
    }

    const PAGE: &str = "data:text/html,<style>html,body{margin:0;height:100%25}%23spin{position:fixed;left:0;top:0;width:40px;height:40px;background:blue;animation:s 1s linear infinite}@keyframes s{to{transform:rotate(360deg)}}</style><body onclick=\"document.body.style.background='red'\"><div id=spin></div>";

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn screencast_frames_and_click() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start(&bin);
        let source = CdpScreencast::start(chrome.cdp(), (1280, 800));
        let rx = source.frames();
        let first = wait(&rx, 3.0, "a first frame", frame);
        assert!(first.page.0 > 0 && first.page.1 > 0);

        source.input(PageInput::Nav(Nav::Go(PAGE.into())));
        wait(&rx, 5.0, "the page's url", |e| matches!(e, SourceEvent::Url(u) if u.starts_with("data:text/html")).then_some(()));
        // a frame of the loaded page: its spinner's center is blue
        let f = wait(&rx, 5.0, "a frame of the page", |e| frame(e).filter(|f| near(pixel(f, 20, 20), [255, 0, 0])));
        assert!(near(center(&f), [255, 255, 255]), "{:?}", center(&f));
        let (x, y) = (f.page.0 as f64 / 2.0, f.page.1 as f64 / 2.0);
        for kind in [MouseKind::Move, MouseKind::Down, MouseKind::Up] {
            let button = if kind == MouseKind::Move { MouseButton::None } else { MouseButton::Left };
            source.input(PageInput::Mouse { kind, x, y, button, clicks: 1 });
        }
        wait(&rx, 3.0, "a red frame", |e| frame(e).filter(|f| near(center(f), [0, 0, 255])));

        // the spinner animates: count frames for a while (all frames acked)
        let start = Instant::now();
        let mut n = 0;
        while start.elapsed() < Duration::from_secs(2) {
            if let Ok(SourceEvent::Frame(_)) = rx.try_recv() {
                n += 1;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        eprintln!("screencast: {n} frames in 2 s ({:.1} fps)", n as f64 / 2.0);
        assert!(n >= 10, "only {n} frames in 2 s: are frames acked?");

        // resize restarts the screencast with a smaller max size
        source.resize((640, 400));
        wait(&rx, 3.0, "a smaller frame", |e| frame(e).filter(|f| f.image.size(0).width.0 <= 640));
        source.stop();
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn reopens_after_browser_restart() {
        let Some(bin) = chromium_bin() else { return };
        let mut chrome = Chromium::start(&bin);
        let source = CdpScreencast::start(chrome.cdp(), (800, 600));
        let rx = source.frames();
        wait(&rx, 3.0, "a first frame", frame);

        chrome.restart();
        wait(&rx, 5.0, "Ended", |e| matches!(e, SourceEvent::Ended).then_some(()));
        let again = CdpScreencast::start(chrome.cdp(), (800, 600));
        wait(&again.frames(), 3.0, "a frame after the restart", frame);
        again.stop();
    }
}
