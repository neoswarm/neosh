//! The host's half of ASCP: what the other machines are running, and how to ask them things.
//!
//! [`neosh_swarm`] owns sockets and signatures. This owns the *picture* — which peers are up, what
//! each of them has, and which project on this machine is the same project as one over there — and
//! translates between a conversation as the session store knows it and an
//! [`AgentSummary`](neosh_proto::AgentSummary) as the wire does.
//!
//! Kept apart from `host.rs` because it is the one part of the workspace that is *about* other
//! workspaces, and because everything here has to survive a peer that is lying, stale or gone.

use std::collections::HashMap;

use neosh_proto::{
    AgentState, AgentSummary, NodeCapabilities, NodeId, NodeInfo, ProjectKey, SessionId, SessionInfo,
};

/// One peer and everything it has told us.
#[derive(Debug, Clone)]
pub struct Remote {
    pub info: NodeInfo,
    pub capabilities: NodeCapabilities,
    /// Its agents, most recently updated first.
    pub agents: Vec<AgentSummary>,
    /// `false` once it has gone. The row stays rather than vanishing: a machine you were working
    /// with going quiet is information, and a list that silently shortens is one you cannot tell
    /// from a list that never had it.
    pub up: bool,
    /// Why it went, when it did.
    pub reason: Option<String>,
}

/// Every node this one knows about, and what it has.
#[derive(Debug, Default)]
pub struct Swarm {
    peers: HashMap<NodeId, Remote>,
    /// The projects this machine has, by key, so a local row can say which peers share it.
    local_projects: HashMap<ProjectKey, String>,
}

impl Swarm {
    pub fn peer_up(&mut self, info: NodeInfo, capabilities: NodeCapabilities) {
        let entry = self.peers.entry(info.id.clone()).or_insert_with(|| Remote {
            info: info.clone(),
            capabilities: capabilities.clone(),
            agents: Vec::new(),
            up: true,
            reason: None,
        });
        entry.info = info;
        entry.capabilities = capabilities;
        entry.up = true;
        entry.reason = None;
    }

    pub fn peer_down(&mut self, node: &NodeId, reason: String) {
        if let Some(p) = self.peers.get_mut(node) {
            p.up = false;
            p.reason = Some(reason);
            // Its agents are *not* cleared. They were real a moment ago and are probably still
            // running; what changed is that we can no longer see them. A board that empties a
            // machine's rows the instant a wifi hiccup drops a connection is one that makes you
            // think work was lost.
        }
    }

    /// Take in what a peer says it is running.
    pub fn inventory(
        &mut self,
        node: &NodeId,
        full: bool,
        agents: Vec<AgentSummary>,
        gone: Vec<SessionId>,
    ) {
        let Some(p) = self.peers.get_mut(node) else {
            // Inventory from a node we have no record of. Only possible if the peer went down
            // between the message arriving and this running; dropping it is right, because the
            // reconnect brings a full snapshot.
            return;
        };
        if full {
            p.agents = agents;
        } else {
            p.agents.retain(|a| !gone.contains(&a.session));
            for a in agents {
                match p.agents.iter_mut().find(|m| m.session == a.session) {
                    Some(m) => *m = a,
                    None => p.agents.push(a),
                }
            }
        }
        p.agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }

    pub fn peers(&self) -> impl Iterator<Item = &Remote> {
        self.peers.values()
    }

    pub fn peer(&self, node: &NodeId) -> Option<&Remote> {
        self.peers.get(node)
    }

    /// Every agent on every peer that is still up.
    ///
    /// A peer that has gone keeps its rows in [`Self::peers`] — where the fact that it is down is
    /// visible next to them — but is left out here, because this is what a list of things you can
    /// act on is built from and you cannot act on those.
    pub fn agents(&self) -> impl Iterator<Item = (&NodeInfo, &AgentSummary)> {
        self.peers.values().filter(|p| p.up).flat_map(|p| p.agents.iter().map(move |a| (&p.info, a)))
    }

    pub fn find(&self, node: &NodeId, session: &SessionId) -> Option<&AgentSummary> {
        self.peers.get(node)?.agents.iter().find(|a| &a.session == session)
    }

    /// Tell the swarm which projects this machine has, so it can answer [`Self::hosts_of`].
    pub fn set_local_projects(&mut self, projects: HashMap<ProjectKey, String>) {
        self.local_projects = projects;
    }

    pub fn is_local_project(&self, key: &ProjectKey) -> bool {
        self.local_projects.contains_key(key)
    }

    /// Which *other* machines have this project, by name.
    ///
    /// The question the sidebar asks to put `mac-studio · linux-box` beside a project row. Only
    /// peers that are up, and only projects they have a conversation in — a machine that has the
    /// directory but has never opened it there is not somewhere the project is "on" in the sense a
    /// person means.
    pub fn hosts_of(&self, key: &ProjectKey) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .peers
            .values()
            .filter(|p| p.up && p.agents.iter().any(|a| &a.project == key))
            .map(|p| p.info.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// A local conversation, as the wire describes it.
///
/// `project` is the cross-machine key rather than the path, which is the whole reason two machines
/// can agree they have the same project. `cwd` goes across as well, but only so the other end can
/// *show* it and hand it back in a `NewSession` — it is a path in this machine's filesystem and
/// means nothing over there.
pub fn summarise(info: &SessionInfo, project: ProjectKey, model: Option<String>) -> AgentSummary {
    AgentSummary {
        session: info.id.clone(),
        project,
        project_name: info.project.clone(),
        cwd: info.cwd.clone(),
        label: info.label.clone(),
        state: state_of(info),
        message_count: info.message_count,
        model,
        turn_started_at: info.turn_started_at,
        updated_at: info.updated_at,
        usage: info.usage.clone(),
    }
}

/// What a conversation looks like from outside.
///
/// `Blocked` is not derivable from `SessionInfo` — the host knows about a pending approval and the
/// store does not — so it is applied by the caller afterwards. Coarse on purpose: this is sent on
/// every change, and a state with more cases than a person can act on is bytes for nothing.
fn state_of(info: &SessionInfo) -> AgentState {
    if info.active_turn.is_some() { AgentState::Running } else { AgentState::Idle }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str) -> NodeInfo {
        NodeInfo {
            id: NodeId(id.into()),
            name: name.into(),
            os: "test".into(),
            version: "0.1.0".into(),
        }
    }

    fn caps() -> NodeCapabilities {
        NodeCapabilities {
            accepts_commands: true,
            accepts_approvals: false,
            streams: true,
            projects: Vec::new(),
        }
    }

    fn agent(session: &str, project: &str, updated: i64) -> AgentSummary {
        AgentSummary {
            session: SessionId(session.into()),
            project: ProjectKey(project.into()),
            project_name: project.into(),
            cwd: format!("/w/{session}"),
            label: session.into(),
            state: AgentState::Idle,
            message_count: 0,
            model: None,
            turn_started_at: None,
            updated_at: updated,
            usage: Default::default(),
        }
    }

    fn with_two() -> Swarm {
        let mut s = Swarm::default();
        s.peer_up(node("aa", "mac-studio"), caps());
        s.peer_up(node("bb", "linux-box"), caps());
        s.inventory(
            &NodeId("aa".into()),
            true,
            vec![agent("s1", "git:x/neosh", 10), agent("s2", "git:x/api", 20)],
            vec![],
        );
        s.inventory(&NodeId("bb".into()), true, vec![agent("s3", "git:x/neosh", 30)], vec![]);
        s
    }

    #[test]
    fn a_full_inventory_replaces_and_sorts_by_recency() {
        let s = with_two();
        let a = s.peer(&NodeId("aa".into())).expect("peer");
        assert_eq!(a.agents[0].session, SessionId("s2".into()), "most recent first");
    }

    #[test]
    fn a_delta_adds_updates_and_removes() {
        let mut s = with_two();
        let mut renamed = agent("s1", "git:x/neosh", 40);
        renamed.label = "renamed".into();
        s.inventory(
            &NodeId("aa".into()),
            false,
            vec![renamed, agent("s9", "git:x/new", 50)],
            vec![SessionId("s2".into())],
        );
        let a = s.peer(&NodeId("aa".into())).expect("peer");
        let labels: Vec<&str> = a.agents.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["s9", "renamed"], "s2 gone, s1 updated, s9 added, newest first");
    }

    /// The question behind "which computers is this project on".
    #[test]
    fn a_project_says_which_other_machines_have_it() {
        let s = with_two();
        assert_eq!(s.hosts_of(&ProjectKey("git:x/neosh".into())), vec!["linux-box", "mac-studio"]);
        assert_eq!(s.hosts_of(&ProjectKey("git:x/api".into())), vec!["mac-studio"]);
        assert!(s.hosts_of(&ProjectKey("git:x/nowhere".into())).is_empty());
    }

    /// A machine that has gone quiet keeps its rows — they were real a moment ago and are probably
    /// still running. What changed is that we cannot see them.
    #[test]
    fn a_peer_that_goes_down_keeps_its_rows_but_leaves_the_actionable_list() {
        let mut s = with_two();
        s.peer_down(&NodeId("bb".into()), "connection closed".into());

        let bb = s.peer(&NodeId("bb".into())).expect("still listed");
        assert!(!bb.up);
        assert_eq!(bb.reason.as_deref(), Some("connection closed"));
        assert_eq!(bb.agents.len(), 1, "and its conversations are still described");

        let live: Vec<&str> = s.agents().map(|(_, a)| a.label.as_str()).collect();
        assert_eq!(live.len(), 2, "but they are not things you can act on");
        assert!(!live.contains(&"s3"));
        // And it stops counting towards "this project is on these machines".
        assert_eq!(s.hosts_of(&ProjectKey("git:x/neosh".into())), vec!["mac-studio"]);
    }

    /// Coming back is not a fresh peer: the row is the same one, now up again.
    #[test]
    fn a_peer_that_comes_back_is_the_same_row() {
        let mut s = with_two();
        s.peer_down(&NodeId("bb".into()), "gone".into());
        s.peer_up(node("bb", "linux-box"), caps());
        let bb = s.peer(&NodeId("bb".into())).expect("peer");
        assert!(bb.up);
        assert_eq!(bb.reason, None, "and it is no longer explaining itself");
    }

    /// Inventory from a node we have no record of is dropped rather than inventing a peer with no
    /// name, no capabilities and no way to be talked to.
    #[test]
    fn inventory_from_an_unknown_node_is_dropped() {
        let mut s = Swarm::default();
        s.inventory(&NodeId("zz".into()), true, vec![agent("s1", "p", 1)], vec![]);
        assert_eq!(s.peers().count(), 0);
    }
}
