# neosh

**Neovim for controlling AI agents.** A terminal workspace where every feature is a plugin.

```sh
cargo install neosh
```

That builds `deno_core` from source, so give it a quarter of an hour. A prebuilt binary is quicker:

```sh
brew install neoswarm/tap/neosh
curl -fsSL https://neoswarm.dev/install.sh | sh
```

No API key needed to start — an existing `claude` login works as is.

## What it is

The workspace is a process and your terminals are viewers of it. Each terminal is somewhere in the
workspace — its own conversation, scroll and panels — while the turns are shared, so closing one
does not stop the work and `neosh` puts you back where you were.

The thesis is that extensibility is the product: every capability ships on the same public API a
third-party plugin author would use, and the sidebar, the model switcher, the palette, git and the
approval prompts are all ordinary plugins written against exactly that surface. If you can see it,
a plugin can replace it.

## The rest of the crates

`neosh` is the binary. The pieces are published alongside it and are usable on their own:
[`neosh-core`](https://crates.io/crates/neosh-core) (buffers, windows, keymaps),
[`neosh-proto`](https://crates.io/crates/neosh-proto) (the wire types),
[`neosh-provider`](https://crates.io/crates/neosh-provider) (model transports),
[`neosh-agent`](https://crates.io/crates/neosh-agent) (sessions, turns, tools),
[`neosh-script`](https://crates.io/crates/neosh-script) (the plugin host),
[`neosh-tui`](https://crates.io/crates/neosh-tui),
[`neosh-syntax`](https://crates.io/crates/neosh-syntax),
[`neosh-vcs`](https://crates.io/crates/neosh-vcs) and
[`neosh-swarm`](https://crates.io/crates/neosh-swarm).

Documentation, configuration and the plugin guide: <https://github.com/neoswarm/neosh>.

## Licence

MIT.
