//! Minimal WebSocket client codec (pure, no IO).

#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Text(String),
    Ping(Vec<u8>),
    Close,
}

pub fn handshake(path: &str, port: u16, key: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes()
}

/// `(is_101, bytes_consumed)` once the response headers are complete.
pub fn handshake_ok(buf: &[u8]) -> Option<(bool, usize)> {
    let (status, _, n) = super::http::parse_head(buf)?;
    Some((status == 101, n))
}

pub fn encode_text(payload: &str, mask: [u8; 4]) -> Vec<u8> {
    encode(0x1, payload.as_bytes(), mask)
}

pub fn encode_pong(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    encode(0xA, payload, mask)
}

fn encode(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => f.push(0x80 | n as u8),
        n if n <= 0xffff => {
            f.push(0x80 | 126);
            f.extend((n as u16).to_be_bytes());
        }
        n => {
            f.push(0x80 | 127);
            f.extend((n as u64).to_be_bytes());
        }
    }
    f.extend(mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Fragmented data message in progress: payload so far, and whether it is text.
    partial: Option<(Vec<u8>, bool)>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Msg> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((fin, op, payload, used)) = parse_frame(&self.buf[pos..]) {
            pos += used;
            match op {
                0x1 | 0x2 => {
                    if fin {
                        if op == 0x1 {
                            out.push(Msg::Text(String::from_utf8_lossy(&payload).into_owned()));
                        }
                        self.partial = None;
                    } else {
                        self.partial = Some((payload, op == 0x1));
                    }
                }
                0x0 => {
                    if let Some((mut acc, is_text)) = self.partial.take() {
                        acc.extend(payload);
                        if !fin {
                            self.partial = Some((acc, is_text));
                        } else if is_text {
                            out.push(Msg::Text(String::from_utf8_lossy(&acc).into_owned()));
                        }
                    }
                }
                0x8 => out.push(Msg::Close),
                0x9 => out.push(Msg::Ping(payload)),
                _ => {} // pong and unknown opcodes
            }
        }
        self.buf.drain(..pos);
        out
    }
}

/// Parses one complete frame: `(fin, opcode, unmasked payload, bytes used)`.
fn parse_frame(b: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if b.len() < 2 {
        return None;
    }
    let masked = b[1] & 0x80 != 0;
    let (len, mut off) = match b[1] & 0x7f {
        126 => (u16::from_be_bytes(b.get(2..4)?.try_into().ok()?) as usize, 4),
        127 => (u64::from_be_bytes(b.get(2..10)?.try_into().ok()?) as usize, 10),
        n => (n as usize, 2),
    };
    let mask = if masked {
        let m: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
        off += 4;
        Some(m)
    } else {
        None
    };
    let end = off.checked_add(len)?;
    let raw = b.get(off..end)?;
    let payload = match mask {
        Some(m) => raw.iter().enumerate().map(|(i, x)| x ^ m[i % 4]).collect(),
        None => raw.to_vec(),
    };
    Some((b[0] & 0x80 != 0, b[0] & 0x0f, payload, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unmask(frame: &[u8]) -> (u8, Vec<u8>) {
        assert!(frame[1] & 0x80 != 0, "client frames must be masked");
        let (len, off) = match frame[1] & 0x7f {
            126 => (u16::from_be_bytes([frame[2], frame[3]]) as usize, 4),
            127 => (u64::from_be_bytes(frame[2..10].try_into().unwrap()) as usize, 10),
            n => (n as usize, 2),
        };
        let m = &frame[off..off + 4];
        let p = frame[off + 4..off + 4 + len]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ m[i % 4])
            .collect();
        (frame[0], p)
    }

    fn server_frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![first];
        match payload.len() {
            n if n < 126 => f.push(n as u8),
            n if n <= 0xffff => {
                f.push(126);
                f.extend((n as u16).to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend((n as u64).to_be_bytes());
            }
        }
        f.extend(payload);
        f
    }

    #[test]
    fn handshake_request_shape() {
        let r = String::from_utf8(handshake("/devtools/page/X", 9222, "abc==")).unwrap();
        assert!(r.starts_with("GET /devtools/page/X HTTP/1.1\r\n"));
        assert!(r.contains("Host: 127.0.0.1:9222\r\n"));
        assert!(r.contains("Upgrade: websocket\r\n"));
        assert!(r.contains("Connection: Upgrade\r\n"));
        assert!(r.contains("Sec-WebSocket-Key: abc==\r\n"));
        assert!(r.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[test]
    fn handshake_101_ok() {
        let ok = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n\x81\x00";
        let hdr = ok.len() - 2;
        assert_eq!(handshake_ok(&ok[..10]), None);
        assert_eq!(handshake_ok(ok), Some((true, hdr)));
        let bad = b"HTTP/1.1 400 Bad Request\r\n\r\n";
        assert_eq!(handshake_ok(bad), Some((false, bad.len())));
    }

    #[test]
    fn encode_text_masked_roundtrip() {
        for s in ["hi", &"x".repeat(300), &"y".repeat(70_000)] {
            let f = encode_text(s, [1, 2, 3, 4]);
            let (b0, p) = unmask(&f);
            assert_eq!(b0, 0x81);
            assert_eq!(p, s.as_bytes());
        }
    }

    #[test]
    fn pong_masked() {
        let (b0, p) = unmask(&encode_pong(b"abc", [9, 8, 7, 6]));
        assert_eq!(b0, 0x8a);
        assert_eq!(p, b"abc");
    }

    #[test]
    fn decoder_handles_fragments_and_64bit_len() {
        let big = "z".repeat(70_000);
        let f = server_frame(0x81, big.as_bytes());
        assert_eq!(f[1], 127);
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for c in f.chunks(1000) {
            out.extend(d.push(c));
        }
        assert_eq!(out, vec![Msg::Text(big)]);

        // FIN=0 text + continuation, with a ping interleaved and split bytes
        let mut s = server_frame(0x01, b"hel");
        s.extend(server_frame(0x89, b"p"));
        s.extend(server_frame(0x00, b"lo "));
        s.extend(server_frame(0x80, b"world"));
        let mut out = Vec::new();
        for b in s.chunks(2) {
            out.extend(d.push(b));
        }
        assert_eq!(out, vec![Msg::Ping(b"p".to_vec()), Msg::Text("hello world".into())]);
    }

    #[test]
    fn decoder_ping_and_close() {
        let mut d = Decoder::default();
        let mut s = server_frame(0x89, b"hi");
        s.extend(server_frame(0x8a, b"pong"));
        s.extend(server_frame(0x82, b"bin"));
        s.extend(server_frame(0x88, &[0x03, 0xe8]));
        assert_eq!(d.push(&s), vec![Msg::Ping(b"hi".to_vec()), Msg::Close]);
    }

    #[test]
    fn decoder_handles_masked_frame() {
        let mut d = Decoder::default();
        let f = encode_text("masked", [5, 6, 7, 8]);
        assert_eq!(d.push(&f), vec![Msg::Text("masked".into())]);
    }
}
