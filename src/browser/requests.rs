//! What agents ask of this screen's browser panes through their terminals'
//! labels (local and remote alike, as the core's terminal list shows them):
//!
//! - `herdr.browser.mine=<n>`: show the user's own browser (`view`) beside
//!   the agent's pane. Each new `<n>` asks once.
//! - `herdr.browser.help=<message>`: the agent needs the user in its own
//!   browser (a login, a captcha). Each new message asks once; empty or gone
//!   means done.
//! - A live local `browser-for=<t>` terminal (agent `t`'s own browser) shows
//!   beside `t`'s pane once per browser terminal: closed, it stays closed
//!   until the next start. Remote agents' browsers never show by themselves.
//!
//! What was there before a machine's terminals were first listed is not news:
//! reloads and reconnects don't reopen anything.

use super::find::{self, VIEW};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const MINE: &str = "herdr.browser.mine";
pub const HELP: &str = "herdr.browser.help";

/// What the window does for an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Show the user's own browser beside `pane` (the asking agent's, if it has one).
    Mine { terminal: String, pane: Option<String> },
    /// Show local agent `agent`'s own browser beside its `pane`.
    Own { agent: String, pane: String },
    /// Tell the user `terminal` (called `name`) needs them in its browser;
    /// `pane`: the agent's, to open its browser beside.
    Help { terminal: String, name: String, message: String, pane: Option<String> },
}

#[derive(Default)]
pub struct Requests {
    /// Machines whose terminals were listed at least once (`""`: this one).
    listed: HashSet<String>,
    /// The last value handled per terminal.
    mine: HashMap<String, String>,
    help: HashMap<String, String>,
    /// Local browser terminals already shown (or there before we looked).
    shown: HashSet<String>,
}

fn machine(address: &str) -> &str {
    find::split(address).0.unwrap_or("")
}

fn label<'a>(rec: &'a Value, key: &str) -> Option<&'a str> {
    rec.get("labels").and_then(|l| l.get(key)).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn running(rec: &Value) -> bool {
    rec.get("state").and_then(Value::as_str).is_none_or(|s| s == "live")
}

/// A local agent's own browser, ready: the agent it is for.
fn own_browser<'a>(address: &str, rec: &'a Value) -> Option<&'a str> {
    let agent = label(rec, "browser-for")?;
    (!address.contains('/') && running(rec) && agent != VIEW && label(rec, "cdp").is_some()).then_some(agent)
}

impl Requests {
    /// What the terminals ask now. `live`: the core's terminals by address;
    /// `pane_of`: an agent's pane in this window.
    pub fn step(&mut self, live: &HashMap<String, Value>, pane_of: impl Fn(&str) -> Option<String>) -> Vec<Ask> {
        let mut machines: HashSet<&str> = live.keys().map(|a| machine(a)).collect();
        machines.insert("");
        // Forget terminals that ended, on machines we can see; a machine
        // that is away keeps its, so its return isn't news.
        let gone = |a: &String| !live.contains_key(a) && machines.contains(machine(a));
        self.mine.retain(|a, _| !gone(a));
        self.help.retain(|a, _| !gone(a));
        self.shown.retain(|a| !gone(a));
        let mut addresses: Vec<&String> = live.keys().collect();
        addresses.sort();
        let mut asks = Vec::new();
        for address in addresses {
            let rec = &live[address];
            let news = self.listed.contains(machine(address));
            match label(rec, MINE) {
                Some(v) if self.mine.get(address).map(String::as_str) != Some(v) => {
                    self.mine.insert(address.clone(), v.to_owned());
                    if news {
                        asks.push(Ask::Mine { terminal: address.clone(), pane: pane_of(address) });
                    }
                }
                Some(_) => {}
                None => {
                    self.mine.remove(address);
                }
            }
            match label(rec, HELP) {
                Some(v) if self.help.get(address).map(String::as_str) != Some(v) => {
                    self.help.insert(address.clone(), v.to_owned());
                    if news {
                        let name = rec.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(address);
                        asks.push(Ask::Help { terminal: address.clone(), name: name.to_owned(), message: v.to_owned(), pane: pane_of(address) });
                    }
                }
                Some(_) => {}
                None => {
                    self.help.remove(address);
                }
            }
            if let Some(agent) = own_browser(address, rec)
                && !self.shown.contains(address)
            {
                if !news {
                    self.shown.insert(address.clone());
                } else if let Some(pane) = pane_of(agent) {
                    // Without a pane yet it waits: the agent's pane may come later.
                    self.shown.insert(address.clone());
                    asks.push(Ask::Own { agent: agent.to_owned(), pane });
                }
            }
        }
        self.listed.extend(machines.into_iter().map(str::to_owned));
        asks
    }
}

/// The help toast's Open: the agent's browser beside its pane, else in a
/// tab in a space on its machine (never beside whatever is focused, which
/// may be on another machine).
pub fn help_action(terminal: &str, pane: Option<&str>) -> Value {
    let args = match pane {
        Some(pane) => serde_json::json!({ "terminal": terminal, "pane": pane }),
        None => serde_json::json!({ "terminal": terminal, "place": "tab" }),
    };
    serde_json::json!({ "name": "browser.show", "args": args, "label": "Open" })
}

/// Whether this machine's view browser runs (a live local `browser-for=view`).
pub fn view_running(live: &HashMap<String, Value>) -> bool {
    live.iter().any(|(a, r)| !a.contains('/') && running(r) && label(r, "browser-for") == Some(VIEW))
}

/// Where `herdr-browser` may be, in order: `HERDR_BROWSER_BIN`; beside the
/// core; on PATH; the usual install places.
pub fn candidates(env: Option<OsString>, core: &Path, path: Option<OsString>, home: &Path, local_app_data: Option<OsString>) -> Vec<PathBuf> {
    let name = format!("herdr-browser{}", std::env::consts::EXE_SUFFIX);
    let mut out: Vec<PathBuf> = env.filter(|e| !e.is_empty()).map(PathBuf::from).into_iter().collect();
    let mut dirs: Vec<PathBuf> = core.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).into_iter().collect();
    dirs.extend(path.iter().flat_map(std::env::split_paths));
    if cfg!(windows) {
        let local = local_app_data.map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Local"));
        dirs.push(local.join("Programs/herdr-browser"));
    } else {
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(home.join(".cargo/bin"));
    out.extend(dirs.into_iter().map(|d| d.join(&name)));
    out
}

/// The installed `herdr-browser`, if any.
pub fn find_bin() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k);
    candidates(env("HERDR_BROWSER_BIN"), &crate::core_bin::path(), env("PATH"), &crate::mods::home(), env("LOCALAPPDATA"))
        .into_iter()
        .find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live(terms: &[(&str, Value)]) -> HashMap<String, Value> {
        terms.iter().map(|(a, labels)| (a.to_string(), json!({"address": a, "name": "", "state": "live", "labels": labels}))).collect()
    }

    fn panes(a: &str) -> Option<String> {
        Some(format!("p-{a}"))
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn mine_is_handled_once_per_value() {
        let mut r = Requests::default();
        // Asked before Herdr looked: not news.
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "1"}))]), panes), []);
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "1"}))]), panes), []);
        let mine = |t: &str| Ask::Mine { terminal: t.into(), pane: Some(format!("p-{t}")) };
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"}))]), panes), [mine("t1")]);
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"}))]), panes), [], "a reload doesn't reopen");
        // A remote agent asks too; its machine's first listing is not news.
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"})), ("rohan/t4", json!({MINE: "7"}))]), panes), []);
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"})), ("rohan/t4", json!({MINE: "8"}))]), panes), [mine("rohan/t4")]);
        // The line drops and comes back: the same value is old news.
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"}))]), panes), []);
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "2"})), ("rohan/t4", json!({MINE: "8"}))]), panes), []);
        // No pane for the agent: beside whatever is focused.
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "3"}))]), none), [Ask::Mine { terminal: "t1".into(), pane: None }]);
        // A new terminal asking at once is news.
        assert_eq!(r.step(&live(&[("t1", json!({MINE: "3"})), ("t2", json!({MINE: "1"}))]), panes), [mine("t2")]);
    }

    #[test]
    fn help_is_told_once_per_message() {
        let mut r = Requests::default();
        assert_eq!(r.step(&live(&[("t1", json!({HELP: "old"}))]), panes), []);
        let mut l = live(&[("t1", json!({HELP: "log in to GitHub"}))]);
        l.get_mut("t1").unwrap()["name"] = json!("claude");
        let help = Ask::Help { terminal: "t1".into(), name: "claude".into(), message: "log in to GitHub".into(), pane: Some("p-t1".into()) };
        assert_eq!(r.step(&l, panes), [help.clone()]);
        assert_eq!(r.step(&l, panes), []);
        // Done, then the same request again: news again.
        assert_eq!(r.step(&live(&[("t1", json!({HELP: ""}))]), panes), []);
        assert_eq!(r.step(&l, panes), [help]);
        // Unnamed: its address.
        assert_eq!(
            r.step(&live(&[("t1", json!({})), ("rohan/t2", json!({}))]), panes),
            []
        );
        assert_eq!(
            r.step(&live(&[("t1", json!({})), ("rohan/t2", json!({HELP: "captcha"}))]), panes),
            [Ask::Help { terminal: "rohan/t2".into(), name: "rohan/t2".into(), message: "captcha".into(), pane: Some("p-rohan/t2".into()) }]
        );
    }

    #[test]
    fn help_opens_beside_the_agents_pane_or_in_a_tab() {
        // A remote agent: its own pane, wherever focus is.
        assert_eq!(help_action("rohan/t2", Some("rohan/p4")), json!({"name": "browser.show", "args": {"terminal": "rohan/t2", "pane": "rohan/p4"}, "label": "Open"}));
        assert_eq!(help_action("rohan/t2", None), json!({"name": "browser.show", "args": {"terminal": "rohan/t2", "place": "tab"}, "label": "Open"}));
        let mut r = Requests::default();
        r.step(&live(&[("rohan/t2", json!({}))]), none);
        assert_eq!(
            r.step(&live(&[("rohan/t2", json!({HELP: "captcha"}))]), none),
            [Ask::Help { terminal: "rohan/t2".into(), name: "rohan/t2".into(), message: "captcha".into(), pane: None }]
        );
    }

    #[test]
    fn a_local_agents_browser_shows_once_per_start() {
        let mut r = Requests::default();
        let browser = |agent: &str| json!({"browser-for": agent, "cdp": "127.0.0.1:9222"});
        // Running before Herdr looked: not news.
        assert_eq!(r.step(&live(&[("t1", json!({})), ("t2", browser("t1"))]), panes), []);
        // A new one.
        let own = Ask::Own { agent: "t3".into(), pane: "p-t3".into() };
        assert_eq!(r.step(&live(&[("t3", json!({})), ("t4", browser("t3"))]), panes), [own.clone()]);
        // Closed by the user: stays closed while that browser runs.
        assert_eq!(r.step(&live(&[("t3", json!({})), ("t4", browser("t3"))]), panes), []);
        // The next start shows again.
        assert_eq!(r.step(&live(&[("t3", json!({}))]), panes), []);
        assert_eq!(r.step(&live(&[("t3", json!({})), ("t5", browser("t3"))]), panes), [Ask::Own { agent: "t3".into(), pane: "p-t3".into() }]);
        // No pane for its agent yet: waits for one.
        assert_eq!(r.step(&live(&[("t6", browser("t9"))]), none), []);
        assert_eq!(r.step(&live(&[("t6", browser("t9"))]), panes), [Ask::Own { agent: "t9".into(), pane: "p-t9".into() }]);
        // Not ready (no cdp yet), the view browser, and remote browsers: never.
        assert_eq!(r.step(&live(&[("t7", json!({"browser-for": "t1"}))]), panes), []);
        assert_eq!(r.step(&live(&[("t8", browser("view"))]), panes), []);
        assert_eq!(r.step(&live(&[("rohan/t1", json!({}))]), panes), []);
        assert_eq!(r.step(&live(&[("rohan/t1", json!({})), ("rohan/t2", browser("t1"))]), panes), []);
        // Ready later: shown then.
        assert_eq!(r.step(&live(&[("t7", browser("t1"))]), panes), [Ask::Own { agent: "t1".into(), pane: "p-t1".into() }]);
    }

    #[test]
    fn view_running_is_local_only() {
        assert!(view_running(&live(&[("t1", json!({"browser-for": "view", "cdp": "127.0.0.1:9"}))])));
        assert!(!view_running(&live(&[("rohan/t1", json!({"browser-for": "view"}))])));
        assert!(!view_running(&live(&[("t1", json!({"browser-for": "t2"}))])));
        let mut done = live(&[("t1", json!({"browser-for": "view"}))]);
        done.get_mut("t1").unwrap()["state"] = json!("finished");
        assert!(!view_running(&done));
    }

    #[test]
    fn herdr_browser_is_found_like_the_core() {
        let exe = std::env::consts::EXE_SUFFIX;
        let home = Path::new("/home/u");
        let got = candidates(Some("/opt/hb".into()), Path::new("/core/bin/hc"), Some(std::env::join_paths(["/a", "/b"]).unwrap()), home, Some("/lad".into()));
        let mut want = vec![PathBuf::from("/opt/hb")];
        want.extend(["/core/bin", "/a", "/b"].map(|d| Path::new(d).join(format!("herdr-browser{exe}"))));
        assert_eq!(got[..4], want[..]);
        if cfg!(windows) {
            assert_eq!(got[4], Path::new("/lad/Programs/herdr-browser").join(format!("herdr-browser{exe}")));
        } else {
            assert_eq!(got[4], home.join(".local/bin/herdr-browser"));
        }
        assert_eq!(got.last().unwrap(), &home.join(".cargo/bin").join(format!("herdr-browser{exe}")));
        // An empty variable and a bare `hc` (no folder) add nothing.
        let got = candidates(Some("".into()), Path::new("hc"), None, home, None);
        assert_eq!(got.len(), 2);
    }
}
