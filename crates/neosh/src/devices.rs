//! `neosh ls` and `neosh connect` — which computers this workspace can reach, and a terminal for one.
//!
//! # A window is for a computer
//!
//! A swarm puts several machines in one workspace, and the workspace draws all of them to everyone:
//! the right answer for a list, and the wrong one for a window you opened to work on the build box.
//! So a terminal can say which computer it is for — [`ViewDevice`] — and everything that draws or
//! starts conversations reads it. `neosh connect lb` is one terminal on `linux-box`; `neosh` with no
//! name asks, unless `swarm.device` already said.
//!
//! # Settled here, before the screen is taken
//!
//! A name that matches nothing, a machine that has not allowed this one yet, a link that will not
//! come up: each is a sentence, and a sentence belongs in the terminal you typed into rather than
//! drawn on the alternate screen and wiped by the restore on the way out. So the name is resolved
//! and the link waited for over the control socket — the same calls a plugin makes — and only a
//! computer that answered is attached to.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use neosh_proto::{ApiCall, ApiOk, LinkState, NodeId, NodeInfo, SwarmNode, ViewDevice};
use serde::Serialize;

use crate::control::{Control, unwrap_ok};

macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

/// The option a terminal arriving without a name reads. Declared by the `devices` plugin.
pub const OPTION: &str = "swarm.device";

/// How long a workspace this command started is given to reach its peers before the list is drawn.
///
/// A fresh workspace dials every paired machine at once and most answer inside a second; a list
/// drawn before then says `connecting` about machines that are, a moment later, up — which is the
/// one thing a list somebody is about to choose from must not get wrong.
const SETTLE: Duration = Duration::from_secs(4);

/// How long `neosh connect` waits for a link that is being dialled.
const REACH: Duration = Duration::from_secs(15);

/// Everything one look at the workspace says about its computers.
struct Survey {
    me: Option<NodeInfo>,
    peers: Vec<SwarmNode>,
    /// The short code each peer is drawn with, by node id — the sidebar's own answer, so `@lb`
    /// here is `@lb` in the panel.
    codes: BTreeMap<String, String>,
    /// `swarm.device`, or `ask` when nothing has declared it yet.
    default: String,
    /// This computer's conversations: how many, and how many are running.
    local: (usize, usize),
}

async fn survey(c: &mut Control) -> anyhow::Result<Survey> {
    let me = unwrap_ok!(c.call(ApiCall::SwarmSelf).await?, ApiOk::SwarmSelf { node } => node, "swarm_self")?;
    let nodes = unwrap_ok!(c.call(ApiCall::SwarmNodes).await?, ApiOk::SwarmNodes { nodes } => nodes, "swarm_nodes")?;
    let peers: Vec<SwarmNode> =
        nodes.into_iter().filter(|n| Some(&n.info.id) != me.as_ref().map(|m| &m.id)).collect();
    // The plugin's, so they cannot disagree with the panel. A workspace still loading has not
    // registered it, and a list without codes is still a list.
    let codes = match c.call(ApiCall::CmdCall { name: "swarm.codes".into(), args: Vec::new() }).await
    {
        Ok(ApiOk::Json { value }) => serde_json::from_value(value).unwrap_or_default(),
        _ => BTreeMap::new(),
    };
    let default = match c.call(ApiCall::OptGet { name: OPTION.into() }).await {
        Ok(ApiOk::Option { entry: Some(e) }) => match e.value {
            neosh_proto::OptionValue::Str(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => "ask".into(),
        },
        _ => "ask".into(),
    };
    let sessions = unwrap_ok!(
        c.call(ApiCall::SessionList { include_archived: false }).await?,
        ApiOk::Sessions { sessions } => sessions,
        "sessions"
    )?;
    let running = sessions.iter().filter(|s| s.active_turn.is_some()).count();
    Ok(Survey { me, peers, codes, default, local: (sessions.len(), running) })
}

/// Connect, and — when this started the workspace — give its links a moment to come up.
async fn reach(
    socket: &std::path::Path,
    args: &[String],
) -> anyhow::Result<(Control, Survey)> {
    let fresh = Control::existing(socket).await?.is_none();
    let mut c = Control::connect(socket, args).await?;
    let mut s = survey(&mut c).await?;
    if fresh {
        let deadline = Instant::now() + SETTLE;
        // Until nothing is still on its first dial, and the plugin that names machines has loaded.
        while Instant::now() < deadline
            && (s.peers.iter().any(|n| matches!(n.link, LinkState::Connecting { .. }))
                || (!s.peers.is_empty() && s.codes.is_empty()))
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
            s = survey(&mut c).await?;
        }
    }
    Ok((c, s))
}

// ---------------------------------------------------------------------------
// neosh ls
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Row {
    /// The node id; `null` only on a machine whose swarm never started.
    id: Option<String>,
    name: String,
    /// What follows the `@` in the project panel. `null` for this computer.
    code: Option<String>,
    this: bool,
    /// `here`, `connected`, `connecting`, `waiting`, `offline`.
    state: &'static str,
    up: bool,
    os: Option<String>,
    version: Option<String>,
    conversations: usize,
    running: usize,
    /// Whether a terminal opened with no name lands here.
    default: bool,
}

#[derive(Serialize)]
struct Listing {
    /// `swarm.device`, verbatim.
    default: String,
    devices: Vec<Row>,
}

fn state_word(link: &LinkState) -> &'static str {
    match link {
        LinkState::Up => "connected",
        LinkState::Connecting { .. } | LinkState::Retrying { .. } => "connecting",
        LinkState::Waiting => "waiting",
        LinkState::Down => "offline",
    }
}

fn rows(s: &Survey) -> Vec<Row> {
    let default = resolve_quiet(s, &s.default);
    let mut out = vec![Row {
        id: s.me.as_ref().map(|m| m.id.0.clone()),
        name: s.me.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| "this computer".into()),
        code: None,
        this: true,
        state: "here",
        up: true,
        os: s.me.as_ref().map(|m| m.os.clone()),
        version: s.me.as_ref().map(|m| m.version.clone()),
        conversations: s.local.0,
        running: s.local.1,
        default: default == Some(ViewDevice::Here),
    }];
    let mut peers: Vec<&SwarmNode> = s.peers.iter().collect();
    peers.sort_by_key(|a| a.info.name.to_lowercase());
    for n in peers {
        out.push(Row {
            id: Some(n.info.id.0.clone()),
            name: n.info.name.clone(),
            code: s.codes.get(&n.info.id.0).cloned(),
            this: false,
            state: state_word(&n.link),
            up: n.up,
            os: Some(n.info.os.clone()).filter(|o| !o.is_empty()),
            version: Some(n.info.version.clone()).filter(|v| !v.is_empty()),
            conversations: n.agents.len(),
            running: n
                .agents
                .iter()
                .filter(|a| {
                    matches!(a.state, neosh_proto::AgentState::Running | neosh_proto::AgentState::Blocked)
                })
                .count(),
            default: default == Some(ViewDevice::Node { node: n.info.id.clone() }),
        });
    }
    out
}

pub async fn run_ls(
    socket: &std::path::Path,
    args: &[String],
    json: bool,
) -> anyhow::Result<()> {
    let (_c, s) = reach(socket, args).await?;
    let devices = rows(&s);
    if json {
        say!("{}", serde_json::to_string(&Listing { default: s.default.clone(), devices })?);
        return Ok(());
    }

    let name_w = devices.iter().map(|d| d.name.chars().count() + if d.this { 7 } else { 0 }).max().unwrap_or(8).max(8);
    for d in &devices {
        // The default wears the mark, because "which one do I get if I just type `neosh`" is the
        // question the list is most often asked.
        let mark = if d.default { "*" } else { " " };
        let code = match &d.code {
            Some(c) => format!("@{c}"),
            None if d.this => String::new(),
            None => "@?".into(),
        };
        let name = if d.this { format!("{} (here)", d.name) } else { d.name.clone() };
        let state = match d.state {
            "here" => "this computer",
            "waiting" => "not allowed yet",
            other => other,
        };
        let count = match (d.conversations, d.running) {
            (0, _) => "no conversations".to_string(),
            (1, 0) => "1 conversation".to_string(),
            (n, 0) => format!("{n} conversations"),
            (n, r) => format!("{n} conversations, {r} running"),
        };
        say!("{mark} {code:<5} {name:<name_w$}  {state:<16} {count}");
    }
    say!("");
    if s.peers.is_empty() {
        say!("No other computers yet. In neosh, ^J adds one by its address.");
        return Ok(());
    }
    let opens = match resolve_quiet(&s, &s.default) {
        Some(ViewDevice::Here | ViewDevice::All) => "this computer".to_string(),
        Some(ViewDevice::Node { node }) => s
            .peers
            .iter()
            .find(|n| n.info.id == node)
            .map(|n| n.info.name.clone())
            .unwrap_or_else(|| s.default.clone()),
        Some(ViewDevice::Ask) | None => "a question: which computer".to_string(),
    };
    say!("`neosh` opens on {opens} ({OPTION} = \"{}\").", s.default);
    say!("`neosh connect <name|code>` opens a terminal on one; `here` is this one.");
    if s.peers.iter().any(|n| matches!(n.link, LinkState::Waiting)) {
        say!("A computer that has not allowed this one yet: press ^J there and allow it.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolving a name
// ---------------------------------------------------------------------------

/// What `neosh connect <query>` means, or why it means nothing.
fn resolve(s: &Survey, query: &str) -> Result<ViewDevice, String> {
    let q = query.trim();
    let q = q.strip_prefix('@').unwrap_or(q);
    let lower = q.to_lowercase();
    match lower.as_str() {
        "" | "ask" => return Ok(ViewDevice::Ask),
        // A terminal is for one computer. `all` was every machine at once, and a setting or a
        // habit that still says it gets the question rather than an error: it is the nearest thing
        // to "I had not picked one".
        "all" | "*" | "every" | "everything" | "everywhere" => return Ok(ViewDevice::Ask),
        "here" | "local" | "this" | "localhost" | "." => return Ok(ViewDevice::Here),
        _ => {}
    }
    if let Some(me) = &s.me
        && (me.name.eq_ignore_ascii_case(q) || me.id.0 == q)
    {
        return Ok(ViewDevice::Here);
    }
    let node = |n: &SwarmNode| ViewDevice::Node { node: n.info.id.clone() };
    let one = |hits: Vec<&SwarmNode>, how: &str| -> Option<Result<ViewDevice, String>> {
        match hits.as_slice() {
            [] => None,
            [n] => Some(Ok(node(n))),
            many => Some(Err(format!(
                "{} computers {how} \"{q}\": {}. Use the id, or a longer name.",
                many.len(),
                many.iter()
                    .map(|n| format!("{} ({})", n.info.name, &n.info.id.0[..n.info.id.0.len().min(8)]))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    };
    // The most specific reading first: a code is chosen to be unique, a name is not.
    let by_code: Vec<&SwarmNode> = s
        .peers
        .iter()
        .filter(|n| s.codes.get(&n.info.id.0).is_some_and(|c| c.eq_ignore_ascii_case(q)))
        .collect();
    if let Some(r) = one(by_code, "are drawn as") {
        return r;
    }
    let by_name: Vec<&SwarmNode> =
        s.peers.iter().filter(|n| n.info.name.eq_ignore_ascii_case(q)).collect();
    if let Some(r) = one(by_name, "are called") {
        return r;
    }
    if q.len() >= 4 {
        let by_id: Vec<&SwarmNode> = s.peers.iter().filter(|n| n.info.id.0.starts_with(q)).collect();
        if let Some(r) = one(by_id, "have an id starting") {
            return r;
        }
    }
    let by_prefix: Vec<&SwarmNode> =
        s.peers.iter().filter(|n| n.info.name.to_lowercase().starts_with(&lower)).collect();
    if let Some(r) = one(by_prefix, "have a name starting") {
        return r;
    }
    if s.peers.is_empty() {
        return Err(format!(
            "no computer called \"{q}\" — no other computer is paired with this one yet. In neosh, \
             ^J adds one by its address."
        ));
    }
    let known: Vec<String> = s
        .peers
        .iter()
        .map(|n| match s.codes.get(&n.info.id.0) {
            Some(c) => format!("{} (@{c})", n.info.name),
            None => n.info.name.clone(),
        })
        .collect();
    Err(format!(
        "no computer called \"{q}\". Paired: {}, and `here`. `neosh ls` lists them.",
        known.join(", ")
    ))
}

/// [`resolve`], for a value somebody wrote in a config file rather than typed just now.
fn resolve_quiet(s: &Survey, value: &str) -> Option<ViewDevice> {
    resolve(s, value).ok()
}

// ---------------------------------------------------------------------------
// neosh connect
// ---------------------------------------------------------------------------

/// Work out which computer `query` names and, when it is another one, wait for it to be reachable.
///
/// Answers with what to attach with. Everything that would make the terminal useless — a name that
/// matches nothing, a machine that has not allowed this one, a link that never comes up — is an
/// error with the fix in it, said before the screen is taken.
pub async fn connect(
    socket: &std::path::Path,
    args: &[String],
    query: Option<&str>,
) -> anyhow::Result<ViewDevice> {
    let (mut c, s) = reach(socket, args).await?;
    let device = match query {
        None => ViewDevice::Ask,
        Some(q) => resolve(&s, q).map_err(|e| anyhow::anyhow!(e))?,
    };
    let ViewDevice::Node { node } = &device else { return Ok(device) };
    wait_up(&mut c, node).await?;
    Ok(device)
}

async fn wait_up(c: &mut Control, node: &NodeId) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut said = false;
    let mut redialled = false;
    loop {
        let nodes = unwrap_ok!(c.call(ApiCall::SwarmNodes).await?, ApiOk::SwarmNodes { nodes } => nodes, "swarm_nodes")?;
        let Some(n) = nodes.into_iter().find(|n| &n.info.id == node) else {
            anyhow::bail!("that computer is no longer paired with this one");
        };
        let name = n.info.name.clone();
        let why = n.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
        match n.link {
            LinkState::Up => {
                if said {
                    eprintln!("connected.");
                }
                return Ok(());
            }
            LinkState::Waiting => anyhow::bail!(
                "{name} has not allowed this computer yet. On {name}, press ^J in neosh and allow \
                 it — then run this again."
            ),
            // Nobody is dialling it: it dialled us last time, or it was disconnected on purpose.
            // Asking for a terminal on it is asking to connect, so this dials once.
            LinkState::Down if !redialled => {
                redialled = true;
                c.call(ApiCall::SwarmReconnect { node: node.clone() }).await?;
            }
            LinkState::Down => anyhow::bail!(
                "could not reach {name}{why}. Is neosh running there? `neosh connect here` opens \
                 this computer."
            ),
            LinkState::Connecting { .. } | LinkState::Retrying { .. } => {}
        }
        if started.elapsed() > REACH {
            anyhow::bail!(
                "could not reach {name}{why} in {}s. Is neosh running there, and is it on this \
                 network? `neosh connect here` opens this computer.",
                REACH.as_secs()
            );
        }
        if !said && started.elapsed() > Duration::from_millis(400) {
            said = true;
            eprintln!("connecting to {name}…");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::NodeCapabilities;

    fn peer(id: &str, name: &str) -> SwarmNode {
        SwarmNode {
            info: NodeInfo {
                id: NodeId(id.into()),
                name: name.into(),
                os: "linux".into(),
                version: "0.5.1".into(),
            },
            capabilities: NodeCapabilities {
                accepts_commands: true,
                accepts_approvals: false,
                streams: true,
                browse: true,
                shells: false,
                rich_stream: true,
                catalogue: true,
                live: true,
                calls: true,
                projects: Vec::new(),
            },
            up: true,
            link: LinkState::Up,
            reason: None,
            agents: Vec::new(),
        }
    }

    fn survey(peers: Vec<SwarmNode>, codes: &[(&str, &str)]) -> Survey {
        Survey {
            me: Some(NodeInfo {
                id: NodeId("me000000".into()),
                name: "laptop".into(),
                os: "macos".into(),
                version: "0.5.1".into(),
            }),
            peers,
            codes: codes.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            default: "ask".into(),
            local: (0, 0),
        }
    }

    fn node(id: &str) -> ViewDevice {
        ViewDevice::Node { node: NodeId(id.into()) }
    }

    #[test]
    fn a_computer_is_found_by_its_code_its_name_its_id_or_the_start_of_its_name() {
        let s = survey(
            vec![peer("aaaa1111", "linux-box"), peer("bbbb2222", "mac-studio")],
            &[("aaaa1111", "lb"), ("bbbb2222", "ms")],
        );
        assert_eq!(resolve(&s, "lb"), Ok(node("aaaa1111")));
        assert_eq!(resolve(&s, "@MS"), Ok(node("bbbb2222")));
        assert_eq!(resolve(&s, "Linux-Box"), Ok(node("aaaa1111")));
        assert_eq!(resolve(&s, "bbbb22"), Ok(node("bbbb2222")));
        assert_eq!(resolve(&s, "mac"), Ok(node("bbbb2222")));
    }

    #[test]
    fn all_here_and_this_machines_own_name_are_not_other_computers() {
        let s = survey(vec![peer("aaaa1111", "linux-box")], &[]);
        // One computer per terminal: `all` is read as not having picked one, and asks.
        assert_eq!(resolve(&s, "all"), Ok(ViewDevice::Ask));
        assert_eq!(resolve(&s, "here"), Ok(ViewDevice::Here));
        assert_eq!(resolve(&s, "laptop"), Ok(ViewDevice::Here));
        assert_eq!(resolve(&s, ""), Ok(ViewDevice::Ask));
    }

    #[test]
    fn a_name_two_machines_share_is_refused_with_both_of_them_named() {
        let s = survey(vec![peer("aaaa1111", "mac"), peer("bbbb2222", "mac")], &[]);
        let err = resolve(&s, "mac").unwrap_err();
        assert!(err.contains("aaaa1111") && err.contains("bbbb2222"), "{err}");
    }

    #[test]
    fn a_name_that_matches_nothing_says_what_would_have() {
        let s = survey(vec![peer("aaaa1111", "linux-box")], &[("aaaa1111", "lb")]);
        let err = resolve(&s, "build").unwrap_err();
        assert!(err.contains("linux-box (@lb)") && err.contains("neosh ls"), "{err}");
        let alone = survey(Vec::new(), &[]);
        assert!(resolve(&alone, "build").unwrap_err().contains("^J"));
    }
}
