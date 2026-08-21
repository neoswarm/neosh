# neosh

A terminal-first agent workspace with Neovim's extensibility model.

The thesis is that **extensibility is the product, not a feature of it**. Every capability ships on
the same public API a third-party plugin author uses — the built-in chat UI is written against the
same `ApiCall` surface `examples/hello-plugin` is. If something needed a private escape hatch, the
API would be wrong.

## Try it

```bash
cargo run                          # uses an existing `claude` CLI login — no API key needed
cargo run -- init                  # write a starter config you can actually edit
cargo run -- paths                 # where it reads and writes
cargo run -- --list-models         # everything reachable right now
cargo run -- --model anthropic/claude-opus-5
cargo run -- --model ollama/llama3.3   # or any local endpoint
```

With no configuration at all, `--list-models` returns **423 models** — OpenRouter's catalogue needs
no credentials, and the `claude-cli` driver reuses a login you already have, which is also what a
first run selects by default.

## What works

A TypeScript plugin, loaded from disk at startup, **with zero core changes**, can:

| | | proven by |
|---|---|---|
| 1 | register a command and bind a key to it | `crates/neosh/tests/hello_plugin.rs` |
| 2 | open a float and render into it | same, plus a `TestBackend` snapshot |
| 3 | attach virtual text that survives an append above it | `neosh-core` buffer tests + end-to-end |
| 4 | register a tool the agent calls | end-to-end, through a recorded turn |
| 5 | veto a tool call from a blocking pre-hook | end-to-end |
| 6 | receive streaming tokens and update a buffer incrementally | end-to-end |

`examples/hello-plugin/main.ts` does all six in ~150 lines and is run as a regression test on every
build, against a recorded provider — no API key, no network, no terminal.

And a user configuration on the same footing: `~/.config/neosh/init.ts` is loaded first, as a plugin
with the full API, and decides what else loads. See [Configuration](#configuration).

## Architecture

```
┌────────── plugin thread (JsRuntime is !Send) ──────────┐
│  main.ts ──▶ @neosh/api ──▶ two ops                    │
└───────────────────────┬────────────────────────────────┘
                ApiCall │ ApiResponse      ← same enum as the JSON-RPC door
                        ▼
┌────────── core task (single writer, owns all state) ───┐
│  Editor { buffers, windows, floats, marks, focus, … }  │
│  Agent  { session, tools, hooks, permissions }         │
└───────────────────────┬────────────────────────────────┘
               UiEvent  │  InputEvent
                        ▼
┌────────── frontend (terminal today, web later) ────────┐
│  mirror ─▶ layout ─▶ ratatui ─▶ crossterm              │
└────────────────────────────────────────────────────────┘
```

| crate | owns |
|---|---|
| `neosh-proto` | every wire type; generates the TypeScript via `ts-rs` |
| `neosh-core` | buffers, windows, floats, extmarks, highlights, focus, keymaps |
| `neosh-provider` | the `Provider` trait, one SSE parser, five drivers, the model catalogue |
| `neosh-agent` | sessions, turns, tool registry, permissions, the agent loop |
| `neosh-script` | the only crate that knows v8 exists |
| `neosh-tui` | the terminal frontend; the only crate that does display-width math |
| `neosh` | the host loop, config, CLI |

Three ideas do most of the work, and each has an ADR:

- **[The frontend resolves layout.](docs/adr/0006-frontend-resolves-layout.md)** The core emits
  declarative geometry, never cell rectangles, so a web frontend is a new consumer rather than a
  rewrite. `--ui-protocol=stdio` emits the identical stream as JSON lines, and the end-to-end tests
  assert on it — which is what keeps the boundary honest rather than aspirational.
- **[Extmarks live on their line.](docs/adr/0007-marks-on-lines-byte-columns.md)** `set_lines` is a
  `Vec::splice`, so marks move with their text for free. "Virtual text survives an append above it"
  is structural, not maintained. Columns are UTF-8 byte offsets; all width math lives in the
  frontend.
- **[Provider drivers are open.](docs/adr/0011-open-provider-drivers.md)** `DriverKind` is a slug,
  not an enum, and a plugin can register one. Models are discovered at runtime, because a baked-in
  list is wrong within weeks.

## Configuration

Same layout Neovim uses — `~/.config/neosh/init.ts`, `~/.local/share/neosh`, `~/.local/state/neosh`
— so symlinking the config directory into a dotfiles repo works as you would expect. `neosh paths`
prints what they resolve to.

`neosh init` writes a starter `init.ts`, a `config.toml`, and — the part that makes TypeScript worth
choosing — a `tsconfig.json` plus the `@neosh/api` types **emitted from the binary you are running**,
so your editor checks your config against the exact version you have.

```ts
// ~/.config/neosh/init.ts — this is a plugin, with the entire API
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.opt.set("agent.model", "claude-cli/sonnet");
  await neosh.rtp.add("~/src/my-plugins");          // decides what else loads
  await neosh.keymap.set("chat", "<C-m>", "models.pick");
}
```

There is no config-only API and nothing a plugin can do that a config cannot — a config language
that can express less than a plugin is exactly the private escape hatch this project is against
(ADR [0012](docs/adr/0012-config-is-a-plugin.md)).

`config.toml` sits alongside for setting values without writing code. Options are declared and
typed, so a misspelled name is an error naming the nearest real one, not a setting that silently
does nothing (ADR [0014](docs/adr/0014-declared-typed-options.md)).

Project config lives in `<repo>/.neosh/` and is split by capability: picking a model and *tightening*
permissions apply immediately, while anything that runs code or widens what the agent may do waits
for `neosh trust`. Trust is keyed on file contents, so a `git pull` cannot quietly change what runs
on your machine (ADR [0013](docs/adr/0013-config-layers-and-project-trust.md)).

`<C-r>` reloads it without restarting — config, plugins, provider instances and permissions.
Settings you delete go back to their defaults, and a config that stops parsing leaves the running
one alone (ADR [0015](docs/adr/0015-timers-and-reload.md)).

Full guide: **[docs/config.md](docs/config.md)**.

## Writing a plugin

```
~/.config/neosh/plugins/my-plugin/
  plugin.toml
  main.ts
```

```toml
name = "my-plugin"
version = "0.1.0"
entry = "main.ts"
permissions = ["tools"]
```

```ts
import type { PluginContext } from "@neosh/api";

export async function activate(ctx: PluginContext) {
  const buf = await ctx.neosh.buf.create({ name: "[hello]" });
  await ctx.neosh.buf.setLines(buf, 0, -1, ["hello from a plugin"]);
  await ctx.neosh.float.open(buf, { border: "rounded", title: "hi" });
}
```

TypeScript is transpiled at load — drop a `.ts` file in and it runs. Types come from
`plugins/api/src/index.ts`, which is *the same file* the host embeds and executes, so the
implementation and the types cannot drift apart.

The generated wire types under `plugins/api/src/generated/` are emitted from the Rust side by
`ts-rs` and committed; `scripts/check.sh` fails the build if they are stale, and `tsc --noEmit`
proves the hand-written layer still agrees with them.

## Models

| driver | credentials | covers |
|---|---|---|
| `claude-cli` | an existing `claude` login | Claude, **no API key** |
| `anthropic` | `ANTHROPIC_API_KEY` | Claude, first-party API |
| `openai-compat` | per instance | OpenAI, OpenRouter, Groq, DeepSeek, xAI, Mistral, Together, Fireworks, Cerebras, Ollama, llama.cpp, LM Studio, vLLM |
| `google` | `GEMINI_API_KEY` | Gemini |
| `mock` | none | recorded fixtures, for CI |

Adding an endpoint that speaks the OpenAI shape is a config entry, never code:

```toml
[[providers]]
id = "my-endpoint"
driver = "openai-compat"
display_name = "My endpoint"
base_url = "https://example.com/v1"
auth = { kind = "env", var = "MY_API_KEY" }
```

Adding a genuinely new protocol is a plugin, via `neosh.provider.register`.

## Development

```bash
./scripts/check.sh    # everything CI runs
cargo test --workspace
```

Testing a plugin or config you wrote — `--ui-protocol=stdio` plus `--mock-script` gives you the whole
stack with no API key, no network and no terminal: **[docs/testing.md](docs/testing.md)**.

393 tests. The first build takes a while — `neosh-script` pulls v8 — but it is the only crate that
does, so day-to-day work on the core rebuilds in seconds.

## Scope

What is deliberately **not** built yet: codebase indexing and embeddings, multi-file diff review,
checkpoints and rollback, a plugin manager with a lockfile, themes, or MCP. The architecture is
shaped so each of those is an addition rather than a rewrite — see `docs/adr/` for where each one
attaches.

Known gaps in what *is* built: options are global-only, with no buffer- or window-local scope; there
is no command line, so commands are invoked by key or from code; and reload does not close windows a
plugin opened. The plugin sandbox provides the ECMAScript built-ins, `console`, `queueMicrotask`,
timers and `Deno.core` — no `fetch`, no filesystem, by design.

## License

MIT
