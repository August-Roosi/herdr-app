//! Chrome DevTools Protocol client over a byte transport (a herdr-core
//! socket channel, or a TCP socket in tests), plus target following.

use super::{http, ws};
use crate::hc::{Core, Opened, Stream};
use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A byte pipe to the browser's debugging port.
pub trait Transport: Send {
    fn send(&self, bytes: &[u8]);
    fn recv(&self) -> async_channel::Receiver<Stream>;
    fn close(&self);
}

/// A `socket.open` channel through herdr-core. Credits the core for every
/// byte handed on, so the stream never stalls after the initial 1 MiB.
pub struct CoreTransport {
    core: Arc<Core>,
    channel: u32,
    rx: async_channel::Receiver<Stream>,
    closed: AtomicBool,
}

impl CoreTransport {
    pub fn new(core: Arc<Core>, opened: Opened) -> CoreTransport {
        // Small buffer: credit tracks what the consumer has (nearly) taken.
        let (tx, rx) = async_channel::bounded(16);
        // Weak: an unclosed transport must not keep the core connection alive.
        let (c, channel) = (Arc::downgrade(&core), opened.channel);
        std::thread::spawn(move || {
            while let Ok(item) = opened.stream.recv_blocking() {
                let n = match &item {
                    Stream::Data(d) => d.len(),
                    _ => 0,
                };
                let end = !matches!(item, Stream::Data(_));
                if tx.send_blocking(item).is_err() || end {
                    return;
                }
                if n > 0
                    && let Some(c) = c.upgrade()
                {
                    c.credit(channel, n);
                }
            }
        });
        CoreTransport { core, channel, rx, closed: AtomicBool::new(false) }
    }
}

impl Drop for CoreTransport {
    fn drop(&mut self) {
        // A connect given up (cancelled, timed out) never handed it on: its
        // channel, route and forwarding thread end here.
        self.close();
    }
}

impl Transport for CoreTransport {
    fn send(&self, bytes: &[u8]) {
        self.core.write(self.channel, bytes);
    }
    fn recv(&self) -> async_channel::Receiver<Stream> {
        self.rx.clone()
    }
    fn close(&self) {
        // Both `Cdp` and its reader may close: detach only once.
        if !self.closed.swap(true, Ordering::Relaxed) {
            self.core.detach(self.channel);
        }
    }
}

/// Asks the browser's HTTP endpoint (`port` is only for the Host header) for
/// its WebSocket path, e.g. `/devtools/browser/<id>`. Closes `http`.
pub async fn discover(http: impl Transport + 'static, port: u16) -> Result<String> {
    let rx = http.recv();
    http.send(&http::get_request("/json/version", port));
    let mut buf = Vec::new();
    let resp = loop {
        if let Some((r, _)) = http::Response::parse(&buf) {
            break Ok(r);
        }
        match rx.recv().await {
            Ok(Stream::Data(d)) => buf.extend(d),
            _ => break Err(anyhow!("debugging port closed before /json/version answered")),
        }
    };
    http.close();
    let resp = resp?;
    if resp.status != 200 {
        bail!("/json/version answered {}", resp.status);
    }
    let v: Value = serde_json::from_slice(&resp.body).context("/json/version is not JSON")?;
    let url = v["webSocketDebuggerUrl"].as_str().ok_or_else(|| anyhow!("/json/version has no webSocketDebuggerUrl"))?;
    ws_path(url).ok_or_else(|| anyhow!("unexpected webSocketDebuggerUrl {url}"))
}

/// `ws://127.0.0.1:P/devtools/browser/<id>` → `/devtools/browser/<id>`.
fn ws_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("ws://").or_else(|| url.strip_prefix("wss://"))?;
    Some(rest[rest.find('/')?..].to_string())
}

#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// Flatten-mode session the event belongs to; `None` for the browser.
    pub session: Option<String>,
}

/// A call's result, or the CDP error message.
type Reply = async_channel::Sender<Result<Value, String>>;

struct Inner {
    transport: Mutex<Box<dyn Transport>>,
    /// `None` once the connection ended: no new calls are accepted.
    pending: Mutex<Option<HashMap<u64, Reply>>>,
    next_id: AtomicU64,
}

impl Inner {
    fn send_text(&self, text: &str) {
        self.transport.lock().unwrap().send(&ws::encode_text(text, next_mask()));
    }
}

/// One CDP WebSocket connection. A reader thread answers pings, resolves
/// calls and forwards events; close or loss ends `events()` and fails calls.
pub struct Cdp {
    inner: Arc<Inner>,
    events: async_channel::Receiver<Event>,
}

impl Cdp {
    /// Upgrades `ws` (its own connection, not the one `discover` used) to a
    /// WebSocket on `path`. `port` is only for the Host header.
    pub async fn connect(ws: impl Transport + 'static, path: &str, port: u16) -> Result<Cdp> {
        let rx = ws.recv();
        ws.send(&ws::handshake(path, port, &handshake_key()));
        let mut buf = Vec::new();
        let rest = loop {
            if let Some((ok, n)) = ws::handshake_ok(&buf) {
                if !ok {
                    ws.close();
                    bail!("websocket upgrade to {path} refused: {}", String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or(""));
                }
                break buf.split_off(n);
            }
            match rx.recv().await {
                Ok(Stream::Data(d)) => buf.extend(d),
                _ => {
                    ws.close();
                    bail!("debugging port closed during websocket upgrade");
                }
            }
        };
        let inner = Arc::new(Inner {
            transport: Mutex::new(Box::new(ws)),
            pending: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(1),
        });
        let (etx, events) = async_channel::unbounded();
        let reader = inner.clone();
        std::thread::spawn(move || read_loop(reader, rx, rest, etx));
        Ok(Cdp { inner, events })
    }

    /// Sends `method` (to `session`, flatten mode) and waits for its result.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = async_channel::bounded(1);
        match self.inner.pending.lock().unwrap().as_mut() {
            Some(p) => p.insert(id, tx),
            None => bail!("cdp {method}: connection closed"),
        };
        self.inner.send_text(&msg.to_string());
        match rx.recv().await {
            Ok(r) => r.map_err(|e| anyhow!("cdp {method}: {e}")),
            Err(_) => Err(anyhow!("cdp {method}: connection closed")),
        }
    }

    /// `call` for threads without an executor (tests).
    #[cfg(test)]
    pub fn call_blocking(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        block_on(self.call(method, params, session))
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    /// True once the connection ended (calls fail from then on).
    pub fn is_closed(&self) -> bool {
        self.inner.pending.lock().unwrap().is_none()
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        // Ends the reader thread too: the transport's stream closes.
        self.inner.transport.lock().unwrap().close();
    }
}

fn read_loop(inner: Arc<Inner>, rx: async_channel::Receiver<Stream>, first: Vec<u8>, events: async_channel::Sender<Event>) {
    let mut dec = ws::Decoder::default();
    let mut bytes = first;
    'outer: loop {
        for msg in dec.push(&bytes) {
            match msg {
                ws::Msg::Ping(p) => inner.transport.lock().unwrap().send(&ws::encode_pong(&p, next_mask())),
                ws::Msg::Close => break 'outer,
                ws::Msg::Text(t) => {
                    let Ok(mut v) = serde_json::from_str::<Value>(&t) else { continue };
                    if let Some(id) = v.get("id").and_then(Value::as_u64) {
                        let reply = inner.pending.lock().unwrap().as_mut().and_then(|p| p.remove(&id));
                        if let Some(reply) = reply {
                            let r = match v.get("error") {
                                Some(e) => Err(e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string())),
                                None => Ok(v["result"].take()),
                            };
                            let _ = reply.try_send(r);
                        }
                    } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                        let event = Event {
                            method: method.to_string(),
                            session: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
                            params: v["params"].take(),
                        };
                        let _ = events.try_send(event);
                    }
                }
            }
        }
        match rx.recv_blocking() {
            Ok(Stream::Data(d)) => bytes = d,
            _ => break,
        }
    }
    // Dropping the reply senders fails waiting calls; dropping `events` ends it.
    inner.pending.lock().unwrap().take();
    inner.transport.lock().unwrap().close();
}

static ENTROPY: AtomicU64 = AtomicU64::new(0);

/// Not cryptographic: WebSocket masks and keys only need to vary.
fn next_u64() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut z = ENTROPY.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed) ^ t ^ (std::process::id() as u64) << 32;
    // splitmix64 finalizer
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn next_mask() -> [u8; 4] {
    (next_u64() as u32).to_le_bytes()
}

fn handshake_key() -> String {
    use base64::Engine;
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&next_u64().to_le_bytes());
    k[8..].copy_from_slice(&next_u64().to_le_bytes());
    base64::engine::general_purpose::STANDARD.encode(k)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInfo {
    pub id: String,
    /// CDP `type`: "page", "iframe", "service_worker", ...
    pub kind: String,
    pub url: String,
    pub title: String,
    /// Bumped when a page target is created or changes url: newest wins.
    pub seq: u64,
}

/// The browser's targets, kept from `Target.*` discovery events.
#[derive(Default)]
pub struct Targets {
    pub list: Vec<TargetInfo>,
    seq: u64,
}

impl Targets {
    /// Applies a CDP event; true if it may change what `pick_target` picks
    /// (a title-only change updates the list but returns false).
    pub fn apply(&mut self, method: &str, params: &Value) -> bool {
        match method {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let i = &params["targetInfo"];
                let Some(id) = i["targetId"].as_str() else { return false };
                let s = |k: &str| i[k].as_str().unwrap_or_default().to_string();
                let (kind, url, title) = (s("type"), s("url"), s("title"));
                let page = kind == "page";
                match self.list.iter_mut().find(|t| t.id == id) {
                    Some(t) => {
                        let moved = page && t.url != url;
                        let relevant = moved || t.kind != kind;
                        if moved {
                            self.seq += 1;
                            t.seq = self.seq;
                        }
                        (t.kind, t.url, t.title) = (kind, url, title);
                        relevant
                    }
                    None => {
                        if page {
                            self.seq += 1;
                        }
                        self.list.push(TargetInfo { id: id.to_string(), kind, url, title, seq: if page { self.seq } else { 0 } });
                        true
                    }
                }
            }
            "Target.targetDestroyed" => {
                let Some(id) = params["targetId"].as_str() else { return false };
                let before = self.list.len();
                self.list.retain(|t| t.id != id);
                self.list.len() != before
            }
            _ => false,
        }
    }
}

/// The target to show: `pinned` while it exists, else the newest page.
pub fn pick_target(targets: &[TargetInfo], pinned: Option<&str>) -> Option<String> {
    if let Some(p) = pinned
        && targets.iter().any(|t| t.id == p)
    {
        return Some(p.to_string());
    }
    targets.iter().filter(|t| t.kind == "page").max_by_key(|t| t.seq).map(|t| t.id.clone())
}

/// `f`, or an error once `limit` passed without it finishing (`f` is then
/// dropped). Needs no executor: a timer thread wakes it, and ends early once
/// `f` finished.
pub async fn within<T>(limit: std::time::Duration, what: &str, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::task::Poll;
    let (done, finished) = channel::<()>();
    let (tx, late) = async_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        if finished.recv_timeout(limit) == Err(RecvTimeoutError::Timeout) {
            let _ = tx.try_send(());
        }
    });
    let mut f = std::pin::pin!(f);
    let mut late = std::pin::pin!(late.recv());
    let out = std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = f.as_mut().poll(cx) {
            return Poll::Ready(v);
        }
        match late.as_mut().poll(cx) {
            Poll::Ready(_) => Poll::Ready(Err(anyhow!("{what}: no answer in {limit:?}"))),
            Poll::Pending => Poll::Pending,
        }
    })
    .await;
    drop(done);
    out
}

/// Drives a future to its end on this thread (tests).
#[cfg(test)]
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Chromium, TcpTransport, read_client_frame, read_head, server_text};
    use super::*;
    use serde_json::json;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    fn t(id: &str, kind: &str, seq: u64) -> TargetInfo {
        TargetInfo { id: id.into(), kind: kind.into(), url: String::new(), title: String::new(), seq }
    }

    #[test]
    fn pick_target_follows_newest_page() {
        let ts = [t("a", "page", 1), t("b", "page", 5), t("c", "page", 3)];
        assert_eq!(pick_target(&ts, None).as_deref(), Some("b"));
        assert_eq!(pick_target(&[], None), None);
    }

    #[test]
    fn pick_target_keeps_pinned() {
        let ts = [t("a", "page", 1), t("b", "page", 5)];
        assert_eq!(pick_target(&ts, Some("a")).as_deref(), Some("a"));
        // a pinned target that went away falls back to the newest page
        assert_eq!(pick_target(&ts, Some("gone")).as_deref(), Some("b"));
    }

    #[test]
    fn pick_target_ignores_non_pages() {
        let ts = [t("a", "page", 1), t("w", "service_worker", 9), t("f", "iframe", 8)];
        assert_eq!(pick_target(&ts, None).as_deref(), Some("a"));
        assert_eq!(pick_target(&[t("w", "service_worker", 9)], None), None);
    }

    #[test]
    fn targets_bump_seq_on_create_and_url_change() {
        let info = |id: &str, kind: &str, url: &str| json!({"targetInfo": {"targetId": id, "type": kind, "url": url, "title": ""}});
        let mut ts = Targets::default();
        assert!(ts.apply("Target.targetCreated", &info("a", "page", "about:blank")));
        assert!(ts.apply("Target.targetCreated", &info("b", "page", "about:blank")));
        assert!(ts.apply("Target.targetCreated", &info("w", "service_worker", "x")));
        assert_eq!(pick_target(&ts.list, None).as_deref(), Some("b"));
        // a title-only change does not move focus; a url change does
        assert!(!ts.apply("Target.targetInfoChanged", &json!({"targetInfo": {"targetId": "a", "type": "page", "url": "about:blank", "title": "t"}})));
        assert_eq!(ts.list.iter().find(|t| t.id == "a").unwrap().title, "t");
        assert_eq!(pick_target(&ts.list, None).as_deref(), Some("b"));
        assert!(ts.apply("Target.targetInfoChanged", &info("a", "page", "https://x/")));
        assert_eq!(pick_target(&ts.list, None).as_deref(), Some("a"));
        assert!(ts.apply("Target.targetDestroyed", &json!({"targetId": "a"})));
        assert_eq!(pick_target(&ts.list, None).as_deref(), Some("b"));
        assert!(!ts.apply("Page.frameNavigated", &json!({})));
    }

    #[cfg(unix)]
    /// A channel through a real (fork) core whose peer is a plain listener.
    /// Returns the core, the transport and the peer's side of the socket.
    fn core_channel(iso: &crate::hc::tests::Isolated) -> (Arc<Core>, CoreTransport, TcpStream) {
        let core = iso.connect();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let opened = core.open_socket(&addr, None).unwrap();
        let (peer, _) = l.accept().unwrap();
        (core.clone(), CoreTransport::new(core, opened), peer)
    }

    #[cfg(unix)]
    /// True once the peer reads end of stream (within 5 s).
    fn peer_sees_eof(mut peer: TcpStream) -> bool {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 64];
        matches!(std::io::Read::read(&mut peer, &mut buf), Ok(0))
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_core_transport_closes_its_channel() {
        let Some(iso) = crate::hc::tests::Isolated::new("transport-drop") else { return };
        let (_core, transport, peer) = core_channel(&iso);
        // e.g. a connect cancelled before it handed the transport on
        drop(transport);
        assert!(peer_sees_eof(peer), "the core kept the socket open");
    }

    #[cfg(unix)]
    #[test]
    fn closing_then_dropping_a_core_transport_detaches_once() {
        let Some(iso) = crate::hc::tests::Isolated::new("transport-close") else { return };
        let (core, transport, peer) = core_channel(&iso);
        transport.close();
        drop(transport);
        assert!(peer_sees_eof(peer));
        // the core connection is still fine
        assert!(core.call("terminal.list", json!({})).is_ok());
    }

    /// Fake browser endpoint: a raw server socket and its client transport.
    fn pair() -> (TcpTransport, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let client = TcpTransport::connect(port);
        let (server, _) = l.accept().unwrap();
        server.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        (client, server)
    }

    #[test]
    fn discover_returns_ws_path() {
        let (client, mut server) = pair();
        let h = std::thread::spawn(move || {
            let head = read_head(&mut server);
            assert!(head.starts_with("GET /json/version HTTP/1.1\r\n"), "{head}");
            assert!(head.contains("Host: 127.0.0.1:9333\r\n"));
            let body = r#"{"Browser":"x","webSocketDebuggerUrl":"ws://127.0.0.1:9333/devtools/browser/abc-1"}"#;
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            // split the response to exercise buffering
            let (a, b) = resp.as_bytes().split_at(30);
            server.write_all(a).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            server.write_all(b).unwrap();
        });
        assert_eq!(block_on(discover(client, 9333)).unwrap(), "/devtools/browser/abc-1");
        h.join().unwrap();
    }

    #[test]
    fn discover_fails_on_early_close() {
        let (client, server) = pair();
        drop(server);
        assert!(block_on(discover(client, 9333)).is_err());
    }

    #[test]
    fn cdp_calls_events_ping_and_close_against_fake() {
        let (client, mut server) = pair();
        let h = std::thread::spawn(move || {
            let head = read_head(&mut server);
            assert!(head.starts_with("GET /devtools/browser/z HTTP/1.1\r\n"), "{head}");
            assert!(head.contains("Sec-WebSocket-Key: "));
            // 101 plus an event in the same packet
            let mut out = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
            out.extend(server_text(r#"{"method":"Target.targetCreated","params":{"a":1}}"#));
            server.write_all(&out).unwrap();

            let (b0, p) = read_client_frame(&mut server);
            assert_eq!(b0, 0x81);
            let m: Value = serde_json::from_slice(&p).unwrap();
            assert_eq!(m["method"], "Browser.getVersion");
            assert!(m.get("sessionId").is_none());
            let id = m["id"].as_u64().unwrap();
            // ping first: the client must pong it
            server.write_all(&[0x89, 2, b'h', b'i']).unwrap();
            let (b0, p) = read_client_frame(&mut server);
            assert_eq!((b0, p.as_slice()), (0x8a, &b"hi"[..]));
            let reply = json!({"id": id, "result": {"product": "fake"}}).to_string();
            server.write_all(&server_text(&reply)).unwrap();

            let (_, p) = read_client_frame(&mut server);
            let m: Value = serde_json::from_slice(&p).unwrap();
            assert_eq!(m["sessionId"], "S1");
            assert_eq!(m["params"]["expression"], "1+1");
            let ev = json!({"method": "Page.loadEventFired", "params": {}, "sessionId": "S1"}).to_string();
            server.write_all(&server_text(&ev)).unwrap();
            let reply = json!({"id": m["id"], "error": {"code": -32000, "message": "nope"}, "sessionId": "S1"}).to_string();
            server.write_all(&server_text(&reply)).unwrap();
            // close frame ends events
            server.write_all(&[0x88, 0]).unwrap();
        });

        let cdp = block_on(Cdp::connect(client, "/devtools/browser/z", 9222)).unwrap();
        let events = cdp.events();
        assert_eq!(cdp.call_blocking("Browser.getVersion", json!({}), None).unwrap()["product"], "fake");
        let e = cdp.call_blocking("Runtime.evaluate", json!({"expression": "1+1"}), Some("S1")).unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");

        let e1 = events.recv_blocking().unwrap();
        assert_eq!((e1.method.as_str(), e1.params["a"].as_i64(), e1.session), ("Target.targetCreated", Some(1), None));
        let e2 = events.recv_blocking().unwrap();
        assert_eq!((e2.method.as_str(), e2.session.as_deref()), ("Page.loadEventFired", Some("S1")));
        assert!(events.recv_blocking().is_err(), "close ends events()");
        assert!(cdp.call_blocking("Browser.getVersion", json!({}), None).is_err(), "calls fail once closed");
        h.join().unwrap();
    }

    #[test]
    fn cdp_connect_rejects_non_101() {
        let (client, mut server) = pair();
        let h = std::thread::spawn(move || {
            read_head(&mut server);
            server.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        assert!(block_on(Cdp::connect(client, "/x", 1)).is_err());
        h.join().unwrap();
    }

    #[test]
    fn handshake_key_is_base64_of_16_bytes_and_masks_vary() {
        use base64::Engine;
        let k = handshake_key();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(&k).unwrap().len(), 16);
        assert_ne!(handshake_key(), k);
        assert_ne!(next_mask(), next_mask());
    }

    fn recv_until(events: &async_channel::Receiver<Event>, secs: u64, mut want: impl FnMut(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match events.try_recv() {
                Ok(e) if want(&e) => return e,
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "timed out waiting for event");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("events ended: {e}"),
            }
        }
    }

    #[test]
    #[ignore = "needs HERDR_TEST_CHROMIUM (a chrome binary); run with --include-ignored"]
    fn cdp_against_real_chromium() {
        let Ok(bin) = std::env::var("HERDR_TEST_CHROMIUM") else {
            eprintln!("HERDR_TEST_CHROMIUM not set; skipping");
            return;
        };
        let chrome = Chromium::start(&bin);
        let path = block_on(discover(TcpTransport::connect(chrome.port), chrome.port)).unwrap();
        assert!(path.starts_with("/devtools/browser/"), "{path}");
        let cdp = block_on(Cdp::connect(TcpTransport::connect(chrome.port), &path, chrome.port)).unwrap();
        let events = cdp.events();
        let mut targets = Targets::default();

        cdp.call_blocking("Target.setDiscoverTargets", json!({"discover": true}), None).unwrap();
        let created = cdp.call_blocking("Target.createTarget", json!({"url": "data:text/html,<title>x</title>"}), None).unwrap();
        let id = created["targetId"].as_str().unwrap().to_string();
        let e = recv_until(&events, 10, |e| {
            targets.apply(&e.method, &e.params);
            e.method == "Target.targetCreated" && e.params["targetInfo"]["targetId"] == id.as_str()
        });
        assert_eq!(e.params["targetInfo"]["type"], "page");
        assert_eq!(pick_target(&targets.list, None).as_deref(), Some(id.as_str()), "follows the newest page");

        // flatten-mode session: calls carry sessionId
        let att = cdp.call_blocking("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None).unwrap();
        let session = att["sessionId"].as_str().unwrap().to_string();
        let r = cdp
            .call_blocking("Runtime.evaluate", json!({"expression": "6*7", "returnByValue": true}), Some(&session))
            .unwrap();
        assert_eq!(r["result"]["value"], 42);
        drop(cdp);
        drop(chrome);
    }
}
