# Working on neosh

A terminal-first agent workspace: Rust core, TypeScript plugins, "the extensibility model of Neovim
and the feature set of Cursor".

The thesis is that **extensibility is the product**. Every capability ships on the same public API a
third-party plugin author would use. If a feature needs a private escape hatch, the API is wrong and
the API is what to fix.

Which means the user owns the workspace, not us: every part of it is theirs to replace, and a plugin
of their own is how they do it. New windows, panels and floats; new commands and keys on any of
them; different colours, different layout, a sidebar that is nothing like ours — none of that is a
feature we grant, it is what the API is for. Ours are the defaults, and a default is something you
turn off.

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

- **Anything neosh can put on screen, a plugin can put on screen — and take off it.** Windows,
  splits, floats, docked panels, extmarks, highlight groups, status segments, commands, keymaps and
  options are all public API, and every bundled plugin is written against exactly that: the sidebar,
  the model switcher, the palette, git, approvals. The test for a new feature is "could a third
  party have written this, and can they replace it?" — if the answer needs an op nobody else may
  call, the op is what to fix, not the feature. In practice that means a new capability lands as
  `ApiCall` + generated TS + a plugin that uses it, in that order; anything the binary draws for
  itself must also be disable-able (`plugins.disabled`) and overridable by a plugin that loads
  after it and wins.
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
- **A panel is a surface, not a program.** Anything a bundled plugin draws, a third party must be
  able to add to and take keys in without forking it. Three mechanisms, and a new panel of ours uses
  all three or it is not finished: a buffer **kind** (`buf.create({ kind })`), which is what
  `KeymapScope::BufKind` binds against and what `win.list` finds it by; **contribution points**
  (`ext.contribute`/`ext.list`), which is how rows and verbs arrive from plugins we have never heard
  of; and **shared vars** (`vars`, scoped to the workspace, a conversation or a project) for anything
  about a thing rather than about us — `state` stays private and a favourite is not private. Every
  key in a panel is an ordinary binding pointed at a named command, so `F1` lists it and `init.ts`
  can move it; a `switch` on `KeyContext` inside a plugin is the thing this replaced. Keep the
  capture, but only as a sink for keys nothing claimed. See ADR 0040.
- **Pairing is a decision each machine makes, and neither of them restarts.** A node presents its
  identity to anyone who connects — as an SSH server presents a host key — so adding a computer is
  typing an address and being shown a name and a fingerprint that came from the far end. The other
  machine is then told somebody is asking. Paired machines go in `$STATE`, never in the user's
  `config.toml`, and a `[[swarm.peers]]` entry written by hand is not the UI's to remove. A dialler
  does not announce a connection until the peer has sent it something: it sends the last message of
  the handshake and cannot tell whether the far end approved, so announcing there says "joined" and
  then "lost" every time pairing is half done.
- **An agent belongs to the machine it was started on.** ASCP moves *descriptions* of agents and
  *requests* to their owners, never the agent: its files, shell and credentials are there, and a
  conversation that could migrate is one whose `cwd` means something else afterwards. The owner may
  refuse anything, and enforces regardless of what it advertised in `NodeCapabilities`. ASCP does no
  NAT traversal — plain TCP, with Tailscale or the like underneath — and does not trust the network
  to say who may steer an agent: a node is its ed25519 public key, and authorisation is a list you
  wrote. `accepts_approvals` is separate from `accepts_commands` and off by default, because
  steering is a message and approving is a write to that machine's disk. See ADR 0041.
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
- **What a driver calls a conversation is the conversation's, not the process's.** A vendor CLI
  keeps the history and hands back a handle; a driver that only remembers it in memory remembers it
  until the workspace stops, and then reopening a conversation starts an agent that has never heard
  of it — full transcript, empty model, and nothing on either side that reports it. It travels as
  `Activity::Resume`, is kept in `Session::resume`, and comes back as `TurnRequest::resume`. See ADR
  0039.
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
- **Everything irreversible asks, and nothing reversible does.** No exceptions on either side: a
  dialog that appears for some deletes and not others is a key whose behaviour you cannot predict
  from the row it is pointed at, and one charged for an action you can undo is what teaches people
  to clear dialogs without reading them. The question says what is at stake — how much, where, and
  what the alternative is — the destructive answer wears `Diagnostic.Error`, and the cursor starts
  on the one that changes nothing. `ui.confirm_destructive = false` is the way out, and it is the
  user's setting rather than the program's guess. See ADR 0039.
- **What you have archived is not in the sidebar.** The panel is the list you work in; a section of
  things you are finished with is the only part of that column that is never the answer, and it
  grows forever. One row with a count, `a` in the panel or `^F` anywhere, and it opens as a picker
  you can filter — because a flat list of dim rows is not a way to find anything. See ADR 0039.
- **A card is a row until you ask for more.** A call that only *looked* at something folds to its
  header with the size of what came back on the end of it; a command keeps its output, an edit keeps
  its diff, and a failure always shows. See ADR 0033.
- **A run of calls that only looked at things is one row**, naming as many of them as fit and
  counting the rest — a turn that read six files reads as one. A run is calls with *nothing drawn
  between them*, asked the same way by the live path and the replay, or switching away and back
  re-folds the conversation. Only a mark that is news gets drawn: `▸` while a call is out, `✗` if it
  failed, and nothing at all when it worked. See ADR 0040.
- **A card says what happened, not which tool did it.** `Ran cargo test`, not `Bash cargo test` —
  read off the arguments, like the colour. A call nothing here classifies keeps the name its author
  gave it, so a plugin's tool is never renamed. **A command's output folds from the middle**: the
  head is how it started and the tail is what it decided, and it is the second one you were waiting
  for. And **one row of air between actions**, which is the cheapest structure there is. See ADR
  0040.
- **A terminal cannot paste a picture, so pasting one is a key.** Bracketed paste is a text
  protocol: a screenshot arrives as nothing, and a dragged file arrives as its path. `^V` asks the
  system clipboard through whichever of `wl-paste`/`xclip`/`pngpaste`/`osascript`/`powershell` is
  there; a pasted line that is unambiguously a path to an image becomes that image. An attachment
  is a **chip above the field**, never a token in it — the message you send is the message you
  typed. `ContentBlock::Image` carries a *path*, not bytes: a transcript that is mostly base64 is
  one nothing can read back, so the bytes live once in the state directory and a driver reads them
  when it builds its request. The media type is read off the bytes, because a `.png` that is really
  a JPEG fails the turn. See ADR 0041.
- **A vendor CLI outlives the turn, so an abandoned turn has to be *drained*.** `Live::lines` is
  per conversation. A turn that is walked away from — `<Esc>`, a switch — leaves the CLI mid-answer
  with the rest of it in the pipe, and the next turn reads it as its own: someone else's reply, and
  then nothing. Stop forwarding, interrupt, read to the `result` that ends it, and leave the process
  at a turn boundary. See ADR 0041.
- **Everything queued into one gap is one message.** A delegating driver is handed the *newest* user
  message and nothing else, so N queued messages meant N-1 questions drawn as asked and never put to
  anybody — which reads exactly like being ignored. `take_steering_into` joins them, in the order
  they were typed. See ADR 0041.
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
through to the binary. `<paste:text>` arrives as a real **bracketed paste**, which is a different
thing from typing the same characters — an image path becoming an attachment, a `/` that dismisses
the menu, a pasted `<esc>` that must not interrupt: none of it is reachable by typing.

Several real bugs — a float silently losing two rows of content, a binding that had never once
fired — were invisible to the test suite and obvious on screen.

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
| `^Y` | Take the last thing you queued back into the composer, to change it or drop it. Readline's yank: `^U`/`^W` kill, `^Y` brings it back |
| `^V` | Attach the image on the clipboard. A key rather than a paste, because a terminal's paste can only ever hand over text |
| `⌥V` | Take the last attached image back off |
| `^P` | Pick a model. Mid-turn too — the running agent is told, and thinks the rest with it |
| `^E` | Reasoning effort and the other per-model options |
| `⌥↑` `⌥↓` | One rung up or down the capability ladder, same provider |
| `⇧⇥` | Permission mode — full access to start with, then ask, allow-listed, deny. Belongs to this conversation, saved with it, and takes effect on the turn that is running |
| `^T` | Projects and conversations. Switching is never refused — turns keep running where they are |
| `^J` | The computers in this workspace. Add one by its address, allow one that is asking, or open what it is running |
| `^F` | What you have archived. Filter it, put one back, or finally throw it away |
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

Dragging an image onto the terminal pastes its path, and a pasted path to an image is attached
rather than typed out. What is attached sits above the field until the message goes.

Composer editing is a text field: `←`/`→` by character and `^←`/`^→` by word, `Home`/`End` and
`^Home`/`^End` for the ends, shift with any of them to select, `^W` and `^U` to delete a word or
back to the start of the line.

## Reading the transcript — `^S`

The answer is the artefact; this is how you get a piece of it out. See ADR 0028.

**A mode's keys are the mode's keys.** `^S` puts the editor in `Normal`, so `chat`'s bindings —
the ones that compose a message — stop firing and these get their keys back. Only four things are
bound in every mode, and they are the escape hatches: `^C`, `^Q`, `^R`, `^S`. Everything that acts
on the composer or scrolls without the cursor is `Chat` alone. A plugin that wants a key here binds
it in `normal`.

Two corollaries, both of which were bugs:

- **A chord is not the letter it is made of.** Chords are matched before the plain keys, and a
  chord this mode does not define does nothing at all. `^B` used to move a word left, `^A` used to
  put you back in the composer.
- **A key another mode claimed never arrives.** Reading's keys come through the *unbound* path,
  which is the last thing consulted — so `^D` was whatever the git plugin bound it to.

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
| `a` | The archive. Nothing archived is ever a row in this panel — `↵` restores one, `^U` puts it back without going there, `^X` deletes |
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
