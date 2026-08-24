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
                PluginOutbound::Call { id, call, .. } => {
                    let value = answer(&call);
                    calls.push(call);
                    rt.send(ScriptInbound::Plugin {
                        plugin: PluginId::from(plugin),
                        msg: PluginInbound::Response { id, response: ApiResponse::Ok { value } },
                    })
                    .unwrap();
                }
                PluginOutbound::Notify { call, .. } => calls.push(call),
                other => panic!("unexpected outbound message {other:?}"),
            },
            ScriptOutbound::Log { level, message } => {
                panic!("runtime error [{level:?}]: {message}")
            }
        }
    }
}

fn load(rt: &ScriptRuntime, plugin: &str, url: String) {
    load_requiring(rt, plugin, url, &[]);
}

fn load_requiring(rt: &ScriptRuntime, plugin: &str, url: String, requires: &[&str]) {
    rt.send(ScriptInbound::Load {
        plugin: PluginId::from(plugin),
        url,
        config: serde_json::json!({}),
        version: neosh_proto::PROTOCOL_VERSION,
        requires: requires.iter().map(|s| s.to_string()).collect(),
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
        assert!(p.url.starts_with("neosh:/builtin/"), "{}: unexpected url {}", p.name, p.url);
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

/// Width is not a character count, and a plugin laying out a column has to know that.
///
/// Driven through the runtime rather than by calling the Rust function, because what is being
/// asserted is that a *plugin* can reach it synchronously: an async round trip per row would make a
/// picker feel like a network service, which is the whole reason these are ops.
///
/// Every string is built from escapes rather than written literally, so the test cannot quietly
/// depend on how this file happens to be Unicode-normalised on disk.
#[tokio::test]
async fn a_plugin_can_measure_and_clip_by_terminal_columns() {
    let dir = workspace("width");
    let url = write_plugin(
        &dir,
        r#"
import { clipToWidth, padToWidth, width } from "@neosh/api";
import type { PluginContext } from "@neosh/api";

const CJK = "日本";        // two characters, four columns
const EMOJI = "👋";      // one character, two columns, two UTF-16 units
const ACCENT = "é";          // two code points, one column

export async function activate(ctx: PluginContext): Promise<void> {
    const show = (s: string) => `${width(s)}/${Array.from(s).length}`;
    const buf = await ctx.neosh.buf.create({ name: "w" });
    await ctx.neosh.buf.setLines(buf, 0, -1, [
        [
            `ascii=${show("abc")}`,
            `cjk=${show(CJK)}`,
            `emoji=${show(EMOJI)}`,
            `accent=${show(ACCENT)}`,
        ].join(" "),
        `clip=${clipToWidth(CJK + "語", 4)}`,
        `pad=[${padToWidth(CJK, 6)}]`,
    ]);
}
"#,
    );

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "w", url);
    let (calls, error) = activate(&rt, &mut rx, "w", |c| match c {
        ApiCall::BufCreate { .. } => ApiOk::Buf { buf: neosh_proto::BufferId(1) },
        _ => ApiOk::Unit,
    })
    .await;
    assert_eq!(error, None);

    let lines = match &calls[1] {
        ApiCall::BufSetLines { lines, .. } => lines.clone(),
        other => panic!("expected buf_set_lines, got {other:?}"),
    };
    assert_eq!(
        lines[0], "ascii=3/3 cjk=4/2 emoji=2/1 accent=1/2",
        "columns and code points are different numbers, and only one of them lays out a column"
    );
    // Four columns is two CJK characters, not two and a half: clipping cuts on a grapheme boundary.
    assert_eq!(lines[1], format!("clip={}", "\u{65e5}\u{672c}"));
    assert_eq!(
        lines[2],
        format!("pad=[{}  ]", "\u{65e5}\u{672c}"),
        "padding counts columns, not characters"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// One plugin imports another's module by name and gets the *same* instance the host activated.
///
/// Two checks in one: the import resolves at all, and the state it sees is the state `activate`
/// set — which is only true if `plugin:lib` is the very module the host loaded rather than a
/// fresh copy of its source.
#[tokio::test]
async fn a_plugin_imports_another_by_name_and_shares_its_state() {
    let lib_dir = workspace("import-lib");
    let lib_url = write_plugin(
        &lib_dir,
        r#"
import type { PluginContext } from "@neosh/api";
export const api = { ready: false, answer: () => 42 };
export async function activate(_ctx: PluginContext): Promise<void> {
    api.ready = true;
}
"#,
    );
    let app_dir = workspace("import-app");
    let app_url = write_plugin(
        &app_dir,
        r#"
import type { PluginContext } from "@neosh/api";
import { api } from "plugin:lib";
export async function activate(ctx: PluginContext): Promise<void> {
    ctx.neosh.log.info(`lib ready=${api.ready} answer=${api.answer()}`);
}
"#,
    );

    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "lib", lib_url);
    load_requiring(&rt, "app", app_url, &["lib"]);

    let (_, error) = activate(&rt, &mut rx, "lib", |_| ApiOk::Unit).await;
    assert_eq!(error, None);
    let (calls, error) = activate(&rt, &mut rx, "app", |_| ApiOk::Unit).await;
    assert_eq!(error, None, "the import should resolve");
    match &calls[0] {
        ApiCall::Log { message, .. } => {
            assert_eq!(message, "lib ready=true answer=42", "same module instance, already activated")
        }
        other => panic!("expected a log, got {other:?}"),
    }
    std::fs::remove_dir_all(&lib_dir).ok();
    std::fs::remove_dir_all(&app_dir).ok();
}

/// Importing a plugin that is not loaded fails with the plugin's name and the fix in the message.
#[tokio::test]
async fn importing_an_unloaded_plugin_names_it() {
    let dir = workspace("import-missing");
    let url = write_plugin(
        &dir,
        r#"
import { api } from "plugin:nothing";
export async function activate(): Promise<void> { api; }
"#,
    );
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "app", url);
    let (_, error) = activate(&rt, &mut rx, "app", |_| ApiOk::Unit).await;
    let error = error.expect("activation fails");
    assert!(error.contains("\"nothing\""), "names the plugin: {error}");
    assert!(error.contains("requires"), "says where the fix goes: {error}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A dependent does not activate until what it requires has — even when the dependency's
/// `activate` is slow — and a failed dependency fails the dependent with its name.
#[tokio::test]
async fn a_dependent_waits_for_its_requirement_to_activate() {
    let lib_dir = workspace("wait-lib");
    let lib_url = write_plugin(
        &lib_dir,
        r#"
import type { PluginContext } from "@neosh/api";
export const state = { activated: false };
export async function activate(ctx: PluginContext): Promise<void> {
    await new Promise((r) => setTimeout(r, 50));
    state.activated = true;
    ctx.neosh.log.info("lib activated");
}
"#,
    );
    let app_dir = workspace("wait-app");
    let app_url = write_plugin(
        &app_dir,
        r#"
import type { PluginContext } from "@neosh/api";
import { state } from "plugin:lib";
export async function activate(ctx: PluginContext): Promise<void> {
    ctx.neosh.log.info(`app sees activated=${state.activated}`);
}
"#,
    );
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "lib", lib_url);
    load_requiring(&rt, "app", app_url, &["lib"]);

    // Both activations are in flight; collect every message until both have reported.
    let mut logs = Vec::new();
    let mut loaded = 0;
    while loaded < 2 {
        match next(&mut rx).await {
            ScriptOutbound::Loaded { error, plugin } => {
                assert_eq!(error, None, "{plugin} should activate");
                loaded += 1;
            }
            ScriptOutbound::Plugin { msg: PluginOutbound::Notify { call: ApiCall::Log { message, .. }, .. }, .. } => {
                logs.push(message)
            }
            ScriptOutbound::Plugin { msg: PluginOutbound::Call { id, .. }, plugin } => {
                rt.send(ScriptInbound::Plugin {
                    plugin,
                    msg: PluginInbound::Response { id, response: ApiResponse::Ok { value: ApiOk::Unit } },
                })
                .unwrap();
            }
            ScriptOutbound::Plugin { .. } => {}
            ScriptOutbound::Log { level, message } => panic!("runtime error [{level:?}]: {message}"),
        }
    }
    assert_eq!(logs, ["lib activated", "app sees activated=true"]);

    let bad_dir = workspace("wait-bad");
    let bad_url = write_plugin(&bad_dir, r#"export async function activate() { throw new Error("nope"); }"#);
    let dep_dir = workspace("wait-dep");
    let dep_url = write_plugin(&dep_dir, r#"export async function activate() {}"#);
    load(&rt, "bad", bad_url);
    load_requiring(&rt, "dep", dep_url, &["bad"]);
    let (_, e1) = activate(&rt, &mut rx, "bad", |_| ApiOk::Unit).await;
    assert!(e1.is_some());
    let (_, e2) = activate(&rt, &mut rx, "dep", |_| ApiOk::Unit).await;
    let e2 = e2.expect("the dependent fails too");
    assert!(e2.contains("\"bad\"") && e2.contains("nope"), "{e2}");
    for d in [lib_dir, app_dir, bad_dir, dep_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// `cmd.call` reaches the handler as a request and carries back what it returned; a handler that
/// throws fails the call with its message.
#[tokio::test]
async fn a_command_called_for_its_answer_returns_one() {
    let dir = workspace("cmd-call");
    let url = write_plugin(
        &dir,
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate(ctx: PluginContext): Promise<void> {
    await ctx.neosh.cmd.register("demo.answer", (args) => ({ got: args, n: 7 }));
    await ctx.neosh.cmd.register("demo.throws", () => { throw new Error("no answer"); });
    await ctx.neosh.cmd.register("demo.nothing", () => {});
}
"#,
    );
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "demo", url);
    let (_, error) = activate(&rt, &mut rx, "demo", |_| ApiOk::Unit).await;
    assert_eq!(error, None);

    async fn ask(
        rt: &ScriptRuntime,
        rx: &mut UnboundedReceiver<ScriptOutbound>,
        name: &str,
    ) -> neosh_proto::PluginResponse {
        let id = neosh_proto::RequestId::new();
        rt.send(ScriptInbound::Plugin {
            plugin: PluginId::from("demo"),
            msg: PluginInbound::Request {
                id: id.clone(),
                request: PluginRequest::Command { name: name.into(), args: vec!["x".into()] },
            },
        })
        .unwrap();
        match next(rx).await {
            ScriptOutbound::Plugin { msg: PluginOutbound::Response { id: back, response }, .. } => {
                assert_eq!(back, id);
                response
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    match ask(&rt, &mut rx, "demo.answer").await {
        neosh_proto::PluginResponse::Command { value } => {
            assert_eq!(value, serde_json::json!({ "got": ["x"], "n": 7 }))
        }
        other => panic!("{other:?}"),
    }
    match ask(&rt, &mut rx, "demo.nothing").await {
        neosh_proto::PluginResponse::Command { value } => assert_eq!(value, serde_json::Value::Null),
        other => panic!("{other:?}"),
    }
    match ask(&rt, &mut rx, "demo.throws").await {
        neosh_proto::PluginResponse::Error { message } => assert!(message.contains("no answer"), "{message}"),
        other => panic!("{other:?}"),
    }
    match ask(&rt, &mut rx, "demo.unknown").await {
        neosh_proto::PluginResponse::Error { message } => assert!(message.contains("demo.unknown"), "{message}"),
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// `deactivate` runs on unload, before the plugin's subscriptions are disposed, and a goodbye
/// that never returns does not hold the unload hostage.
#[tokio::test]
async fn deactivate_runs_on_unload_and_is_bounded() {
    let dir = workspace("deactivate");
    let url = write_plugin(
        &dir,
        r#"
import type { PluginContext } from "@neosh/api";
let ctx: PluginContext;
export async function activate(c: PluginContext): Promise<void> { ctx = c; }
export async function deactivate(): Promise<void> {
    ctx.neosh.log.info("goodbye");
    await new Promise(() => {});   // never settles
}
"#,
    );
    let (rt, mut rx) = ScriptRuntime::spawn();
    load(&rt, "bye", url);
    let (_, error) = activate(&rt, &mut rx, "bye", |_| ApiOk::Unit).await;
    assert_eq!(error, None);

    rt.send(ScriptInbound::Unload { plugin: PluginId::from("bye") }).unwrap();
    match next(&mut rx).await {
        ScriptOutbound::Plugin { msg: PluginOutbound::Notify { call: ApiCall::Log { message, .. }, .. }, .. } => {
            assert_eq!(message, "goodbye")
        }
        other => panic!("expected the goodbye, got {other:?}"),
    }
    // And the runtime is still answering afterwards: the hung deactivate was abandoned.
    let again = write_plugin(&workspace("deactivate-2"), r#"export async function activate() {}"#);
    load(&rt, "next", again);
    let (_, error) = activate(&rt, &mut rx, "next", |_| ApiOk::Unit).await;
    assert_eq!(error, None);
    std::fs::remove_dir_all(&dir).ok();
}
