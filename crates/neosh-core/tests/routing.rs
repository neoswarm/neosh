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
