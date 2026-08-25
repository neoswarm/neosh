# neosh

This npm package is a **name reservation**. It installs nothing.

neosh is a terminal-first agent workspace, and it is a Rust binary rather than a Node program.
Install it one of these ways:

```sh
brew install neoswarm/tap/neosh          # macOS and Linux
cargo install neosh                      # from source, needs a Rust toolchain
```

…or take a prebuilt binary from [the releases page](https://github.com/neoswarm/neosh/releases).

The npm packages that *are* real are the plugin ones — `@neosh/api` is what a plugin author writes
against:

```sh
npm install --save-dev @neosh/api
```

Everything else lives at <https://github.com/neoswarm/neosh>.

## Why is this here?

Two reasons. It keeps the name pointing at the real project rather than at whatever someone else
publishes under it. And npm cannot attach a trusted publisher to a package that does not exist yet,
so a package has to be created before its releases can be tokenless — this one is created and then
left alone.
