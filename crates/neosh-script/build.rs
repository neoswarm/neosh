//! Tell cargo when the embedded `@neosh/api` tree changes.
//!
//! `include_str!` is tracked by rustc automatically, but `include_dir!` is a proc macro reading the
//! filesystem, which cargo cannot see. Without this, regenerating the ts-rs bindings would leave a
//! stale copy of the types baked into the binary — and `neosh init` would hand a plugin author
//! types describing a version that no longer exists.

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // `rerun-if-changed` on a directory is recursive.
    for dir in ["../../plugins/api/src", "../../plugins/builtin"] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
    }
}
