#!/bin/sh
# Installs neosh.
#
#   curl -fsSL https://neoswarm.dev/install.sh | sh
#
# Takes the prebuilt binary for this machine, checks it against the checksum published beside it,
# and puts it in ~/.local/bin — or in $NEOSH_INSTALL_DIR. Never asks for sudo, and never touches
# your config.
#
# Falls back to building from source when there is no binary for this platform, which needs git and
# the Rust toolchain (https://rustup.rs). Set NEOSH_FROM_SOURCE=1 to insist on it.
#
# NEOSH_VERSION pins a release; the default is the newest.
set -eu

REPO="https://github.com/neoswarm/neosh"
API="https://api.github.com/repos/neoswarm/neosh"
INSTALL_DIR="${NEOSH_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is required and was not found"; }

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin | Linux) ;;
  *) fail "neosh runs on macOS and Linux. For anything else, see $REPO" ;;
esac

# The triple the release assets are named by. Anything else falls through to a source build rather
# than failing, because "no binary for you" and "no neosh for you" are different sentences.
target=""
case "$os $arch" in
  "Darwin arm64")          target="aarch64-apple-darwin" ;;
  "Darwin x86_64")         target="x86_64-apple-darwin" ;;
  "Linux x86_64")          target="x86_64-unknown-linux-gnu" ;;
  "Linux aarch64" | "Linux arm64") target="aarch64-unknown-linux-gnu" ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

build_from_source() {
  say "Building neosh from source."
  need git
  command -v cargo >/dev/null 2>&1 ||
    fail "the Rust toolchain is required to build from source. Install it from https://rustup.rs"
  say "Cloning $REPO"
  git clone --depth 1 --quiet "$REPO" "$TMP/neosh"
  say "Building in release mode. A first build takes a few minutes."
  cargo build --release --quiet --manifest-path "$TMP/neosh/Cargo.toml" --bin neosh
  install -m 755 "$TMP/neosh/target/release/neosh" "$INSTALL_DIR/neosh"
}

# `shasum` is on macOS, `sha256sum` on most Linux. Having neither is not a reason to refuse the
# install, but it is a reason to say the check did not happen.
verify() {
  file="$1"
  want="$2"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$file" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  else
    say "Note: no sha256 tool found, so the download was not verified."
    return 0
  fi
  [ "$got" = "$want" ] || fail "checksum mismatch for $(basename "$file") — refusing to install"
}

install_prebuilt() {
  need curl
  need tar

  if [ -n "${NEOSH_VERSION:-}" ]; then
    tag="$NEOSH_VERSION"
  else
    # Parsed rather than jq'd, because jq is not a thing you can assume on a fresh machine.
    tag="$(curl -fsSL "$API/releases/latest" |
      sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  fi
  [ -n "$tag" ] || return 1

  name="neosh-$target"
  url="$REPO/releases/download/$tag/$name.tar.gz"

  say "Downloading neosh $tag for $target"
  curl -fsSL "$url" -o "$TMP/$name.tar.gz" || return 1

  # The checksum is published beside the tarball. A release without one is a release we do not
  # install silently — but an older release genuinely may not have it, so this only enforces what
  # it can fetch.
  if curl -fsSL "$url.sha256" -o "$TMP/$name.tar.gz.sha256" 2>/dev/null; then
    verify "$TMP/$name.tar.gz" "$(cut -d' ' -f1 < "$TMP/$name.tar.gz.sha256")"
  else
    say "Note: no checksum published for $tag, so the download was not verified."
  fi

  tar -C "$TMP" -xzf "$TMP/$name.tar.gz"
  [ -f "$TMP/$name/neosh" ] || return 1
  install -m 755 "$TMP/$name/neosh" "$INSTALL_DIR/neosh"
}

mkdir -p "$INSTALL_DIR"

if [ -n "${NEOSH_FROM_SOURCE:-}" ] || [ -z "$target" ]; then
  [ -z "$target" ] && say "No prebuilt binary for $os $arch."
  build_from_source
elif ! install_prebuilt; then
  say "Could not fetch a prebuilt binary; falling back to a source build."
  build_from_source
fi

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
