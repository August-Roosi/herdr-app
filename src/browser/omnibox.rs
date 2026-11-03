//! What the address bar does with what was typed, like a browser's omnibox.

const SEARCH: &str = "https://duckduckgo.com/?q=";

/// The address to open for `input`, or "" when there is nothing to do.
pub fn resolve(input: &str) -> String {
    let t = input.trim();
    if t.is_empty() || has_scheme(t) {
        return t.to_owned();
    }
    if !t.contains(char::is_whitespace) {
        let (host, port) = split_host(t);
        if host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok() || host.starts_with('[') {
            return format!("http://{t}");
        }
        let tld = host.rsplit_once('.').map(|(_, tld)| tld);
        if tld.is_some_and(|l| l.len() >= 2 && l.chars().all(|c| c.is_ascii_alphabetic())) || (port && !host.is_empty()) {
            return format!("https://{t}");
        }
    }
    format!("{SEARCH}{}", encode(t))
}

fn has_scheme(t: &str) -> bool {
    let Some((scheme, _)) = t.split_once(':') else { return false };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c))
        && !is_host_port(t)
}

/// `name:3000` style: a scheme-looking word followed by a port.
fn is_host_port(t: &str) -> bool {
    let Some((name, rest)) = t.split_once(':') else { return false };
    let digits = rest.split('/').next().unwrap_or("");
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && name != "about" && !name.is_empty()
}

/// The host part (brackets kept) and whether a port follows.
fn split_host(t: &str) -> (&str, bool) {
    let authority = t.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(end) = authority.find(']') {
        return (&authority[..=end], authority.len() > end + 1);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, true),
        _ => (authority, false),
    }
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn the_address_bar_opens_sites_and_searches() {
        let q = "https://duckduckgo.com/?q=";
        assert_eq!(resolve("facebook"), format!("{q}facebook"));
        assert_eq!(resolve("facebook.com"), "https://facebook.com");
        assert_eq!(resolve("  example.org/a?b=1 "), "https://example.org/a?b=1");
        assert_eq!(resolve("localhost"), "http://localhost");
        assert_eq!(resolve("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(resolve("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(resolve("[::1]:5173"), "http://[::1]:5173");
        assert_eq!(resolve("192.168.1.5"), "http://192.168.1.5");
        assert_eq!(resolve("myhost:8080"), "https://myhost:8080");
        assert_eq!(resolve("http://a.b"), "http://a.b");
        assert_eq!(resolve("file:///tmp/x"), "file:///tmp/x");
        assert_eq!(resolve("about:blank"), "about:blank");
        assert_eq!(resolve("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(resolve("how to cook rice"), format!("{q}how+to+cook+rice"));
        assert_eq!(resolve("a&b c"), format!("{q}a%26b+c"));
        assert_eq!(resolve("see example.com now"), format!("{q}see+example.com+now"));
        assert_eq!(resolve("   "), "");
    }
}
