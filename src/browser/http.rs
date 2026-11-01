//! Minimal HTTP/1.1 client codec (pure, no IO).

#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn get_request(path: &str, port: u16) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").into_bytes()
}

impl Response {
    /// Returns `None` until headers and the `Content-Length` body are complete.
    /// On success also returns the number of bytes consumed.
    pub fn parse(buf: &[u8]) -> Option<(Response, usize)> {
        let (status, headers, head_len) = parse_head(buf)?;
        let mut len = 0usize;
        for line in headers.split("\r\n").skip(1) {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().ok()?;
            }
        }
        let end = head_len.checked_add(len)?;
        if buf.len() < end {
            return None;
        }
        Some((Response { status, body: buf[head_len..end].to_vec() }, end))
    }
}

/// Parses the status line and locates the end of the headers.
/// Returns `(status, header_text, bytes_through_blank_line)`.
pub(super) fn parse_head(buf: &[u8]) -> Option<(u16, String, usize)> {
    let pos = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, head, pos + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_request_has_host_and_close() {
        let r = String::from_utf8(get_request("/json/version", 9222)).unwrap();
        assert!(r.starts_with("GET /json/version HTTP/1.1\r\n"));
        assert!(r.contains("Host: 127.0.0.1:9222\r\n"));
        assert!(r.contains("Connection: close\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parse_waits_for_full_body() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\ncontent-length: 5\r\n\r\nhello";
        assert!(Response::parse(&full[..20]).is_none());
        assert!(Response::parse(&full[..full.len() - 2]).is_none());
        let (r, n) = Response::parse(full).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(n, full.len());
    }

    #[test]
    fn parse_without_content_length_has_empty_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nX: y\r\n\r\n";
        let (r, n) = Response::parse(raw).unwrap();
        assert_eq!(r.status, 404);
        assert!(r.body.is_empty());
        assert_eq!(n, raw.len());
    }
}
