//! The TypeScript plugin runtime.
//!
//! This is the only crate that knows v8 exists. Everything above it speaks
//! [`neosh_proto`](neosh_proto) messages, so replacing this runtime — with a different JS engine,
//! or with a subprocess speaking JSON-RPC — changes one crate and nothing else.

pub mod loader;
pub mod manifest;
pub mod runtime;

pub use loader::{
    API_SOURCE, API_SPECIFIER, API_TREE, BUILTIN_TREE, BuiltinPlugin, NeoshModuleLoader,
    api_types_are_current, builtin_plugins, write_api_types,
};
pub use manifest::{DiscoverError, DiscoveredPlugin, discover, load_manifest};
pub use runtime::{ScriptInbound, ScriptOutbound, ScriptRuntime};
