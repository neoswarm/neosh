//! Raw-key capture: the primitive a picker, a filter box or any modal widget needs.
//!
//! The invariant worth protecting is the *precedence*. A capture takes what the keymaps did not
//! want, and nothing more. A modal that swallowed every binding would be a trap — you could open a
//! picker and lose the key that quits.

use neosh_core::{CoreEffect, Editor};
use neosh_proto::{
    ApiCall, Dock, Gravity, KeyCode, KeyMods, KeyPress, Mode, PluginId, ViewId, WindowId,
    WindowLayout,
};

fn key(c: &str) -> KeyPress {
    KeyPress { code: KeyCode::Char { c: c.into() }, mods: KeyMods::default() }
}

fn ctrl(c: &str) -> KeyPress {
    KeyPress {
        code: KeyCode::Char { c: c.into() },
        mods: KeyMods { ctrl: true, ..Default::default() },
    }
}

/// An editor with one window focused, and a plugin owning `picker.key`.
fn setup() -> (Editor, PluginId, WindowId) {
    let mut e = Editor::new();
    let plugin = PluginId::from("picker");
    let buf = e.create_buffer("[picker]");
    let win = e.open_window(buf, WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None });
    e.apply(&plugin, ApiCall::CmdRegister { name: "picker.key".into(), desc: None })
        .expect("registers");
    e.apply(&plugin, ApiCall::FocusPush { win }).expect("focuses");
    e.drain_effects();
    (e, plugin, win)
}

fn invoked(effects: &[CoreEffect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|f| match f {
            CoreEffect::InvokeCommand { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

fn unhandled(effects: &[CoreEffect]) -> usize {
    effects.iter().filter(|f| matches!(f, CoreEffect::UnhandledKey { .. })).count()
}

#[test]
fn a_capture_receives_keys_no_binding_wanted() {
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() })
        .expect("captures");

    e.feed_key(ViewId::LOCAL, key("a"));
    let effects = e.drain_effects();
    assert_eq!(invoked(&effects), vec!["picker.key"]);
    assert_eq!(unhandled(&effects), 0, "a captured key must not also be reported as unhandled");
}

#[test]
fn the_captured_key_is_passed_through_so_one_handler_can_switch_on_it() {
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();

    e.feed_key(ViewId::LOCAL, key("z"));
    let effects = e.drain_effects();
    let CoreEffect::InvokeCommand { key: Some(ctx), .. } = &effects[0] else {
        panic!("expected a key context, got {effects:?}");
    };
    assert_eq!(ctx.key.code, KeyCode::Char { c: "z".into() });
    assert_eq!(ctx.win, Some(win));
}

#[test]
fn bindings_still_win_while_a_capture_is_active() {
    // The point of the whole design: opening a picker must not take away `<C-q>`.
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::CmdRegister { name: "quit".into(), desc: None }).unwrap();
    e.apply(&plugin, ApiCall::KeymapSet {
        mode: Mode::Normal,
        lhs: "<C-q>".into(),
        command: "quit".into(),
        scope: None,
        desc: None,
    })
    .unwrap();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, ctrl("q"));
    assert_eq!(invoked(&e.drain_effects()), vec!["quit"], "the binding, not the capture");
}

#[test]
fn without_a_capture_the_key_is_reported_unhandled_as_before() {
    let (mut e, _plugin, _win) = setup();
    e.feed_key(ViewId::LOCAL, key("a"));
    let effects = e.drain_effects();
    assert_eq!(unhandled(&effects), 1);
    assert!(invoked(&effects).is_empty());
}

#[test]
fn a_capture_only_applies_to_the_focused_window() {
    let (mut e, plugin, win) = setup();
    let other = e.create_buffer("[other]");
    let other_win = e.open_window(other, WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None });
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.apply(&plugin, ApiCall::FocusPush { win: other_win }).unwrap();
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("a"));
    let effects = e.drain_effects();
    assert!(invoked(&effects).is_empty(), "another window's capture must not fire");
    assert_eq!(unhandled(&effects), 1);
}

#[test]
fn closing_the_window_releases_the_capture() {
    // Otherwise every unbound key resolves to a command whose picker is gone, and the keyboard
    // appears to stop working with no message explaining why.
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.apply(&plugin, ApiCall::WinClose { win }).unwrap();
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("a"));
    let effects = e.drain_effects();
    assert!(invoked(&effects).is_empty());
}

#[test]
fn a_float_that_closes_on_blur_releases_its_capture_too() {
    let mut e = Editor::new();
    let plugin = PluginId::from("picker");
    let buf = e.create_buffer("[picker]");
    e.apply(&plugin, ApiCall::CmdRegister { name: "picker.key".into(), desc: None }).unwrap();
    let win = e.open_window(buf, WindowLayout::Float {
        config: neosh_proto::FloatConfig {
            close_on_blur: true,
            ..neosh_proto::FloatConfig::default()
        },
    });
    e.apply(&plugin, ApiCall::FocusPush { win }).unwrap();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.apply(&plugin, ApiCall::FocusPop).unwrap();
    e.drain_effects();

    // The window is gone; re-focusing its id must not resurrect the capture.
    e.feed_key(ViewId::LOCAL, key("a"));
    assert!(invoked(&e.drain_effects()).is_empty());
}

#[test]
fn unloading_the_plugin_releases_its_captures() {
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.remove_plugin(&plugin);
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("a"));
    let effects = e.drain_effects();
    assert!(
        invoked(&effects).is_empty(),
        "a capture pointing at a command that no longer exists would report `no such command` on \
         every keystroke"
    );
    assert_eq!(unhandled(&effects), 1, "and the key falls back to normal handling");
}

#[test]
fn releasing_restores_ordinary_routing() {
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.apply(&plugin, ApiCall::KeymapRelease { win }).unwrap();
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("a"));
    assert_eq!(unhandled(&e.drain_effects()), 1);
}

#[test]
fn capturing_on_a_window_that_does_not_exist_is_an_error() {
    // Silently accepting it produces "my picker ignores every key", which is a much harder bug to
    // find than a rejected call.
    let (mut e, plugin, _win) = setup();
    let r = e.apply(&plugin, ApiCall::KeymapCapture {
        win: WindowId(9999),
        command: "picker.key".into(),
    });
    assert!(matches!(r, Err(neosh_proto::ApiError::NotFound { .. })), "got {r:?}");
}

#[test]
fn a_second_capture_on_the_same_window_replaces_the_first() {
    let (mut e, plugin, win) = setup();
    e.apply(&plugin, ApiCall::CmdRegister { name: "second.key".into(), desc: None }).unwrap();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "picker.key".into() }).unwrap();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "second.key".into() }).unwrap();
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("a"));
    assert_eq!(invoked(&e.drain_effects()), vec!["second.key"]);
}

#[test]
fn closing_a_window_takes_the_keys_it_claimed_with_it() {
    // A widget claims its navigation keys window-scoped so they outrank global bindings while it is
    // on screen. Left behind, they are never resolved — a closed window cannot be focused — but
    // they accumulate in the table and in every listing built from it.
    let mut e = Editor::new();
    let plugin = PluginId::from("test");
    let buf = e.create_buffer("[picker]");
    let win = e.open_window(buf, neosh_proto::WindowLayout::Docked { pane: None,
        dock: neosh_proto::Dock::Bottom,
        size: Some(3),
        gravity: Gravity::Start,
        wrap: None,
    });

    e.apply(&plugin, ApiCall::KeymapSet {
        mode: Mode::Chat,
        lhs: "<C-n>".into(),
        command: "picker.key".into(),
        scope: Some(neosh_proto::KeymapScope::Window { win }),
        desc: None,
    })
    .expect("bind");
    // Through the API, because that is what a help screen or a `:map` listing would see.
    let listed = |e: &mut Editor| match e.apply(&plugin, ApiCall::KeymapList { mode: Some(Mode::Chat) }) {
        Ok(neosh_proto::ApiOk::Keymaps { keymaps }) => {
            keymaps.iter().filter(|k| k.command == "picker.key").count()
        }
        other => panic!("expected a keymap listing, got {other:?}"),
    };
    assert_eq!(listed(&mut e), 1, "bound while the window is up");

    e.apply(&plugin, ApiCall::WinClose { win }).expect("close");
    assert_eq!(listed(&mut e), 0, "and gone with it");
}

// ---------------------------------------------------------------------------
// Modal floats
// ---------------------------------------------------------------------------
//
// The other half of the same problem, and the half a capture cannot solve. A widget shadows the
// keys it *wants*; it has no way to name the keys it merely does not want to happen, and there is
// no list it could write down anyway — `^T`, `^G`, `^L` and whatever a plugin binds next week.
// `FloatConfig::modal` is that statement, made once: while this has focus, global bindings do not
// resolve.

/// An editor with a modal float focused, and `quit` bound globally to stand in for `^Q`.
fn modal_setup() -> (Editor, PluginId, WindowId) {
    let mut e = Editor::new();
    let plugin = PluginId::from("panel");
    let buf = e.create_buffer("[panel]");
    let win = e.open_window(buf, WindowLayout::Float {
        config: neosh_proto::FloatConfig { modal: true, ..Default::default() },
    });
    for name in ["quit", "session.new", "panel.key"] {
        e.apply(&plugin, ApiCall::CmdRegister { name: name.into(), desc: None }).expect("registers");
    }
    for (lhs, command) in [("<C-q>", "quit"), ("<C-n>", "session.new")] {
        e.apply(&plugin, ApiCall::KeymapSet {
            mode: Mode::Normal,
            lhs: lhs.into(),
            command: command.into(),
            scope: None,
            desc: None,
        })
        .expect("binds");
    }
    e.apply(&plugin, ApiCall::FocusPush { win }).expect("focuses");
    e.drain_effects();
    (e, plugin, win)
}

#[test]
fn a_modal_takes_the_global_bindings_out_of_reach() {
    let (mut e, _plugin, _win) = modal_setup();
    e.feed_key(ViewId::LOCAL, ctrl("n"));
    let effects = e.drain_effects();
    assert!(invoked(&effects).is_empty(), "`^N` must not start a conversation behind the panel");
}

#[test]
fn a_modal_keeps_its_own_bindings() {
    // Scoped to the window, which is what every widget here does with the keys it wants. Modality
    // is about *global* bindings; it must not cost a panel the keys it bound for itself.
    let (mut e, plugin, win) = modal_setup();
    e.apply(&plugin, ApiCall::KeymapSet {
        mode: Mode::Normal,
        lhs: "<C-n>".into(),
        command: "panel.key".into(),
        scope: Some(neosh_proto::KeymapScope::Window { win }),
        desc: None,
    })
    .expect("binds");
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, ctrl("n"));
    assert_eq!(invoked(&e.drain_effects()), vec!["panel.key"]);
}

#[test]
fn the_escape_hatches_survive_a_modal() {
    // The reason this is safe at all. A panel that failed to bind a way out would otherwise be a
    // terminal somebody has to kill, and "the panel is buggy" must never be the same event as
    // "the program is gone".
    let (mut e, _plugin, _win) = modal_setup();
    e.feed_key(ViewId::LOCAL, ctrl("q"));
    assert_eq!(invoked(&e.drain_effects()), vec!["quit"]);
}

#[test]
fn a_modal_that_binds_an_escape_key_itself_keeps_it() {
    // Nearer scope wins, exactly as it does everywhere else — the escape list is consulted only
    // once the panel's own scopes have declined the key.
    let (mut e, plugin, win) = modal_setup();
    e.apply(&plugin, ApiCall::KeymapSet {
        mode: Mode::Normal,
        lhs: "<C-q>".into(),
        command: "panel.key".into(),
        scope: Some(neosh_proto::KeymapScope::Window { win }),
        desc: None,
    })
    .expect("binds");
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, ctrl("q"));
    assert_eq!(invoked(&e.drain_effects()), vec!["panel.key"]);
}

#[test]
fn an_emptied_escape_list_is_honoured_and_an_undeclared_one_is_not() {
    // Absent means nobody declared the option; empty means somebody said so. Defaulting the first
    // to "no escapes" would make the safety net depend on a declaration having been remembered.
    let (mut e, plugin, _win) = modal_setup();
    e.apply(&plugin, ApiCall::OptDeclare {
        spec: neosh_proto::OptionSpec {
            name: "ui.modal_escape_keys".into(),
            ty: neosh_proto::OptionType::List,
            default: neosh_proto::OptionValue::List(Vec::new()),
            description: None,
        },
    })
    .expect("declares");
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, ctrl("q"));
    assert!(invoked(&e.drain_effects()).is_empty(), "an empty list means no way past the panel");
}

#[test]
fn a_modal_swallows_what_nothing_claimed() {
    // Otherwise the key reaches the host, which puts characters in the composer behind the float
    // and reads `<Esc>` as "interrupt the turn". Typing into a field you cannot see is worse than
    // a key that does nothing.
    let (mut e, _plugin, _win) = modal_setup();
    e.feed_key(ViewId::LOCAL, key("x"));
    let effects = e.drain_effects();
    assert_eq!(unhandled(&effects), 0);
    assert!(invoked(&effects).is_empty());
}

#[test]
fn a_modal_with_a_capture_still_gets_its_raw_keys() {
    // A filter box in a modal panel: the capture is what makes typing work, and modality must not
    // take it away.
    let (mut e, plugin, win) = modal_setup();
    e.apply(&plugin, ApiCall::KeymapCapture { win, command: "panel.key".into() }).expect("captures");
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, key("x"));
    assert_eq!(invoked(&e.drain_effects()), vec!["panel.key"]);
}

#[test]
fn an_ordinary_float_is_unaffected() {
    // `modal` defaults off, and every float that has ever been opened here relies on that.
    let mut e = Editor::new();
    let plugin = PluginId::from("panel");
    let buf = e.create_buffer("[hint]");
    let win = e.open_window(buf, WindowLayout::Float { config: neosh_proto::FloatConfig::default() });
    e.apply(&plugin, ApiCall::CmdRegister { name: "quit".into(), desc: None }).expect("registers");
    e.apply(&plugin, ApiCall::KeymapSet {
        mode: Mode::Normal,
        lhs: "<C-q>".into(),
        command: "quit".into(),
        scope: None,
        desc: None,
    })
    .expect("binds");
    e.apply(&plugin, ApiCall::FocusPush { win }).expect("focuses");
    e.drain_effects();

    e.feed_key(ViewId::LOCAL, ctrl("q"));
    assert_eq!(invoked(&e.drain_effects()), vec!["quit"]);
    e.feed_key(ViewId::LOCAL, key("x"));
    assert_eq!(unhandled(&e.drain_effects()), 1, "still reaches the composer");
}
