# Working on neosh

A terminal-first agent workspace: Rust core, TypeScript plugins, "the extensibility model of Neovim
and the feature set of Cursor".

The thesis is that **extensibility is the product**. Every capability ships on the same public API a
third-party plugin author would use. If a feature needs a private escape hatch, the API is wrong and
the API is what to fix.

## Layout

| Crate | Owns |
|---|---|
| `neosh-proto` | Every wire type. `#[derive(TS)]` on all of them; generated `.ts` is committed. |
| `neosh-core` | Buffers, windows, floats, extmarks, highlights, focus, keymaps, the single-writer `Editor`. No I/O. |
| `neosh-script` | The `deno_core` host and the op surface. The only crate that knows deno exists. |
| `neosh-provider` | `trait Provider`, the shared SSE parser, and one driver per transport. |
| `neosh-agent` | `Session`, `Turn`, the tool registry, the permission layer, the agent loop. |
| `neosh-tui` | Terminal frontend. Owns **all** display-width maths. |
| `neosh` | The binary. Wires the tasks, owns config, hosts the chat. |

`plugins/api/` is `@neosh/api` — generated protocol types plus a hand-written ergonomic layer.
`plugins/builtin/` are ordinary plugins that happen to ship in the binary. `docs/adr/` is one file
per decision, and records the *reasoning*, not the choice.

## Rules that are not negotiable

- **Unicode width is not optional.** `unicode-width` + `unicode-segmentation`. Column maths on
  `char` counts corrupts layout on day one. Columns on the wire are UTF-8 byte offsets; motion steps
  by grapheme cluster; every display-width question is answered in `neosh-tui`.
- **No `unwrap()` on anything touching plugin input or a network response.**
- **Secrets never touch disk in plaintext and never appear in logs or event payloads.** They are
  `secrecy::SecretString`, read at request time, and structurally absent from `UiEvent`.
- **Every public API has a TS type generated from or checked against the Rust side.** Drift fails
  the build.
- **Streaming markdown caches to the last *complete* block boundary** and re-parses only the
  trailing partial block. See ADR 0026.
- `Editor::handles` is a **deny-list**. A new API call that is not added to it silently routes to
  the core.
- **A driver's account of its own loop is not a content block.** Sub-agents, plans, compaction and
  the commands a driver accepts go in `ProviderEvent::Activity`; `TurnAssembler` never reads it. If
  something new has to be squeezed into `TextDelta` to be seen, the family is what to extend. See
  ADR 0034.
- **An agent driver keeps one process per conversation, keyed by `TurnRequest::conversation`.** Not
  per turn, and never one per driver — several conversations run at once, and a driver holding a
  single session id resumes the second into the first one's history. (ACP is the exception and says
  why: its session lives on the agent's side.)
- **Take the transport that can do the most.** `claude` in stream-json mode with the control
  protocol; `codex app-server`, not `codex exec`. The cheaper one is not simpler for long — it is
  the one where streaming, approvals and interrupts turn out to be impossible rather than missing.

## Verification

Per crate, not workspace-wide — the full run is heavy and `neosh`'s integration tests take about a
minute on their own:

```sh
cargo test -p neosh-core -- --test-threads 2
cargo test -p neosh --test builtin_plugins -- --test-threads 2
cargo test -p neosh-proto export_bindings && git diff --exit-code plugins/api/src/generated
cd plugins/api && npx tsc --noEmit
```

**Terminal behaviour is checked by driving the real binary**, not by reading the code. `scripts/shot.py`
runs neosh under a pty and prints what the screen actually looks like:

```sh
/tmp/ptyenv/bin/python scripts/shot.py --cols 110 --rows 34 '<c-p>' '<wait:2000>'
```

Keys are literal text or `<c-p>` / `<cr>` / `<esc>` / `<s-tab>` / `<wait:400>`; `--arg=` passes
through to the binary. Several real bugs — a float silently losing two rows of content, a binding
that had never once fired — were invisible to the test suite and obvious on screen.

After editing anything under `plugins/`, `touch crates/neosh-script/src/loader.rs` before building:
the plugin tree is embedded with `include_dir!` and cargo will not notice otherwise.

---

# Keys

Everything here is an ordinary binding in the same table your `init.ts` writes to, bound to a
*command name* rather than a callback. Setting the same key replaces it. `F1` lists the live
registry — this table is what ships.

## Chat

| Key | Does |
|---|---|
| `⏎` | Send. While a turn is running, **steer** it: the message is held and taken in at the next gap. |
| `⇧⏎` | Newline, so a pasted snippet stays one message |
| `^P` | Pick a model. Mid-turn too — the running agent is told, and thinks the rest with it |
| `^E` | Reasoning effort and the other per-model options |
| `⌥↑` `⌥↓` | One rung up or down the capability ladder, same provider |
| `⇧⇥` | Permission mode — ask, allow-listed, full access, deny. Takes effect on the turn that is running |
| `^T` | Projects and conversations. Switching is never refused — turns keep running where they are |
| `^N` | New conversation. In a repository it asks where: here, a new worktree, an existing one, elsewhere |
| `^O` | Add a project |
| `^B` | Toggle the sidebar |
| `^K` | Command palette |
| `/` | On an empty line: every command by name — neosh's, and whatever the agent says it accepts |
| `^G` | Git status |
| `^D` | Show what changed |
| `^S` | Read the transcript — see below |
| `^A` `^X` | Select everything, cut the selection |
| `^C` | Copy the selection, or clear the draft, or (twice) quit |
| `PgUp` `PgDn` `^End` | Scroll, and back to the newest message |
| `^R` | Reload configuration |
| `^Q` | Quit |
| `F1` | Every binding, live |
| `Esc` | Interrupt the turn in this conversation. The agent is asked to stop, so the conversation survives it |

Composer editing is a text field: `←`/`→` by character and `^←`/`^→` by word, `Home`/`End` and
`^Home`/`^End` for the ends, shift with any of them to select, `^W` and `^U` to delete a word or
back to the start of the line.

## Reading the transcript — `^S`

The answer is the artefact; this is how you get a piece of it out. See ADR 0028.

| Key | Does |
|---|---|
| `hjkl`, arrows | Move |
| `w` `b` | By word |
| `0` `$` | Ends of the line |
| `gg` `G` | Ends of the transcript |
| `^D` `^U` | Half a screen |
| `^F` `^B`, `PgUp`/`PgDn` | A screen |
| `zz` `zt` `zb` | Put the cursor's line in the middle, at the top, at the bottom |
| `[` `]` | Previous / next **turn** |
| `{` `}` | Previous / next **block** |
| `⇥` `za` | Open or fold the **tool card** under the cursor — the whole diff, the whole output |
| `/` `?` then `n` `N` | Search, and step through the matches |
| `v` | Start a selection that motions extend |
| `V` | Select this whole line |
| `y` | Copy the selection, and leave |
| `yy` `Y` | Copy the line |
| `yc` | Copy the **code block** the cursor is in, without its indent or language line |
| `ym` | Copy the whole **turn** — the question and everything it produced |
| `ya` | Copy the entire transcript |
| `i` `a` `o` `⏎` | Back to the composer |
| `Esc` | Drop the search highlight, then leave |

## The project panel — `^T`

| Key | Does |
|---|---|
| `↵` | Open a conversation, fold a project, or add one from the `+` row |
| `j` `k` | Move |
| `n` | New conversation here |
| `o` | Add a project |
| `f` | Pin a project to the top |
| `J` `K` | Reorder within a group |
| `r` | Rename a conversation |
| `x` `X` | Archive, delete |
| `u` | Bring an archived one back |
| `?` | The keys for whatever row you are on |
| `Esc` | Back to the composer |

## Pickers

| Key | Does |
|---|---|
| `↵` | Choose |
| `↑`/`↓`, `^N`/`^P`, `^J`/`^K` | Move |
| type | Filter |
| `⇥` `⇧⇥`, `→` `←` | Between the two panes of the model picker |
| `Esc`, `^C` | Close |

In the model picker specifically — chords, because every bare letter a picker takes is a letter its
filter can never contain:

| Key | Does |
|---|---|
| `^S` | Sign in to the provider the rail is on |
| `^R` | Ask that endpoint what it serves again |
| `^A` | Add a model by id, for something the catalogue has never heard of |
| `^D` | Remove one you added |

All of these are settings — `ui.keys.next`, `ui.keys.accept`, and so on — so rebinding them changes
what the pickers say they do.
