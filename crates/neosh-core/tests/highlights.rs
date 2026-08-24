//! Highlights as a registry with owners, and windows that read group names through a remap.
//!
//! Two things a plugin could not do before: take its colours away when it leaves, and recolour
//! one panel's window without redefining the groups everything else draws with.

use std::collections::BTreeMap;

use neosh_core::{CoreEffect, Editor};
use neosh_proto::{ApiCall, ApiOk, BufferId, HighlightDef, HlTarget, PluginId, UiEvent, WindowLayout};

fn link(to: &str) -> HighlightDef {
    HighlightDef::Link { to: to.into() }
}

fn plugin(name: &str) -> PluginId {
    PluginId::from(name)
}

fn window_highlights(events: &[UiEvent]) -> Vec<(u32, BTreeMap<String, String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::WindowHighlights { win, map } => Some((win.0, map.clone())),
            _ => None,
        })
        .collect()
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn a_plugins_groups_leave_with_it_and_the_theme_comes_back() {
    let mut e = Editor::new();
    e.drain_ui();
    let acme = plugin("acme");
    e.apply(&acme, ApiCall::HlDefine { name: "Comment".into(), def: link("Accent"), default: false })
        .expect("defines");
    e.apply(&acme, ApiCall::HlDefine { name: "Acme.Row".into(), def: link("Normal"), default: false })
        .expect("defines");
    e.drain_ui();
    e.drain_effects();

    e.remove_plugin(&acme);
    let ui = e.drain_ui();
    assert!(
        ui.iter().any(|ev| matches!(ev, UiEvent::HighlightDefined { name, def } if name == "Comment" && *def != link("Accent"))),
        "the theme's Comment is said again: {ui:?}"
    );
    assert!(
        ui.iter().any(|ev| matches!(ev, UiEvent::HighlightCleared { name } if name == "Acme.Row")),
        "a group the theme never had is cleared: {ui:?}"
    );
    let effects = e.drain_effects();
    assert!(
        effects.iter().any(|f| matches!(f, CoreEffect::HighlightsChanged { names } if names.contains(&"Comment".to_string()) && names.contains(&"Acme.Row".to_string()))),
        "and everybody is told which: {effects:?}"
    );
    match e.apply(&acme, ApiCall::HlGet { name: "Acme.Row".into() }).expect("answers") {
        ApiOk::Highlight { def, resolved } => {
            assert_eq!(def, None);
            assert_eq!(resolved, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_default_definition_does_not_take_a_group_somebody_has() {
    let mut e = Editor::new();
    let user = plugin("user");
    let acme = plugin("acme");
    // `init.ts` runs first and says what it wants.
    e.apply(&user, ApiCall::HlDefine { name: "Acme.Row".into(), def: link("Accent"), default: false })
        .expect("defines");
    // The plugin ships its look as a default, so it loses to the user whichever loaded first.
    e.apply(&acme, ApiCall::HlDefine { name: "Acme.Row".into(), def: link("Comment"), default: true })
        .expect("call succeeds");
    match e.apply(&acme, ApiCall::HlList).expect("lists") {
        ApiOk::Highlights { groups } => {
            let row = groups.iter().find(|g| g.name == "Acme.Row").expect("listed");
            assert_eq!(row.def, link("Accent"));
            assert_eq!(row.owner.as_deref(), Some("user"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_the_owner_can_reset_a_group() {
    let mut e = Editor::new();
    let acme = plugin("acme");
    let other = plugin("other");
    e.apply(&acme, ApiCall::HlDefine { name: "Comment".into(), def: link("Accent"), default: false })
        .expect("defines");
    assert!(e.apply(&other, ApiCall::HlReset { name: "Comment".into() }).is_err());
    e.apply(&acme, ApiCall::HlReset { name: "Comment".into() }).expect("own group");
    match e.apply(&acme, ApiCall::HlGet { name: "Comment".into() }).expect("answers") {
        ApiOk::Highlight { def, .. } => assert_ne!(def, Some(link("Accent")), "back on the theme"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_kind_remap_reaches_every_window_of_the_kind_including_later_ones() {
    let mut e = Editor::new();
    let acme = plugin("acme");
    let sidebar = plugin("sidebar");
    let buf = match e
        .apply(&sidebar, ApiCall::BufCreate { name: Some("[sidebar]".into()), scratch: true, kind: Some("neosh.sidebar".into()) })
        .expect("creates")
    {
        ApiOk::Buf { buf } => buf,
        other => panic!("{other:?}"),
    };
    let win = e.open_window(buf, WindowLayout::Docked {
        dock: neosh_proto::Dock::Left,
        size: Some(30),
        gravity: Default::default(),
        wrap: None,
    });
    e.drain_ui();

    e.apply(&acme, ApiCall::WinSetHighlights {
        target: HlTarget::Kind { name: "neosh.sidebar".into() },
        map: map(&[("Normal", "Acme.Panel")]),
    })
    .expect("sets");
    assert_eq!(window_highlights(&e.drain_ui()), vec![(win.0, map(&[("Normal", "Acme.Panel")]))]);

    // A second window of the kind, opened afterwards, is told on its first frame.
    let later = e.open_window(buf, WindowLayout::Float { config: Default::default() });
    assert_eq!(window_highlights(&e.drain_ui()), vec![(later.0, map(&[("Normal", "Acme.Panel")]))]);

    // The window's own remap sits over the kind's.
    e.apply(&acme, ApiCall::WinSetHighlights {
        target: HlTarget::Window { win },
        map: map(&[("Normal", "Acme.Focused")]),
    })
    .expect("sets");
    assert_eq!(window_highlights(&e.drain_ui()), vec![(win.0, map(&[("Normal", "Acme.Focused")]))]);

    // A buffer that *becomes* the kind picks the remap up.
    let plain = match e
        .apply(&sidebar, ApiCall::BufCreate { name: None, scratch: true, kind: None })
        .expect("creates")
    {
        ApiOk::Buf { buf } => buf,
        other => panic!("{other:?}"),
    };
    e.apply(&sidebar, ApiCall::WinSetBuf { win: later, buf: plain }).expect("sets");
    assert_eq!(window_highlights(&e.drain_ui()), vec![(later.0, BTreeMap::new())], "no kind, no remap");
    e.apply(&sidebar, ApiCall::BufSetKind { buf: plain, kind: Some("neosh.sidebar".into()) })
        .expect("sets");
    assert_eq!(window_highlights(&e.drain_ui()), vec![(later.0, map(&[("Normal", "Acme.Panel")]))]);

    // Somebody else cannot take the remap over, and unloading the owner clears it everywhere.
    assert!(
        e.apply(&plugin("other"), ApiCall::WinSetHighlights {
            target: HlTarget::Kind { name: "neosh.sidebar".into() },
            map: map(&[("Normal", "Other")]),
        })
        .is_err()
    );
    e.remove_plugin(&acme);
    let mut cleared = window_highlights(&e.drain_ui());
    cleared.sort();
    assert_eq!(cleared, vec![(win.0, BTreeMap::new()), (later.0, BTreeMap::new())]);

    // And a republish says it again for a frontend that was not there.
    e.apply(&acme, ApiCall::WinSetHighlights {
        target: HlTarget::Window { win },
        map: map(&[("Normal", "Acme.Focused")]),
    })
    .expect("sets");
    e.drain_ui();
    e.republish();
    assert!(window_highlights(&e.drain_ui()).contains(&(win.0, map(&[("Normal", "Acme.Focused")]))));
    let _ = BufferId(0);
}
