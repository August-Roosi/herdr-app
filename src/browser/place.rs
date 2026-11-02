//! Where `browser.show` puts an agent's browser pane, and which browser
//! panes are on screen. Pure model work, so the window only calls it.

use super::find;
use crate::layout::{Axis, Content, Node};
use crate::model::{Model, Tab};
use serde_json::{Value, json};

/// Where the pane goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Split beside the focused (or given) pane.
    Right,
    Down,
    /// A new tab in the agent's space.
    Tab,
}

impl Place {
    /// `browser.show`'s `place` (default `right`).
    pub fn parse(place: Option<&str>) -> Result<Place, String> {
        match place {
            None | Some("right") => Ok(Place::Right),
            Some("down") => Ok(Place::Down),
            Some("tab") => Ok(Place::Tab),
            Some(other) => Err(format!("browser.show place is \"right\", \"down\" or \"tab\", not {other:?}")),
        }
    }
}

/// Shows `agent`'s browser (`t3`, `rohan/t3`, `base`, `view`) as a pane and
/// focuses it; one it already has is focused instead. The pane goes in a
/// space on the agent's machine: machine documents name agents relative to
/// their machine. The user's own browser (`view`) goes in any space.
/// Returns what `browser.show` answers.
pub fn show(model: &mut Model, agent: &str, place: Place, pane: Option<String>) -> Result<Value, String> {
    let agent = agent.trim();
    let (machine, bare) = find::split(agent);
    if bare.is_empty() {
        return Err("browser.show needs terminal: the agent (t3, rohan/t3), base or view".into());
    }
    let anywhere = agent == find::VIEW;
    let on = |m: &Model, space: &str| m.space(space).is_some_and(|s| anywhere || s.machine.as_deref() == machine);
    // The block's id is its space's machine's, like every block there.
    let machine_of = |m: &Model, space: &str| m.space(space).and_then(|s| s.machine.clone());
    let elsewhere = || format!("{agent}'s browser goes in a space on {}", machine.unwrap_or("this computer"));
    let shown = model.tabs().flat_map(|c| c.root.panes()).find(|n| n.content() == Some(&block_content(agent))).map(|n| n.id.clone());
    if let Some(id) = shown {
        model.focus_pane(&id);
        return Ok(json!({ "pane": id }));
    }
    let block = |id: &str| Node::block(id, block_content(agent));
    match place {
        Place::Right | Place::Down => {
            let target = pane.or_else(|| model.focused_pane()).ok_or("no pane to split")?;
            let Some(space) = model.space_of(&target).map(|s| s.id.clone()) else { return Err(format!("no pane {target:?}")) };
            if !on(model, &space) {
                return Err(elsewhere());
            }
            let on_machine = machine_of(model, &space);
            let id = model.fresh_id_on("p", on_machine.as_deref());
            let axis = if place == Place::Right { Axis::Row } else { Axis::Column };
            model.split_with(&target, axis, block(&id));
            Ok(json!({ "pane": id }))
        }
        Place::Tab => {
            // The agent's space; for base (no pane of its own), the shown one
            // or the first on its machine.
            let space = model
                .pane_of(agent)
                .and_then(|p| model.space_of(&p))
                .map(|s| s.id.clone())
                .or_else(|| model.active.clone().filter(|s| on(model, s)))
                .or_else(|| model.spaces.iter().find(|s| on(model, &s.id)).map(|s| s.id.clone()))
                .ok_or_else(elsewhere)?;
            let on_machine = machine_of(model, &space);
            let (tab, id) = (model.fresh_id_on("tb", on_machine.as_deref()), model.fresh_id_on("p", on_machine.as_deref()));
            model.add_tab(&space, Tab::with_pane(&tab, "browser", block(&id)));
            model.active = Some(space.clone());
            Ok(json!({ "space": space, "tab": tab, "pane": id }))
        }
    }
}

fn block_content(agent: &str) -> Content {
    Content::Browser { browser: agent.to_owned() }
}

/// The browser panes on screen (block id → agent): the layout's own and the
/// shown tab's (`tab`, when the layout shows one).
pub fn visible(chrome: &Node, tab: Option<&Node>) -> Vec<(String, String)> {
    chrome
        .visible_blocks()
        .into_iter()
        .chain(tab.map(Node::visible_blocks).unwrap_or_default())
        .filter_map(|n| match n.content() {
            Some(Content::Browser { browser }) => Some((n.id.clone(), browser.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        let mut m = Model::default();
        m.add_space("s1", "work", "/work", Tab::new("c1", "", "x1", "t1"));
        m
    }

    fn browser(agent: &str) -> Option<Content> {
        Some(Content::Browser { browser: agent.into() })
    }

    #[test]
    fn place_defaults_to_right() {
        assert_eq!(Place::parse(None), Ok(Place::Right));
        assert_eq!(Place::parse(Some("down")), Ok(Place::Down));
        assert_eq!(Place::parse(Some("tab")), Ok(Place::Tab));
        assert!(Place::parse(Some("left")).unwrap_err().contains("\"right\", \"down\" or \"tab\""));
    }

    #[test]
    fn browser_show_splits_right() {
        let mut m = model();
        let r = show(&mut m, "t1", Place::Right, None).unwrap();
        let pane = r["pane"].as_str().unwrap().to_owned();
        let c = m.tab("c1").unwrap();
        // Beside the focused pane, in a row, and focused.
        assert_eq!(c.root.axis(), Some(Axis::Row));
        assert_eq!(c.root.children().iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["x1", pane.as_str()]);
        assert_eq!(c.root.find(&pane).unwrap().content().cloned(), browser("t1"));
        assert_eq!(c.focused.as_deref(), Some(pane.as_str()));
        // Not a terminal: nothing to keep, park or kill.
        assert_eq!(m.address_of(&pane), None);
        assert_eq!(m.all_addresses(), ["t1"]);
    }

    #[test]
    fn browser_show_splits_down_beside_a_given_pane() {
        let mut m = model();
        m.split("x1", Axis::Row, "x2", "t2");
        let r = show(&mut m, "t2", Place::Down, Some("x1".into())).unwrap();
        let pane = r["pane"].as_str().unwrap();
        let c = m.tab("c1").unwrap();
        let parent = c.root.parent_of(pane).unwrap();
        assert_eq!(parent.axis(), Some(Axis::Column));
        assert_eq!(parent.children()[0].id, "x1");
        assert!(show(&mut m, "t3", Place::Right, Some("nope".into())).unwrap_err().contains("nope"));
    }

    #[test]
    fn browser_show_tab_place() {
        let mut m = model();
        m.add_space("s2", "web", "/web", Tab::new("c2", "", "x2", "t2"));
        assert_eq!(m.active.as_deref(), Some("s2"));
        // The agent's own space, not the shown one.
        let r = show(&mut m, "t1", Place::Tab, None).unwrap();
        let (tab, pane) = (r["tab"].as_str().unwrap(), r["pane"].as_str().unwrap());
        assert_eq!(r["space"], json!("s1"));
        assert_eq!(m.space_of(tab).unwrap().id, "s1");
        assert_eq!(m.active.as_deref(), Some("s1"));
        assert_eq!(m.active_tab().unwrap().id, tab);
        assert_eq!(m.tab(tab).unwrap().root.find(pane).unwrap().content().cloned(), browser("t1"));
        // The base browser has no pane of its own: the shown space.
        let r = show(&mut m, "base", Place::Tab, None).unwrap();
        assert_eq!(r["space"], json!("s1"));
    }

    #[test]
    fn showing_again_focuses_the_pane_it_has() {
        let mut m = model();
        let first = show(&mut m, "t1", Place::Right, None).unwrap();
        m.focus_pane("x1");
        let again = show(&mut m, "t1", Place::Tab, None).unwrap();
        assert_eq!(again["pane"], first["pane"]);
        assert_eq!(m.focused_pane().as_deref(), first["pane"].as_str());
        assert_eq!(m.tab("c1").unwrap().root.panes().len(), 2);
    }

    #[test]
    fn a_browser_stays_on_its_agents_machine() {
        let mut m = model();
        // A local space can't hold another machine's browser (machine
        // documents name agents relative to their own machine).
        assert!(show(&mut m, "rohan/t3", Place::Right, None).unwrap_err().contains("rohan"));
        assert!(show(&mut m, "rohan/base", Place::Tab, None).unwrap_err().contains("rohan"));
        assert!(show(&mut m, " ", Place::Right, None).is_err());
        assert_eq!(m.tab("c1").unwrap().root.panes().len(), 1);
    }

    #[test]
    fn the_view_browser_goes_in_any_space() {
        let mut m = model();
        crate::mirror::Document::capture(&m, None).replace(&mut m, Some("rohan"));
        // Beside a remote agent's pane: the block is the remote machine's
        // (its id), the browser stays this screen's own.
        let r = show(&mut m, "view", Place::Right, Some("rohan/x1".into())).unwrap();
        let pane = r["pane"].as_str().unwrap().to_owned();
        assert!(pane.starts_with("rohan/"), "{pane}");
        assert_eq!(m.tab("rohan/c1").unwrap().root.find(&pane).unwrap().content().cloned(), browser("view"));
        // Asked again (another agent, another space): the one it has is focused.
        let again = show(&mut m, "view", Place::Right, Some("x1".into())).unwrap();
        assert_eq!(again["pane"], json!(pane));
        // A tab: in the shown space, wherever that is.
        let mut m = model();
        crate::mirror::Document::capture(&m, None).replace(&mut m, Some("rohan"));
        m.active = Some("rohan/s1".into());
        let r = show(&mut m, "view", Place::Tab, None).unwrap();
        assert_eq!(r["space"], json!("rohan/s1"));
        assert!(r["pane"].as_str().unwrap().starts_with("rohan/"));
    }

    #[test]
    fn only_casts_change_the_page_viewport() {
        assert!(!find::is_cast("view"));
        assert!(find::is_cast("t1") && find::is_cast("rohan/t1") && find::is_cast("base"));
    }

    #[test]
    fn visible_lists_shown_browser_blocks() {
        let mut m = model();
        show(&mut m, "t1", Place::Right, None).unwrap();
        let chrome: Node = serde_json::from_value(json!({ "id": "root", "type": "row", "children": [
            { "id": "main", "type": "block", "content": { "tab": "active" } },
            { "id": "side", "type": "block", "content": { "browser": "base" } },
            { "id": "gone", "type": "block", "content": { "browser": "t9" }, "hidden": true }
        ]}))
        .unwrap();
        let tab = m.active_tab().unwrap().shown();
        let got = visible(&chrome, Some(tab));
        let pane = m.focused_pane().unwrap();
        assert_eq!(got, [("side".to_owned(), "base".to_owned()), (pane, "t1".to_owned())]);
        assert_eq!(visible(&chrome, None), [("side".to_owned(), "base".to_owned())]);
    }
}
