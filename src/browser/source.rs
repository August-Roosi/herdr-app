//! Frame sources for the browser pane: `CdpScreencast` shows a target of
//! an agent's Chromium through `Page.startScreencast` and sends user input
//! back as CDP `Input.*` events.

use super::cdp::{Cdp, Event, TargetInfo, Targets, pick_target};
use super::input;
use serde_json::{Value, json};
use std::pin::Pin;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
    /// The image area in device px, and the window's scale factor.
    fn resize(&self, px: (u32, u32), scale: f32);
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
    // Kept for the tests' throughput checks; nothing in the app reads it yet.
    #[cfg_attr(not(test), allow(dead_code))]
    stats: Arc<Stats>,
    scale: Option<Arc<AtomicU32>>,
}

/// What a cast needs: which browser it shows and the window's scale factor
/// (shared with `resize`).
struct Cast {
    key: String,
    scale: Arc<AtomicU32>,
    /// Still counted in `CASTS`: a drop (also a panic's, or a failed spawn's) uncounts it.
    counted: bool,
}

impl Cast {
    /// Uncounts the cast; true when it was the last of its browser.
    fn release(&mut self) -> bool {
        std::mem::take(&mut self.counted) && casts_remove(&self.key)
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.release();
    }
}

/// Casts running per browser, process-wide: the page's override is cleared
/// only when the last one of a browser stops.
static CASTS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

fn casts_add(key: &str) {
    *CASTS.lock().unwrap().get_or_insert_default().entry(key.to_owned()).or_default() += 1;
}

/// Counts one cast less; true when it was the last of its browser.
fn casts_remove(key: &str) -> bool {
    let mut guard = CASTS.lock().unwrap();
    let map = guard.get_or_insert_default();
    match map.get_mut(key) {
        Some(n) if *n > 1 => {
            *n -= 1;
            false
        }
        _ => {
            map.remove(key);
            true
        }
    }
}

/// Whether `key` has a single cast running (the caller's own).
fn casts_alone(key: &str) -> bool {
    CASTS.lock().unwrap().as_ref().and_then(|m| m.get(key)).is_none_or(|n| *n <= 1)
}

/// The page size in CSS px for a pane of `px` device px at `scale`.
fn css_size(px: (u32, u32), scale: f32) -> (u32, u32) {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (((px.0 as f32 / scale).round() as u32).max(1), ((px.1 as f32 / scale).round() as u32).max(1))
}

/// Decoded frames and the time spent decoding them.
#[derive(Default)]
struct Stats {
    frames: AtomicU64,
    micros: AtomicU64,
    /// The most calls ever in flight at once.
    flights: AtomicU64,
}

impl CdpScreencast {
    /// A view of the page: the page's own viewport is never touched.
    pub fn start(cdp: Cdp, px: (u32, u32)) -> Self {
        Self::start_with(cdp, px, None)
    }

    /// A cast: while shown, the page's viewport follows the pane (`px` at
    /// `scale`), so text is drawn 1:1. `key` names the browser (machine and
    /// port): the override is cleared when its last cast stops.
    pub fn start_cast(cdp: Cdp, px: (u32, u32), key: String, scale: f32) -> Self {
        Self::start_with(cdp, px, Some(Cast { key, scale: Arc::new(AtomicU32::new(scale.to_bits())), counted: true }))
    }

    fn start_with(cdp: Cdp, px: (u32, u32), cast: Option<Cast>) -> Self {
        let scale = cast.as_ref().map(|c| c.scale.clone());
        if let Some(c) = &cast {
            casts_add(&c.key);
        }
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
        // closes the slot (ending the decoder) however the worker ends: also
        // when it panics or never starts
        let guard = CloseOnDrop(slot.clone());
        let worker_stats = stats.clone();
        let worker = std::thread::Builder::new().name("cdp-screencast".into()).spawn(move || {
            let _guard = guard;
            let mut w = Worker {
                cdp: Arc::new(cdp),
                out: out.clone(),
                slot: slot.clone(),
                px,
                applied_scale: cast.as_ref().map_or(0, |c| c.scale.load(Ordering::Relaxed)),
                cast,
                targets: Targets::default(),
                pinned: None,
                shown: None,
                listed: None,
                buttons: 0,
                flights: Vec::new(),
                held: std::collections::VecDeque::new(),
                last_frame: None,
                last_probe: None,
                events_first: false,
                move_pending: None,
                stats: worker_stats,
            };
            let r = w.run(cmd_rx, ack_rx);
            slot.close();
            let _ = decoder.join();
            if r.is_err() {
                out.push(SourceEvent::Ended);
            }
            // after the view heard of the end; a lost link has nothing to clear
            w.finish_cast(r.is_ok());
        });
        match worker {
            Ok(_) => CdpScreencast { cmds, frames, stats, scale },
            Err(e) => Self::failed(cmds, stats, e),
        }
    }

    fn failed(cmds: async_channel::Sender<Cmd>, stats: Arc<Stats>, e: std::io::Error) -> Self {
        tracing::warn!("browser: cannot start screencast threads: {e}");
        let (out, frames) = Out::new();
        out.push(SourceEvent::Ended);
        CdpScreencast { cmds, frames, stats, scale: None }
    }

    /// The most calls the worker had in flight at once.
    #[cfg(test)]
    pub fn max_in_flight(&self) -> u64 {
        self.stats.flights.load(Ordering::Relaxed)
    }

    /// Frames decoded so far and the total time spent decoding them.
    #[cfg(test)]
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
    fn resize(&self, px: (u32, u32), scale: f32) {
        if let Some(s) = &self.scale {
            s.store(scale.to_bits(), Ordering::Relaxed);
        }
        let _ = self.cmds.try_send(Cmd::Resize(px));
    }
    fn stop(&self) {
        let _ = self.cmds.try_send(Cmd::Stop);
    }
}

/// How far a call may go unanswered before the source gives up.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the last cast waits for the page to take its override off.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(2);

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

struct CloseOnDrop(Arc<Slot>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
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
    /// A `mouseMoved`: at most one is in flight.
    is_move: bool,
}

struct Worker {
    cdp: Arc<Cdp>,
    out: Arc<Out>,
    slot: Arc<Slot>,
    px: (u32, u32),
    /// Set for a cast (see `start_cast`).
    cast: Option<Cast>,
    /// The cast's scale (bits) as last sent.
    applied_scale: u32,
    targets: Targets,
    pinned: Option<String>,
    shown: Option<Shown>,
    /// The last `SourceEvent::Targets` sent.
    listed: Option<(Vec<TargetInfo>, Option<String>)>,
    /// Mouse buttons held (CDP `buttons` bitmask).
    buttons: u32,
    flights: Vec<Flight>,
    /// Commands taken out of order by `coalesce` or `latest_resize`, handled next.
    held: std::collections::VecDeque<Cmd>,
    /// When the shown target's last frame came.
    last_frame: Option<Instant>,
    /// When the connection was last checked (see `probe_at`).
    last_probe: Option<Instant>,
    /// Alternates which of commands and events `next` looks at first.
    events_first: bool,
    /// The latest mouse move, waiting for the one in flight to finish.
    move_pending: Option<PageInput>,
    stats: Arc<Stats>,
}

enum Next {
    Event(Event),
    Cmd(Cmd),
    Ack(Ack),
    /// Time to check a quiet screencast's connection.
    Probe,
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
                // with commands held, newer ones would jump ahead of them
                Next::Cmd(Cmd::Input(i @ PageInput::Mouse { kind: MouseKind::Move, .. })) if self.held.is_empty() => {
                    let (c, held) = coalesce(Cmd::Input(i), &cmds);
                    self.held.extend(held);
                    self.command(c)?;
                }
                Next::Cmd(Cmd::Resize(px)) => {
                    let Some((px, rest)) = latest_resize(px, &cmds) else { return Ok(()) };
                    self.held.extend(rest);
                    self.command(Cmd::Resize(px))?;
                }
                Next::Probe => {
                    // a reply that never comes ends the source (see `next`)
                    self.last_probe = Some(Instant::now());
                    self.fire("Target.getTargets", json!({}), None);
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
        if let Some(c) = self.held.pop_front() {
            return Ok(match c {
                Cmd::Stop => Next::Stop,
                c => Next::Cmd(c),
            });
        }
        self.events_first = !self.events_first;
        let events_first = self.events_first;
        let (flights, move_pending, cdp) = (&mut self.flights, &mut self.move_pending, &self.cdp);
        let probe = probe_at(self.last_frame, self.last_probe);
        // the earliest deadline of calls still in flight: when to look again
        let wake = std::cell::Cell::new(None);
        let mut e = std::pin::pin!(events.recv());
        let mut c = std::pin::pin!(cmds.recv());
        let mut a = std::pin::pin!(acks.recv());
        block_on_wake(
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
                let now = Instant::now();
                if flights.iter().any(|f| f.deadline <= now) {
                    tracing::warn!("browser: a call went unanswered for {CALL_TIMEOUT:?}");
                    lost = true;
                }
                if lost {
                    return Poll::Ready(Err(Gone));
                }
                if probe.is_some_and(|t| t <= now) {
                    return Poll::Ready(Ok(Next::Probe));
                }
                wake.set(flights.iter().map(|f| f.deadline).chain(probe).min());
                if !flights.iter().any(|f| f.is_move)
                    && let Some(m) = move_pending.take()
                {
                    return Poll::Ready(Ok(Next::Cmd(Cmd::Input(m))));
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
            &wake,
        )
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

    /// Sends a call now; `next` collects its reply (or its timeout).
    fn fire(&mut self, method: &str, params: Value, session: Option<&str>) {
        self.fire_flight(method, params, session, false);
    }

    fn fire_flight(&mut self, method: &str, params: Value, session: Option<&str>, is_move: bool) {
        let (cdp, method, session) = (self.cdp.clone(), method.to_string(), session.map(str::to_string));
        let mut reply: Reply = Box::pin(async move { cdp.call(&method, params, session.as_deref()).await });
        // the first poll sends the request
        let waker = std::task::Waker::noop();
        if reply.as_mut().poll(&mut std::task::Context::from_waker(waker)).is_pending() {
            self.flights.push(Flight { reply, deadline: Instant::now() + CALL_TIMEOUT, is_move });
            self.stats.flights.fetch_max(self.flights.len() as u64, Ordering::Relaxed);
        }
    }

    /// Shows what `pick_target` picks, switching sessions if it changed.
    fn follow(&mut self) -> Result<(), Gone> {
        let want = pick_target(&self.targets.list, self.pinned.as_deref());
        if want.as_deref() != self.shown.as_ref().map(|s| s.target.as_str()) {
            if let Some(old) = self.shown.take() {
                if self.cast.as_ref().is_some_and(|c| casts_alone(&c.key)) {
                    self.call("Emulation.clearDeviceMetricsOverride", json!({}), Some(&old.session))?.ok();
                }
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
        self.override_viewport()?;
        self.start_screencast()?;
        if let Some(t) = self.targets.list.iter().find(|t| t.id == target) {
            self.out.push(SourceEvent::Url(t.url.clone()));
        }
        Ok(())
    }

    /// A cast fits the page's viewport to the pane. A view never does: the
    /// viewport stays the agent's and only the frames' max size follows the pane.
    fn override_viewport(&mut self) -> Result<(), Gone> {
        let Some(cast) = &self.cast else { return Ok(()) };
        let bits = cast.scale.load(Ordering::Relaxed);
        self.applied_scale = bits;
        let Some(s) = &self.shown else { return Ok(()) };
        let scale = f32::from_bits(bits);
        let (w, h) = css_size(self.px, scale);
        let params = json!({"width": w, "height": h, "deviceScaleFactor": scale, "mobile": false});
        self.call("Emulation.setDeviceMetricsOverride", params, Some(&s.session))?.ok();
        Ok(())
    }

    /// When the worker ends: the last cast of a browser gives the page its own viewport back.
    fn finish_cast(&mut self, link_alive: bool) {
        let Some(mut cast) = self.cast.take() else { return };
        if cast.release() && link_alive {
            if let Some(s) = &self.shown {
                let call = self.cdp.call("Emulation.clearDeviceMetricsOverride", json!({}), Some(&s.session));
                let _ = block_on_deadline(call, Instant::now() + CLEAR_TIMEOUT);
            }
        }
    }

    fn start_screencast(&self) -> Result<(), Gone> {
        let Some(s) = &self.shown else { return Ok(()) };
        let params = json!({"format": "jpeg", "quality": 85, "maxWidth": self.px.0, "maxHeight": self.px.1});
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
                self.last_frame = Some(Instant::now());
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
                let scale = self.cast.as_ref().map_or(self.applied_scale, |c| c.scale.load(Ordering::Relaxed));
                if px == self.px && scale == self.applied_scale {
                    return Ok(());
                }
                self.px = px;
                if let Some(s) = &self.shown {
                    self.call("Page.stopScreencast", json!({}), Some(&s.session))?.ok();
                }
                self.override_viewport()?;
                self.start_screencast()
            }
            Cmd::Input(i) => self.input(i),
            Cmd::Stop => Ok(()),
        }
    }

    fn input(&mut self, i: PageInput) -> Result<(), Gone> {
        let Some(session) = self.shown.as_ref().map(|s| s.session.clone()) else { return Ok(()) };
        let session = Some(session.as_str());
        match &i {
            PageInput::Mouse { kind: MouseKind::Move, .. } if self.flights.iter().any(|f| f.is_move) => {
                // one move in flight at a time; only the latest waits
                self.move_pending = Some(i);
                return Ok(());
            }
            // a press, release or wheel carries its own position
            PageInput::Mouse { .. } | PageInput::Wheel { .. } => self.move_pending = None,
            _ => {}
        }
        let is_move = matches!(i, PageInput::Mouse { kind: MouseKind::Move, .. });
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
            self.fire_flight(method, params, session, is_move);
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

/// After a resize, the latest size queued behind it and the other commands
/// queued, in order; `None` when a stop was queued (it wins: nothing else is
/// done). Each resize restarts the screencast, a round trip or two: a pane
/// dragged wider queues one per layout pass.
fn latest_resize(px: (u32, u32), cmds: &async_channel::Receiver<Cmd>) -> Option<((u32, u32), Vec<Cmd>)> {
    let (mut px, mut rest) = (px, Vec::new());
    while let Ok(c) = cmds.try_recv() {
        match c {
            Cmd::Resize(p) => px = p,
            Cmd::Stop => return None,
            c => rest.push(c),
        }
    }
    Some((px, rest))
}

/// How long a live screencast may send no frame before the source checks
/// the connection is still there (a laptop back from sleep: no error, no
/// frames). A static page sends none either, so at most one check per stall.
const STALL: Duration = Duration::from_secs(5);

/// When to check a quiet screencast: a stall after the last frame or check;
/// `None` before the first frame. A check's answer is no frame: it does not
/// move the next check closer.
fn probe_at(last_frame: Option<Instant>, last_probe: Option<Instant>) -> Option<Instant> {
    let frame = last_frame?;
    Some(last_probe.map_or(frame, |p| p.max(frame)) + STALL)
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

/// Drives `f` on this thread to its end, also polling it again when the
/// time `f` left in `wake` comes.
fn block_on_wake<F: Future>(f: F, wake: &std::cell::Cell<Option<Instant>>) -> F::Output {
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
            return v;
        }
        match wake.get() {
            None => std::thread::park(),
            Some(t) => std::thread::park_timeout(t.saturating_duration_since(Instant::now())),
        }
    }
}

/// Drives `f` on this thread until it ends or `deadline` passes (`None`).
fn block_on_deadline<F: Future>(f: F, deadline: Instant) -> Option<F::Output> {
    let mut f = std::pin::pin!(f);
    let wake = std::cell::Cell::new(Some(deadline));
    block_on_wake(
        std::future::poll_fn(|cx| match f.as_mut().poll(cx) {
            Poll::Ready(v) => Poll::Ready(Some(v)),
            Poll::Pending if Instant::now() >= deadline => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }),
        &wake,
    )
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Chromium, FakeBrowser, PAGE, chromium_bin};
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

    /// Waits (up to 5 s) for the worker to end: the source's channel closes.
    fn ends(rx: &async_channel::Receiver<SourceEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Err(async_channel::TryRecvError::Closed) => return,
                Ok(SourceEvent::Ended) => panic!("the source failed instead of stopping"),
                _ => {
                    assert!(Instant::now() < deadline, "the worker did not stop");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn queued_resizes_restart_the_screencast_once_and_a_stop_wins() {
        // the first answer waits, so everything below is queued by then
        let fake = FakeBrowser::start(Duration::from_millis(300));
        let source = CdpScreencast::start(fake.cdp(), (800, 600));
        for i in 0..50 {
            source.resize((700 + i, 500), 1.0);
        }
        source.stop();
        ends(&source.frames());
        let (starts, stops) = (fake.count("Page.startScreencast"), fake.count("Page.stopScreencast"));
        // the first start, then at most one restart
        assert!(starts <= 2 && stops <= 1, "{starts} starts, {stops} stops: {:?}", fake.calls.lock().unwrap());
        assert_eq!(starts, 1, "a queued stop wins over the resizes before it");
    }

    #[test]
    fn queued_resizes_keep_the_latest_and_the_others_in_order() {
        let (tx, rx) = async_channel::unbounded();
        tx.try_send(Cmd::Resize((2, 2))).unwrap();
        tx.try_send(mv(1.0)).unwrap();
        tx.try_send(Cmd::Resize((3, 3))).unwrap();
        tx.try_send(Cmd::Select(None)).unwrap();
        let Some((px, rest)) = latest_resize((1, 1), &rx) else { panic!("no stop was queued") };
        assert_eq!(px, (3, 3));
        assert!(matches!(rest.as_slice(), [m, Cmd::Select(None)] if x_of(m) == Some(1.0)));
        assert!(rx.is_empty());
        tx.try_send(Cmd::Resize((4, 4))).unwrap();
        tx.try_send(Cmd::Stop).unwrap();
        tx.try_send(Cmd::Resize((5, 5))).unwrap();
        assert!(latest_resize((1, 1), &rx).is_none(), "a stop wins");
    }

    #[test]
    fn a_quiet_screencast_is_probed_every_stall() {
        let t0 = Instant::now();
        assert_eq!(probe_at(None, None), None, "not before the first frame");
        assert_eq!(probe_at(Some(t0), None), Some(t0 + STALL));
        // a probe is no frame: the next one comes a stall later, not sooner
        assert_eq!(probe_at(Some(t0), Some(t0 + STALL)), Some(t0 + 2 * STALL));
        // a frame after the probe moves it
        let f = t0 + STALL + Duration::from_secs(2);
        assert_eq!(probe_at(Some(f), Some(t0 + STALL)), Some(f + STALL));
        assert_eq!(STALL, Duration::from_secs(5));
    }

    #[test]
    fn a_stalled_screencast_is_probed_and_a_live_one_is_not() {
        let fake = FakeBrowser::start(Duration::ZERO);
        let source = CdpScreencast::start(fake.cdp(), (800, 600));
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.count("Page.startScreencast") == 0 {
            assert!(Instant::now() < deadline, "no screencast started");
            std::thread::sleep(Duration::from_millis(5));
        }
        let probes = || fake.count("Target.getTargets") - 1;
        // no frame yet: nothing to call stalled
        std::thread::sleep(STALL + Duration::from_millis(500));
        assert_eq!(probes(), 0);
        // a frame, then quiet
        fake.send(&json!({"method": "Page.screencastFrame", "params": {"data": "", "sessionId": 1, "metadata": {}}, "sessionId": "S1"}));
        std::thread::sleep(STALL - Duration::from_secs(1));
        assert_eq!(probes(), 0, "probed before a stall");
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(probes(), 1, "{:?}", fake.calls.lock().unwrap());
        // answered: still alive
        assert!(!matches!(source.frames().try_recv(), Ok(SourceEvent::Ended)));
        source.stop();
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
        source.resize((640, 400), 1.0);
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
        // one move in flight at a time (plus acks and the like)
        eprintln!("screencast: at most {} calls in flight", source.max_in_flight());
        assert!(source.max_in_flight() <= 4, "{} calls in flight", source.max_in_flight());
        source.stop();
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn static_page_stays_alive() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start_on(&bin, "data:text/html,hi");
        let source = CdpScreencast::start(chrome.cdp(), (800, 600));
        let rx = source.frames();
        let f = wait(&rx, 5.0, "a first frame", frame);
        let (x, y) = (f.page.0 as f64 / 2.0, f.page.1 as f64 / 2.0);
        // let startup's late events (extension workers) pass, so the click's
        // calls are the last thing the source waits on
        std::thread::sleep(Duration::from_secs(3));
        for kind in [MouseKind::Down, MouseKind::Up] {
            source.input(PageInput::Mouse { kind, x, y, button: MouseButton::Left, clicks: 1 });
        }
        // nothing changes for longer than a call's timeout: still alive
        let start = Instant::now();
        while start.elapsed() < CALL_TIMEOUT + Duration::from_secs(2) {
            match rx.try_recv() {
                Ok(SourceEvent::Ended) | Err(async_channel::TryRecvError::Closed) => panic!("ended after {:?} on a static page", start.elapsed()),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        source.input(PageInput::Nav(Nav::Go(PAGE.into())));
        wait(&rx, 5.0, "a frame after navigating", |e| frame(e).filter(|f| pixel(f, 2, 32)[0] > 100 && pixel(f, 2, 32)[2] < 60));
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

    /// `window.innerWidth` of the browser's first page, read over its own connection.
    fn inner_width(chrome: &Chromium) -> i64 {
        let cdp = chrome.cdp();
        let r = cdp.call_blocking("Target.getTargets", json!({}), None).unwrap();
        let id = r["targetInfos"].as_array().unwrap().iter().find(|t| t["type"] == "page").unwrap()["targetId"].as_str().unwrap().to_string();
        let att = cdp.call_blocking("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None).unwrap();
        let session = att["sessionId"].as_str().unwrap().to_string();
        let r = cdp.call_blocking("Runtime.evaluate", json!({"expression": "window.innerWidth", "returnByValue": true}), Some(&session)).unwrap();
        r["result"]["value"].as_i64().unwrap()
    }

    fn width_becomes(chrome: &Chromium, want: i64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = inner_width(chrome);
            if got == want {
                return;
            }
            assert!(Instant::now() < deadline, "innerWidth {got}, want {want}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn cast_viewport_follows_pane() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start(&bin);
        width_becomes(&chrome, 1280);
        let source = CdpScreencast::start_cast(chrome.cdp(), (1000, 700), "test:cast_viewport".into(), 1.0);
        width_becomes(&chrome, 1000);
        source.resize((800, 600), 1.0);
        width_becomes(&chrome, 800);
        source.resize((1600, 1200), 2.0);
        width_becomes(&chrome, 800);
        source.stop();
        width_becomes(&chrome, 1280);
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn cast_started_with_a_stale_size_gets_the_pane_size() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start(&bin);
        // the view starts a lookup with a first guess, then tells the real size
        let source = CdpScreencast::start_cast(chrome.cdp(), (1000, 700), "test:stale".into(), 1.0);
        source.resize((900, 600), 1.5);
        width_becomes(&chrome, 600);
        source.stop();
        width_becomes(&chrome, 1280);
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn view_never_overrides() {
        let Some(bin) = chromium_bin() else { return };
        let chrome = Chromium::start(&bin);
        let source = CdpScreencast::start(chrome.cdp(), (1000, 700));
        let rx = source.frames();
        wait(&rx, 5.0, "a frame", frame);
        assert_eq!(inner_width(&chrome), 1280);
        source.resize((800, 600), 1.0);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(inner_width(&chrome), 1280);
        source.stop();
    }

    #[test]
    fn cast_registry_clears_only_with_the_last_cast() {
        let key = "test:registry";
        casts_add(key);
        casts_add(key);
        assert!(!casts_alone(key));
        assert!(!casts_remove(key));
        assert!(casts_alone(key));
        assert!(casts_remove(key), "the last one clears");
        assert!(casts_alone(key));
    }

    #[test]
    fn a_dropped_cast_uncounts_itself() {
        let key = "test:guard".to_string();
        casts_add(&key);
        casts_add(&key);
        let a = Cast { key: key.clone(), scale: Arc::new(AtomicU32::new(1)), counted: true };
        let mut b = Cast { key: key.clone(), scale: Arc::new(AtomicU32::new(1)), counted: true };
        drop(a);
        assert!(casts_alone(&key));
        assert!(b.release(), "the last one clears");
        assert!(!b.release(), "once");
        drop(b);
        assert!(casts_alone(&key));
        assert!(CASTS.lock().unwrap().as_ref().unwrap().get(&key).is_none());
    }

    #[test]
    fn css_size_divides_by_the_scale() {
        assert_eq!(css_size((1000, 700), 1.0), (1000, 700));
        assert_eq!(css_size((1500, 900), 1.5), (1000, 600));
        assert_eq!(css_size((0, 0), 0.0), (1, 1));
    }
}
