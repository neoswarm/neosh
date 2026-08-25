# Contributing to neosh

Thanks for wanting to help. This page tells you everything you need: how to set up, how to check
your work, and what makes a pull request easy to merge.

Questions first? Ask on [Discord](https://discord.gg/cWdMAqcAn5) or open an
[issue](https://github.com/neoswarm/neosh/issues). For anything larger than a fix, open an issue
before you write code, so nobody builds something twice.

## The one idea to know

Every feature in neosh ships on the same public API a third party plugin would use. The sidebar,
the palette, git: all plugins. So before adding a capability to the core, ask "could a plugin do
this?" If yes, it should be one. If it almost could but the API is missing something, the API is
what to extend. [AGENTS.md](AGENTS.md) records the rules and the reasoning, and `docs/adr/` holds
one file per decision.

## Setup

You need stable Rust and Node 20 or newer.

```bash
git clone https://github.com/neoswarm/neosh
cd neosh
cargo run
```

That builds and starts a workspace. Good to know while developing:

- The workspace is a daemon. `cargo run` attaches to an already running workspace for your config
  directory if one exists, and that process keeps executing the binary it started with. If a change
  seems to not take effect, stop the old workspace first (`neosh stop`) or run with `--no-daemon`.
- After editing anything under `plugins/`, run `touch crates/neosh-script/src/loader.rs` before
  building. The plugin tree is embedded with `include_dir!` and cargo does not notice otherwise.

## Checking your work

One script runs everything CI runs:

```bash
./scripts/check.sh
```

That is the whole contract. If it passes locally, CI agrees. It covers the workspace tests, the
generated TypeScript bindings, and type-checking every plugin against the published API.

For a faster loop while working, test the crate you touched:

```bash
cargo test -p neosh-core
cargo test -p neosh --test builtin_plugins
```

Two things the checks care about that are easy to miss:

- **Generated bindings are committed.** If you change a type in `neosh-proto`, regenerate with
  `TS_RS_EXPORT_DIR="$PWD/plugins/api/src/generated" cargo test -p neosh-proto` and commit the
  `.ts` files. Drift fails the build.
- **Terminal behaviour is checked on a real screen.** `scripts/shot.py` runs neosh under a pty and
  prints what the screen actually looks like:

  ```bash
  python scripts/shot.py --cols 110 --rows 34 --arg=--no-daemon '<c-p>' '<wait:2000>'
  ```

  Several real bugs were invisible to the test suite and obvious in a screenshot. If your change
  touches anything visual, look at it before you open the PR.

## Opening a pull request

- **One logical change per PR.** A small, focused PR gets reviewed the same day. A PR that does
  three things waits until someone has an afternoon.
- **Show the screen.** neosh is a terminal app, so what it looks like is the product. For anything
  visual, attach a screenshot, a gif, or a short recording to the PR. Before and after is ideal.
  `scripts/shot.py` output pasted as text works too.
- **Match the surrounding code.** Same comment density, same naming, same idiom. A comment states a
  constraint the code cannot show, not what the next line does.
- **Write the commit message like the log.** Read `git log --oneline` first; the messages describe
  what changed and why it matters, in a sentence.
- **No `unwrap()`** on anything touching plugin input or a network response, and secrets never
  touch disk in plaintext. The full list of non-negotiable rules is in [AGENTS.md](AGENTS.md).

## Reporting bugs

Use the [bug report template](https://github.com/neoswarm/neosh/issues/new/choose). The single most
useful thing you can attach is a picture or a recording of the broken screen: you can drag an image
or a video straight into the issue text. After that, your terminal emulator, your OS, and
`neosh --version` let us reproduce it.

## Security

Found a vulnerability? Report it privately through
[GitHub security advisories](https://github.com/neoswarm/neosh/security/advisories/new) rather than
a public issue.

## License

By contributing you agree that your contributions are licensed under the [MIT license](LICENSE).
