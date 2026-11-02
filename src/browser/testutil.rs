//! Test helpers shared by the browser modules: a TCP transport and a
//! headless Chromium.

use super::cdp::{Cdp, Transport, block_on, discover};
use crate::hc::Stream;
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Test transport over a plain TCP socket.
pub struct TcpTransport {
    stream: TcpStream,
    rx: async_channel::Receiver<Stream>,
}

impl TcpTransport {
    pub fn connect(port: u16) -> TcpTransport {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        Self::new(stream)
    }

    pub fn new(stream: TcpStream) -> TcpTransport {
        let (tx, rx) = async_channel::unbounded();
        let mut r = stream.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send_blocking(Stream::Closed(Value::Null));
                        return;
                    }
                    Ok(n) => {
                        if tx.send_blocking(Stream::Data(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        TcpTransport { stream, rx }
    }
}

impl Transport for TcpTransport {
    fn send(&self, bytes: &[u8]) {
        let _ = (&self.stream).write_all(bytes);
    }
    fn recv(&self) -> async_channel::Receiver<Stream> {
        self.rx.clone()
    }
    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The test page: white, red once clicked, and a 64 px canvas at the top
/// left that is redrawn (differently) every animation frame, so the
/// screencast always has a fresh frame to send.
pub const PAGE: &str = "data:text/html,<style>html,body{margin:0;height:100%25;background:white}canvas{position:fixed;left:0;top:0}</style><body onclick=\"document.body.style.background='red'\"><canvas width=64 height=64></canvas><script>const c=document.querySelector('canvas').getContext('2d');let n=0;(function f(){n++;c.fillStyle='rgb(0,0,'+(128+n%25128)+')';c.fillRect(0,0,64,64);c.fillStyle='white';c.fillRect(n%2564,0,4,64);requestAnimationFrame(f)})()</script>";

/// A headless Chromium on a free loopback port, killed (whole process
/// group) and its profile removed on drop.
pub struct Chromium {
    bin: String,
    url: String,
    child: std::process::Child,
    dir: std::path::PathBuf,
    pub port: u16,
}

impl Chromium {
    pub fn start(bin: &str) -> Chromium {
        Self::start_on(bin, PAGE)
    }

    /// Starts showing `url`.
    pub fn start_on(bin: &str, url: &str) -> Chromium {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::path::PathBuf::from(format!("/var/tmp/herdr-cdp-test-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Self::spawn(bin, url, port, &dir);
        Chromium { bin: bin.to_string(), url: url.to_string(), child, dir, port }
    }

    /// Kills the browser and starts it again on the same port and profile.
    pub fn restart(&mut self) {
        self.kill();
        // the old process may hold the port for a moment
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
            assert!(Instant::now() < deadline, "port {} still open after kill", self.port);
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child = Self::spawn(&self.bin, &self.url, self.port, &self.dir);
    }

    fn spawn(bin: &str, url: &str, port: u16, dir: &std::path::Path) -> std::process::Child {
        let mut cmd = std::process::Command::new(bin);
        cmd.args([
            "--headless=new",
            "--remote-debugging-address=127.0.0.1",
            &format!("--remote-debugging-port={port}"),
            "--no-sandbox",
            "--password-store=basic",
            &format!("--user-data-dir={}", dir.display()),
            "--window-size=1280,800",
            url,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        // its own process group, so `kill` takes its helpers too
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "chromium did not open port {port}");
            std::thread::sleep(Duration::from_millis(100));
        }
        child
    }

    fn kill(&mut self) {
        let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}", self.child.id())]).status();
        let _ = self.child.wait();
    }

    /// A browser-level CDP connection.
    pub fn cdp(&self) -> Cdp {
        let path = block_on(discover(TcpTransport::connect(self.port), self.port)).unwrap();
        block_on(Cdp::connect(TcpTransport::connect(self.port), &path, self.port)).unwrap()
    }
}

impl Drop for Chromium {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The Chromium binary for gated tests, or `None` (skip).
pub fn chromium_bin() -> Option<String> {
    let bin = std::env::var("HERDR_TEST_CHROMIUM").ok();
    if bin.is_none() {
        eprintln!("HERDR_TEST_CHROMIUM not set; skipping");
    }
    bin
}

/// Reads an HTTP request's head (up to the blank line).
pub fn read_head(s: &mut TcpStream) -> String {
    let mut got = Vec::new();
    let mut b = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        got.push(b[0]);
    }
    String::from_utf8(got).unwrap()
}

/// A server's (unmasked) text frame.
pub fn server_text(s: &str) -> Vec<u8> {
    let mut f = vec![0x81u8];
    let n = s.len();
    if n < 126 {
        f.push(n as u8);
    } else {
        f.push(126);
        f.extend((n as u16).to_be_bytes());
    }
    f.extend(s.as_bytes());
    f
}

/// Reads one masked client frame: `(first byte, payload)`.
pub fn read_client_frame(s: &mut TcpStream) -> (u8, Vec<u8>) {
    try_read_client_frame(s).unwrap()
}

/// `read_client_frame`, failing when the stream ends.
pub fn try_read_client_frame(s: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    assert!(h[1] & 0x80 != 0, "client frames are masked");
    let len = match h[1] & 0x7f {
        126 => {
            let mut l = [0u8; 2];
            s.read_exact(&mut l)?;
            u16::from_be_bytes(l) as usize
        }
        127 => {
            let mut l = [0u8; 8];
            s.read_exact(&mut l)?;
            u64::from_be_bytes(l) as usize
        }
        n => n as usize,
    };
    let mut m = [0u8; 4];
    s.read_exact(&mut m)?;
    let mut p = vec![0u8; len];
    s.read_exact(&mut p)?;
    for (i, b) in p.iter_mut().enumerate() {
        *b ^= m[i % 4];
    }
    Ok((h[0], p))
}

/// A browser-level CDP endpoint without a browser: one page (`P1`, session
/// `S1`), every call answered (the first after `hold`), the methods called
/// recorded in order.
pub struct FakeBrowser {
    pub calls: Arc<Mutex<Vec<String>>>,
    writer: Arc<Mutex<Option<TcpStream>>>,
    port: u16,
}

impl FakeBrowser {
    pub fn start(hold: Duration) -> FakeBrowser {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (calls, writer) = (Arc::<Mutex<Vec<String>>>::default(), Arc::<Mutex<Option<TcpStream>>>::default());
        let (seen, out) = (calls.clone(), writer.clone());
        std::thread::spawn(move || {
            let Ok((mut s, _)) = l.accept() else { return };
            read_head(&mut s);
            let mut w = s.try_clone().unwrap();
            w.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n").unwrap();
            *out.lock().unwrap() = Some(w);
            let mut first = true;
            while let Ok((b0, p)) = try_read_client_frame(&mut s) {
                match b0 & 0x0f {
                    1 => {}
                    8 => return,
                    _ => continue,
                }
                let m: Value = serde_json::from_slice(&p).unwrap();
                let method = m["method"].as_str().unwrap_or_default().to_owned();
                seen.lock().unwrap().push(method.clone());
                if std::mem::take(&mut first) {
                    std::thread::sleep(hold);
                }
                let result = match method.as_str() {
                    "Target.getTargets" => json!({"targetInfos": [{"targetId": "P1", "type": "page", "url": "about:blank", "title": ""}]}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    _ => json!({}),
                };
                let mut reply = json!({"id": m["id"], "result": result});
                if let Some(session) = m.get("sessionId") {
                    reply["sessionId"] = session.clone();
                }
                send(&out, &reply);
            }
        });
        FakeBrowser { calls, writer, port }
    }

    /// A CDP connection to it.
    pub fn cdp(&self) -> Cdp {
        block_on(Cdp::connect(TcpTransport::connect(self.port), "/devtools/browser/fake", self.port)).unwrap()
    }

    /// Sends `msg` (an event) to the client.
    pub fn send(&self, msg: &Value) {
        send(&self.writer, msg);
    }

    /// How often `method` was called.
    pub fn count(&self, method: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|m| *m == method).count()
    }
}

fn send(writer: &Mutex<Option<TcpStream>>, msg: &Value) {
    if let Some(w) = writer.lock().unwrap().as_mut() {
        let _ = w.write_all(&server_text(&msg.to_string()));
    }
}
