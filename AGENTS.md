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
| `neosh-syntax` | Syntax highlighting: what a token *is*, never what colour it is. The only crate that knows a grammar exists. |
| `neosh-tui` | Terminal frontend. Owns **all** display-width maths. |
| `neosh` | The binary. Wires the tasks, owns config, hosts the chat — and is both the workspace (`daemon.rs`) and the terminal that views it (`client.rs`). |

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
- **Which way a diff line went is a background; what the code says is the foreground.**
  `ExtmarkOpts::line_hl_group` bands the whole row and every ranged group is patched over it. A
  `Diff.*Line` group that sets a foreground silently un-highlights the code on it. Colours for code
  come from `neosh-syntax`, which maps grammar scopes onto `Syntax.*` and never loads a theme of its
  own — the palette owns colour, here as everywhere. See ADR 0035.
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
- **The workspace is a process; the terminal is a viewer of it.** `neosh` attaches to the workspace
  for your config directory, starting one if there is none. Closing a terminal detaches — turns keep
  running. `neosh stop` is what ends a workspace, and `^Q` must never be able to. `UiEvent` is a
  delta stream, so anything a client needs on reattach has to be *kept* by the editor and said again
  by `Editor::republish`; a value that is only forwarded is a value that comes back wrong. One
  viewer at a time, newest wins. See ADR 0036.
- **Take the transport that can do the most.** `claude` in stream-json mode with the control
  protocol; `codex app-server`, not `codex exec`. The cheaper one is not simpler for long — it is
  the one where streaming, approvals and interrupts turn out to be impossible rather than missing.
- **The composer is the field; a menu over it is a suggestion about what is in it.** Anything that
  completes what is being typed mirrors it back with `onQuery`, matches on the *name* rather than
  fuzzily over descriptions — an accept key that runs things must never be pointed at a row nobody
  aimed at — and sits on `Anchor::Dock { dock: Bottom }`, which places a float flush against the
  strip so no caller subtracts its own height. See ADR 0037.
- **A conversation owns its permission mode**, and `None` means "nobody chose here, use the
  configured one" rather than a mode of its own. The shared `PermissionLayer` therefore holds the
  *configured* mode and is never written by `⇧⇥` — the moment it follows the active conversation,
  every conversation without a mode inherits the last one you touched. The configured default is
  full access. See ADR 0038.
- **A card is a row until you ask for more.** A call that only *looked* at something folds to its
  header with the size of what came back on the end of it; a command keeps its output, an edit keeps
  its diff, and a failure always shows. A turn that read six files should read as six rows. See ADR
  0033.
- **Redrawing a row means clearing its marks first.** A mark clamps rather than dies when the line
  under it is replaced, and at equal priority the *narrower* one wins — so a row rewritten every
  tick accumulates marks and ends up painted by the shortest one from several seconds ago.

## Verification

Per crate, not workspace-wide — the full run is heavy and `neosh`'s integration tests take about a
minute on their own:

```sh
cargo test -p neosh-core -- --test-threads 2
cargo test -p neosh --test builtin_plugins -- --test-threads 2
cargo test -p neosh --test workspace -- --test-threads 2
cargo test -p neosh-proto export_bindings && git diff --exit-code plugins/api/src/generated
cd plugins/api && npx tsc --noEmit
```

`tests/workspace.rs` runs a real `neosh --serve` and talks to it over a real socket, because what it
asserts is that one process survives another going away. Each sandbox gets its own socket via its
own `--config-dir` — which is also what stops the suite attaching to *your* workspace and stopping
it.

**Terminal behaviour is checked by driving the real binary**, not by reading the code. `scripts/shot.py`
runs neosh under a pty and prints what the screen actually looks like:

```sh
/tmp/ptyenv/bin/python scripts/shot.py --cols 110 --rows 34 --arg=--no-daemon '<c-p>' '<wait:2000>'
```

`--no-daemon` keeps it to one process, so a screenshot is of a workspace that starts and ends with
the shot rather than one left over from the last one.

`--color` prints what colour every run of every row is instead of the plain screen — the only way to
check a diff band, a syntax colour or a dim attribute, none of which the plain dump can see. `--grep`
narrows it to the rows containing a string.

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
| `⇧↑` | Take the last thing you queued back into the composer, to change it or drop it |
| `^P` | Pick a model. Mid-turn too — the running agent is told, and thinks the rest with it |
| `^E` | Reasoning effort and the other per-model options |
| `⌥↑` `⌥↓` | One rung up or down the capability ladder, same provider |
| `⇧⇥` | Permission mode — full access to start with, then ask, allow-listed, deny. Belongs to this conversation, saved with it, and takes effect on the turn that is running |
| `^T` | Projects and conversations. Switching is never refused — turns keep running where they are |
| `^N` | New conversation. In a repository it asks where: here, a new worktree, an existing one, elsewhere |
| `^O` | Add a project |
| `^B` | Toggle the sidebar |
| `^K` | Command palette |
| `/` | Completes a command by name — neosh's, and whatever the agent says it accepts. Keep typing; the composer is still the field, and `↵` sends what you typed when nothing matches |
| `^G` | Git status |
| `^D` | Show what changed |
| `^S` | Read the transcript — see below |
| `^A` `^X` | Select everything, cut the selection |
| `^C` | Copy the selection, or clear the draft, or (twice) quit |
| `PgUp` `PgDn` `^End` | Scroll, and back to the newest message |
| `^R` | Reload configuration |
| `^Q` | Close this terminal. Whatever is running keeps running — `neosh` puts you back |
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
