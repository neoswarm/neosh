#!/bin/sh
# Installs neosh by building it from source.
#
#   curl -fsSL https://neoswarm.dev/install.sh | sh
#
# Needs git and the Rust toolchain (https://rustup.rs). Installs a single
# binary to ~/.local/bin, or to $NEOSH_INSTALL_DIR if set. Never asks for
# sudo, and never touches your config.
set -eu

REPO="https://github.com/neoswarm/neosh"
INSTALL_DIR="${NEOSH_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

case "$(uname -s)" in
  Darwin | Linux) ;;
  *) fail "neosh runs on macOS and Linux. For anything else, see $REPO" ;;
esac

command -v git >/dev/null 2>&1 || fail "git is required and was not found"
command -v cargo >/dev/null 2>&1 ||
  fail "the Rust toolchain is required. Install it from https://rustup.rs and run this again"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Cloning $REPO"
git clone --depth 1 --quiet "$REPO" "$TMP/neosh"

say "Building neosh in release mode. A first build takes a few minutes."
cargo build --release --quiet --manifest-path "$TMP/neosh/Cargo.toml" --bin neosh

mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/neosh/target/release/neosh" "$INSTALL_DIR/neosh"
say "Installed $INSTALL_DIR/neosh"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say ""
    say "$INSTALL_DIR is not on your PATH. Add this line to your shell profile:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

say ""
say "Run 'neosh' to start. Docs: https://neoswarm.dev/docs"
