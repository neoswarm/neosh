//! Who a node is, and which nodes it will talk to.
//!
//! A node's identity is an ed25519 keypair. The public half, hex-encoded, *is* the
//! [`NodeId`](neosh_proto::NodeId) — there is no name to be spoofed and no certificate authority to
//! run, because for a workspace that is one person's several computers a CA is infrastructure in
//! exchange for nothing.
//!
//! Authorisation is a list you wrote. A peer whose id is not in it is refused at the handshake,
//! before it can say anything else. That is the whole of the access control, and it is deliberately
//! the kind you can read: the file is a list of hex keys with names beside them, and taking a
//! machine out of the swarm is deleting a line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use neosh_proto::NodeId;

use crate::SwarmError;

/// This node's keypair, and the peers it will accept.
pub struct Identity {
    signing: SigningKey,
    id: NodeId,
    /// Authorised peers, by id, with whatever the user called them.
    ///
    /// A `BTreeMap` so the file round-trips in a stable order — a list of machines that reshuffles
    /// itself on every write is one you cannot usefully keep in version control, and people do keep
    /// this sort of file in version control.
    allowed: BTreeMap<NodeId, String>,
}

impl std::fmt::Debug for Identity {
    /// Hand-written because the derived one would print the signing key.
    ///
    /// Not a hypothetical: an identity ends up inside a struct somebody derives `Debug` on, and one
    /// `tracing::debug!` later the private key is in a log file. The type refusing to print it is
    /// the only version of this rule that survives contact with the rest of the program.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("id", &self.id)
            .field("allowed", &self.allowed.len())
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Load the key at `path`, or make one and save it.
    ///
    /// Generated on first use rather than asked for: a swarm protocol whose first step is "run
    /// keygen" is one people do not get to the second step of. The file is written `0600` on unix,
    /// and a key found with looser permissions is refused rather than quietly used — a private key
    /// the rest of the machine can read is not a private key, and carrying on would mean the
    /// authorisation list is guarding nothing.
    pub fn load_or_create(path: &Path) -> Result<Self, SwarmError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_hex(text.trim(), Some(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let signing = SigningKey::generate(&mut rand::rngs::OsRng);
                let hex = hex::encode(signing.to_bytes());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, &hex)?;
                restrict(path)?;
                tracing::info!(?path, "generated a swarm identity");
                Ok(Self::from_signing(signing))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// An identity with no file behind it. For tests, and for `--clean`.
    pub fn ephemeral() -> Self {
        Self::from_signing(SigningKey::generate(&mut rand::rngs::OsRng))
    }

    fn from_hex(text: &str, path: Option<&Path>) -> Result<Self, SwarmError> {
        if let Some(p) = path {
            check_permissions(p)?;
        }
        let bytes: [u8; 32] = hex::decode(text)
            .map_err(|_| SwarmError::BadIdentity("the key file is not hex".into()))?
            .try_into()
            .map_err(|_| SwarmError::BadIdentity("a key is 32 bytes".into()))?;
        Ok(Self::from_signing(SigningKey::from_bytes(&bytes)))
    }

    fn from_signing(signing: SigningKey) -> Self {
        let id = NodeId(hex::encode(signing.verifying_key().to_bytes()));
        Self { signing, id, allowed: BTreeMap::new() }
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Authorise a peer. Replaces the name if it was already there.
    pub fn allow(&mut self, id: NodeId, name: impl Into<String>) {
        self.allowed.insert(id, name.into());
    }

    pub fn revoke(&mut self, id: &NodeId) -> bool {
        self.allowed.remove(id).is_some()
    }

    pub fn allowed(&self) -> impl Iterator<Item = (&NodeId, &str)> {
        self.allowed.iter().map(|(k, v)| (k, v.as_str()))
    }

    /// Whether a peer may connect.
    ///
    /// A node always accepts itself, which is what makes "two neosh processes on one machine" a
    /// thing you can test with rather than a special case in the transport.
    pub fn accepts(&self, id: &NodeId) -> bool {
        id == &self.id || self.allowed.contains_key(id)
    }

    pub fn sign(&self, message: &[u8]) -> String {
        hex::encode(self.signing.sign(message).to_bytes())
    }
}

/// Check a signature against the key a [`NodeId`] *is*.
///
/// The id and the verifying key are the same 32 bytes, which is the property the whole scheme rests
/// on: there is no lookup between "who you say you are" and "the key that proves it", so there is
/// nothing to poison.
pub fn verify(id: &NodeId, message: &[u8], signature_hex: &str) -> bool {
    let Ok(key_bytes) = hex::decode(&id.0) else { return false };
    let Ok(key_bytes) = <[u8; 32]>::try_from(key_bytes) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else { return false };
    let Ok(sig_bytes) = hex::decode(signature_hex) else { return false };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(sig_bytes) else { return false };
    key.verify(message, &Signature::from_bytes(&sig_bytes)).is_ok()
}

/// What the peer has to sign, for one connection.
///
/// The version and both ids are inside it, not just the nonce. Signing a bare random number proves
/// only that somebody holds the key; signing *this* string proves they meant to authenticate to
/// this node, for this protocol version — so a signature captured from one exchange cannot be
/// replayed into another between different machines.
pub fn challenge(version: u32, nonce_hex: &str, dialler: &NodeId, listener: &NodeId) -> Vec<u8> {
    format!("ascp/{version}\n{nonce_hex}\n{dialler}\n{listener}").into_bytes()
}

/// 32 random bytes, hex.
pub fn nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Where the key lives under a state directory.
pub fn key_path(state_dir: &Path) -> PathBuf {
    state_dir.join("swarm").join("node.key")
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), SwarmError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), SwarmError> {
    Ok(())
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), SwarmError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(SwarmError::BadIdentity(format!(
            "{} is mode {mode:o}; a swarm key the rest of the machine can read is not one. \
             `chmod 600` it, or delete it and a new one will be made.",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<(), SwarmError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_id_is_its_public_key_and_verifies_its_own_signature() {
        let me = Identity::ephemeral();
        let sig = me.sign(b"hello");
        assert!(verify(me.id(), b"hello", &sig));
        assert!(!verify(me.id(), b"goodbye", &sig), "a signature is over one message");
    }

    #[test]
    fn another_nodes_signature_does_not_pass_as_mine() {
        let me = Identity::ephemeral();
        let them = Identity::ephemeral();
        let sig = them.sign(b"hello");
        assert!(!verify(me.id(), b"hello", &sig));
        assert!(verify(them.id(), b"hello", &sig));
    }

    /// The list is the whole of the access control, so "not on it" has to mean "no".
    #[test]
    fn a_peer_is_refused_until_it_is_on_the_list() {
        let mut me = Identity::ephemeral();
        let them = Identity::ephemeral();
        assert!(!me.accepts(them.id()));
        me.allow(them.id().clone(), "linux-box");
        assert!(me.accepts(them.id()));
        assert!(me.revoke(them.id()));
        assert!(!me.accepts(them.id()));
    }

    /// Otherwise a second neosh on the same machine is a special case in the transport rather than
    /// the obvious thing to test against.
    #[test]
    fn a_node_always_accepts_itself() {
        let me = Identity::ephemeral();
        assert!(me.accepts(me.id()));
    }

    /// Garbage in the id must be a "no", not a panic: the id arrives from the network.
    #[test]
    fn a_malformed_id_or_signature_verifies_as_false_rather_than_panicking() {
        let me = Identity::ephemeral();
        let sig = me.sign(b"hi");
        assert!(!verify(&NodeId("not hex".into()), b"hi", &sig));
        assert!(!verify(&NodeId("ab".into()), b"hi", &sig));
        assert!(!verify(me.id(), b"hi", "not hex"));
        assert!(!verify(me.id(), b"hi", "abcd"));
    }

    /// A signature is bound to the pair of nodes and the version, so one cannot be lifted from an
    /// exchange with machine A and replayed at machine B.
    #[test]
    fn a_challenge_is_bound_to_both_ends() {
        let a = NodeId("aa".into());
        let b = NodeId("bb".into());
        let c = NodeId("cc".into());
        let n = nonce();
        assert_ne!(challenge(0, &n, &a, &b), challenge(0, &n, &a, &c));
        assert_ne!(challenge(0, &n, &a, &b), challenge(1, &n, &a, &b));
        assert_ne!(challenge(0, &n, &a, &b), challenge(0, &nonce(), &a, &b));
    }

    #[test]
    fn a_nonce_is_not_the_same_twice() {
        assert_ne!(nonce(), nonce());
        assert_eq!(nonce().len(), 64);
    }

    #[test]
    fn a_key_survives_a_restart_and_the_file_is_not_world_readable() {
        let dir = std::env::temp_dir().join(format!("neosh-swarm-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = key_path(&dir);

        let first = Identity::load_or_create(&path).expect("creates");
        let again = Identity::load_or_create(&path).expect("loads");
        assert_eq!(first.id(), again.id(), "the same machine keeps the same name");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a private key is not for the rest of the machine");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Refusing is the point: carrying on would mean the allow-list is guarding a key anybody on
    /// the machine could have copied.
    #[cfg(unix)]
    #[test]
    fn a_loose_key_file_is_refused_rather_than_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("neosh-swarm-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = key_path(&dir);
        Identity::load_or_create(&path).expect("creates");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = Identity::load_or_create(&path).expect_err("refuses");
        assert!(format!("{err}").contains("600"), "and says how to fix it: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
