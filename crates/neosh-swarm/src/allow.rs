//! Which machines this one will talk to, and where they live.
//!
//! Split out of [`Identity`](crate::Identity) because the two have different lifetimes. A keypair is
//! decided once and never changes; the list of machines you work with changes while you are sitting
//! there, and a swarm you have to restart to add a computer to is a swarm you add one computer to.
//!
//! Shared by the node loop and every connection task, behind a read-write lock that is taken for
//! reading on each handshake and for writing when somebody pairs. Contention is not a consideration
//! at these numbers — the interesting property is that a peer added now is authorised for the next
//! connection attempt without anything being torn down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use neosh_proto::NodeId;
use serde::{Deserialize, Serialize};

use crate::SwarmError;

/// One authorised machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Peer {
    /// What to call it until it says its own name.
    #[serde(default)]
    pub name: String,
    /// What *this* machine's user calls it, whatever it calls itself.
    ///
    /// Separate from `name` rather than overwriting it, because the two answer different
    /// questions: `name` is the best guess available before the machine has spoken, and this is a
    /// decision somebody made. Keeping them apart is what lets a rename be undone — clearing this
    /// gives the announced name back — and what stops a handshake, which refreshes `name`, from
    /// quietly reverting a choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// `host:port`, when we know where to find it.
    ///
    /// Absent for a machine that dialled *us* and was approved: it knows where we are, and the
    /// address it happened to arrive from is not necessarily one we could dial back on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// Where it came from, so the UI can say what it may change.
    #[serde(default)]
    pub source: Source,
}

/// Whether a peer was written by hand or added by pairing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// From `[[swarm.peers]]` in the user's config. Not ours to remove — a UI that deleted a line
    /// somebody wrote by hand, and then had it come back on the next reload, would be worse than
    /// one that says it cannot.
    Config,
    /// Added by pairing, and kept in the state directory.
    #[default]
    Paired,
}

/// The list, shared and changeable.
#[derive(Clone, Default)]
pub struct Allowed {
    inner: Arc<RwLock<BTreeMap<NodeId, Peer>>>,
    /// Where paired machines are remembered. `None` under `--clean`.
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Allowed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allowed").field("peers", &self.list().len()).finish()
    }
}

impl Allowed {
    /// Load whatever pairing has remembered. A missing or unreadable file is an empty list rather
    /// than a failure to start: the config's own peers still work, and refusing to open a workspace
    /// because a cache is corrupt is the wrong trade.
    pub fn load(state_dir: Option<&Path>) -> Self {
        let path = state_dir.map(|d| d.join("swarm").join("peers.json"));
        let mut inner: BTreeMap<NodeId, Peer> = BTreeMap::new();
        if let Some(p) = &path
            && let Ok(bytes) = std::fs::read(p)
        {
            match serde_json::from_slice::<BTreeMap<NodeId, Peer>>(&bytes) {
                Ok(loaded) => inner = loaded,
                Err(e) => tracing::warn!(?p, "swarm peer store is unreadable, starting empty: {e}"),
            }
        }
        Self { inner: Arc::new(RwLock::new(inner)), path }
    }

    pub fn accepts(&self, id: &NodeId) -> bool {
        self.inner.read().is_ok_and(|m| m.contains_key(id))
    }

    pub fn get(&self, id: &NodeId) -> Option<Peer> {
        self.inner.read().ok()?.get(id).cloned()
    }

    pub fn list(&self) -> Vec<(NodeId, Peer)> {
        self.inner
            .read()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Authorise a machine. Replaces what was there under the same id.
    pub fn add(&self, id: NodeId, peer: Peer) {
        if let Ok(mut m) = self.inner.write() {
            m.insert(id, peer);
        }
        self.save();
    }

    /// Withdraw authorisation. Answers whether there was anything to withdraw, and refuses to
    /// touch a peer the config declared — that line would simply come back on the next reload, and
    /// a removal that silently undoes itself is worse than one that says no.
    pub fn remove(&self, id: &NodeId) -> Result<bool, SwarmError> {
        let removed = {
            let Ok(mut m) = self.inner.write() else { return Ok(false) };
            match m.get(id) {
                Some(p) if p.source == Source::Config => {
                    return Err(SwarmError::Protocol(
                        "that machine is in your config file; remove the `[[swarm.peers]]` entry"
                            .into(),
                    ));
                }
                Some(_) => m.remove(id).is_some(),
                None => false,
            }
        };
        if removed {
            self.save();
        }
        Ok(removed)
    }

    /// Give a machine the name this user calls it, or `None` to go back to the one it announces.
    ///
    /// Answers whether there was a peer to rename. Refused for a config-declared one for the same
    /// reason [`Allowed::remove`] is: only paired peers are written to the state file, so an alias
    /// set here would survive until the next reload and then silently revert — and the config's
    /// `name` field is one line away.
    pub fn rename(&self, id: &NodeId, alias: Option<String>) -> Result<bool, SwarmError> {
        let renamed = {
            let Ok(mut m) = self.inner.write() else { return Ok(false) };
            match m.get_mut(id) {
                Some(p) if p.source == Source::Config => {
                    return Err(SwarmError::Protocol(
                        "that machine is in your config file; change the `name` on its \
                         `[[swarm.peers]]` entry"
                            .into(),
                    ));
                }
                Some(p) => {
                    // An empty string is somebody clearing the field rather than choosing to call
                    // a machine nothing, and a row with no name is one you cannot pick out of a
                    // list. Both spellings of "undo this" arrive here.
                    p.alias = alias.filter(|a| !a.trim().is_empty()).map(|a| a.trim().to_string());
                    true
                }
                None => false,
            }
        };
        if renamed {
            self.save();
        }
        Ok(renamed)
    }

    /// Only the paired ones are written. Config peers belong to the config, and writing them here
    /// would mean a machine removed from `config.toml` staying authorised for ever.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(m) = self.inner.read() else { return };
        let paired: BTreeMap<&NodeId, &Peer> =
            m.iter().filter(|(_, p)| p.source == Source::Paired).collect();
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_vec_pretty(&paired)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            tracing::warn!("could not save swarm peers: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired(name: &str) -> Peer {
        Peer { name: name.into(), alias: None, addr: None, source: Source::Paired }
    }

    #[test]
    fn a_machine_is_refused_until_it_is_added_and_again_once_removed() {
        let a = Allowed::load(None);
        let id = NodeId("aa".into());
        assert!(!a.accepts(&id));
        a.add(id.clone(), paired("linux-box"));
        assert!(a.accepts(&id));
        assert_eq!(a.remove(&id).expect("removable"), true);
        assert!(!a.accepts(&id));
    }

    /// The change has to be visible to the connection tasks holding a clone, or pairing would need
    /// a restart — which is the whole thing this type exists to avoid.
    #[test]
    fn a_clone_sees_a_peer_added_after_it_was_cloned() {
        let a = Allowed::load(None);
        let held = a.clone();
        let id = NodeId("bb".into());
        assert!(!held.accepts(&id));
        a.add(id.clone(), paired("later"));
        assert!(held.accepts(&id), "the list is shared, not copied");
    }

    /// A line somebody wrote by hand is not ours to delete: it would come back on the next reload.
    #[test]
    fn a_config_peer_cannot_be_removed_from_the_ui() {
        let a = Allowed::load(None);
        let id = NodeId("cc".into());
        a.add(id.clone(), Peer { name: "declared".into(), alias: None, addr: None, source: Source::Config });
        let err = a.remove(&id).expect_err("refused");
        assert!(format!("{err}").contains("config"), "and says where to go instead: {err}");
        assert!(a.accepts(&id), "and it is still authorised");
    }

    #[test]
    fn paired_machines_survive_a_restart_and_config_ones_are_not_written() {
        let dir = std::env::temp_dir().join(format!("neosh-allow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");

        let a = Allowed::load(Some(&dir));
        a.add(NodeId("aa".into()), paired("kept"));
        a.add(
            NodeId("bb".into()),
            Peer { name: "declared".into(), alias: None, addr: None, source: Source::Config },
        );

        let fresh = Allowed::load(Some(&dir));
        assert!(fresh.accepts(&NodeId("aa".into())), "pairing is remembered");
        assert!(
            !fresh.accepts(&NodeId("bb".into())),
            "a config peer is the config's to re-declare, not ours to cache"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What you call a machine is a decision, so it is written down and it is undoable.
    #[test]
    fn a_rename_is_remembered_across_a_restart_and_clearing_it_gives_the_hostname_back() {
        let dir = std::env::temp_dir().join(format!("neosh-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");

        let a = Allowed::load(Some(&dir));
        let id = NodeId("aa".into());
        a.add(id.clone(), paired("Mac"));
        assert!(a.rename(&id, Some("  the loud one ".into())).expect("allowed"));

        let fresh = Allowed::load(Some(&dir));
        assert_eq!(
            fresh.get(&id).expect("peer").alias.as_deref(),
            Some("the loud one"),
            "trimmed on the way in, so the file holds what a list would draw"
        );
        assert_eq!(
            fresh.get(&id).expect("peer").name,
            "Mac",
            "the announced name is kept beside it — this is an alias, not an overwrite"
        );

        // Both spellings of undo. An empty field is what somebody who cleared the prompt sends.
        assert!(fresh.rename(&id, Some("   ".into())).expect("allowed"));
        assert_eq!(Allowed::load(Some(&dir)).get(&id).expect("peer").alias, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Renaming a config peer is refused for the same reason removing one is: only paired peers
    /// are written to the state file, so the alias would last exactly until the next reload.
    #[test]
    fn a_config_peer_cannot_be_renamed_from_the_ui() {
        let a = Allowed::load(None);
        let id = NodeId("cc".into());
        a.add(
            id.clone(),
            Peer { name: "declared".into(), alias: None, addr: None, source: Source::Config },
        );
        let err = a.rename(&id, Some("mine".into())).expect_err("refused");
        assert!(format!("{err}").contains("name"), "and names the field to change: {err}");
        assert_eq!(a.get(&id).expect("peer").alias, None);
    }

    /// A machine nobody has heard of is not renamed into existence.
    #[test]
    fn renaming_an_unknown_machine_reports_that_there_was_nothing_to_rename() {
        let a = Allowed::load(None);
        assert!(!a.rename(&NodeId("zz".into()), Some("ghost".into())).expect("no error"));
    }
}
