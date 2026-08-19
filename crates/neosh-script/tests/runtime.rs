//! The plugin runtime, end to end.
//!
//! Loads real TypeScript from disk, transpiles it, activates it, and checks that what comes back
//! across the boundary is exactly the protocol the rest of the host expects.

use std::path::PathBuf;

use neosh_proto::{
    ApiCall, ApiOk, ApiResponse, PluginId, PluginInbound, PluginOutbound, PluginRequest,
};
use neosh_script::{ScriptInbound, ScriptOutbound, ScriptRuntime};
use tokio::sync::mpsc::UnboundedReceiver;

fn workspace(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("neosh-rt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_plugin(dir: &PathBuf, source: &str) -> String {
    std::fs::write(dir.join("main.ts"), source).unwrap();
    url::Url::from_file_path(dir.join("main.ts")).unwrap().to_string()
}

/// Wait for a message, failing the test rather than hanging forever.
async fn next(rx: &mut UnboundedReceiver<ScriptOutbound>) -> ScriptOutbound {
    tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
        .await
        .expect("the plugin runtime went quiet")
        .expect("the plugin runtime closed its channel")
}

/// Collect messages until the plugin reports it finished activating, answering every call with
/// `answer`. Returns the API calls the plugin made.
async fn activate(
    rt: &ScriptRuntime,
    rx: &mut UnboundedReceiver<ScriptOutbound>,
    plugin: &str,
    answer: impl Fn(&ApiCall) -> ApiOk,
) -> (Vec<ApiCall>, Option<String>) {
    let mut calls = Vec::new();
    loop {
        match next(rx).await {
            ScriptOutbound::Loaded { error, .. } => return (calls, error),
            ScriptOutbound::Plugin { msg, .. } => match msg {
                PluginOutbound::Call { id, call } => {
                    let value = answer(&call);
                    calls.push(call);
                    rt.send(ScriptInbound::Plugin {
                        plugin: PluginId::from(plugin),
                        msg: PluginInbound::Response { id, response: ApiResponse::Ok { value } },
                    })
                    .unwrap();
                }
                PluginOutbound::Notify { call } => calls.push(call),
                other => panic!("unexpected outbound message {other:?}"),
            },
            ScriptOutbound::Log { level, message } => {
                panic!("runtime error [{level:?}]: {message}")
            }
        }
    }
}

fn load(rt: &ScriptRuntime, plugin: &str, url: String) {
    rt.send(ScriptInbound::Load {
        plugin: PluginId::from(plugin),
        url,
        config: serde_json::json!({}),
        version: neosh_proto::PROTOCOL_VERSION,
    })
    .unwrap();
}

/// A plugin written in TypeScript — with types, which must be erased at load — reaches the host.
#[tokio::test]
async fn a_typescript_plugin_activates_and_calls_the_api() {
    let dir = workspace("activate");
    let url = write_plugin(
        &dir,
        r#"
import type { PluginContext } from "@neosh/api";

interface Greeting { readonly text: string }

export async function activate(ctx: PluginContext): Promise<void> {
    const g: Greeting = { text: "from typescript" };
    const buf = await ctx.neosh.buf.create({ name: "demo" });
    await ctx.neosh.buf.setLines(buf, 0, -1, [g.text]);
    ctx.neosh.log.info("activated");
}
"#,
    );

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "demo", url);

    let (calls, error) = activate(&rt, &mut rx, "demo", |c| match c {
        ApiCall::BufCreate { .. } => ApiOk::Buf { buf: neosh_proto::BufferId(7) },
        _ => ApiOk::Unit,
    })
    .await;

    assert_eq!(error, None, "activation should succeed");
    assert!(matches!(calls[0], ApiCall::BufCreate { .. }));
    match &calls[1] {
        ApiCall::BufSetLines { buf, lines, .. } => {
            assert_eq!(buf.0, 7, "the plugin used the id the host returned");
            assert_eq!(lines, &vec!["from typescript".to_string()]);
        }
        other => panic!("expected buf_set_lines, got {other:?}"),
    }
    assert!(matches!(calls[2], ApiCall::Log { .. }));
    std::fs::remove_dir_all(&dir).ok();
}

/// A tool registered from TypeScript is invoked by the host and its result comes back.
#[tokio::test]
async fn a_plugin_tool_runs_and_returns_a_result() {
    let dir = workspace("tool");
    let url = write_plugin(
        &dir,
        r#"
import type { PluginContext } from "@neosh/api";
export function activate(ctx: PluginContext) {
    ctx.neosh.tool.register(
        { name: "shout", description: "uppercase", inputSchema: { type: "object" } },
        (input) => ({ content: String((input as { text: string }).text).toUpperCase(), is_error: false }),
    );
}
"#,
    );

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "t", url);
    let (calls, error) = activate(&rt, &mut rx, "t", |_| ApiOk::Unit).await;
    assert_eq!(error, None);
    assert!(matches!(calls[0], ApiCall::ToolRegister { .. }));

    rt.send(ScriptInbound::Plugin {
        plugin: PluginId::from("t"),
        msg: PluginInbound::Request {
            id: neosh_proto::RequestId::from("r1"),
            request: PluginRequest::RunTool {
                name: "shout".into(),
                input: serde_json::json!({"text": "hello"}),
            },
        },
    })
    .unwrap();

    match next(&mut rx).await {
        ScriptOutbound::Plugin { msg: PluginOutbound::Response { response, .. }, .. } => {
            match response {
                neosh_proto::PluginResponse::Tool { result } => {
                    assert_eq!(result.content, "HELLO");
                    assert!(!result.is_error);
                }
                other => panic!("expected a tool result, got {other:?}"),
            }
        }
        other => panic!("expected a response, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A blocking hook implemented in TypeScript can veto.
#[tokio::test]
async fn a_plugin_hook_can_veto_across_the_boundary() {
    let dir = workspace("hook");
    let url = write_plugin(
        &dir,
        r#"
import type { PluginContext, HookPayload, HookOutcome } from "@neosh/api";
export function activate(ctx: PluginContext) {
    ctx.neosh.hook.register("tool_pre", (p: HookPayload): HookOutcome => {
        if (p.hook === "tool_pre" && p.call.name === "dangerous") {
            return { action: "veto", reason: "not on my watch" };
        }
        return { action: "continue" };
    }, { blocking: true });
}
"#,
    );

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "policy", url);
    let (calls, error) = activate(&rt, &mut rx, "policy", |_| ApiOk::Unit).await;
    assert_eq!(error, None);
    assert!(
        matches!(&calls[0], ApiCall::HookRegister { blocking: true, .. }),
        "got {:?}",
        calls[0]
    );

    let payload = neosh_proto::HookPayload::ToolPre {
        call: neosh_proto::ToolCall {
            id: neosh_proto::ToolCallId::from("c1"),
            turn: neosh_proto::TurnId::from("t1"),
            name: "dangerous".into(),
            input: serde_json::json!({}),
        },
    };
    rt.send(ScriptInbound::Plugin {
        plugin: PluginId::from("policy"),
        msg: PluginInbound::Request {
            id: neosh_proto::RequestId::from("h1"),
            request: PluginRequest::Hook { hook: neosh_proto::HookName::ToolPre, payload },
        },
    })
    .unwrap();

    match next(&mut rx).await {
        ScriptOutbound::Plugin { msg: PluginOutbound::Response { response, .. }, .. } => {
            match response {
                neosh_proto::PluginResponse::Hook { outcome } => {
                    assert_eq!(outcome, neosh_proto::HookOutcome::Veto {
                        reason: "not on my watch".into()
                    });
                }
                other => panic!("expected a hook outcome, got {other:?}"),
            }
        }
        other => panic!("expected a response, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A plugin that throws during activation is reported, not fatal.
#[tokio::test]
async fn a_throwing_plugin_is_reported_rather_than_taking_the_host_down() {
    let dir = workspace("throws");
    let url = write_plugin(&dir, "export function activate() { throw new Error('boom'); }");

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "bad", url);
    let (_, error) = activate(&rt, &mut rx, "bad", |_| ApiOk::Unit).await;
    let error = error.expect("the failure must be reported");
    assert!(error.contains("boom"), "the actual message should survive: {error}");

    // The runtime keeps working: a second plugin still loads.
    let dir2 = workspace("throws2");
    let url2 = write_plugin(&dir2, "export function activate(ctx) { ctx.neosh.log.info('ok'); }");
    load(&rt, "good", url2);
    let (calls, error) = activate(&rt, &mut rx, "good", |_| ApiOk::Unit).await;
    assert_eq!(error, None);
    assert!(matches!(calls[0], ApiCall::Log { .. }));

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

/// A module with no `activate` export is a clear error rather than a silent no-op.
#[tokio::test]
async fn a_module_without_activate_is_an_error() {
    let dir = workspace("noactivate");
    let url = write_plugin(&dir, "export const nothing = 1;");
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "x", url);
    let (_, error) = activate(&rt, &mut rx, "x", |_| ApiOk::Unit).await;
    assert!(error.unwrap().contains("activate"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Bare specifiers have no resolution here, and must fail at load with an explanation.
#[tokio::test]
async fn importing_an_npm_package_fails_with_a_useful_message() {
    let dir = workspace("bare");
    // The import must be *used*, or the transpiler elides it as a possible type-only import and
    // the module loads fine — which would make this test pass for the wrong reason.
    let url = write_plugin(
        &dir,
        "import _ from 'lodash';\nexport function activate() { console.log(_); }",
    );
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "y", url);
    let (_, error) = activate(&rt, &mut rx, "y", |_| ApiOk::Unit).await;
    let error = error.expect("must fail");
    assert!(error.contains("lodash"), "{error}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Bundled plugins
// ---------------------------------------------------------------------------

/// Every plugin embedded in the binary must have a manifest that parses, a name matching its
/// directory, and an entry that is the file the loader will actually serve.
///
/// Without this, a mistyped `plugin.toml` produces a plugin that silently never loads — the loader
/// skips what it cannot read, which is the right runtime behaviour and the wrong thing to discover
/// from a user.
#[test]
fn every_bundled_plugin_is_well_formed() {
    let plugins = neosh_script::builtin_plugins();
    assert!(!plugins.is_empty(), "the binary should ship with plugins");

    for p in &plugins {
        let manifest: neosh_proto::PluginManifest = toml::from_str(&p.manifest)
            .unwrap_or_else(|e| panic!("{}/plugin.toml does not parse: {e}", p.name));
        assert_eq!(
            manifest.name, p.name,
            "the manifest name must match the directory, because the directory is the plugin id"
        );
        assert_eq!(
            manifest.entry, "main.ts",
            "{}: bundled plugins are a single `main.ts`, because a `neosh:` URL cannot resolve a \
             relative import",
            p.name
        );
        assert_eq!(
            manifest.api_version,
            neosh_proto::PROTOCOL_VERSION,
            "{} targets a protocol this build does not speak",
            p.name
        );
        assert!(!p.entry.is_empty(), "{}: entry source is empty", p.name);
        assert!(
            p.entry.contains("export async function activate")
                || p.entry.contains("export function activate"),
            "{}: no activate export, so nothing would happen when it loads",
            p.name
        );
        assert!(p.url.starts_with("neosh:builtin/"), "{}: unexpected url {}", p.name, p.url);
    }
}

#[test]
fn the_bundled_plugins_are_the_ones_we_expect() {
    // A list rather than a count: silently losing the sidebar from a release is the failure this
    // catches, and a count would still pass if one were swapped for another.
    let names: std::collections::BTreeSet<String> =
        neosh_script::builtin_plugins().into_iter().map(|p| p.name).collect();
    for expected in ["git", "model", "sidebar"] {
        assert!(names.contains(expected), "missing bundled plugin {expected}; have {names:?}");
    }
}
