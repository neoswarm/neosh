---
layout: ../../layouts/Docs.astro
title: Installation
description: Four ways to get neosh, and what to run after any of them.
---

## Requirements

- macOS or Linux.
- `git`.
- The Rust toolchain, for every method except Homebrew. Get it from [rustup.rs](https://rustup.rs).

## Install script

The recommended route. It checks for `git` and `cargo`, builds a release binary from the latest source, and installs it to `~/.local/bin`:

```sh
curl -fsSL https://neoswarm.dev/install.sh | sh
```

Set `NEOSH_INSTALL_DIR` to install somewhere else:

```sh
curl -fsSL https://neoswarm.dev/install.sh | NEOSH_INSTALL_DIR=/usr/local/bin sh
```

The script never asks for sudo, never touches your config, and prints every path it writes to. Reading it first is one click: [install.sh](https://neoswarm.dev/install.sh).

## Homebrew

```sh
brew install neoswarm/tap/neosh
```

## Cargo

```sh
cargo install --locked --git https://github.com/neoswarm/neosh neosh
```

## From source

For hacking on neosh itself:

```sh
git clone https://github.com/neoswarm/neosh
cd neosh
cargo run
```

## After installing

Start it:

```sh
neosh
```

No API key is needed. An existing `claude` login works as is, and hundreds of models are reachable before you configure anything.

Two commands worth knowing:

```sh
neosh init     # writes a starter config and the API types
neosh paths    # where config, data and state live on this machine
```

## Updating

Re-run the install script, or `brew update && brew upgrade neosh`, or re-run the `cargo install` command. (The `brew update` is not optional: Homebrew reads formulae from a clone of the tap on your disk and only refreshes it once a day, so a bare `brew upgrade` spends the first day of a release insisting the version you have is the newest one.) Each replaces the binary in place. A workspace that is already running keeps executing the old binary until you stop it; the terminal tells you when the two have drifted.

## Uninstalling

Remove the binary, then the directories `neosh paths` lists if you want a clean slate:

```sh
rm ~/.local/bin/neosh
rm -rf ~/.config/neosh ~/.local/share/neosh ~/.local/state/neosh
```
