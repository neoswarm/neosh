# neosh

**Neovim for controlling AI agents.** A terminal workspace where every feature is a plugin.

```sh
npm install -g neosh
neosh
```

No API key needed to start — an existing `claude` login works as is.

This package is a launcher. The binary itself arrives as one of four platform packages
(`@neosh/cli-darwin-arm64` and siblings), and npm installs only the one that runs on your machine.
There is no `postinstall` download, so `--ignore-scripts` and offline caches both work.

macOS and Linux only: the workspace talks to its terminals over a Unix socket, so there is nothing
to ship for Windows yet.

## Other ways in

```sh
brew install neoswarm/tap/neosh          # macOS and Linux
cargo install neosh                      # from source, needs a Rust toolchain
curl -fsSL https://neoswarm.dev/install.sh | sh
```

Prebuilt binaries are on [the releases page](https://github.com/neoswarm/neosh/releases).

## What it is

The workspace is a process and your terminals are viewers of it. Each terminal is somewhere in the
workspace — its own conversation, scroll and panels — while the turns are shared, so closing one
does not stop the work and `neosh` puts you back where you were.

The thesis is that extensibility is the product: every capability ships on the same public API a
third-party plugin author would use, and the sidebar, the model switcher, the palette, git and the
approval prompts are all ordinary plugins written against exactly that surface. If you can see it,
a plugin can replace it.

Writing one starts with [`@neosh/api`](https://www.npmjs.com/package/@neosh/api).

Documentation: <https://github.com/neoswarm/neosh>.

## Licence

MIT.
