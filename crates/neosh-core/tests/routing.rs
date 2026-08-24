//! Which side of the boundary a call lands on.
//!
//! `Editor::handles` is a deny-list: anything not named there is treated as UI-domain and applied
//! in the core. That default is right for the common case — most new calls are buffers, windows and
//! marks — but it means a new *capability* has to be added in two places, and forgetting the second
//! shows up as a runtime error inside a plugin rather than a compile failure.
//!
//! These pin the calls where getting it wrong is least obvious, because they look like ordinary
//! key/value work and are in fact backed by the filesystem, which the core has no access to.

use neosh_proto::ApiCall;
use neosh_core::Editor;

#[test]
fn plugin_state_is_the_hosts_business_not_the_editors() {
    for call in [
        ApiCall::StateGet { key: "favorites".into() },
        ApiCall::StateSet { key: "favorites".into(), value: serde_json::json!(["/a"]) },
        ApiCall::StateDelete { key: "favorites".into() },
    ] {
        assert!(
            !Editor::handles(&call),
            "{call:?} needs a filesystem, which is exactly what the core does not have",
        );
    }
}

#[test]
fn the_editor_says_so_rather_than_silently_doing_nothing_when_a_call_is_misrouted() {
    // The failure mode this guards against is a new capability quietly landing in the core and
    // returning `Unit` — a plugin would then persist nothing and never find out.
    let mut e = Editor::new();
    let plugin = neosh_proto::PluginId::from("test");
    let err = e
        .apply(&plugin, ApiCall::StateGet { key: "k".into() })
        .expect_err("the editor cannot answer this");
    assert!(
        format!("{err:?}").contains("agent-domain"),
        "the error should name the boundary that was crossed: {err:?}",
    );
}

/// Vars and events are the host's too, and for two different reasons worth keeping straight.
///
/// A var is persisted and scoped to a conversation or a project, neither of which the core knows
/// exists. An event is a broadcast to every plugin, and the bridge is what holds them. Both look
/// like key/value work from the call site, which is exactly why they are pinned here.
#[test]
fn shared_vars_and_events_are_the_hosts_business_too() {
    let scope = neosh_proto::VarScope::Project { cwd: "/w/x".into() };
    for call in [
        ApiCall::VarGet { scope: scope.clone(), key: "k".into() },
        ApiCall::VarSet { scope: scope.clone(), key: "k".into(), value: serde_json::json!(1) },
        ApiCall::VarDelete { scope: scope.clone(), key: "k".into() },
        ApiCall::VarAll { scope },
        ApiCall::EventEmit { name: "acme.thing".into(), data: None },
    ] {
        assert!(
            !Editor::handles(&call),
            "{call:?} outlives the core's knowledge or reaches every plugin; either way it is the host's",
        );
    }
}

/// Contributions, by contrast, stay in the core — they are a registration owned by a plugin, the
/// same as a command or a keymap, and they have to die with it in the same place those do.
#[test]
fn contributions_belong_to_the_core_beside_commands_and_keymaps() {
    for call in [
        ApiCall::ExtContribute {
            point: "sidebar.section".into(),
            id: "todo".into(),
            item: serde_json::json!({}),
            priority: 0,
        },
        ApiCall::ExtRemove { point: "sidebar.section".into(), id: "todo".into() },
        ApiCall::ExtList { point: "sidebar.section".into() },
    ] {
        assert!(Editor::handles(&call), "{call:?} is a registration, not an effect");
    }
}

/// A withdrawn plugin's rows go with it.
///
/// The failure this guards against is a row left in somebody's panel invoking a command that no
/// longer exists — which is also what makes `plugins.disabled` mean what it says for a plugin whose
/// only visible effect was a contribution.
#[test]
fn unloading_a_plugin_takes_its_contributions_with_it() {
    let mut e = Editor::new();
    let acme = neosh_proto::PluginId::from("acme");
    let other = neosh_proto::PluginId::from("other");

    for (who, id) in [(&acme, "a"), (&other, "b")] {
        e.apply(who, ApiCall::ExtContribute {
            point: "sidebar.section".into(),
            id: id.into(),
            item: serde_json::json!({ "text": id }),
            priority: 0,
        })
        .expect("contributes");
    }

    let listed = |e: &mut Editor| match e
        .apply(&acme, ApiCall::ExtList { point: "sidebar.section".into() })
        .expect("lists")
    {
        neosh_proto::ApiOk::Contributions { contributions } => contributions,
        other => panic!("expected contributions, got {other:?}"),
    };
    assert_eq!(listed(&mut e).len(), 2);

    e.remove_plugin(&acme);
    let left = listed(&mut e);
    assert_eq!(left.len(), 1, "only the withdrawn plugin's row went");
    assert_eq!(left[0].plugin, "other");
}

/// Contributing twice under one id replaces rather than duplicates, and priority decides the order.
///
/// Both matter to a panel that redraws: a contributor refreshing what it offers must not have to
/// withdraw first, and equal priorities have to break the same way every run or the rows shuffle
/// between restarts.
#[test]
fn a_contribution_is_replaced_by_id_and_ordered_by_priority() {
    let mut e = Editor::new();
    let acme = neosh_proto::PluginId::from("acme");
    let zeta = neosh_proto::PluginId::from("zeta");

    let contribute = |e: &mut Editor, who: &neosh_proto::PluginId, id: &str, n: i64, priority| {
        e.apply(who, ApiCall::ExtContribute {
            point: "p".into(),
            id: id.into(),
            item: serde_json::json!(n),
            priority,
        })
        .expect("contributes");
    };

    contribute(&mut e, &acme, "row", 1, 0);
    contribute(&mut e, &acme, "row", 2, 0);
    contribute(&mut e, &zeta, "row", 3, 0);
    contribute(&mut e, &acme, "top", 4, 10);

    let list = match e.apply(&acme, ApiCall::ExtList { point: "p".into() }).expect("lists") {
        neosh_proto::ApiOk::Contributions { contributions } => contributions,
        other => panic!("expected contributions, got {other:?}"),
    };
    let items: Vec<i64> = list.iter().map(|c| c.item.as_i64().unwrap_or(-1)).collect();
    // 4 first on priority; then acme's `row` — replaced, so 2 rather than 1 — then zeta's, because
    // the tiebreak is plugin id and `acme` sorts before `zeta`.
    assert_eq!(items, vec![4, 2, 3]);
}

/// A key bound on a buffer kind fires in a window the binder never saw, and loses to one bound on
/// that particular buffer.
#[test]
fn a_kind_scoped_binding_reaches_a_panel_it_does_not_own() {
    let mut e = Editor::new();
    let panel = neosh_proto::PluginId::from("panel");
    let acme = neosh_proto::PluginId::from("acme");

    // The panel publishes what it is. This is the only thing it does for anyone else's benefit.
    let buf = match e
        .apply(&panel, ApiCall::BufCreate {
            name: Some("[tasks]".into()),
            scratch: true,
            kind: Some("acme.tasks".into()),
        })
        .expect("creates")
    {
        neosh_proto::ApiOk::Buf { buf } => buf,
        other => panic!("expected a buffer, got {other:?}"),
    };
    let win = match e
        .apply(&panel, ApiCall::WinOpen {
            buf,
            layout: neosh_proto::WindowLayout::Docked {
                dock: neosh_proto::Dock::Left,
                size: Some(30),
                gravity: neosh_proto::Gravity::Start,
                wrap: None,
            },
        })
        .expect("opens")
    {
        neosh_proto::ApiOk::Win { win } => win,
        other => panic!("expected a window, got {other:?}"),
    };

    // A third party binds against the *kind* — it has no way to name `win`, which is the point.
    for (cmd, scope) in [
        ("acme.mine", neosh_proto::KeymapScope::BufKind { name: "acme.tasks".into() }),
        ("acme.global", neosh_proto::KeymapScope::Global),
    ] {
        e.apply(&acme, ApiCall::CmdRegister { name: cmd.into(), desc: None }).expect("registers");
        e.apply(&acme, ApiCall::KeymapSet {
            mode: neosh_proto::Mode::Chat,
            lhs: "d".into(),
            command: cmd.into(),
            scope: Some(scope),
            desc: None,
        })
        .expect("binds");
    }

    e.set_mode(neosh_proto::Mode::Chat);
    e.apply(&panel, ApiCall::FocusPush { win }).expect("focuses");
    let _ = e.drain_effects();
    e.feed_key(neosh_proto::KeyPress {
        code: neosh_proto::KeyCode::Char { c: "d".into() },
        mods: neosh_proto::KeyMods::default(),
    });

    let invoked: Vec<String> = e
        .drain_effects()
        .into_iter()
        .filter_map(|f| match f {
            neosh_core::CoreEffect::InvokeCommand { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(invoked, vec!["acme.mine".to_string()], "the kind binding beat the global one");
}

/// `win.list` is how a plugin finds a panel it did not open, so it has to report the kind.
#[test]
fn windows_are_listed_with_what_is_in_them() {
    let mut e = Editor::new();
    let panel = neosh_proto::PluginId::from("panel");
    let buf = match e
        .apply(&panel, ApiCall::BufCreate {
            name: Some("[tasks]".into()),
            scratch: true,
            kind: Some("acme.tasks".into()),
        })
        .expect("creates")
    {
        neosh_proto::ApiOk::Buf { buf } => buf,
        other => panic!("expected a buffer, got {other:?}"),
    };
    e.apply(&panel, ApiCall::WinOpen {
        buf,
        layout: neosh_proto::WindowLayout::Docked {
            dock: neosh_proto::Dock::Left,
            size: Some(30),
            gravity: neosh_proto::Gravity::Start,
            wrap: None,
        },
    })
    .expect("opens");

    let windows = match e.apply(&panel, ApiCall::WinList).expect("lists") {
        neosh_proto::ApiOk::Windows { windows } => windows,
        other => panic!("expected windows, got {other:?}"),
    };
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].kind.as_deref(), Some("acme.tasks"));
    assert_eq!(windows[0].name, "[tasks]");
}

/// A command *call* waits for a plugin's answer, and the core cannot wait for anything: it applies
/// a call and returns. `CmdExec` stays in the core — it is a key press, queued as an effect — and
/// `CmdCall` goes to the host, which knows how to ask a plugin a question and hold the caller's
/// request open until it is answered.
#[test]
fn calling_a_command_for_its_answer_is_the_hosts_business() {
    let call = ApiCall::CmdCall { name: "sidebar.cursor".into(), args: vec![] };
    assert!(!Editor::handles(&call), "{call:?} has to wait for a plugin, which the core cannot");
    assert!(
        Editor::handles(&ApiCall::CmdExec { name: "sidebar.cursor".into(), args: vec![] }),
        "exec is fire-and-forget and stays an effect",
    );
}

#[test]
fn the_core_says_who_owns_a_command() {
    let mut e = Editor::new();
    let plugin = neosh_proto::PluginId::from("sidebar");
    e.apply(&plugin, ApiCall::CmdRegister { name: "sidebar.cursor".into(), desc: None })
        .expect("registers");
    assert_eq!(e.command_owner("sidebar.cursor"), Some(plugin));
    assert_eq!(e.command_owner("nothing"), None);
}

/// One rule for every name: the user's `init.ts` outranks a plugin, a plugin outranks a bundled
/// default — and a lower tier registering a name a higher one holds is *kept waiting* rather than
/// refused, so a bundled panel's `activate` does not fail over a command the user had already
/// decided to own.
#[test]
fn a_higher_tier_owns_a_command_name_and_gives_it_back() {
    let mut e = Editor::new();
    let user = neosh_proto::PluginId::from("user");
    let sidebar = neosh_proto::PluginId::from("sidebar");
    e.mark_user(&user);
    e.mark_bundled(&sidebar);

    // `init.ts` first, as it loads; the sidebar's registration then succeeds but does not win.
    e.apply(&user, ApiCall::CmdRegister { name: "sidebar.toggle".into(), desc: None }).expect("user");
    e.apply(&sidebar, ApiCall::CmdRegister { name: "sidebar.toggle".into(), desc: None })
        .expect("a default does not fail over a name it was always going to lose");
    assert_eq!(e.command_owner("sidebar.toggle"), Some(user.clone()));

    // The other order — the plugin first, the user later — ends the same way.
    e.apply(&sidebar, ApiCall::CmdRegister { name: "sidebar.focus".into(), desc: None }).expect("sidebar");
    e.apply(&user, ApiCall::CmdRegister { name: "sidebar.focus".into(), desc: None }).expect("user");
    assert_eq!(e.command_owner("sidebar.focus"), Some(user.clone()));

    // Two plugins of one tier: the first keeps it and the second is told.
    let a = neosh_proto::PluginId::from("a");
    let b = neosh_proto::PluginId::from("b");
    e.apply(&a, ApiCall::CmdRegister { name: "shared".into(), desc: None }).expect("a");
    assert!(e.apply(&b, ApiCall::CmdRegister { name: "shared".into(), desc: None }).is_err());

    // When the user lets go, the name goes back to the sidebar rather than to nobody.
    e.apply(&user, ApiCall::CmdUnregister { name: "sidebar.toggle".into() }).expect("unregisters");
    assert_eq!(e.command_owner("sidebar.toggle"), Some(sidebar.clone()));
    e.remove_plugin(&user);
    assert_eq!(e.command_owner("sidebar.focus"), Some(sidebar));
}
