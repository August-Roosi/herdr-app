//! Maps pane input (window px, GPUI keystrokes) to CDP `Input.*` params.

use serde_json::{Value, json};

/// Maps `p` in a pane rect `(x, y, w, h)` (px) to page CSS px. The page
/// (`(w, h)` CSS px) is scaled to fit the pane and centered; `None` outside
/// the shown image.
pub fn map_point(pane: (f32, f32, f32, f32), page: (u32, u32), p: (f32, f32)) -> Option<(f64, f64)> {
    let (x, y, w, h) = (pane.0 as f64, pane.1 as f64, pane.2 as f64, pane.3 as f64);
    let (pw, ph) = (page.0 as f64, page.1 as f64);
    if pw <= 0.0 || ph <= 0.0 || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (w / pw).min(h / ph);
    let (ox, oy) = (x + (w - pw * scale) / 2.0, y + (h - ph * scale) / 2.0);
    let (px, py) = ((p.0 as f64 - ox) / scale, (p.1 as f64 - oy) / scale);
    (px >= 0.0 && py >= 0.0 && px < pw && py < ph).then_some((px, py))
}

/// `Input.dispatchKeyEvent` params for a keystroke; empty for plain
/// printable text, which goes through `Input.insertText` instead.
pub fn key_event(keystroke: &gpui::Keystroke, down: bool) -> Vec<Value> {
    let m = &keystroke.modifiers;
    let chord = m.control || m.alt || m.platform;
    if !chord && text(keystroke).is_some() {
        return Vec::new();
    }
    let Some((key, code, vk)) = named(&keystroke.key) else { return Vec::new() };
    let modifiers = m.alt as u8 | (m.control as u8) << 1 | (m.platform as u8) << 2 | (m.shift as u8) << 3;
    let mut ev = json!({"key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": modifiers});
    ev["type"] = json!(match down {
        false => "keyUp",
        // Enter's text makes the page's keypress (form submit) happen
        true if key == "Enter" && !chord => {
            ev["text"] = json!("\r");
            ev["unmodifiedText"] = json!("\r");
            "keyDown"
        }
        true => "rawKeyDown",
    });
    vec![ev]
}

/// The text a keystroke types, if it types any.
fn text(k: &gpui::Keystroke) -> Option<String> {
    let t = match &k.key_char {
        Some(c) => c.clone(),
        None if k.key == "space" => " ".to_string(),
        None if k.key.chars().count() == 1 => k.key.clone(),
        None => return None,
    };
    (!t.is_empty() && !t.chars().any(char::is_control)).then_some(t)
}

/// GPUI key name → DOM `key`, `code` and Windows virtual key code.
fn named(key: &str) -> Option<(String, String, u32)> {
    let fixed = |k: &str, c: &str, vk: u32| Some((k.to_string(), c.to_string(), vk));
    match key {
        "enter" => fixed("Enter", "Enter", 13),
        "tab" => fixed("Tab", "Tab", 9),
        "backspace" => fixed("Backspace", "Backspace", 8),
        "escape" => fixed("Escape", "Escape", 27),
        "space" => fixed(" ", "Space", 32),
        "delete" => fixed("Delete", "Delete", 46),
        "insert" => fixed("Insert", "Insert", 45),
        "home" => fixed("Home", "Home", 36),
        "end" => fixed("End", "End", 35),
        "pageup" => fixed("PageUp", "PageUp", 33),
        "pagedown" => fixed("PageDown", "PageDown", 34),
        "left" => fixed("ArrowLeft", "ArrowLeft", 37),
        "up" => fixed("ArrowUp", "ArrowUp", 38),
        "right" => fixed("ArrowRight", "ArrowRight", 39),
        "down" => fixed("ArrowDown", "ArrowDown", 40),
        _ => {
            if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
                return Some((format!("F{n}"), format!("F{n}"), 111 + n));
            }
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            match c {
                'a'..='z' => Some((key.to_string(), format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as u32)),
                '0'..='9' => Some((key.to_string(), format!("Digit{c}"), c as u32)),
                _ => {
                    let (code, vk) = match c {
                        '-' => ("Minus", 189),
                        '=' => ("Equal", 187),
                        '[' => ("BracketLeft", 219),
                        ']' => ("BracketRight", 221),
                        '\\' => ("Backslash", 220),
                        ';' => ("Semicolon", 186),
                        '\'' => ("Quote", 222),
                        ',' => ("Comma", 188),
                        '.' => ("Period", 190),
                        '/' => ("Slash", 191),
                        '`' => ("Backquote", 192),
                        _ => ("", 0),
                    };
                    Some((key.to_string(), code.to_string(), vk))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    #[test]
    fn map_point_letterbox_and_scale() {
        // 1280×800 in 1000×1000: 1000×625 at y 187.5
        for s in [1.0f32, 2.0] {
            let pane = (0.0, 0.0, 1000.0 * s, 1000.0 * s);
            assert_eq!(map_point(pane, (1280, 800), (500.0 * s, 500.0 * s)), Some((640.0, 400.0)));
            assert_eq!(map_point(pane, (1280, 800), (500.0 * s, 100.0 * s)), None);
            assert_eq!(map_point(pane, (1280, 800), (0.0, 187.5 * s)), Some((0.0, 0.0)));
            assert_eq!(map_point(pane, (1280, 800), (500.0 * s, 813.0 * s)), None);
        }
        // pane offset in the window; pillarbox (tall page in a wide pane)
        assert_eq!(map_point((100.0, 50.0, 1000.0, 1000.0), (1280, 800), (600.0, 550.0)), Some((640.0, 400.0)));
        assert_eq!(map_point((0.0, 0.0, 400.0, 200.0), (100, 100), (200.0, 100.0)), Some((50.0, 50.0)));
        assert_eq!(map_point((0.0, 0.0, 400.0, 200.0), (100, 100), (50.0, 100.0)), None);
        assert_eq!(map_point((0.0, 0.0, 400.0, 200.0), (0, 0), (50.0, 100.0)), None);
    }

    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn key_event_enter_and_ctrl_a() {
        assert_eq!(
            key_event(&ks("enter"), true),
            vec![json!({"type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0, "text": "\r", "unmodifiedText": "\r"})]
        );
        assert_eq!(key_event(&ks("enter"), false), vec![json!({"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0})]);
        assert_eq!(
            key_event(&ks("ctrl-a"), true),
            vec![json!({"type": "rawKeyDown", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": 2})]
        );
        assert_eq!(key_event(&ks("ctrl-a"), false), vec![json!({"type": "keyUp", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": 2})]);
        assert_eq!(
            key_event(&ks("shift-tab"), true),
            vec![json!({"type": "rawKeyDown", "key": "Tab", "code": "Tab", "windowsVirtualKeyCode": 9, "modifiers": 8})]
        );
        assert_eq!(
            key_event(&ks("ctrl-alt-cmd-left"), true),
            vec![json!({"type": "rawKeyDown", "key": "ArrowLeft", "code": "ArrowLeft", "windowsVirtualKeyCode": 37, "modifiers": 7})]
        );
        assert_eq!(key_event(&ks("f5"), true)[0]["windowsVirtualKeyCode"], 116);
        assert_eq!(key_event(&ks("ctrl-5"), true)[0]["code"], "Digit5");
        // plain printable text goes through Input.insertText
        assert!(key_event(&ks("a"), true).is_empty());
        assert!(key_event(&ks("a"), false).is_empty());
        assert!(key_event(&ks("A"), true).is_empty(), "shift is still plain text");
        let mut e = ks("e");
        e.key_char = Some("é".into());
        assert!(key_event(&e, true).is_empty());
        assert!(key_event(&ks("space"), true).is_empty());
    }
}
