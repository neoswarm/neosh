//! The `claude` CLI's control protocol, which is how it asks a question.
//!
//! # What this is
//!
//! `claude --print` normally has nobody to ask, so anything needing approval is refused. There is a
//! way out, and it is not the documented one: **`--permission-prompt-tool stdio`**, which is hidden
//! from `--help` and is exactly what the Claude Agent SDK passes when its caller supplies a
//! `canUseTool` callback. With it, every permission decision the CLI would have made alone arrives
//! on stdout as a `control_request`, and the CLI blocks until a `control_response` comes back on
//! stdin:
//!
//! ```text
//! →  {"type":"control_request","request_id":"1","request":{"subtype":"can_use_tool",
//!     "tool_name":"Bash","input":{"command":"cargo test"},"tool_use_id":"toolu_…",
//!     "permission_suggestions":[…]}}
//! ←  {"type":"control_response","response":{"subtype":"success","request_id":"1",
//!     "response":{"behavior":"allow"}}}
//! ```
//!
//! It requires `--input-format stream-json` (and therefore `--output-format stream-json` and
//! `--print`), which also means the prompt stops being a positional argument and becomes a message
//! written to stdin. That is the whole cost, and it buys the thing that could not be had otherwise.
//!
//! # Why the suggestions matter
//!
//! The request carries `permission_suggestions`: the exact rules the CLI would write if the user
//! said "and don't ask again". That is what makes *"Yes, and don't ask again for `cargo test`"*
//! possible — it is the CLI's own sentence, about its own rule, and no client-side yes/no can
//! reproduce it. neosh offers them verbatim and hands the chosen one straight back as
//! `updatedPermissions`. See ADR 0032.

use std::collections::HashMap;
use std::path::Path;

use neosh_proto::{Capability, PermissionOption, PermissionOptionKind};
use serde_json::{Value, json};

use crate::approval::{PermissionAnswer, PermissionRequest};

/// The option id for "yes, this once". Fixed rather than generated: it is neosh's own answer, not
/// one of the CLI's, and it has to be recognisable on the way back.
const ALLOW_ONCE: &str = "allow-once";
/// The option id for "no".
const DENY: &str = "deny";

/// One `can_use_tool` request, read into something a person can be asked.
#[derive(Debug, Clone, PartialEq)]
pub struct CanUseTool {
    /// Correlates the answer. Opaque; echoed back untouched.
    pub request_id: String,
    /// Which tool call this is about, echoed back so the CLI can match it even if the turn has
    /// moved on.
    pub tool_use_id: Option<String>,
    pub request: PermissionRequest,
    /// The `updatedPermissions` entry each "always" option would apply, by option id. Kept here
    /// rather than sent out to be answered with, because it is the CLI's own rule object: nothing
    /// outside this module has any business reading it, and nothing should try to build one.
    pub rules: HashMap<String, Value>,
}

/// Read a line from the CLI as a permission request, or `None` if it is not one.
pub fn can_use_tool(v: &Value, cwd: &Path) -> Option<CanUseTool> {
    if v.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let req = v.get("request")?;
    if req.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let request_id = v.get("request_id").and_then(Value::as_str)?.to_string();
    let tool_name = req.get("tool_name").and_then(Value::as_str).unwrap_or("a tool");
    let input = req.get("input").cloned().unwrap_or(Value::Null);

    let mut options = vec![PermissionOption {
        id: ALLOW_ONCE.into(),
        label: "Yes".into(),
        kind: PermissionOptionKind::AllowOnce,
    }];
    let mut rules = HashMap::new();
    // Suppressed when accepting the rule would grant more than the ask itself did — the CLI works
    // that out and says so, and second-guessing it here would be widening a permission on a
    // judgement neosh is not in a position to make.
    let suppress = req
        .get("suppress_always_allow_rule")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !suppress {
        for (i, s) in req
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            // Only the ones that say yes. A suggestion that adds a *deny* rule is a different
            // answer to a different question, and offering it beside "Yes" would be a trap.
            if s.get("behavior").and_then(Value::as_str) != Some("allow") {
                continue;
            }
            let Some(label) = suggestion_label(s) else { continue };
            let id = format!("always-{i}");
            rules.insert(id.clone(), s.clone());
            options.push(PermissionOption {
                id,
                label,
                kind: PermissionOptionKind::AllowAlways,
            });
        }
    }
    options.push(PermissionOption {
        id: DENY.into(),
        label: "No".into(),
        kind: PermissionOptionKind::RejectOnce,
    });

    Some(CanUseTool {
        request_id,
        tool_use_id: req.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
        request: PermissionRequest {
            title: title(req, tool_name, &input),
            capability: capability(tool_name, &input),
            options,
            cwd: cwd.to_path_buf(),
        },
        rules,
    })
}

/// The line the CLI is waiting for.
pub fn response(ask: &CanUseTool, answer: &PermissionAnswer) -> Value {
    let tool_use_id = ask.tool_use_id.clone();
    let mut body = match answer {
        PermissionAnswer::Deny => json!({ "behavior": "deny", "message": "denied" }),
        PermissionAnswer::Allow => json!({ "behavior": "allow" }),
        PermissionAnswer::Option(id) if id == DENY => {
            json!({ "behavior": "deny", "message": "denied" })
        }
        PermissionAnswer::Option(id) => match ask.rules.get(id) {
            // "And don't ask again" is the CLI's rule, applied by the CLI. neosh neither writes it
            // nor interprets it — it hands back the object the CLI offered.
            Some(rule) => json!({ "behavior": "allow", "updatedPermissions": [rule] }),
            // An id from neither the suggestions nor `ALLOW_ONCE` cannot mean "yes, and also this
            // other thing", so it is read as the plain yes it definitely is.
            None => json!({ "behavior": "allow" }),
        },
    };
    // Telemetry the CLI would otherwise infer conservatively. Answering it honestly costs nothing
    // and makes a human allow-once distinguishable from a rule that happened to match.
    if let Some(obj) = body.as_object_mut() {
        let how = match answer {
            PermissionAnswer::Deny => "user_reject",
            PermissionAnswer::Option(id) if id == DENY => "user_reject",
            PermissionAnswer::Option(id) if ask.rules.contains_key(id) => "user_permanent",
            _ => "user_temporary",
        };
        obj.insert("decisionClassification".into(), json!(how));
        if let Some(id) = tool_use_id {
            obj.insert("toolUseID".into(), json!(id));
        }
    }
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": ask.request_id, "response": body },
    })
}

/// One user message, in the shape `--input-format stream-json` expects.
pub fn user_message(content: Value) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

/// The line a person is asked to answer.
///
/// The literal call — `Bash(touch marker.txt && chmod 700 marker.txt)` — and not the request's
/// `description`, which reads better and is the wrong thing. That field is the *model's* account of
/// what its command does ("Create marker.txt and set permissions to 700"), and a permission prompt
/// is the one place a model's account of its own actions must not be what you read. The command is
/// the fact; the description is a claim about it.
///
/// `decision_reason` is appended when the CLI sent one, because that *is* the CLI's own voice: it
/// is why the ask escalated, often a safety check, and it is the part that changes the answer.
fn title(req: &Value, tool: &str, input: &Value) -> String {
    let shown = req
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(tool);
    let call = describe(shown, input);
    match req.get("decision_reason").and_then(Value::as_str).map(sanitize) {
        Some(why) if !why.is_empty() => format!("{call} \u{2014} {why}"),
        _ => call,
    }
}

/// Strip escapes and control characters from producer-authored text.
///
/// The CLI's own schema says `decision_reason` may carry ANSI and must be sanitized before it is
/// rendered. A terminal frontend that pasted it through would let a warning about a dangerous
/// command repaint the prompt asking about it.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI and OSC both end at the first byte outside the parameter range; dropping to the
            // next alphabetic (or the terminator) is enough for text that is only ever a sentence.
            for skip in chars.by_ref() {
                if skip.is_ascii_alphabetic() || skip == '\u{7}' {
                    break;
                }
            }
            continue;
        }
        // A control character becomes a space rather than nothing: dropping the one that
        // separated two words silently joins them, and a prompt is the wrong place to invent a
        // word nobody wrote.
        out.push(if c.is_control() { ' ' } else { c });
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a rule suggestion is offering, in a line a person can choose.
fn suggestion_label(s: &Value) -> Option<String> {
    match s.get("type").and_then(Value::as_str)? {
        "addRules" | "replaceRules" => {
            let rules = s.get("rules").and_then(Value::as_array)?;
            let what: Vec<String> = rules
                .iter()
                .filter_map(|r| {
                    let tool = r.get("toolName").and_then(Value::as_str)?;
                    Some(match r.get("ruleContent").and_then(Value::as_str) {
                        Some(c) if !c.is_empty() => format!("{tool}({c})"),
                        _ => tool.to_string(),
                    })
                })
                .collect();
            if what.is_empty() {
                return None;
            }
            // Where the rule is written is the difference between "for now" and "for good", and it
            // is the part a person most needs to see before clicking.
            let scope = match s.get("destination").and_then(Value::as_str) {
                Some("session") => "for this session",
                Some("localSettings") => "in this project",
                Some("projectSettings") => "for everyone on this project",
                Some("userSettings") => "everywhere",
                _ => "from now on",
            };
            Some(format!("Yes, and don't ask again for {} {scope}", what.join(", ")))
        }
        "addDirectories" => {
            let dirs = s.get("directories").and_then(Value::as_array)?;
            let first = dirs.first().and_then(Value::as_str)?;
            Some(format!("Yes, and allow {first}"))
        }
        // `setMode` and `removeRules` change policy rather than answer this question, and a row
        // that silently switched the permission mode would be the worst kind of surprise.
        _ => None,
    }
}

/// The capability a tool call amounts to, for the permission layer.
///
/// Anything unrecognised is [`Capability::Exec`], which is the strictest of the four: an unknown
/// effect gated as if it were arbitrary execution is a prompt too many, and the alternative is a
/// prompt too few. That includes every MCP tool, whose name is `mcp__server__tool` and whose
/// effects are by definition not knowable from here.
fn capability(tool: &str, input: &Value) -> Capability {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    match tool {
        "Bash" | "BashOutput" | "KillShell" => Capability::Exec {
            command: s("command").unwrap_or_else(|| tool.to_string()),
        },
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => Capability::WriteFile {
            path: s("file_path").or_else(|| s("path")).unwrap_or_else(|| ".".into()),
        },
        "Read" | "Glob" | "Grep" => Capability::ReadFile {
            path: s("file_path").or_else(|| s("path")).unwrap_or_else(|| ".".into()),
        },
        "WebFetch" | "WebSearch" => Capability::Network {
            host: s("url").as_deref().and_then(host_of).unwrap_or_else(|| "the web".into()),
        },
        _ => Capability::Exec { command: describe(tool, input) },
    }
}

/// A one-line summary of a call, for when the CLI sent no description of its own.
fn describe(tool: &str, input: &Value) -> String {
    for key in ["command", "file_path", "path", "url", "pattern", "query"] {
        if let Some(v) = input.get(key).and_then(Value::as_str)
            && !v.is_empty()
        {
            return format!("{tool}({v})");
        }
    }
    tool.to_string()
}

/// The host part of a URL, without pulling in a URL parser for one field.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real-shaped `can_use_tool`, with the two suggestions the CLI offers for a bash call.
    fn ask(extra: Value) -> Value {
        let mut req = json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": { "command": "cargo test -p neosh-core" },
            "tool_use_id": "toolu_01",
            "description": "Run the neosh-core tests",
            "permission_suggestions": [
                { "type": "addRules",
                  "rules": [{ "toolName": "Bash", "ruleContent": "cargo test:*" }],
                  "behavior": "allow", "destination": "session" },
                { "type": "addRules",
                  "rules": [{ "toolName": "Bash", "ruleContent": "cargo test:*" }],
                  "behavior": "allow", "destination": "localSettings" },
            ],
        });
        if let (Some(o), Some(e)) = (req.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        json!({ "type": "control_request", "request_id": "req-1", "request": req })
    }

    fn read(v: &Value) -> CanUseTool {
        can_use_tool(v, Path::new("/w")).expect("a permission request")
    }

    #[test]
    fn only_a_permission_request_is_one() {
        for other in [
            json!({ "type": "stream_event", "event": {} }),
            json!({ "type": "control_request", "request_id": "x",
                    "request": { "subtype": "interrupt" } }),
            json!({ "type": "control_response", "response": {} }),
        ] {
            assert_eq!(can_use_tool(&other, Path::new("/w")), None, "{other}");
        }
    }

    #[test]
    fn the_prompt_shows_the_command_not_the_models_account_of_it() {
        // Captured from a real ask: `description` is the model's own sentence about what its
        // command does, which is the one thing a permission prompt must not put in the user's
        // place of "what will actually run".
        let v = ask(json!({
            "tool_name": "Bash",
            "display_name": "Bash",
            "input": { "command": "touch marker.txt && chmod 700 marker.txt" },
            "description": "Create marker.txt and set permissions to 700",
        }));
        assert_eq!(read(&v).request.title, "Bash(touch marker.txt && chmod 700 marker.txt)");
    }

    #[test]
    fn a_write_says_which_file() {
        let v = ask(json!({
            "tool_name": "Write", "display_name": "Write",
            "input": { "file_path": "/w/hello.txt", "content": "neosh" },
            "description": "hello.txt",
        }));
        assert_eq!(read(&v).request.title, "Write(/w/hello.txt)");
    }

    #[test]
    fn why_the_ask_escalated_rides_along_because_that_is_the_clis_own_voice() {
        let v = ask(json!({ "decision_reason": "rm -rf on a path outside the workspace" }));
        assert_eq!(
            read(&v).request.title,
            "Bash(cargo test -p neosh-core) \u{2014} rm -rf on a path outside the workspace"
        );
    }

    #[test]
    fn escapes_in_producer_text_cannot_repaint_the_prompt() {
        // The CLI's schema says this field may carry ANSI. A frontend that pasted it through would
        // let a warning about a dangerous command rewrite the dialog asking about it.
        let v = ask(json!({ "decision_reason": "\u{1b}[2Jnot\u{1b}[31m dangerous\u{7}at all" }));
        assert_eq!(
            read(&v).request.title,
            "Bash(cargo test -p neosh-core) \u{2014} not dangerous at all"
        );
    }

    #[test]
    fn the_suggestions_become_the_dont_ask_again_rows_in_the_clis_own_words() {
        let got = read(&ask(Value::Null));
        assert_eq!(
            got.request.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
            [
                "Yes",
                "Yes, and don't ask again for Bash(cargo test:*) for this session",
                "Yes, and don't ask again for Bash(cargo test:*) in this project",
                "No",
            ],
            "the scope is the part a person most needs to see before clicking"
        );
        assert_eq!(got.request.options[1].kind, PermissionOptionKind::AllowAlways);
        assert_eq!(got.request.options[3].kind, PermissionOptionKind::RejectOnce);
    }

    #[test]
    fn a_rule_that_would_grant_more_than_the_ask_is_not_offered() {
        let got = read(&ask(json!({ "suppress_always_allow_rule": true })));
        assert_eq!(
            got.request.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
            ["Yes", "No"],
        );
    }

    #[test]
    fn a_suggestion_that_denies_is_never_offered_beside_yes() {
        let v = ask(json!({ "permission_suggestions": [
            { "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "rm:*" }],
              "behavior": "deny", "destination": "session" },
        ] }));
        let got = read(&v);
        assert_eq!(got.request.options.len(), 2, "{:?}", got.request.options);
    }

    #[test]
    fn a_suggestion_that_changes_policy_rather_than_answering_is_not_a_row() {
        // "setMode" would switch the permission mode as a side effect of saying yes once.
        let v = ask(json!({ "permission_suggestions": [
            { "type": "setMode", "mode": "acceptEdits", "destination": "session" },
        ] }));
        assert_eq!(read(&v).request.options.len(), 2);
    }

    #[test]
    fn the_tool_becomes_the_capability_policy_understands() {
        let at = |tool: &str, input: Value| {
            let v = ask(json!({ "tool_name": tool, "input": input }));
            read(&v).request.capability
        };
        assert_eq!(
            at("Bash", json!({ "command": "ls" })),
            Capability::Exec { command: "ls".into() }
        );
        assert_eq!(
            at("Edit", json!({ "file_path": "src/main.rs" })),
            Capability::WriteFile { path: "src/main.rs".into() }
        );
        assert_eq!(
            at("Read", json!({ "file_path": "a.txt" })),
            Capability::ReadFile { path: "a.txt".into() }
        );
        assert_eq!(
            at("WebFetch", json!({ "url": "https://example.com:443/x" })),
            Capability::Network { host: "example.com".into() }
        );
    }

    #[test]
    fn an_mcp_tool_is_gated_as_if_it_were_arbitrary_execution() {
        // Its effects are not knowable from here, and the strictest reading is the safe one.
        let v = ask(json!({ "tool_name": "mcp__github__create_pr", "input": { "title": "x" } }));
        assert!(matches!(read(&v).request.capability, Capability::Exec { .. }));
    }

    #[test]
    fn a_plain_yes_says_allow_and_writes_no_rule() {
        let got = read(&ask(Value::Null));
        let r = response(&got, &PermissionAnswer::Option("allow-once".into()));
        assert_eq!(r["response"]["request_id"], "req-1");
        assert_eq!(r["response"]["response"]["behavior"], "allow");
        assert!(r["response"]["response"].get("updatedPermissions").is_none());
        assert_eq!(r["response"]["response"]["toolUseID"], "toolu_01");
        assert_eq!(r["response"]["response"]["decisionClassification"], "user_temporary");
    }

    #[test]
    fn dont_ask_again_hands_back_the_clis_own_rule_untouched() {
        let got = read(&ask(Value::Null));
        let id = got.request.options[1].id.clone();
        let r = response(&got, &PermissionAnswer::Option(id));
        let applied = &r["response"]["response"]["updatedPermissions"][0];
        assert_eq!(applied["type"], "addRules");
        assert_eq!(applied["destination"], "session");
        assert_eq!(applied["rules"][0]["ruleContent"], "cargo test:*");
        assert_eq!(r["response"]["response"]["decisionClassification"], "user_permanent");
    }

    #[test]
    fn no_is_a_deny_with_a_message_the_model_can_act_on() {
        let got = read(&ask(Value::Null));
        for answer in
            [PermissionAnswer::Deny, PermissionAnswer::Option("deny".into())]
        {
            let r = response(&got, &answer);
            assert_eq!(r["response"]["response"]["behavior"], "deny", "{answer:?}");
            assert!(r["response"]["response"]["message"].is_string(), "{answer:?}");
            assert_eq!(r["response"]["response"]["decisionClassification"], "user_reject");
        }
    }

    #[test]
    fn a_yes_naming_no_option_is_still_a_yes() {
        // Policy answers this way: "full access" is a yes to the question, not a choice between
        // the CLI's three ways of saying yes.
        let got = read(&ask(Value::Null));
        let r = response(&got, &PermissionAnswer::Allow);
        assert_eq!(r["response"]["response"]["behavior"], "allow");
        assert!(r["response"]["response"].get("updatedPermissions").is_none());
    }

    #[test]
    fn an_option_id_from_nowhere_is_read_as_the_plain_yes_it_is() {
        let got = read(&ask(Value::Null));
        let r = response(&got, &PermissionAnswer::Option("always-99".into()));
        assert_eq!(r["response"]["response"]["behavior"], "allow");
        assert!(r["response"]["response"].get("updatedPermissions").is_none());
    }

    #[test]
    fn the_prompt_goes_in_as_a_message() {
        let m = user_message("what is in src".into());
        assert_eq!(m["type"], "user");
        assert_eq!(m["message"]["role"], "user");
        assert_eq!(m["message"]["content"], "what is in src");
        assert!(m.get("parent_tool_use_id").is_some(), "the CLI reads this key even when null");
    }
}
