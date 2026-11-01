//! Frame sources for the browser pane: `CdpScreencast` shows a target of
//! an agent's Chromium through `Page.startScreencast` and sends user input
//! back as CDP `Input.*` events.

use super::cdp::{Cdp, Event, TargetInfo, Targets, pick_target};
use super::input;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

/// Shows a target of a browser-level CDP connection. A worker thread talks
/// CDP; a decoder thread turns the latest frame into an image. Ends with
/// `SourceEvent::Ended` when the connection closes or a call goes
/// unanswered for `CALL_TIMEOUT`.
pub struct CdpScreencast {
    cmds: async_channel::Sender<Cmd>,
    frames: async_channel::Receiver<SourceEvent>,
    stats: Arc<Stats>,
}

/// Decoded frames and the time spent decoding them.
#[derive(Default)]
struct Stats {
    frames: AtomicU64,
    micros: AtomicU64,
}

impl CdpScreencast {
    pub fn start(cdp: Cdp, px: (u32, u32)) -> Self {
        let (cmds, cmd_rx) = async_channel::unbounded();
        let (out, frames) = Out::new();
        let out = Arc::new(out);
        let stats = Arc::new(Stats::default());
        let (acks, ack_rx) = async_channel::unbounded();
        let slot = Arc::new(Slot::default());
        let decoder = {
            let (slot, out, stats) = (slot.clone(), out.clone(), stats.clone());
            std::thread::Builder::new().name("cdp-decode".into()).spawn(move || decode_loop(&slot, &out, &stats, &acks))
        };
        let decoder = match decoder {
            Ok(d) => d,
            Err(e) => return Self::failed(cmds, stats, e),
        };
        let worker = std::thread::Builder::new().name("cdp-screencast".into()).spawn(move || {
            let mut w = Worker {
                cdp: Arc::new(cdp),
                out: out.clone(),
                slot: slot.clone(),
                px,
                targets: Targets::default(),
                pinned: None,
                shown: None,
                listed: None,
                buttons: 0,
                flights: Vec::new(),
                held: None,
                events_first: false,
            };
            let r = w.run(cmd_rx, ack_rx);
            slot.close();
            let _ = decoder.join();
            if r.is_err() {
                out.push(SourceEvent::Ended);
            }
        });
        match worker {
            Ok(_) => CdpScreencast { cmds, frames, stats },
            Err(e) => Self::failed(cmds, stats, e),
        }
    }

    fn failed(cmds: async_channel::Sender<Cmd>, stats: Arc<Stats>, e: std::io::Error) -> Self {
        tracing::warn!("browser: cannot start screencast threads: {e}");
        let (out, frames) = Out::new();
        out.push(SourceEvent::Ended);
        CdpScreencast { cmds, frames, stats }
    }

    /// Frames decoded so far and the total time spent decoding them.
    pub fn decode_stats(&self) -> (u64, Duration) {
        (self.stats.frames.load(Ordering::Relaxed), Duration::from_micros(self.stats.micros.load(Ordering::Relaxed)))
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

/// The view's channel. Events (urls, targets, the end) are never dropped;
/// of frames only the latest waits, so a slow view skips to the newest.
struct Out {
    tx: async_channel::Sender<SourceEvent>,
    rx: async_channel::Receiver<SourceEvent>,
    /// Serializes pushes (worker and decoder) so a re-queue keeps order.
    lock: Mutex<()>,
}

impl Out {
    fn new() -> (Out, async_channel::Receiver<SourceEvent>) {
        let (tx, rx) = async_channel::unbounded();
        (Out { tx, rx: rx.clone(), lock: Mutex::new(()) }, rx)
    }

    fn push(&self, ev: SourceEvent) {
        let _g = self.lock.lock().unwrap();
        if matches!(ev, SourceEvent::Frame(_)) && !self.rx.is_empty() {
            // drop a pending frame (at most one waits); keep events in order
            let q: Vec<SourceEvent> = std::iter::from_fn(|| self.rx.try_recv().ok()).collect();
            for e in q.into_iter().filter(|e| !matches!(e, SourceEvent::Frame(_))) {
                let _ = self.tx.try_send(e);
            }
        }
        let _ = self.tx.try_send(ev);
    }
}

/// A screencast frame not decoded yet: the frame's ack id and its session.
struct Raw {
    params: Value,
    session: Option<String>,
}

/// The latest undecoded frame, handed from the worker to the decoder.
#[derive(Default)]
struct Slot {
    state: Mutex<(Option<Raw>, bool /* closed */)>,
    ready: Condvar,
}

impl Slot {
    /// Puts `raw` in; returns the frame it replaced, never decoded.
    fn put(&self, raw: Raw) -> Option<Raw> {
        let old = self.state.lock().unwrap().0.replace(raw);
        self.ready.notify_one();
        old
    }

    /// The next frame, or `None` once closed.
    fn take(&self) -> Option<Raw> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.1 {
                return None;
            }
            if let Some(r) = s.0.take() {
                return Some(r);
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().1 = true;
        self.ready.notify_one();
    }
}

/// An ack the worker should send: (frame ack id, session).
type Ack = (Value, Option<String>);

fn decode_loop(slot: &Slot, out: &Out, stats: &Stats, acks: &async_channel::Sender<Ack>) {
    while let Some(raw) = slot.take() {
        // taken = consumed: the browser may send the next frame meanwhile
        let _ = acks.try_send((raw.params["sessionId"].clone(), raw.session));
        let t = Instant::now();
        match decode(&raw.params) {
            Some(f) => {
                stats.frames.fetch_add(1, Ordering::Relaxed);
                stats.micros.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                tracing::trace!("browser: frame decoded in {:?}", t.elapsed());
                out.push(SourceEvent::Frame(f));
            }
            None => tracing::debug!("browser: undecodable screencast frame"),
        }
    }
}

/// The source is done: the connection closed or stopped answering.
struct Gone;

struct Shown {
    target: String,
    session: String,
}

type Reply = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;

/// A call sent without waiting for its reply.
struct Flight {
    reply: Reply,
    deadline: Instant,
}

struct Worker {
    cdp: Arc<Cdp>,
    out: Arc<Out>,
    slot: Arc<Slot>,
    px: (u32, u32),
    targets: Targets,
    pinned: Option<String>,
    shown: Option<Shown>,
    /// The last `SourceEvent::Targets` sent.
    listed: Option<(Vec<TargetInfo>, Option<String>)>,
    /// Mouse buttons held (CDP `buttons` bitmask).
    buttons: u32,
    flights: Vec<Flight>,
    /// A command taken out of order by `coalesce`, handled next.
    held: Option<Cmd>,
    /// Alternates which of commands and events `next` looks at first.
    events_first: bool,
}

enum Next {
    Event(Event),
    Cmd(Cmd),
    Ack(Ack),
    Stop,
}

impl Worker {
    fn run(&mut self, cmds: async_channel::Receiver<Cmd>, acks: async_channel::Receiver<Ack>) -> Result<(), Gone> {
        let events = self.cdp.events();
        self.call("Target.setDiscoverTargets", json!({"discover": true}), None)?.ok();
        if let Ok(r) = self.call("Target.getTargets", json!({}), None)? {
            for info in r["targetInfos"].as_array().into_iter().flatten() {
                self.targets.apply("Target.targetCreated", &json!({"targetInfo": info}));
            }
        }
        self.follow()?;
        loop {
            match self.next(&events, &cmds, &acks)? {
                Next::Stop => return Ok(()),
                Next::Cmd(Cmd::Input(i @ PageInput::Mouse { kind: MouseKind::Move, .. })) => {
                    let (c, held) = coalesce(Cmd::Input(i), &cmds);
                    self.held = held;
                    self.command(c)?;
                }
                Next::Cmd(c) => self.command(c)?,
                Next::Event(e) => self.event(e)?,
                Next::Ack((id, session)) => self.fire("Page.screencastFrameAck", json!({"sessionId": id}), session.as_deref()),
            }
        }
    }

    /// Waits for the next ack, command or event (commands and events take
    /// turns), meanwhile finishing calls in flight.
    fn next(&mut self, events: &async_channel::Receiver<Event>, cmds: &async_channel::Receiver<Cmd>, acks: &async_channel::Receiver<Ack>) -> Result<Next, Gone> {
        if let Some(c) = self.held.take() {
            return Ok(match c {
                Cmd::Stop => Next::Stop,
                c => Next::Cmd(c),
            });
        }
        self.events_first = !self.events_first;
        let events_first = self.events_first;
        let deadline = self.flights.iter().map(|f| f.deadline).min();
        let (flights, cdp) = (&mut self.flights, &self.cdp);
        let mut e = std::pin::pin!(events.recv());
        let mut c = std::pin::pin!(cmds.recv());
        let mut a = std::pin::pin!(acks.recv());
        let next = block_on_deadline(
            std::future::poll_fn(|cx| {
                let mut lost = false;
                flights.retain_mut(|f| match f.reply.as_mut().poll(cx) {
                    Poll::Pending => true,
                    Poll::Ready(r) => {
                        if let Err(e) = r {
                            tracing::debug!("browser: {e}");
                            lost |= cdp.is_closed();
                        }
                        false
                    }
                });
                if lost {
                    return Poll::Ready(Err(Gone));
                }
                if let Poll::Ready(Ok(ack)) = a.as_mut().poll(cx) {
                    return Poll::Ready(Ok(Next::Ack(ack)));
                }
                let mut ev = |cx: &mut std::task::Context<'_>| e.as_mut().poll(cx).map(|r| r.map(Next::Event).map_err(|_| Gone));
                let mut cmd = |cx: &mut std::task::Context<'_>| {
                    c.as_mut().poll(cx).map(|r| {
                        Ok(match r {
                            Ok(Cmd::Stop) | Err(_) => Next::Stop,
                            Ok(c) => Next::Cmd(c),
                        })
                    })
                };
                if events_first {
                    if let Poll::Ready(r) = ev(cx) {
                        return Poll::Ready(r);
                    }
                    cmd(cx)
                } else {
                    if let Poll::Ready(r) = cmd(cx) {
                        return Poll::Ready(r);
                    }
                    ev(cx)
                }
            }),
            deadline,
        );
        next.unwrap_or_else(|| {
            tracing::warn!("browser: a call went unanswered for {CALL_TIMEOUT:?}");
            Err(Gone)
        })
    }

    /// A call's result or CDP error; `Gone` on timeout or a closed connection.
    fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Result<Value, anyhow::Error>, Gone> {
        match block_on_deadline(self.cdp.call(method, params, session), Some(Instant::now() + CALL_TIMEOUT)) {
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

    /// Sends a call now; `next` collects its reply (or its timeout).
    fn fire(&mut self, method: &str, params: Value, session: Option<&str>) {
        let (cdp, method, session) = (self.cdp.clone(), method.to_string(), session.map(str::to_string));
        let mut reply: Reply = Box::pin(async move { cdp.call(&method, params, session.as_deref()).await });
        // the first poll sends the request
        let waker = std::task::Waker::noop();
        if reply.as_mut().poll(&mut std::task::Context::from_waker(waker)).is_pending() {
            self.flights.push(Flight { reply, deadline: Instant::now() + CALL_TIMEOUT });
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
                // the decoder acks a frame when it takes it; one it never
                // takes (replaced by a newer frame) is acked here
                if let Some(old) = self.slot.put(Raw { params: e.params.clone(), session: e.session.clone() }) {
                    self.fire("Page.screencastFrameAck", json!({"sessionId": old.params["sessionId"]}), old.session.as_deref());
                }
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
            self.fire(method, params, session);
        }
        Ok(())
    }
}

/// After a mouse move, the latest of the moves queued right behind it; the
/// first other command found is returned too, to be handled next.
fn coalesce(c: Cmd, cmds: &async_channel::Receiver<Cmd>) -> (Cmd, Option<Cmd>) {
    let is_move = |c: &Cmd| matches!(c, Cmd::Input(PageInput::Mouse { kind: MouseKind::Move, .. }));
    if !is_move(&c) {
        return (c, None);
    }
    let mut latest = c;
    while let Ok(n) = cmds.try_recv() {
        if !is_move(&n) {
            return (latest, Some(n));
        }
        latest = n;
    }
    (latest, None)
}

/// A `Page.screencastFrame`'s JPEG decoded straight to BGRA, with the
/// page's size in CSS px.
fn decode(params: &Value) -> Option<Frame> {
    use base64::Engine as _;
    use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
    let jpeg = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str()?).ok()?;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(&jpeg), options);
    let bgra = decoder.decode().ok()?;
    let info = decoder.info()?;
    let img = image::RgbaImage::from_raw(info.width as u32, info.height as u32, bgra)?;
    let m = &params["metadata"];
    let page = (m["deviceWidth"].as_f64()?.round() as u32, m["deviceHeight"].as_f64()?.round() as u32);
    Some(Frame { image: Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(img)])), page })
}

/// Drives `f` on this thread until it ends or `deadline` passes (`None`).
fn block_on_deadline<F: Future>(f: F, deadline: Option<Instant>) -> Option<F::Output> {
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
        match deadline {
            None => std::thread::park(),
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return None;
                }
                std::thread::park_timeout(d - now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Chromium, PAGE, chromium_bin};
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
    fn out_keeps_latest_frame_and_every_event() {
        let (out, rx) = Out::new();
        for w in 1..=3 {
            out.push(tiny(w));
        }
        assert_eq!(kinds(&rx), ["f3"]);
        out.push(SourceEvent::Url("u1".into()));
        out.push(tiny(1));
        out.push(SourceEvent::Targets(vec![], None));
        out.push(tiny(2));
        out.push(SourceEvent::Url("u2".into()));
        out.push(SourceEvent::Ended);
        assert_eq!(kinds(&rx), ["u1", "targets", "f2", "u2", "ended"]);
        // only events queued: a new frame still gets through, nothing is lost
        for i in 0..10 {
            out.push(SourceEvent::Url(format!("u{i}")));
        }
        out.push(tiny(9));
        let got = kinds(&rx);
        assert_eq!(got.len(), 11);
        assert_eq!(got.last().map(String::as_str), Some("f9"));
    }

    fn mv(x: f64) -> Cmd {
        Cmd::Input(PageInput::Mouse { kind: MouseKind::Move, x, y: 0.0, button: MouseButton::None, clicks: 0 })
    }

    fn x_of(c: &Cmd) -> Option<f64> {
        match c {
            Cmd::Input(PageInput::Mouse { kind: MouseKind::Move, x, .. }) => Some(*x),
            _ => None,
        }
    }

    #[test]
    fn queued_mouse_moves_coalesce() {
        let (tx, rx) = async_channel::unbounded();
        for x in [2.0, 3.0] {
            tx.try_send(mv(x)).unwrap();
        }
        tx.try_send(Cmd::Resize((1, 1))).unwrap();
        tx.try_send(mv(4.0)).unwrap();
        let (c, held) = coalesce(mv(1.0), &rx);
        assert_eq!(x_of(&c), Some(3.0), "the latest queued move wins");
        assert!(matches!(held, Some(Cmd::Resize((1, 1)))), "the next other command is kept, in order");
        assert_eq!(x_of(&rx.try_recv().unwrap()), Some(4.0));
        // non-moves pass through untouched
        tx.try_send(mv(5.0)).unwrap();
        let (c, held) = coalesce(Cmd::Select(None), &rx);
        assert!(matches!(c, Cmd::Select(None)) && held.is_none());
        assert_eq!(rx.len(), 1);
    }

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
        // a frame of the loaded page: its canvas is blue, the rest white
        let f = wait(&rx, 5.0, "a frame of the page", |e| frame(e).filter(|f| pixel(f, 2, 32)[0] > 100 && pixel(f, 2, 32)[2] < 60));
        assert!(near(center(&f), [255, 255, 255]), "{:?}", center(&f));
        let (x, y) = (f.page.0 as f64 / 2.0, f.page.1 as f64 / 2.0);
        for kind in [MouseKind::Move, MouseKind::Down, MouseKind::Up] {
            let button = if kind == MouseKind::Move { MouseButton::None } else { MouseButton::Left };
            source.input(PageInput::Mouse { kind, x, y, button, clicks: 1 });
        }
        wait(&rx, 3.0, "a red frame", |e| frame(e).filter(|f| near(center(f), [0, 0, 255])));

        // the canvas animates: count frames for a while (all frames acked)
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

    /// Frames for `secs`, and the frame-to-frame intervals' max.
    fn count(rx: &async_channel::Receiver<SourceEvent>, secs: f64, mut during: impl FnMut()) -> (u32, Duration) {
        let start = Instant::now();
        let (mut n, mut last, mut worst) = (0, Instant::now(), Duration::ZERO);
        while start.elapsed() < Duration::from_secs_f64(secs) {
            during();
            match rx.try_recv() {
                Ok(SourceEvent::Frame(_)) => {
                    n += 1;
                    worst = worst.max(last.elapsed());
                    last = Instant::now();
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_micros(500)),
            }
        }
        (n, worst)
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn screencast_fps_animated() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start(&bin);
        let source = CdpScreencast::start(chrome.cdp(), (1280, 800));
        let rx = source.frames();
        wait(&rx, 3.0, "a first frame", frame);
        // warm up, then measure
        count(&rx, 0.5, || {});
        let (d0, t0) = source.decode_stats();
        let (n, worst) = count(&rx, 5.0, || {});
        let (d1, t1) = source.decode_stats();
        let decode_ms = (t1 - t0).as_secs_f64() * 1000.0 / (d1 - d0).max(1) as f64;
        eprintln!("screencast idle input: {:.1} fps, worst gap {worst:?}, decode {decode_ms:.2} ms/frame", n as f64 / 5.0);

        // a mouse-move storm (one per ms) must not starve frames
        let mut x = 0.0;
        let (m, worst_m) = count(&rx, 3.0, || {
            x = (x + 1.0) % 600.0;
            source.input(PageInput::Mouse { kind: MouseKind::Move, x: x + 100.0, y: 300.0, button: MouseButton::None, clicks: 0 });
        });
        eprintln!("screencast during mouse moves: {:.1} fps, worst gap {worst_m:?}", m as f64 / 3.0);
        assert!(n >= 5 * 20, "{n} frames in 5 s");
        assert!(m >= 3 * 20, "{m} frames in 3 s of mouse moves");
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
