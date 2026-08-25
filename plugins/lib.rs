//! The plugin tree, embedded.
//!
//! This crate is a directory before it is a library: its package root is `plugins/`, so the
//! `@neosh/api` sources and every bundled plugin sit *inside* it rather than two levels above it.
//! That is the only arrangement `include_dir!` and `cargo package` both accept, and it is what
//! makes `neosh-script` publishable — the embedding is here, and the deno host that reads it is
//! there.
//!
//! Nothing in here is Rust apart from this file. It is a shipping container.

/// The ergonomic plugin API, as TypeScript source.
pub const API_SOURCE: &str = include_str!("api/src/index.ts");

/// The whole `@neosh/api` source tree, embedded.
///
/// [`API_SOURCE`] is what the host executes; this is the same file plus the ts-rs-generated wire
/// types it imports, so `neosh init` can write out a copy for `tsc` to resolve against. Emitting
/// them from the binary rather than fetching a package means the types always describe the neosh
/// that is actually running — an upgrade cannot leave a plugin author checking against last
/// version's API.
pub static API_TREE: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/api/src");

/// The plugins that ship inside the binary.
///
/// These are ordinary plugins — same manifest, same API, same lifecycle — that happen to be
/// embedded rather than read from disk. Nothing about them is privileged: the sidebar and the model
/// picker are written against the surface a third-party author has, which is how we find out when
/// that surface is not enough.
///
/// Loaded from `neosh:/builtin/<name>/main.ts`, and the leading slash is load-bearing: a `neosh:`
/// URL with an opaque path is *cannot-be-a-base*, so `./options.ts` next to it has nothing to
/// resolve against and fails with a URL error nobody can act on. Bundled plugins used to be one
/// file each because of it — which made them the only plugins in the system that could not be
/// split up, and this crate's whole claim is that they are ordinary.
pub static BUILTIN_TREE: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/builtin");
