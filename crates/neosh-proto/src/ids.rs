//! Opaque handles.
//!
//! Every object the plugin API exposes is addressed by id rather than by a handle object. That is a
//! deliberate constraint: an out-of-process plugin gets a byte-identical API, with no proxy objects
//! that only work when the plugin happens to share an address space with the core.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

macro_rules! numeric_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default,
        )]
        #[serde(transparent)]
        #[ts(export)]
        pub struct $name(pub u32);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<u32> for $name {
            fn from(v: u32) -> Self {
                Self(v)
            }
        }
    };
}

numeric_id!(
    /// A text buffer. Buffers are the only place text lives.
    BufferId
);
numeric_id!(
    /// A viewport onto a buffer. Covers both docked windows and floats.
    WindowId
);
numeric_id!(
    /// An extmark namespace. Namespaces let a plugin clear only its own annotations.
    NamespaceId
);
numeric_id!(
    /// An annotation within a namespace.
    ExtmarkId
);
numeric_id!(
    /// A claimed raw-cell surface.
    SurfaceId
);
numeric_id!(
    /// One rectangle of the main region, and everything docked inside it.
    ///
    /// A pane is *where* rather than *what*: it owns no buffer and draws nothing. A chat pane is a
    /// transcript window docked `Main` in it plus a composer docked `Bottom` in it; a pane showing
    /// one thing is one window docked `Main`. That is the whole of why splitting did not need a
    /// second dock vocabulary — [`Dock`](crate::Dock) already answers "where inside this rectangle",
    /// and a pane is just a smaller rectangle to ask it about.
    ///
    /// Panes live in a tree per tab ([`PaneNode`](crate::PaneNode)). The tree says how the main
    /// region is divided; the frontend turns it into rectangles, exactly as it does for docks.
    PaneId
);
numeric_id!(
    /// One tab of one view: a title, and a tree of panes.
    ///
    /// Per view rather than per workspace, for the same reason a window is: what the agent produced
    /// is the workspace's, and where you are looking is yours. Two terminals attached to one
    /// workspace have their own tabs over the same conversations.
    TabId
);
numeric_id!(
    /// One attached terminal.
    ///
    /// A workspace can have several, and they are not copies of each other: a window belongs to
    /// exactly one, so the conversation on screen, the scroll position, the composer and the
    /// panels open over it are all facts about a view rather than about the workspace. Buffers are
    /// not — what the agent produced is the workspace's, and a conversation shown in two terminals
    /// is one transcript with two cursors on it.
    ///
    /// On the wire because a plugin has to be able to say which terminal it is drawing into. It
    /// was kept out of the protocol deliberately at first, when every view saw the same frame and
    /// the number bought nothing; a float that must land in *this* terminal is what changed.
    ViewId
);

impl ViewId {
    /// The frontend of a process that is its own terminal — `--no-daemon`, `--ui-protocol=stdio`,
    /// every integration test that drives the host directly.
    ///
    /// There is exactly one and it never goes away, so nothing has to represent it. It exists so
    /// that "which view was that from" has an answer in the one-process case too, rather than
    /// every caller carrying an `Option` that is only ever `None` in half the builds.
    pub const LOCAL: ViewId = ViewId(0);
}

/// Identifies a loaded plugin. Stable across reloads; derived from the manifest `name`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(transparent)]
#[ts(export)]
pub struct PluginId(pub String);

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PluginId {
    fn from(v: &str) -> Self {
        Self(v.to_string())
    }
}

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        #[serde(transparent)]
        #[ts(export)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(v: &str) -> Self {
                Self(v.to_string())
            }
        }
    };
}

string_id!(
    /// One conversation, persistent across restarts.
    SessionId
);
string_id!(
    /// One user message plus everything the agent did in response, including tool calls.
    TurnId
);
string_id!(
    /// Correlates a `tool_use` block with its result.
    ToolCallId
);
string_id!(
    /// Correlates an in-flight provider stream with its cancellation.
    StreamId
);
string_id!(
    /// One sub-agent, from the moment it is spawned to the moment it reports.
    ///
    /// Distinct from the [`ToolCallId`] of the call that started it: a driver may run a task
    /// nobody asked for by name — a background job, a workflow member — and those have no call to
    /// borrow an id from. When there *is* one, it is carried alongside rather than reused, so the
    /// tree can be rebuilt without either id having to mean two things.
    TaskId
);
string_id!(
    /// Correlates a request across the plugin boundary in either direction.
    RequestId
);

/// A v4 UUID without pulling `uuid` into this crate's dependency tree.
///
/// `neosh-proto` is depended on by every other crate including the TS binding generator, so it is
/// kept as close to zero-dependency as possible.
fn uuid_v4() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(0);
        chunk.copy_from_slice(&h.finish().to_ne_bytes()[..chunk.len()]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    let h = |r: &[u8]| r.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&bytes[0..4]),
        h(&bytes[4..6]),
        h(&bytes[6..8]),
        h(&bytes[8..10]),
        h(&bytes[10..16])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_well_formed_and_distinct() {
        let a = TurnId::new();
        let b = TurnId::new();
        assert_ne!(a, b, "ids must not collide");
        assert_eq!(a.0.len(), 36);
        assert_eq!(a.0.as_bytes()[14], b'4', "version nibble");
        assert!(matches!(a.0.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }
}
