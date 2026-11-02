//! Finding an agent's browser in a `terminal.list` result: the terminal
//! labeled `browser-for=<bare id>` on the agent's own machine.

use serde_json::Value;

/// An agent's browser: where its debugging port is and which terminal runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// `None`: this machine.
    pub machine: Option<String>,
    pub port: u16,
    /// The browser terminal's address (`t7`, `rohan/t7`).
    pub terminal: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Found),
    /// The agent's machine is not connected: its terminals are not listed.
    LineDown(String),
    NoBrowser,
}

/// The user's own browser (`herdr-browser view`): always the one on this
/// screen's machine, whichever space shows it. Its block is never qualified
/// with a machine (mirror.rs), and it may sit in any space (place.rs).
pub const VIEW: &str = "view";

/// A cast: the live view of an agent's own browser (`t3`, `rohan/t3`,
/// `base`), where the pane may fit the page to itself. The view browser shows
/// pages at their own size and is never a cast.
pub fn is_cast(agent: &str) -> bool {
    agent != VIEW
}

/// `rohan/t3` → (`Some("rohan")`, `t3`); `t3` → (`None`, `t3`).
pub fn split(address: &str) -> (Option<&str>, &str) {
    match address.split_once('/') {
        Some((machine, id)) => (Some(machine), id),
        None => (None, address),
    }
}

/// The browser of `agent` (`t3`, `rohan/t3`, `base`, `rohan/base`) in a
/// `terminal.list` result. The label holds the bare id, which other machines
/// reuse: only a terminal on the agent's own machine counts.
pub fn find_browser(list: &Value, agent: &str) -> Option<Found> {
    let (machine, bare) = split(agent);
    list["terminals"].as_array()?.iter().rev().find_map(|t| {
        let address = t["address"].as_str()?;
        if split(address).0 != machine || t["labels"]["browser-for"].as_str() != Some(bare) {
            return None;
        }
        let port = t["labels"]["cdp"].as_str()?.strip_prefix("127.0.0.1:")?.parse().ok()?;
        Some(Found { machine: machine.map(str::to_owned), port, terminal: address.to_owned() })
    })
}

/// Whether `machine` is added here but not connected (`machines[].state`
/// from `terminal.list` or `machine.list`). An unknown machine is not down.
pub fn line_down(machines: &Value, machine: &str) -> bool {
    machines["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"].as_str() == Some(machine))
        .and_then(|m| m["state"].as_str().or_else(|| m["line"].as_str()))
        .is_some_and(|state| state != "connected")
}

/// What the pane does next: connect, wait for the line, or say there is no browser.
pub fn state_for(list: &Value, machines: &Value, agent: &str) -> Lookup {
    if let Some(found) = find_browser(list, agent) {
        return Lookup::Found(found);
    }
    match split(agent).0 {
        Some(machine) if line_down(machines, machine) => Lookup::LineDown(machine.to_owned()),
        _ => Lookup::NoBrowser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn term(address: &str, labels: Value) -> Value {
        json!({"address": address, "state": "live", "labels": labels})
    }

    fn list(terminals: Vec<Value>) -> Value {
        json!({"terminals": terminals, "hidden_finished": 0})
    }

    #[test]
    fn find_browser_filters_by_machine() {
        let l = list(vec![
            term("t2", json!({})),
            term("t3", json!({"browser-for": "t1", "cdp": "127.0.0.1:9222"})),
            term("rohan/t9", json!({"browser-for": "t1", "cdp": "127.0.0.1:9333"})),
        ]);
        assert_eq!(find_browser(&l, "t1"), Some(Found { machine: None, port: 9222, terminal: "t3".into() }));
        assert_eq!(find_browser(&l, "rohan/t1"), Some(Found { machine: Some("rohan".into()), port: 9333, terminal: "rohan/t9".into() }));
        assert_eq!(find_browser(&l, "t2"), None);
        assert_eq!(find_browser(&l, "other/t1"), None, "another machine's t1 is not this one");
        assert_eq!(find_browser(&json!({}), "t1"), None);
    }

    #[test]
    fn find_browser_base() {
        let l = list(vec![
            term("t4", json!({"browser-for": "base", "cdp": "127.0.0.1:9400"})),
            term("rohan/t5", json!({"browser-for": "base", "cdp": "127.0.0.1:9401"})),
        ]);
        assert_eq!(find_browser(&l, "base"), Some(Found { machine: None, port: 9400, terminal: "t4".into() }));
        assert_eq!(find_browser(&l, "rohan/base"), Some(Found { machine: Some("rohan".into()), port: 9401, terminal: "rohan/t5".into() }));
    }

    #[test]
    fn the_view_browser_is_this_machines() {
        let l = list(vec![
            term("rohan/t5", json!({"browser-for": "view", "cdp": "127.0.0.1:9500"})),
            term("t6", json!({"browser-for": "view", "cdp": "127.0.0.1:9600"})),
        ]);
        // Another machine's view browser is that machine's user's, not ours.
        assert_eq!(find_browser(&l, VIEW), Some(Found { machine: None, port: 9600, terminal: "t6".into() }));
        assert_eq!(find_browser(&list(vec![l["terminals"][0].clone()]), VIEW), None);
    }

    #[test]
    fn find_browser_bad_cdp_label_is_none() {
        for cdp in [json!("127.0.0.1"), json!("127.0.0.1:abc"), json!("127.0.0.1:99999"), json!("10.0.0.1:9222"), json!(9222), Value::Null] {
            let l = list(vec![term("t3", json!({"browser-for": "t1", "cdp": cdp}))]);
            assert_eq!(find_browser(&l, "t1"), None, "{cdp}");
        }
    }

    #[test]
    fn state_reconnecting_when_line_down() {
        let machines = json!({"machines": [
            {"name": "rohan", "state": "connecting"},
            {"name": "anteos", "state": "connected"},
        ]});
        let l = list(vec![term("t3", json!({"browser-for": "t1", "cdp": "127.0.0.1:9222"}))]);
        assert_eq!(state_for(&l, &machines, "rohan/t1"), Lookup::LineDown("rohan".into()));
        assert_eq!(state_for(&l, &machines, "t2"), Lookup::NoBrowser);
        assert_eq!(state_for(&l, &machines, "anteos/t1"), Lookup::NoBrowser, "connected: really no browser");
        assert_eq!(state_for(&l, &machines, "t1"), Lookup::Found(Found { machine: None, port: 9222, terminal: "t3".into() }));
        assert!(line_down(&machines, "rohan"));
        assert!(!line_down(&machines, "anteos"));
        assert!(!line_down(&machines, "unknown"), "not added here: nothing to wait for");
        for state in ["needs_login", "needs_update", "unreachable"] {
            assert!(line_down(&json!({"machines": [{"name": "rohan", "state": state}]}), "rohan"), "{state}");
        }
    }
}
