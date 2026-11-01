//! The browser pane: finds an agent's browser through the core's terminal
//! labels, shows its screencast and sends the person's input back. Keeps
//! looking (every second) while there is no browser, its machine is away or
//! the connection ended.

use super::cdp::{self, Cdp, CoreTransport, TargetInfo};
use super::find::{self, Found, Lookup};
use super::input;
use super::source::{CdpScreencast, Frame, FrameSource, MouseButton as PageButton, MouseKind, Nav, PageInput, SourceEvent};
use crate::hc::{self, Core};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use serde_json::json;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The top bar's height.
pub const BAR: f32 = 28.0;
const STATUS: f32 = 18.0;
const RETRY: Duration = Duration::from_secs(1);
const SETUP: Duration = Duration::from_secs(10);
/// The screencast's size until the pane has been laid out (device px).
const FIRST_PX: (u32, u32) = (1280, 800);
/// What the local core is called in messages.
const HERE: &str = "this computer";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    NoBrowser,
    Reconnecting,
    /// The core on this machine has no `socket.open`.
    TooOld(String),
    Live,
}

/// The words for a state; `None` while live (the status line shows fps).
pub fn state_text(state: &State) -> Option<String> {
    Some(match state {
        State::NoBrowser => "no browser yet — it starts when the agent first uses one".into(),
        State::Reconnecting => "reconnecting…".into(),
        State::TooOld(machine) => format!("core on {machine} is too old for the browser pane"),
        State::Connecting => "connecting…".into(),
        State::Live => return None,
    })
}

/// What the paint closures and the view share.
#[derive(Default)]
struct Shared {
    source: Option<Box<dyn FrameSource>>,
    /// The image area in device px, as last told to the source.
    px: Option<(u32, u32)>,
    /// Where the frame was drawn, in window px.
    rect: Option<Bounds<Pixels>>,
    /// The image last drawn: dropped from the atlas when the next one is.
    painted: Option<Arc<RenderImage>>,
}

pub struct BrowserView {
    pub agent: String,
    pub state: State,
    frame: Option<Frame>,
    /// Page targets, in the browser's order.
    tabs: Vec<TargetInfo>,
    shown: Option<String>,
    url: String,
    /// The URL bar's text while it is being edited.
    editing: Option<String>,
    /// An IME's text being composed (not sent yet).
    marked: Option<String>,
    /// Show the newest page (else the one picked in the tab strip).
    follow: bool,
    /// Whether a frame came since the lookup loop started.
    was_live: bool,
    frame_times: VecDeque<Instant>,
    shared: Rc<RefCell<Shared>>,
    focus: FocusHandle,
    _task: Option<Task<()>>,
}

/// How one lookup ended.
enum Attempt {
    Source(CdpScreencast),
    Lookup(Lookup),
    TooOld(String),
    Failed(anyhow::Error),
}

/// Looks for `agent`'s browser and connects to it. Blocking core calls: run
/// it off the UI thread.
async fn attempt(core: &Arc<Core>, agent: &str, px: (u32, u32)) -> Attempt {
    let (machine, bare) = find::split(agent);
    // The core forwards the filter to each connected machine.
    let list = match core.call("terminal.list", json!({"labels": {"browser-for": bare}})) {
        Ok(list) => list,
        Err(e) => return Attempt::Failed(e),
    };
    // `terminal.list` names the added machines' lines; older cores leave them out.
    let listed = |m: &str| list["machines"].as_array().is_some_and(|ms| ms.iter().any(|x| x["name"].as_str() == Some(m)));
    let machines = match machine {
        Some(m) if !listed(m) => core.call("machine.list", json!({})).unwrap_or_default(),
        _ => list.clone(),
    };
    match find::state_for(&list, &machines, agent) {
        Lookup::Found(found) => connect(core, &found, px).await,
        other => Attempt::Lookup(other),
    }
}

async fn connect(core: &Arc<Core>, found: &Found, px: (u32, u32)) -> Attempt {
    connect_within(core, found, px, SETUP).await
}

/// Reaches the browser's debugging port; each step that waits on the
/// browser fails after `limit` (a port that accepts but never answers).
async fn connect_within(core: &Arc<Core>, found: &Found, px: (u32, u32), limit: Duration) -> Attempt {
    let on = found.machine.as_deref();
    let machine = on.unwrap_or(HERE).to_owned();
    // checked before the agent's machine: its browser is reached through here too
    if let Some(old) = too_old_here(&core.hello) {
        return Attempt::TooOld(old);
    }
    let address = format!("127.0.0.1:{}", found.port);
    let open = || match core.open_socket(&address, on) {
        Ok(opened) => Ok(CoreTransport::new(core.clone(), opened)),
        Err(e) if hc::too_old(&e) => Err(Attempt::TooOld(machine.clone())),
        Err(e) => Err(Attempt::Failed(e)),
    };
    let http = match open() {
        Ok(t) => t,
        Err(a) => return a,
    };
    let path = match cdp::within(limit, "/json/version", cdp::discover(http, found.port)).await {
        Ok(path) => path,
        Err(e) => return Attempt::Failed(e),
    };
    let ws = match open() {
        Ok(t) => t,
        Err(a) => return a,
    };
    match cdp::within(limit, "websocket upgrade", Cdp::connect(ws, &path, found.port)).await {
        Ok(cdp) => Attempt::Source(CdpScreencast::start(cdp, px)),
        Err(e) => Attempt::Failed(e),
    }
}

/// The machine to name as too old before trying to reach any agent's
/// browser, judged from the local core's hello: every browser, also
/// one on another machine, is reached through this computer's core, so its
/// age shows first. A remote core's shows when `socket.open` is tried there.
fn too_old_here(hello: &serde_json::Value) -> Option<String> {
    let has_socket = hello["methods"].as_array().is_none_or(|ms| ms.iter().any(|m| m == "socket.open"));
    (!has_socket).then(|| HERE.to_owned())
}

/// Hands a source's events to the view until it ends. False once the view is gone.
async fn pump(this: &WeakEntity<BrowserView>, rx: async_channel::Receiver<SourceEvent>, cx: &mut AsyncApp) -> bool {
    while let Ok(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        match this.update(cx, |view, cx| view.apply(batch, cx)) {
            Ok(true) => {}
            Ok(false) => return true,
            Err(_) => return false,
        }
    }
    true
}

impl BrowserView {
    /// The pane for `agent` (`t3`, `rohan/t3`, `base`, `rohan/base`).
    pub fn new(core: Arc<Core>, agent: String, cx: &mut Context<Self>) -> Self {
        let mut view = Self::blank(agent, cx);
        view.start(core, cx);
        view
    }

    fn blank(agent: String, cx: &mut Context<Self>) -> Self {
        cx.on_release(|view: &mut Self, cx| {
            let mut shared = view.shared.borrow_mut();
            if let Some(source) = shared.source.take() {
                source.stop();
            }
            if let Some(image) = shared.painted.take() {
                cx.drop_image(image, None);
            }
        })
        .detach();
        BrowserView {
            agent,
            state: State::Connecting,
            frame: None,
            tabs: Vec::new(),
            shown: None,
            url: String::new(),
            editing: None,
            marked: None,
            follow: true,
            was_live: false,
            frame_times: VecDeque::new(),
            shared: Rc::default(),
            focus: cx.focus_handle(),
            _task: None,
        }
    }

    /// (Re)starts looking for the browser through `core` (also after the
    /// app reconnected to its core).
    pub fn start(&mut self, core: Arc<Core>, cx: &mut Context<Self>) {
        self.end_source();
        self.state = State::Connecting;
        self.was_live = false;
        cx.notify();
        let agent = self.agent.clone();
        // Replacing the task drops a lookup still running on an older core.
        self._task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(px) = this.update(cx, |view, _| view.shared.borrow().px.unwrap_or(FIRST_PX)) else { return };
                let (core, agent) = (core.clone(), agent.clone());
                let tried = cx.background_executor().spawn(async move { attempt(&core, &agent, px).await }).await;
                match this.update(cx, |view, cx| view.attempted(tried, cx)) {
                    Ok(Some(rx)) => {
                        if !pump(&this, rx, cx).await {
                            return;
                        }
                        if this.update(cx, |view, cx| view.source_ended(cx)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
                cx.background_executor().timer(RETRY).await;
            }
        }));
    }

    /// Shows `source` (a test's fake), without looking for a browser.
    #[cfg(test)]
    fn with_source(agent: String, source: Box<dyn FrameSource>, cx: &mut Context<Self>) -> Self {
        let mut view = Self::blank(agent, cx);
        let rx = view.install(source);
        view._task = Some(cx.spawn(async move |this, cx| {
            if pump(&this, rx, cx).await {
                let _ = this.update(cx, |view, cx| view.source_ended(cx));
            }
        }));
        view
    }

    fn install(&mut self, source: Box<dyn FrameSource>) -> async_channel::Receiver<SourceEvent> {
        if !self.follow {
            source.select(self.shown.clone());
        }
        let rx = source.frames();
        self.shared.borrow_mut().source = Some(source);
        rx
    }

    fn attempted(&mut self, tried: Attempt, cx: &mut Context<Self>) -> Option<async_channel::Receiver<SourceEvent>> {
        cx.notify();
        match tried {
            Attempt::Source(source) => {
                if !self.was_live {
                    self.state = State::Connecting;
                }
                return Some(self.install(Box::new(source)));
            }
            Attempt::Lookup(Lookup::LineDown(_)) => self.state = State::Reconnecting,
            Attempt::Lookup(Lookup::NoBrowser) => {
                self.state = State::NoBrowser;
                self.clear_page();
            }
            Attempt::Lookup(Lookup::Found(_)) => {}
            Attempt::TooOld(machine) => {
                self.state = State::TooOld(machine);
                self.clear_page();
            }
            Attempt::Failed(e) => {
                tracing::debug!("browser pane {}: {e:#}", self.agent);
                if self.was_live {
                    self.state = State::Reconnecting;
                }
            }
        }
        None
    }

    fn clear_page(&mut self) {
        self.frame = None;
        self.tabs.clear();
        self.shown = None;
        self.url.clear();
        self.frame_times.clear();
    }

    /// Applies a source's events; false once it ended.
    fn apply(&mut self, batch: Vec<SourceEvent>, cx: &mut Context<Self>) -> bool {
        cx.notify();
        for event in batch {
            match event {
                SourceEvent::Frame(frame) => {
                    self.frame = Some(frame);
                    self.state = State::Live;
                    self.was_live = true;
                    let now = Instant::now();
                    self.frame_times.push_back(now);
                    while self.frame_times.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
                        self.frame_times.pop_front();
                    }
                }
                SourceEvent::Targets(list, shown) => {
                    self.tabs = pages(list);
                    self.shown = shown;
                }
                SourceEvent::Url(url) => self.url = url,
                SourceEvent::Ended => return false,
            }
        }
        true
    }

    fn source_ended(&mut self, cx: &mut Context<Self>) {
        self.end_source();
        self.state = State::Reconnecting;
        self.frame_times.clear();
        cx.notify();
    }

    fn end_source(&mut self) {
        if let Some(source) = self.shared.borrow_mut().source.take() {
            source.stop();
        }
    }

    fn send(&self, ev: PageInput) {
        if let Some(source) = &self.shared.borrow().source {
            source.input(ev);
        }
    }

    fn select(&self, target: Option<String>) {
        if let Some(source) = &self.shared.borrow().source {
            source.select(target);
        }
    }

    /// Frames shown in the last second.
    fn fps(&self) -> usize {
        let now = Instant::now();
        self.frame_times.iter().filter(|t| now.duration_since(**t) <= Duration::from_secs(1)).count()
    }

    /// `p` (window px) as a point on the page, if it is over the drawn frame;
    /// `clamp` moves it onto the frame's edge first (a release outside).
    fn page_point(&self, p: Point<Pixels>, clamp: bool) -> Option<(f64, f64)> {
        let rect = self.shared.borrow().rect?;
        let page = self.frame.as_ref()?.page;
        let (x, y, w, h) = (f32::from(rect.origin.x), f32::from(rect.origin.y), f32::from(rect.size.width), f32::from(rect.size.height));
        let (mut px_, mut py_) = (f32::from(p.x), f32::from(p.y));
        if clamp {
            px_ = px_.clamp(x, x + w - 0.01);
            py_ = py_.clamp(y, y + h - 0.01);
        }
        input::map_point((x, y, w, h), page, (px_, py_))
    }

    fn mouse(&mut self, kind: MouseKind, button: Option<MouseButton>, p: Point<Pixels>, clicks: usize) {
        if let Some((x, y)) = self.page_point(p, kind == MouseKind::Up) {
            self.send(PageInput::Mouse { kind, x, y, button: page_button(button), clicks: clicks as u32 });
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.key_down_with(ev, native_text(ev), cx);
    }

    /// `native`: the OS delivers this key's text through the input handler.
    fn key_down_with(&mut self, ev: &KeyDownEvent, native: bool, cx: &mut Context<Self>) {
        // Windows' AltGr, dead keys and IMEs: left to propagate, the text
        // comes back through `EntityInputHandler` (GPUI also sets the flag on
        // replayed shortcut prefixes, which `native` rules out).
        if ev.prefer_character_input && native {
            return;
        }
        if self.editing.is_some() {
            self.edit_url(&ev.keystroke, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(i) = key_input(&ev.keystroke, true) {
            self.send(i);
            cx.stop_propagation();
        }
    }

    /// Text the OS typed (AltGr, dead keys, IMEs): into the URL bar while it
    /// is edited, else to the page.
    fn typed(&mut self, text: &str, cx: &mut Context<Self>) {
        self.marked = None;
        if text.is_empty() {
            return;
        }
        match self.editing.as_mut() {
            Some(url) => url.push_str(text),
            None => self.send(PageInput::Text(text.to_owned())),
        }
        cx.notify();
    }

    fn key_up(&mut self, ev: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_none()
            && let Some(i) = key_input(&ev.keystroke, false)
        {
            self.send(i);
            cx.stop_propagation();
        }
    }

    fn edit_url(&mut self, k: &Keystroke, cx: &mut Context<Self>) {
        let Some(text) = self.editing.as_mut() else { return };
        let m = &k.modifiers;
        match k.key.as_str() {
            "enter" => {
                let url = go_url(text);
                self.editing = None;
                if !url.is_empty() {
                    self.url = url.clone();
                    self.send(PageInput::Nav(Nav::Go(url)));
                }
            }
            "escape" => self.editing = None,
            "backspace" => {
                text.pop();
            }
            "v" if m.control || m.platform => {
                if let Some(pasted) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    text.push_str(pasted.trim());
                }
            }
            _ if !(m.control || m.alt || m.platform) => {
                if let Some(t) = input::text(k) {
                    text.push_str(&t);
                }
            }
            _ => {}
        }
    }

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, nav: Nav, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px_1()
                .rounded_sm()
                .text_color(c("muted"))
                .hover(|d| d.bg(c("hover")))
                .child(label)
                .on_click(cx.listener(move |view, _, _, _| view.send(PageInput::Nav(nav.clone()))))
        };
        let tabs = self.tabs.iter().map(|t| {
            let id = t.id.clone();
            let shown = self.shown.as_deref() == Some(t.id.as_str());
            let title = if t.title.is_empty() { t.url.clone() } else { t.title.clone() };
            div()
                .id(SharedString::from(format!("browser-tab-{}", t.id)))
                .max_w(px(140.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .px_1()
                .rounded_sm()
                .when(shown, |d| d.bg(c("selected")).text_color(c("foreground")))
                .when(!shown, |d| d.text_color(c("dim")).hover(|d| d.bg(c("hover"))))
                .child(title)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.follow = false;
                    view.select(Some(id.clone()));
                    cx.notify();
                }))
        });
        let url = match &self.editing {
            Some(text) => format!("{text}▏"),
            None => self.url.clone(),
        };
        div()
            .h(px(BAR))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_1()
            .text_xs()
            .bg(c("chrome"))
            .border_b_1()
            .border_color(c("border"))
            .children(tabs)
            .child(button("browser-back", "◀", Nav::Back, cx))
            .child(button("browser-forward", "▶", Nav::Forward, cx))
            .child(button("browser-reload", "⟳", Nav::Reload, cx))
            .child(
                div()
                    .id("browser-url")
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .px_1()
                    .rounded_sm()
                    .bg(c("input"))
                    .text_color(if self.editing.is_some() { c("foreground") } else { c("muted") })
                    .child(url)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.editing = Some(view.url.clone());
                        view.focus.focus(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("browser-follow")
                    .px_1()
                    .rounded_sm()
                    .text_color(if self.follow { c("accent") } else { c("dim") })
                    .hover(|d| d.bg(c("hover")))
                    .child("follow")
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.follow = !view.follow;
                        view.select(if view.follow { None } else { view.shown.clone() });
                        cx.notify();
                    })),
            )
    }
}

impl Drop for BrowserView {
    fn drop(&mut self) {
        self.end_source();
    }
}

fn c(name: &str) -> Hsla {
    crate::settings::color_or_literal(name)
}

/// The tab strip's targets: pages only (not workers, iframes, ...).
pub fn pages(list: Vec<TargetInfo>) -> Vec<TargetInfo> {
    list.into_iter().filter(|t| t.kind == "page").collect()
}

fn page_button(b: Option<MouseButton>) -> PageButton {
    match b {
        Some(MouseButton::Left) => PageButton::Left,
        Some(MouseButton::Middle) => PageButton::Middle,
        Some(MouseButton::Right) => PageButton::Right,
        _ => PageButton::None,
    }
}

/// A keystroke as page input: typed text goes as `Text`, other keys and
/// chords as key events; `None` for what the page can't take.
pub fn key_input(k: &Keystroke, down: bool) -> Option<PageInput> {
    let m = &k.modifiers;
    if !(m.control || m.alt || m.platform)
        && let Some(text) = input::text(k)
    {
        return down.then_some(PageInput::Text(text));
    }
    (!input::key_event(k, down).is_empty()).then(|| PageInput::Key { keystroke: k.clone(), down })
}

/// What the URL bar's text navigates to: a bare host gets a scheme.
pub fn go_url(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() || t.contains("://") || ["about:", "data:", "file:", "chrome:", "javascript:"].iter().any(|s| t.starts_with(s)) {
        return t.to_owned();
    }
    let local = ["localhost", "127.", "0.0.0.0", "[::1]"].iter().any(|h| t.starts_with(h));
    format!("{}://{t}", if local { "http" } else { "https" })
}

/// The largest rect of `image`'s aspect that fits `area`, centered.
pub fn fit(area: Bounds<Pixels>, image: (f32, f32)) -> Bounds<Pixels> {
    let (w, h) = (f32::from(area.size.width), f32::from(area.size.height));
    if image.0 <= 0.0 || image.1 <= 0.0 || w <= 0.0 || h <= 0.0 {
        return Bounds::new(area.origin, size(px(0.0), px(0.0)));
    }
    let scale = (w / image.0).min(h / image.1);
    let (dw, dh) = (image.0 * scale, image.1 * scale);
    Bounds::new(point(area.origin.x + px((w - dw) / 2.0), area.origin.y + px((h - dh) / 2.0)), size(px(dw), px(dh)))
}

#[cfg(windows)]
fn native_text(ev: &KeyDownEvent) -> bool {
    gpui_windows::is_native_character_input(ev)
}

/// Elsewhere text comes with the key (an IME's still reaches the handler).
#[cfg(not(windows))]
fn native_text(_: &KeyDownEvent) -> bool {
    false
}

/// OS text input (AltGr, dead keys, IMEs) types here, as the terminal's
/// does: the finished text goes to the page (or the URL bar being edited).
impl EntityInputHandler for BrowserView {
    fn text_for_range(&mut self, range: std::ops::Range<usize>, adjusted: &mut Option<std::ops::Range<usize>>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
        let units: Vec<u16> = self.marked.as_deref().unwrap_or_default().encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&units[range]))
    }

    fn selected_text_range(&mut self, _ignore_disabled: bool, _w: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        let len = self.marked.as_ref().map_or(0, |m| m.encode_utf16().count());
        Some(UTF16Selection { range: len..len, reversed: false })
    }

    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        self.marked.as_ref().filter(|m| !m.is_empty()).map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, _range: Option<std::ops::Range<usize>>, text: &str, _w: &mut Window, cx: &mut Context<Self>) {
        self.typed(text, cx);
    }

    fn replace_and_mark_text_in_range(&mut self, _range: Option<std::ops::Range<usize>>, text: &str, _selected: Option<std::ops::Range<usize>>, _w: &mut Window, _cx: &mut Context<Self>) {
        self.marked = (!text.is_empty()).then(|| text.to_owned());
    }

    fn bounds_for_range(&mut self, _range: std::ops::Range<usize>, element: Bounds<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        // the page's caret is not known here: the IME's window goes to the page's corner
        Some(Bounds::new(element.origin, size(px(1.0), px(16.0))))
    }

    fn character_index_for_point(&mut self, _point: Point<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        None
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.frame.clone();
        // While focused, the OS's text input (AltGr, dead keys, IMEs) types here.
        let input = self.focus.is_focused(window).then(|| (self.focus.clone(), cx.entity()));
        let (layout_shared, paint_shared) = (self.shared.clone(), self.shared.clone());
        let status = state_text(&self.state).unwrap_or_else(|| format!("{} fps", self.fps()));
        let empty = self.frame.is_none().then(|| status.clone());

        div()
            .id(SharedString::from(format!("browser-{}", self.agent)))
            .track_focus(&self.focus)
            .key_context("Browser")
            .size_full()
            .flex()
            .flex_col()
            .bg(c("background"))
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .child(self.top_bar(cx))
            .child(
                div()
                    .id("browser-page")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .on_mouse_down(MouseButton::Left, cx.listener(|view, ev: &MouseDownEvent, window, cx| {
                        view.focus.focus(window, cx);
                        if view.editing.take().is_some() {
                            cx.notify();
                        }
                        view.mouse(MouseKind::Down, Some(ev.button), ev.position, ev.click_count);
                    }))
                    .on_mouse_down(MouseButton::Middle, cx.listener(|view, ev: &MouseDownEvent, window, cx| {
                        view.focus.focus(window, cx);
                        view.mouse(MouseKind::Down, Some(ev.button), ev.position, ev.click_count);
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|view, ev: &MouseDownEvent, window, cx| {
                        view.focus.focus(window, cx);
                        cx.stop_propagation();
                        view.mouse(MouseKind::Down, Some(ev.button), ev.position, ev.click_count);
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    .on_mouse_up(MouseButton::Middle, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    .on_mouse_up(MouseButton::Right, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    // Released outside the page (over the bar, another pane): the page lets go too.
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    .on_mouse_up_out(MouseButton::Middle, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    .on_mouse_up_out(MouseButton::Right, cx.listener(|view, ev: &MouseUpEvent, _, _| view.mouse(MouseKind::Up, Some(ev.button), ev.position, ev.click_count)))
                    .on_mouse_move(cx.listener(|view, ev: &MouseMoveEvent, _, _| view.mouse(MouseKind::Move, ev.pressed_button, ev.position, 0)))
                    .on_scroll_wheel(cx.listener(|view, ev: &ScrollWheelEvent, _, cx| {
                        cx.stop_propagation();
                        let d = ev.delta.pixel_delta(px(20.0));
                        if let Some((x, y)) = view.page_point(ev.position, false) {
                            // GPUI: positive moves the content down; CDP: positive scrolls down.
                            view.send(PageInput::Wheel { x, y, dx: -f64::from(f32::from(d.x)), dy: -f64::from(f32::from(d.y)) });
                        }
                    }))
                    .child(
                        canvas(
                            move |bounds, window, _| {
                                let scale = window.scale_factor();
                                let dev = ((f32::from(bounds.size.width) * scale).round().max(1.0) as u32, (f32::from(bounds.size.height) * scale).round().max(1.0) as u32);
                                let mut shared = layout_shared.borrow_mut();
                                if shared.px != Some(dev) {
                                    shared.px = Some(dev);
                                    // Only the screencast's size: the page's viewport stays the agent's.
                                    if let Some(source) = &shared.source {
                                        source.resize(dev);
                                    }
                                }
                            },
                            move |bounds, _, window, cx| {
                                if let Some((focus, view)) = input {
                                    window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                                }
                                let mut shared = paint_shared.borrow_mut();
                                let Some(frame) = &frame else {
                                    shared.rect = None;
                                    return;
                                };
                                let s = frame.image.size(0);
                                let outer = fit(bounds, (s.width.0 as f32, s.height.0 as f32));
                                // The page sits 1 px inside a line in the pane-border colour, so
                                // its edge shows even when the page is as light as the letterbox.
                                let rect = Bounds::new(outer.origin + point(px(1.0), px(1.0)), outer.size - size(px(2.0), px(2.0)));
                                shared.rect = Some(rect);
                                let _ = window.paint_image(rect, rect, Corners::default(), frame.image.clone(), 0, false);
                                window.paint_quad(outline(outer, c("border"), BorderStyle::Solid));
                                // Each frame is a new image: free the last one's atlas space.
                                if let Some(old) = shared.painted.replace(frame.image.clone())
                                    && old.id != frame.image.id
                                {
                                    let _ = window.drop_image(old);
                                }
                            },
                        )
                        .size_full(),
                    )
                    .when_some(empty, |d, text| {
                        d.child(div().absolute().inset_0().flex().items_center().justify_center().text_sm().text_color(c("dim")).child(text))
                    }),
            )
            .child(
                div()
                    .h(px(STATUS))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .px_2()
                    .text_xs()
                    .text_color(c("dim"))
                    .border_t_1()
                    .border_color(c("border"))
                    .child(status),
            )
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // gpui's glob import brings its own `test`
    use std::prelude::v1::test;
    use std::sync::Mutex;

    /// A source fed by the test; records what the view asked of it.
    struct Fake {
        rx: async_channel::Receiver<SourceEvent>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FrameSource for Fake {
        fn frames(&self) -> async_channel::Receiver<SourceEvent> {
            self.rx.clone()
        }
        fn input(&self, ev: PageInput) {
            self.calls.lock().unwrap().push(format!("{ev:?}"));
        }
        fn select(&self, target: Option<String>) {
            self.calls.lock().unwrap().push(format!("select {target:?}"));
        }
        fn resize(&self, px: (u32, u32)) {
            self.calls.lock().unwrap().push(format!("resize {}x{}", px.0, px.1));
        }
        fn stop(&self) {
            self.calls.lock().unwrap().push("stop".into());
        }
    }

    /// A solid BGRA frame.
    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Frame {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(bgra));
        Frame { image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(img)])), page: (w, h) }
    }

    fn red(p: &image::Rgba<u8>) -> bool {
        p.0[0] > 200 && p.0[1] < 60 && p.0[2] < 60
    }

    #[test]
    fn state_texts() {
        assert_eq!(state_text(&State::NoBrowser).unwrap(), "no browser yet — it starts when the agent first uses one");
        assert_eq!(state_text(&State::Reconnecting).unwrap(), "reconnecting…");
        assert_eq!(state_text(&State::TooOld("rohan".into())).unwrap(), "core on rohan is too old for the browser pane");
        assert_eq!(state_text(&State::Connecting).unwrap(), "connecting…");
        assert_eq!(state_text(&State::Live), None);
    }

    #[test]
    fn a_local_core_without_sockets_is_too_old_for_every_agent() {
        let old = json!({"methods": ["terminal.list"]});
        let new = json!({"methods": ["terminal.list", "socket.open"]});
        // named for local and remote agents alike (connect_within asks before
        // looking at the agent's machine)
        assert_eq!(too_old_here(&old).as_deref(), Some(HERE));
        assert_eq!(too_old_here(&new), None);
        // a hello without a method list: try
        assert_eq!(too_old_here(&json!({})), None);
    }

    #[test]
    fn keys_text_goes_as_text_and_chords_as_keys() {
        let ks = |s: &str| Keystroke::parse(s).unwrap();
        assert!(matches!(key_input(&ks("a"), true), Some(PageInput::Text(t)) if t == "a"));
        assert!(key_input(&ks("a"), false).is_none(), "text has no key up");
        assert!(matches!(key_input(&ks("ctrl-a"), true), Some(PageInput::Key { down: true, .. })));
        assert!(matches!(key_input(&ks("enter"), false), Some(PageInput::Key { down: false, .. })));
        assert!(matches!(key_input(&ks("backspace"), true), Some(PageInput::Key { .. })));
        assert!(matches!(key_input(&ks("space"), true), Some(PageInput::Text(t)) if t == " "));
    }

    #[test]
    fn urls_get_a_scheme() {
        assert_eq!(go_url(" localhost:3000/x "), "http://localhost:3000/x");
        assert_eq!(go_url("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(go_url("example.com"), "https://example.com");
        assert_eq!(go_url("http://example.com"), "http://example.com");
        assert_eq!(go_url("about:blank"), "about:blank");
        assert_eq!(go_url("  "), "");
    }

    #[test]
    fn tabs_are_pages_only() {
        let t = |id: &str, kind: &str| TargetInfo { id: id.into(), kind: kind.into(), url: String::new(), title: String::new(), seq: 0 };
        let got = pages(vec![t("a", "page"), t("w", "service_worker"), t("i", "iframe"), t("b", "page")]);
        assert_eq!(got.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn fit_keeps_the_aspect_ratio_centered() {
        let area = Bounds::new(point(px(0.0), px(28.0)), size(px(1000.0), px(954.0)));
        let r = fit(area, (1280.0, 800.0));
        assert_eq!(r.size, size(px(1000.0), px(625.0)));
        assert_eq!(r.origin, point(px(0.0), px(28.0 + (954.0 - 625.0) / 2.0)));
        let tall = fit(Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(200.0))), (100.0, 100.0));
        assert_eq!(tall, Bounds::new(point(px(100.0), px(0.0)), size(px(200.0), px(200.0))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn renders_frame_scaled() {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let (tx, rx) = async_channel::unbounded();
        let calls = Arc::new(Mutex::new(Vec::new()));
        // BGRA red, the page's own size
        tx.try_send(SourceEvent::Frame(solid(1280, 800, [0, 0, 255, 255]))).unwrap();
        tx.try_send(SourceEvent::Url("http://localhost:3000/".into())).unwrap();
        let fake = Fake { rx, calls: calls.clone() };
        let handle = cx
            .open_window(size(px(1000.), px(1000.)), |_, cx| cx.new(|cx| BrowserView::with_source("t1".into(), Box::new(fake), cx)))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let scale = window.scale_factor();
            window.refresh();
            let _ = window.draw(cx);
            let image = window.render_to_image().unwrap();
            let at = |x: f32, y: f32| *image.get_pixel((x * scale) as u32, (y * scale) as u32);
            assert!(red(&at(500.0, 500.0)), "center: {:?}", at(500.0, 500.0));
            assert!(!red(&at(500.0, 50.0)), "letterbox: {:?}", at(500.0, 50.0));
            assert!(!red(&at(500.0, 10.0)), "top bar: {:?}", at(500.0, 10.0));
            // the page's edge is marked, so a white page differs from the letterbox
            assert!(!red(&at(0.5, 500.0)), "edge: {:?}", at(0.5, 500.0));
            assert_ne!(at(0.5, 500.0), at(500.0, 50.0), "edge looks like the letterbox");
            assert!(red(&at(3.0, 500.0)), "inside the edge: {:?}", at(3.0, 500.0));
            // the image area, in device px, is the screencast's size
            let area = ((1000.0 * scale) as u32, ((1000.0 - BAR - STATUS) * scale) as u32);
            let told = calls.lock().unwrap().clone();
            assert!(told.iter().any(|c| c.starts_with("resize ") && c.contains(&format!("{}x", area.0))), "{told:?}, want ~{area:?}");
        })
        .unwrap();
        cx.update(|cx| {
            handle
                .update(cx, |view, _, _| {
                    assert_eq!(view.state, State::Live);
                    assert_eq!(view.url, "http://localhost:3000/");
                    // the frame's center is the page's center; the letterbox is no page
                    view.mouse(MouseKind::Down, Some(MouseButton::Left), point(px(500.0), px(28.0 + 477.0)), 1);
                    view.mouse(MouseKind::Down, Some(MouseButton::Left), point(px(500.0), px(50.0)), 1);
                    // a release outside lands on the frame's edge
                    view.mouse(MouseKind::Up, Some(MouseButton::Left), point(px(500.0), px(5.0)), 1);
                })
                .unwrap()
        });
        let mice: Vec<String> = calls.lock().unwrap().iter().filter(|c| c.starts_with("Mouse")).cloned().collect();
        assert_eq!(mice.len(), 2, "{mice:?}");
        assert!(mice[0].contains("kind: Down, x: 640.0, y: 400.0, button: Left, clicks: 1"), "{mice:?}");
        assert!(mice[1].contains("kind: Up, x: 640.0, y: 0.0"), "{mice:?}");
        tx.try_send(SourceEvent::Ended).unwrap();
        cx.run_until_parked();
        cx.update(|cx| handle.update(cx, |view, _, _| assert_eq!(view.state, State::Reconnecting)).unwrap());
        assert_eq!(calls.lock().unwrap().last().map(String::as_str), Some("stop"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_text_input_reaches_the_page_and_the_url_bar() {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let (_tx, rx) = async_channel::unbounded();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake { rx, calls: calls.clone() };
        let handle = cx
            .open_window(size(px(1000.), px(1000.)), |_, cx| cx.new(|cx| BrowserView::with_source("t1".into(), Box::new(fake), cx)))
            .unwrap();
        cx.run_until_parked();
        // Windows' AltGr+2 on a German layout: control+alt, typing "@"
        let mut keystroke = Keystroke::parse("ctrl-alt-2").unwrap();
        keystroke.key_char = Some("@".into());
        let altgr = KeyDownEvent { keystroke, is_held: false, prefer_character_input: true };
        let plain = KeyDownEvent { keystroke: Keystroke::parse("a").unwrap(), is_held: false, prefer_character_input: false };
        let take = || std::mem::take(&mut *calls.lock().unwrap()).into_iter().filter(|c| !c.starts_with("resize ")).collect::<Vec<_>>();
        cx.update(|cx| {
            handle
                .update(cx, |view, window, cx| {
                    // the OS types it: the key is left to the input handler
                    view.key_down_with(&altgr, true, cx);
                    view.replace_text_in_range(None, "@", window, cx);
                })
                .unwrap()
        });
        assert_eq!(take(), [r#"Text("@")"#], "no dispatchKeyEvent, the text as text");
        cx.update(|cx| {
            handle
                .update(cx, |view, window, cx| {
                    // where the OS does not take it (Linux), the chord and plain keys are as before
                    view.key_down_with(&altgr, false, cx);
                    view.key_down_with(&plain, false, cx);
                    // the URL bar takes the OS's text while it is edited
                    view.editing = Some("a".into());
                    view.key_down_with(&altgr, true, cx);
                    view.replace_text_in_range(None, "@", window, cx);
                    assert_eq!(view.editing.as_deref(), Some("a@"));
                })
                .unwrap()
        });
        let got = take();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("Key {") && got[0].contains("down: true"), "{got:?}");
        assert_eq!(got[1], r#"Text("a")"#);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn right_and_middle_releases_outside_the_page_reach_it() {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let (tx, rx) = async_channel::unbounded();
        let calls = Arc::new(Mutex::new(Vec::new()));
        tx.try_send(SourceEvent::Frame(solid(1280, 800, [0, 0, 255, 255]))).unwrap();
        let fake = Fake { rx, calls: calls.clone() };
        let handle = cx
            .open_window(size(px(1000.), px(1000.)), |_, cx| cx.new(|cx| BrowserView::with_source("t1".into(), Box::new(fake), cx)))
            .unwrap();
        cx.run_until_parked();
        for button in [MouseButton::Right, MouseButton::Middle, MouseButton::Left] {
            cx.update_window(handle.into(), |_, window, cx| {
                window.refresh();
                let _ = window.draw(cx);
                let (on_page, on_bar) = (point(px(500.0), px(505.0)), point(px(500.0), px(5.0)));
                let modifiers = Modifiers::default();
                window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: on_page, pressed_button: None, modifiers }), cx);
                window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button, position: on_page, modifiers, click_count: 1, first_mouse: false }), cx);
                window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: on_bar, pressed_button: Some(button), modifiers }), cx);
                // released over the top bar: outside the page
                window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button, position: on_bar, modifiers, click_count: 1 }), cx);
            })
            .unwrap();
            let want = format!("{:?}", page_button(Some(button)));
            let all = calls.lock().unwrap().clone();
            let ups = all.iter().filter(|c| c.contains("kind: Up") && c.contains(&format!("button: {want}"))).count();
            assert_eq!(ups, 1, "{button:?} release outside the page: {all:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn setup_gives_up_on_a_port_that_never_answers() {
        let Some(iso) = crate::hc::tests::Isolated::new("browser-mute") else { return };
        let core = iso.connect();
        // accepts, never answers
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (peer_tx, peer_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((s, _)) = l.accept() {
                let _ = peer_tx.send(s);
            }
        });
        let found = Found { machine: None, port, terminal: "t9".into() };
        let start = Instant::now();
        let tried = cdp::block_on(connect_within(&core, &found, (800, 600), Duration::from_millis(300)));
        let Attempt::Failed(e) = tried else { panic!("a mute port must fail the attempt") };
        assert!(e.to_string().contains("no answer"), "{e:#}");
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        // the abandoned channel is closed, not leaked
        let mut peer = peer_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        let mut n = std::io::Read::read(&mut peer, &mut buf).unwrap();
        // the HTTP request came first
        if n > 0 {
            n = std::io::Read::read(&mut peer, &mut buf).unwrap();
        }
        assert_eq!(n, 0, "the core kept the socket open");
        assert_eq!(SETUP, Duration::from_secs(10));
    }

    #[test]
    fn within_lets_a_ready_future_through() {
        let ok = cdp::block_on(cdp::within(Duration::from_secs(5), "x", async { Ok(7) }));
        assert_eq!(ok.unwrap(), 7);
        let late = cdp::block_on(cdp::within(Duration::from_millis(50), "the thing", std::future::pending::<anyhow::Result<()>>()));
        assert_eq!(late.unwrap_err().to_string(), "the thing: no answer in 50ms");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary) and the fork's core; run with --include-ignored"]
    fn finds_a_labeled_browser_through_the_core_and_shows_it() {
        use crate::browser::testutil::{Chromium, chromium_bin};
        let Some(bin) = chromium_bin() else { return };
        let Some(iso) = crate::hc::tests::Isolated::new("browser-view") else { return };
        let core = iso.connect();
        let chrome = Chromium::start(&bin);
        let tried = |agent: &str| cdp::block_on(attempt(&core, agent, (800, 600)));
        assert!(matches!(tried("t1"), Attempt::Lookup(Lookup::NoBrowser)));
        let labels = json!({"browser-for": "t1", "cdp": format!("127.0.0.1:{}", chrome.port)});
        core.run(vec!["sleep".into(), "60".into()], None, labels, None).unwrap();
        assert!(matches!(tried("t2"), Attempt::Lookup(Lookup::NoBrowser)), "another agent's browser");
        assert!(matches!(tried("rohan/t1"), Attempt::Lookup(Lookup::NoBrowser)), "another machine's t1");
        let Attempt::Source(source) = tried("t1") else { panic!("t1's browser not found or not reached") };
        let rx = source.frames();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no frame in 20 s");
            match rx.try_recv() {
                Ok(SourceEvent::Frame(f)) => {
                    assert!(f.page.0 > 0 && f.page.1 > 0);
                    break;
                }
                Ok(SourceEvent::Ended) => panic!("ended before a frame"),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        source.stop();
    }
}
