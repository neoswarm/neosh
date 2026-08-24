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
  key in a panel is an ordinary binding pointed at a named command, so `^Z` lists it and `init.ts`
  can move it; a `switch` on `KeyContext` inside a plugin is the thing this replaced. Keep the
  capture, but only as a sink for keys nothing claimed. See ADR 0040.
- **A panel you are in the middle of using has the keyboard.** `FloatConfig::modal` takes
  `KeymapScope::Global` out of the chain, and a key nothing claimed is swallowed rather than reaching
  the composer behind the float. Shadowing the keys a widget wants is the other half and not a
  substitute: a panel can name the keys it *wants* and never the ones it merely does not want to
  happen, so `^N` over the option sheet opened a conversation behind it and `^T` opened the project
  panel underneath with focus in neither. Nearer scopes still resolve, so a modal is exactly as
  rebindable as any other panel. `ui.modal_escape_keys` — `^Q` and `^R` — resolve globally anyway,
  consulted only *after* the panel has declined the key; **absent is not empty**, because the one
  thing between a broken panel and a lost terminal must not depend on somebody having declared a
  default. A modal that borrows a global key to open itself owes a binding to close itself, which is
  why `^E` shuts the sheet from inside. Answering a question is not this: `^T` is how you reach a
  question waiting in another conversation. See ADR 0047.
- **A default binding is a key every terminal sends.** Not "a key most people can press once they
  have found the setting": `F1` is brightness on a Mac out of the box, `⌥V` is `√`, and a key
  neosh never receives is a key no amount of rebinding will fix. That leaves Ctrl-with-a-letter,
  `⇧⇥`, `⏎`, `⌫` and `Esc` — and Ctrl with *punctuation* is not in the list, because a plain
  terminal cannot tell `Ctrl+/` from `Ctrl+7`. Arrows, `PgUp`, `Home` and `End` are bound wherever
  they mean something and are **never the only way** to do anything, which is also why the first
  key in a `ui.keys.*` list is the chord: the first one is what a hint row prints, and a legend is
  a promise about a keyboard. A capability that runs out of chords loses its key and keeps its
  command — `^K` runs it by name and `init.ts` can bind it — because inventing a prefix layer for
  one verb costs every user a concept to save one of them a keystroke. Alt is bound on the terms
  arrows are: only where a terminal-sendable route to the same thing already exists, so `⌥Y`
  copies the directory in chat and `yp` does it in the reader. See ADR 0048.
- **The cursor is on a character, and the frontend is the only thing that knows where that is.**
  Reading the transcript is a normal mode, so `$` is the last character rather than the space after
  it, `h` at column zero stays on its row, nothing parks past the end of one, and a selection
  contains the character the cursor is on — `v` then `y` copies a letter instead of reporting an
  empty selection. That last one is `SelectShape` on the window (`exclusive` / `inclusive` / `line`),
  not an invariant the caller re-establishes after every motion, because a motion that forgets to is
  a frame with the wrong rows lit. The motions are `crate::vim`, over plain rows, and deliberately
  not a second set of answers in `CursorMotion`: that vocabulary is the composer's and every text
  field's, and one `h` that means two things is a call whose behaviour depends on who is asking.
  **And the caret is placed from the rows that were drawn**, never from `cursor_row - top_line`:
  the transcript wraps, one buffer row is four screen rows, and the two counts agree until the
  first long paragraph and then diverge by one row per wrap — far enough in, the caret was off the
  window and therefore not drawn at all, which is a mode with no cursor in it. A window bends its
  own scroll to keep its cursor on screen and reports back what it actually drew, including `rows`,
  the buffer rows on the screen — which is what `^D` and `H`/`M`/`L` are counted in and is not the
  height. See ADR 0051.
- **A list is a place you move in.** Anywhere there is a cursor over rows and no text field — the
  project panel, the transcript reader — the motions are Vim's and they take a count: `5j` is five
  rows you can *land on*, `^D`/`^U` are half of the panel's real height, `12G` is a row rather than
  twelve of anything, and a half-typed count is on screen because a first press with no visible
  effect is indistinguishable from a key that does nothing. The motion after it spends it and
  anything else ends it. Not in the core, and not in a picker: the panel keys live in `chat` mode,
  which is the mode the composer types in, so a digit that became a count globally is a digit you
  could no longer send. Two more things belong to the same list: the row you are *in* stays
  unfolded (`ListRow.expand`, and only ever one row — the cursor is what asks, everywhere else),
  and the foot is held against the bottom edge (`render({ pinned })`, padded in *drawn lines*
  because an unfolded row is several). A dock resizes in place with `win.resize` — reopening gives
  the window a new id and drops the keyboard, so a panel resized from inside itself used to throw
  you back to the composer on every press. And what a foot *says* is on the same budget: the plan
  strip is the one limit that would refuse the next request, plus anything else already critical,
  because three bars and a name and a sentence is five rows of a column whose job is your
  conversations. `⇥` steps it up to every window and then to all of it — contributed as a
  `sidebar.action` with `on: "custom"`, which is how a plugin puts a key on rows it owns rather
  than on every row in somebody else's panel. See ADR 0049.
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
  why: its session lives on the agent's side.) **And exactly one process, which means a turn that
  belongs to no conversation has to be *asked about* rather than keyed on.** A branch name, a
  thread title, a commit message — `Agent::complete` — sends an empty `conversation`, and empty is
  an ordinary map key: every one of them shared a slot that nothing ever emptied, so a workspace
  ran a second `claude` beside the one you were talking to, with the same flags and the same
  access, answering nobody, for as long as it lived. `TurnRequest::is_one_shot` is the question,
  and the answer is a slot nobody else can find and a process closed when the turn is. The other
  half is that an *interrupt* must not cost a conversation its process either: `<Esc>` asks the CLI
  to stop, and a CLI that stops is one to keep — the deadline the closing context question arms is
  not the deadline the interrupt armed, however much they share a timer.
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
  by `Editor::republish`; a value that is only forwarded is a value that comes back wrong. See ADR
  0036.
- **The workspace you are looking at is not necessarily the binary you just built.** `cargo run`
  unlinks `target/debug/neosh`, writes a new one and then *attaches to the workspace already
  serving this config directory* — which goes on executing the inode it started with, now marked
  `(deleted)`. Every plugin runs there, the sidebar included, so a rebuilt panel is drawn by
  last night's code and nothing on screen says why: the change is what gets debugged, and the
  change was never the problem. `PROTOCOL_VERSION` refuses the pair that cannot talk; `BuildId`
  is the pair that can and should not both be trusted. It is a **notice, never a refusal** —
  everything works, and locking somebody out of their own running turns because a file changed
  is the worse answer — it is **captured at startup** or there is nothing left to stat by the
  time it is asked for, and the **terminal** is what says it, because a stale workspace is
  running the only code it has ever had and cannot know it is behind. An unreadable stamp is
  *unknown* rather than old, or the warning fires every time and stops meaning anything. See ADR
  0058.
- **What the agent produced is the workspace's; where you are looking is yours.** Attaching joins,
  it does not take over, and it does not make a copy: every terminal has a **view** of its own —
  its own conversation on screen, transcript, scroll, cursor, folded cards, composer and panels —
  while the conversations, the turns running in them and everything a plugin registered are shared.
  A window belongs to exactly one view and a buffer does not, which is the whole mechanism;
  `neosh_core::Window` has always held the cursor, the anchor and the scroll, so the window map
  only had to gain an owner. A **client** is a socket and a **view** is a set of windows: `^Q`
  closes the client it was pressed in, read off the tag input arrives with and never from whoever
  spoke most recently, because the host can be behind the channel. A frame is therefore *routed* —
  `push_ui` reads the tag off the window an event names, and a terminal whose share is only the
  trailing flush is not woken up at all. A transcript is per view rather than per conversation
  because folding a card open is navigation: `⇥` on a diff must not move the rows under somebody
  else in the same conversation. `on_screen` is gone and `views_showing` replaced it — a turn draws
  once in each terminal showing its conversation, which may be none, and that is when the unread
  mark is set. A terminal attaching lands in the most recent conversation nobody else is reading;
  the last screen of all is kept for whoever comes back, which is what makes leaving mid-answer and
  returning find the answer still arriving. **The smallest-terminal rule is gone** — it existed
  because a card was shared content — and what is still merged is one window two terminals share.
  `republish` says only what the arriving view can see and has to stay idempotent, and
  `ViewportChanged` is still where the host learns a width. See ADR 0059, and ADR 0042 for the
  connection half.
- **A plugin says which terminal it is drawing into, and almost never has to.** Three rules in
  order: a float anchored to a window goes where that window is; a buffer only one terminal shows
  names it; otherwise it is the terminal being served, which for anything done in answer to a key
  is exact. So every float that builds its window per invocation — the palette, the model sheet,
  git — needed no change. A **dock** is the exception, because one that exists once exists in one
  place: `view.onOpen` is when to make it (fired for the terminals already here as well as the ones
  still to come) and `view.onClose` is when to let go. When it does have to be said, `KeyContext`
  carries the view, `win.open`/`float.open`/`session.switch` take one, and a command handler's
  third argument is the whole `neosh` namespace bound to the terminal the key was pressed in — so
  `here.win.open(...)` lands where the person pressing the key is looking. `SessionChanged` names
  the terminal that moved, because "the active conversation" has as many answers as there are
  screens; a question waits until some terminal is reading its conversation and opens there.
  See ADR 0059.
- **Every view gets every event.** `Agent` fans its stream out to one queue per view. Never a
  `broadcast` channel: lagging consumers drop the oldest, silently, under load — and a view that
  missed one `ToolFinished` has a card that spins forever with nothing to put it right.
- **A setting you cannot send is still a setting, and every model has some.** A knob is a
  `ProviderOptionDescriptor` the *driver* advertises — discovered where an endpoint says what it
  takes (OpenRouter's `supported_parameters`, `codex`'s `model/list`), written down where it will
  not — and a provider with no knobs listed is a bug in a catalogue, not a model without options.
  Some of them are words rather than parameters: `ultrathink` and `ultracode` are read by a
  *harness*, so they live on `claude-cli` and never on `anthropic`, they are **two rungs past the
  top of the same ladder** rather than a level and a switch beside it — one message carries one
  word, and a slider marked "how hard should it think" with an `off / on` under it is two questions
  about one decision — they travel as `prompt_injected_values`, and the injection happens once,
  above every driver, on the copy of the message this turn sends — never on the transcript, never on a tool
  round, and always with a sendable value put back in the selection's place. `^E` shows *all* of
  them at once and applies as you move, because a knob you cannot see is a knob you do not have. See
  ADR 0043.
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
- **A question is not a permission.** *May this happen* has one question, a yes-or-no answer, and a
  policy that can answer it without waking anybody; *which of these* has a list of them, sometimes
  several answers each, sometimes an answer nobody listed, and nothing in a mode or an allow-list
  knows which database you want. Routed through the permission path — which is where
  `AskUserQuestion` was — the configured `allow` said yes before anybody was asked, the bare yes
  carried no answers, and `claude` told its own model *"The user did not answer the questions."* So
  a question is its own family end to end: `UserQuestion` on the wire, `QuestionAsker` beside
  `PermissionAsker`, `ask_user` for a plugin to serve and `neosh.ask` for a plugin to raise, and a
  `DriverQuestioner` that does not *hold* a permission layer rather than one that declines to call
  it. **The question text is its own key** — the agent looks an answer up by the question it answers,
  so a generated id is an answer to nothing — and **nobody answering is a denial with a sentence in
  it**, never an `allow` with an empty map, because the empty map is what the agent reads as
  "ignored me". See ADR 0043.
- **A request nobody answers is a turn that never ends.** The control protocol blocks: the CLI waits
  for a `control_response` carrying the id it sent, for as long as that takes. So the two
  classifiers over one pipe must never both stand aside — `can_use_tool` asks *"would
  `ask_user_question` claim this?"* rather than testing the flag itself, because
  `requires_user_interaction` says *somebody has to see this*, not *this is a question*.
  `ExitPlanMode` sets it and carries a **plan**, so the permission path stood aside for the question
  path and the question path declined it for want of a question, and a conversation sat under
  `Running ExitPlanMode… 93m 15s` — a spinner nothing on screen could tell from work. Leaving plan
  mode is an ordinary permission, asked even under `--dangerously-skip-permissions`, and full access
  says yes to it. The other half is that a line reaching the bottom of the reader is **refused out
  loud** by request id rather than dropped: a refusal ends a turn badly and in the transcript,
  silence does not end it at all, and only one of the two is recoverable from the keyboard. See ADR
  0057.
- **Everything irreversible asks, and nothing reversible does.** No exceptions on either side: a
  dialog that appears for some deletes and not others is a key whose behaviour you cannot predict
  from the row it is pointed at, and one charged for an action you can undo is what teaches people
  to clear dialogs without reading them. The question says what is at stake — how much, where, and
  what the alternative is — the destructive answer wears `Diagnostic.Error`, and the cursor starts
  on the one that changes nothing. `ui.confirm_destructive = false` is the way out, and it is the
  user's setting rather than the program's guess. See ADR 0039.
- **A worktree lives inside its project, and a display string is not a grouping key.** The sidebar
  nests a worktree under the repository it is a tree of, named by its branch — four scratch trees
  of one repository are not four projects — and it groups on `SessionInfo::repo_root`, never by
  parsing `neosh · fix/thing` back apart. Each nested tree is an ordinary project row: its own
  fold, rank and `n`; counts, unread marks and recency fold upward into the repository's row. A
  *relative* `worktree.root` puts trees inside the repository as `<repo>/<configured>/<branch>` —
  no `<repo>` level, nothing else's trees can land there — and `add_worktree` writes the directory
  into the repository's `.gitignore` (tracked, so every clone gets it; skipped when already
  ignored), or every `git status` reads as one giant untracked directory. See ADR 0046.
- **A branch is named by what you asked for, once you have asked.** A worktree you did not name
  starts on `wily-nimbus`, because naming a branch before the work is a decision made at the worst
  possible moment — and the first message sent in it *is* that decision, arriving on its own, so
  the branch is renamed from it and never touched again. Only a name nobody chose: the mark is a
  session var written by whoever generated it (`git.branch.scratch`), never a pattern match on the
  shape of the name, which cannot tell `brisk-otter` we picked from `brisk-otter` you typed. At
  turn *start*, because the panel is drawn all through a turn — `git branch -m` is one ref write,
  so it is safe under a running agent and safe on uncommitted work. **One attempt, ever**: the mark
  is spent before the model is asked, or a cheap model's hiccup becomes a request per message. The
  *type* is the model's — `feature/`, `fix/`, `chore/` — so `git.branch.prefix` applies only to a
  name that arrived without one, or you get `feature/fix/the-login` under a type that is now wrong.
  `git.branch.model` is unset by default and the plugin sends **no selection at all** rather than a
  guess, so the fallback stays the host's: `gen.model`, then the model you are already talking to.
  And the host has to put *its own* label right — `ProjectFacts` is asked once per directory and
  read on every redraw, so `GitRenameBranch` is the one git write handled on the host loop, which
  is what makes the sidebar row follow instead of saying `wily-nimbus` until restart. The directory
  keeps the old name on purpose: moving it would invalidate the conversation's `cwd`. See ADR 0051.
- **A project outlives the conversations in it.** The panel's list is written down (`sidebar.projects`,
  a workspace var) rather than worked out from where the conversations happen to be — derived, it
  deleted the directory you had worked in all month the moment you cleared out the last thread in
  it, and left nothing to start the next one from. A project arrives by being added or by a
  conversation being started in it, keeps the name the host gave it (`sidebar.name`, so emptying a
  worktree does not rename it to `wt-fe3c0d93`), and leaves by `X` on its heading and nothing else.
  Empty, that asks nothing — `o` puts it back; with conversations still in it, it is a delete of
  every one of them and asks like one. See ADR 0039.
- **What you have archived is not in the sidebar at all, and it is something you can empty.** The
  panel is the list you work in; a section of things you are finished with is the only part of that
  column that is never the answer, and it grows forever. ADR 0039 left one dim row with a count
  behind — a door rather than a drawer — and that row is now **off by default** too, for the same
  argument one step further: a permanent line about what you are *not* doing is the line that
  column can least spare, and the way in was never the row. `^F` from anywhere, `a` from the panel
  — advertised in its key strip — or `^K`; `archive.sidebar = true` puts the count back for anybody
  who wants it. It opens as a **popup panel of its own** — its own plugin, its own buffer kind, every key
  an ordinary binding, `archive.action` for somebody else's verb, and the row you are on unfolds.
  A picker was the right shape for the conversation you archived this morning and regret, and forty
  minutes of dialogs for a workspace you have used since spring: so rows can be *ticked*
  (`<Space>`), every verb means what is ticked or the row under the cursor, and `^X` empties
  **what the panel is currently listing** — which is what the filter is for, since narrowing to one
  project and pressing it is that project and nothing else. It asks, in numbers, by ADR 0039's
  rule. What is on disk is in scope: `RESTORE_LIMIT` means a workspace loads the most recent two
  hundred conversations and the rest are files nothing could show, open or delete, so the panel
  reads `session.stored` as well as `session.list` and the host brings one in from its file the
  moment any verb names it — otherwise "empty the archive" reports success over a directory it
  never touched. **Nothing here deletes on a timer**: `archive.auto_days` *archives* what has gone
  idle, because archiving is reversible and free, and `archive.retention_days` only ever counts —
  `archive.sweep` is the same number with a person behind it. See ADR 0060.
- **A notification is for something you did not ask for and cannot see.** Both halves, and a
  message that fails either is not one. `MessageLevel` says how *bad* a thing is and never whether
  you need to know about it, which is how one channel ended up carrying a hundred and seventy call
  sites sorted by nothing: `favourited ~/proj` beside a row that had just grown a pin, `pulling…`
  stacked *above* the `up to date` that superseded it, and — the part that matters — no
  notification at all for the two events that stop a turn dead. So `NoticeKind` is the other axis:
  a **reply** is feedback for the key you pressed and does not stack, because two keys pressed
  quickly are two keys; **progress** is keyed and replaced in place, because it is a state and not
  a message; an **alert** is news, and the only kind that may leave the terminal. Where it goes is
  decided by where *you* are — nothing leaves the terminal about a conversation you are looking at,
  and that one line is what kills the noise. **And the raise belongs to the terminal, not the
  workspace**: it is an OSC the view writes on the stream the UI is already drawn on, for exactly
  the reason `Clipboard` is OSC 52 rather than a clipboard library — a coding agent runs on the big
  machine and you are sitting at a laptop, so anything the workspace raises itself is raised on the
  wrong computer. The desktop fallback is *only* for nothing being attached, when there is no
  stream and no wrong machine to be on. Away is focus (mode 1004) and, on a terminal that cannot
  say, idleness — never "assume focused", which is the one wrong answer that means a whole class of
  terminal never notifies. A turn shorter than `notify.min_turn` finished while you were still
  looking at the key that started it. And `unread` stays the *record*: this points at it once and
  gets out of the way. See ADR 0057.
- **A turn that finished while you were elsewhere is news until you go and look.** The panel says
  what is *happening* and stops the moment it stops, so an answer that arrived while you were in
  another conversation looks exactly like an answer you read yesterday. `SessionInfo::unread` is set
  when a turn ends off screen and cleared by *arriving* — no dismissal key, because a mark you clear
  by hand is a second chore attached to the first. It lives on the conversation and is persisted, so
  every list agrees and a restart does not lose it; it wears `Status.Unread`, the amber the palette
  already uses for *act now*, and it never moves — motion means "something is happening you cannot
  see", and this is the opposite. A folded project carries a dot for what it is hiding. See ADR 0042.
- **A conversation that is asking you something is not a conversation that is working.** Its turn is
  still in flight — blocked on the hook — so `active_turn` is set and every list draws the spinner,
  which is the one row in the panel that most needs finding drawn as the nineteen that do not. Which
  ones are waiting travels as `question.asking`, a **workspace var** written by whatever serves
  `ask_user` and read by whatever draws conversations, because it is a fact about them rather than
  about the panel that happened to learn it. It **outranks working** wherever the two meet, it wears
  `Status.Pending` and therefore *moves* — a block ends when you answer, which is exactly what
  `Status.Unread` must not do — and the queue behind it is in memory while the var is on disk, so it
  is announced once at startup, empty, and that is what clears the one left behind by a workspace
  that stopped mid-question. See ADR 0043.
- **A question is the last thing in the transcript until something answers it.** A turn's closing
  rows — its plan, what it left running, what it changed — are about the answer they close, so they
  splice in *above* a question steered in after that answer rather than at the end of the buffer,
  where they sat under a message you had just typed. And a question the turn ended without
  answering says so in a row of its own, out of the stop reason: a bare question reads exactly like
  one still being thought about, and a toast that has faded is not an answer to "why is nothing
  happening". A plugin veto is a `Refusal` carrying its reason and never `Cancelled`, which is what
  `<Esc>` means. **An entry in the running-turns map lives until `TurnEnded` arrives** — `<Esc>`
  flags it rather than removing it, because removing it early let a message typed in the gap start
  a *second* turn in one conversation, which the first turn's ending then dismantled. Steering never
  takes a message it cannot deliver: the tool-gap take is guarded on cancellation, and what stays in
  the queue becomes the next turn. Leaving the reader goes back to following the newest, and so does
  sending — whether it starts a turn or joins one. See ADR 0052.
- **An orchestrator is a plugin, and it drives conversations by name.** `agent.command` carries the
  same `AgentCommand` vocabulary `swarm.command` does, at a conversation named by id and defaulting
  to the one on screen; `swarm.command` aimed at this node runs it here rather than dialling itself.
  Watching was always per-conversation — `Token`, `ToolFinished`, `TurnEnded` all name one — so
  until this existed the local API could see every conversation in the workspace and drive exactly
  one of them, and fanning work out meant `SessionSwitch` before every message, which moves the
  transcript out from under whoever is reading it. **Which model answers is a hook**: `turn.route`
  fires after the words are settled and before the driver is resolved, may re-point the turn or veto
  it with a reason, and writes its choice back to the conversation so the footer and `^P` agree with
  the turn. **The declared permissions are enforced** — a tool, a provider driver, a *blocking* hook
  and a raw-cell surface each need the word in `plugin.toml`, an observer needs nothing, and drawing
  and reading stay free. **A plugin arrives as a git clone**: `neosh plugin add <url>`, validated
  before it is moved into place, named by its manifest, and never able to delete what you put in
  your own config directory by hand. See ADR 0053.
- **A card is a row until you ask for more.** A call that only *looked* at something folds to its
  header with the size of what came back on the end of it; a command keeps its output — a stack of
  them keeps the last one's — an edit keeps its diff, and a failure always shows. See ADR 0033.
- **A run of calls of one kind is one row**, naming as many of them as fit and counting the rest —
  a turn that read six files reads as one, and so does a stretch of it spent on `git add`, `git
  commit`, `git push`. Reads with reads and commands with commands, never one of each, and never an
  edit. A run is calls with *nothing drawn between them*, asked the same way by the live path and
  the replay, or switching away and back re-folds the conversation. A stack of commands keeps the
  **last one's output** — what a command printed is the answer, and the ones before it are how you
  got there — names each command by what it *is* rather than how it was spelled (`cargo test`, not
  `cargo test -p neosh-core -- --test-threads 2`), and gives every command back in full, with what
  it said, on `⇥`. A run **never continues past a failure**, so a failed command is the last call
  on its card and its output is the one showing. While a run is still going the row is fitted from
  the *end*: then it is a report of what is happening rather than an account of what happened, and
  the call being waited on is the one name that must never be the name that did not fit. Only a
  mark that is news gets drawn: `▸` while a call is out, `✗` if it failed, and nothing at all when
  it worked. See ADR 0040 and ADR 0051.
- **A card says what happened, not which tool did it.** `Ran cargo test`, not `Bash cargo test` —
  read off the arguments, like the colour. A call nothing here classifies keeps the name its author
  gave it, so a plugin's tool is never renamed. **A command's output folds from the middle**: the
  head is how it started and the tail is what it decided, and it is the second one you were waiting
  for. **A card is attached to what it did**: no blank row above it, and its body under a corner
  (`└`) at a four-column indent rather than a rule down the whole left side. Air between every two
  actions and a wall beside every diff were both structure said twice — the header is at column
  zero, the body is indented, and that is already a list. See ADR 0040 and ADR 0050.
- **A turn is something you can watch, not only something you read back.** A command is named by
  what it *does* and never by how it got there: a leading `cd /home/you/projects/thing &&` is the
  one part of that row you already knew — the conversation has a directory — and clipped to a
  header it is the only part that fits, so it comes off before the command is named and a stack of
  six folds to what was run rather than to `cd, cd, cd`. Reading is a place, and a turn writes
  below it: parked on the **last row** you are carried along with what arrives, anywhere else you
  stay exactly where you are, and neither happens while a selection or a search is holding two
  positions. And **the card the cursor is in is open** — ADR 0049's rule one surface along, bounded
  by `chat.preview_lines` because nine hundred rows appearing under `j` is what the fold exists to
  prevent, and never fewer rows than the folded card showed. `⇥` stops meaning "open this" and
  starts meaning "keep it", `c`/`C` step call to call because `[`/`]` is a whole turn and `{`/`}`
  is one block for a run of nine cards, and a preview never fires while the tail is carrying you:
  following is watching, and a redraw settles the answer streaming above it. See ADR 0057.
- **A terminal cannot paste a picture, so pasting one is a key.** Bracketed paste is a text
  protocol: a screenshot arrives as nothing, and a dragged file arrives as its path. `^V` asks the
  system clipboard through whichever of `wl-paste`/`xclip`/`pngpaste`/`osascript`/`powershell` is
  there; a pasted line that is unambiguously a path to an image becomes that image. A clipboard is
  **asked what it holds, not told**: the best picture type *on offer* is the one requested, because
  asking for `image/png` and nothing else reads a JPEG, a WebP and a BMP as an empty clipboard. And
  a picture copied off a page is often not on the clipboard at all — what a browser leaves is an
  address (`text/uri-list`, an `<img src>` in `text/html`, a `data:` URI, a plain URL), so
  `from_clipboard` answers with bytes *or* a `Remote` to go and get. That fetch is spawned rather
  than awaited on the loop, carries the conversation it was asked in, and is the one attachment
  that says something on the way. A photograph too busy to fit as a PNG is sent as a JPEG rather
  than refused over an encoding. See ADR 0051. An attachment is a **chip above the field**, never a
  token in it — the message you send is the message you typed. `ContentBlock::Image` carries a
  *path*, not bytes: a transcript that is mostly base64 is one nothing can read back, so the bytes
  live once in the state directory and a driver reads them when it builds its request. The media
  type is read off the bytes, because a `.png` that is really a JPEG fails the turn. See ADR 0041.
- **An agent that speaks when nobody asked is still speaking to you.** ADR 0054 stopped a turn
  `claude` starts for itself — a backgrounded command finishing, enqueued as a message to itself and
  answered — from being read as the answer to your next question, by draining the pipe before the
  prompt goes in and dropping what it found. This is the half it deferred: the same turn, *heard
  while it happens*. The reader outlives the turn the way the process does — stdout is read for the
  life of the child, not the length of an answer — so nothing is lost and a pipe nobody reads cannot
  fill and stop the CLI mid-sentence. It reports what nothing is reading (`Unasked`), and the host
  opens a turn to hold it: one that **says nothing** (`listen_only`, no user message pushed, and no
  drain, because dropping what the turn exists to show is the one mistake it cannot make), because
  a turn opened to hold an answer has no question above it. **A turn still ends on its own
  `result`** — `init` opens one and `result` closes one for their turns as much as ours, so a prompt
  queued behind one has two endings to wait for, and ending on the first is how a message that had
  not been read yet came back as `{"type":"thinking","text":""}`. See ADR 0056.
- **A vendor CLI outlives the turn, so an abandoned turn has to be *drained*.** `Live::lines` is
  per conversation. A turn that is walked away from — `<Esc>`, a switch — leaves the CLI mid-answer
  with the rest of it in the pipe, and the next turn reads it as its own: someone else's reply, and
  then nothing. Stop forwarding, interrupt, read to the `result` that ends it, and leave the process
  at a turn boundary. See ADR 0041.
- **Everything queued into one gap is one message.** A delegating driver is handed the *newest* user
  message and nothing else, so N queued messages meant N-1 questions drawn as asked and never put to
  anybody — which reads exactly like being ignored. `take_steering_into` joins them, in the order
  they were typed. See ADR 0041.
- **A flash is one thing happening; a burst is a redraw.** `Animation::Flash` is the only motion
  here that fires once, so it needs a moment to count from — and a highlight group, shared by every
  row using it, cannot hold one. The *mark* does: an extmark is created once, so the frontend keys
  the one-shot from the first time it saw that mark, and scrolling a row away and back cannot fire
  it again. More than a few arrivals at once are a bulk redraw — republishing on reattach mints
  every mark in a conversation — and none of them flash, because a transcript that blinks when you
  switch to it is the worst version of this. A spinner (`Animation::Frames`) is the other half:
  frames travel as a *name* because only the frontend knows what the terminal can draw, and it
  clips or pads every frame to the width of the run underneath, so a spinner can never move the
  column after it. The buffer keeps the glyph that was written — a frame is what a cell looks like,
  not what the line says. See ADR 0045.
- **Compaction moves the meter, because compaction is what moved the context.** A driver that has
  once answered "how full is it" turns off the estimate derived from usage, so the next answer only
  arrives with the next request — and a `Compacted` whose `after` nobody applied left the footer
  reporting the pre-compaction number for the rest of the conversation, under a card that said
  `180k → 12k`. See ADR 0050.
- **What the plan has left cannot be counted, only reported and then kept.** A subscription's
  allowance is opaque and is not a function of anything on this machine — a turn run in `claude`
  directly spent it too. `claude` says so on a `rate_limit_event` line and `codex` on
  `account/rateLimits/updated`; both are `Activity::Quota`, both are *kept* by the host, and a
  mid-turn line **patches** while a poll **replaces** — the first carries one window and says
  nothing about the others, the second is the only thing that can express a limit that has gone. A
  window id is a *string* and severity is the **vendor's** grade: the `limits` array grew three
  kinds between two releases, and 94% weekly is `critical` where 55% session is `normal`. Polling at
  rest reads the vendor CLI's own login — `SecretString`, request-time, never on disk, structurally
  absent from `UiEvent` — and is `usage.poll`. **Quota and token history never share an axis**: a
  percentage of an opaque allowance does not convert into a token count. History comes from the
  vendors' own transcripts, never from ours, and a bounded scan that hits its cap says so. See ADR
  0044.
- **Redrawing a row means clearing its marks first.** A mark clamps rather than dies when the line
  under it is replaced, and at equal priority the *narrower* one wins — so a row rewritten every
  tick accumulates marks and ends up painted by the shortest one from several seconds ago.
- **A selection is two positions, so moving either end redraws it.** `WinMotion` and `WinSetCursor`
  differ on purpose — the first throws the selection away unless told to extend, the second leaves
  the anchor alone, which is what makes a *jump* take the text it jumped over — but both have to
  repaint. Only `WinMotion` did, so `v` and then anything but `hjkl` (`^U`, `{`, `[`, a search hit)
  selected six lines, showed none of them, and copied all six: a selection you cannot check is worse
  than none. `V` is not a second kind of selection either — it is the same pair of positions with an
  invariant re-established after every motion, because extending *upwards* from a line-start anchor
  otherwise drops the one line you definitely meant.
- **Arriving in a conversation reseats the view, not just the buffer.** The transcript's scroll
  offset, cursor and selection are *kept* — that is what lets a client attaching later be told where
  it is — so `enter_session` believing "back at the end" is not the same as the window being there.
  Read a long answer, go up, come out, switch project, and the next conversation was drawn from a
  row of a transcript that no longer existed. Reading is a place *in* a conversation, so the switch
  takes you out of it too. ADR 0036's rule, one more time: a value that is only forwarded is a value
  that comes back wrong.
- **Following the newest is not a row, and row zero is.** `top_line` is an `Option`: `None` is
  unscrolled — the frontend's own answer, the last screenful of a transcript and the first of
  everything else — and `Some(0)` is the top. Encoded as the same `0`, the first row of a
  conversation was the one place in it nothing could ask for: `^U` and `PageUp` up to the top asked
  for row zero, which was read as "follow the tail", and the window drew the *end* instead — cursor
  off screen, and nothing on the way there to say what had happened. A frontend decides where an
  unscrolled window sits, which is why the state has to be sayable rather than spelled with a row
  number that means something else.

## Verification

Per crate, not workspace-wide — the full run is heavy and `neosh`'s integration tests take about a
minute on their own:

```sh
cargo test -p neosh-core -- --test-threads 2
cargo test -p neosh --test builtin_plugins -- --test-threads 2
cargo test -p neosh --test workspace -- --test-threads 2
cargo test -p neosh --test plugin_manager -- --test-threads 2
# ts-rs reads the export directory from the environment, and an exported `TS_RS_EXPORT_DIR`
# beats `.cargo/config.toml` — so in a worktree this has to say which checkout it means, or
# the generated files land in another one and the diff below passes having read nothing.
TS_RS_EXPORT_DIR="$PWD/plugins/api/src/generated" cargo test -p neosh-proto export_bindings \
  && git diff --exit-code plugins/api/src/generated
cd plugins/api && npx tsc --noEmit
```

`tests/workspace.rs` runs a real `neosh --serve` and talks to it over a real socket, because what it
asserts is that one process survives another going away. Each sandbox gets its own socket via its
own `--config-dir` — which is also what stops the suite attaching to *your* workspace and stopping
it. `tests/plugin_manager.rs` does the same for installing: a real git repository, a real clone, and
a real workspace started afterwards to see whether the plugin actually ran — a `--config-dir` **and**
a `NEOSH_DATA_DIR` of its own, because that is where an install lands.

`tests/builtin_plugins.rs` starts a whole neosh per test and waits on what appears on screen, so it
is timing-sensitive: under load a handful of unrelated tests fail on a `wait_for` timeout, and a
different handful each run. A failure there is worth re-running alone (`--test-threads 1 <name>`)
before believing it.

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
*command name* rather than a callback. Setting the same key replaces it. `^Z` lists the live
registry — this table is what ships.

Every key here is one **every terminal sends on every platform**: Ctrl-with-a-letter, `⇧⇥`, `⏎`,
`⌫`, `Esc`. Nothing needs `fn`, nothing needs Option-as-Alt, and nothing is reachable only by an
arrow — arrows and `PgUp`/`Home`/`End` are bound wherever they mean something, and are never the
only way to do anything. See ADR 0048.

## Chat

| Key | Does |
|---|---|
| `⏎` | Send. While a turn is running, **steer** it: the message is held and taken in at the next gap. |
| `⇧⏎` | Newline, so a pasted snippet stays one message |
| `^Y` | Take the last thing you queued back into the composer, to change it or drop it. Readline's yank: `^U`/`^W` kill, `^Y` brings it back |
| `^V` | Attach the image on the clipboard — the picture itself, or the one it only names: a page's `<img>`, a URL, a file. A key rather than a paste, because a terminal's paste can only ever hand over text |
| `⌫` | On an empty composer, take the last attached image back off |
| `^P` | Pick a model. Mid-turn too — the running agent is told, and thinks the rest with it |
| `^E` | Everything this model can be told, on one panel: effort, thinking, fast mode, context, and whatever a driver invented. `h`/`l` along a row, `j`/`k` between them, arrows too, and it applies as you move. Nothing else reaches the keyboard while it is open; `^E` again closes it |
| `⇧⇥` | Permission mode — full access to start with, then ask, allow-listed, deny. Belongs to this conversation, saved with it, and takes effect on the turn that is running |
| `^T` | Projects and conversations. Switching is never refused — turns keep running where they are |
| `^J` | The computers in this workspace. Add one by its address, allow one that is asking, or open what it is running |
| `^F` | What you have archived — see below. Filter it, put some back, or finally empty it |
| `^N` | New conversation. In a repository it asks where: here, a worktree you need not name, one kept inside the project, one you do name, an existing one, another machine, elsewhere. A worktree you did not name is named by your first message — `fix/composer-paste-truncation`, not `wily-nimbus` |
| `^O` | Add a project |
| `^B` | Toggle the sidebar |
| `^K` | Command palette |
| `/` | Completes a command by name — neosh's, and whatever the agent says it accepts. Keep typing; the composer is still the field, and `↵` sends what you typed when nothing matches |
| `^L` | What the plan has left, and where the week went. Live gauges per limit, and the last 30 days from the agents' own transcripts. The sidebar keeps one row of it; `⇥` on that row asks for more |
| `^G` | Git status |
| `^D` | Show what changed |
| `^S` | Read the transcript — see below |
| `⌥Y` | Copy this conversation's directory — in a worktree, the worktree's path. The one Alt chord that ships, because it is never the only way: `yp` in the reader, `y` in the project panel, `^K` by name |
| `^A` `^X` | Select everything, cut the selection |
| `^C` | Copy the selection, or clear the draft, or (twice) quit |
| `PgUp` `PgDn` `^End` | Scroll, and back to the newest message |
| `^R` | Reload configuration |
| `^Q` | Close this terminal. Whatever is running keeps running, and so does every other terminal open on this workspace — `neosh` puts you back |
| `^Z` | Every binding, live |
| `Esc` | Interrupt the turn in this conversation. The agent is asked to stop, so the conversation survives it |

Dragging an image onto the terminal pastes its path, and a pasted path to an image is attached
rather than typed out. What is attached sits above the field until the message goes.

Composer editing is a text field: `←`/`→` by character and `^←`/`^→` by word, `Home`/`End` and
`^Home`/`^End` for the ends, shift with any of them to select, `^W` and `^U` to delete a word or
back to the start of the line. The capability ladder — `model.upgrade`, `model.downgrade` — has no
default key: it had `⌥↑`/`⌥↓`, which is not a key every terminal sends, and `^K` runs both by
name. Copying this conversation's directory — `session.copy.path`, which in a worktree is the
worktree's path — keeps `⌥Y` on the terms arrows are bound on: a key that means something where it
arrives and is never the only way — `^K` or `/copy` here, `y` on any row of the project panel, and
`yp` in the reader.

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

**The cursor is on a character, not between two.** Which is why the caret is a *block* here and a
bar in the composer, why `$` is the last character rather than the space after it, why `h` at column
zero stops rather than going to the row above, and why `v` then `y` copies the character you were
looking at instead of saying there is nothing there. A screen is counted in the buffer rows actually
on it, which after wrapping is not the window's height. See ADR 0051.

| Key | Does |
|---|---|
| `hjkl`, arrows | Move. A count first does it that many times: `5j` |
| `w` `b` `e` `ge` | By word. `W` `B` `E` by **WORD** — everything that is not a space |
| `0` `^` `$` `g_` | The start of the line, its first non-blank, its last character, its last non-blank |
| `f` `F` `t` `T` then `;` `,` | To a character on this line, or just before it. `;` again, `,` the other way |
| `%` | The bracket matching the one under the cursor, however many rows away |
| `gg` `G` | Ends of the transcript. With a count it is a row: `12G`, `12gg` |
| `^D` `^U` | Half a screen, or that many of them |
| `^F` `^B`, `PgUp`/`PgDn` | A screen |
| `^E` `^Y` | Scroll a line without moving the cursor — until it would go off the edge |
| `H` `M` `L` | The top, middle and bottom row **of the window** |
| `zz` `zt` `zb` | Put the cursor's line in the middle, at the top, at the bottom |
| `[` `]` | Previous / next **turn** |
| `c` `C` | Next / previous **tool call**. A count first: `3c`. The card you land on opens itself |
| `{` `}` | Previous / next **block** |
| `⇥` `za` | *Keep* the **tool card** under the cursor open — all of it, and it stays that way when you move off |
| `/` `?` then `n` `N` | Search, and step through the matches. `n` is *onwards*, so after `?` it goes up |
| `*` `#` | Search for the word under the cursor, forwards or back |
| `v` | Start a selection that motions extend. Again, or on a `V` selection, gives it up |
| `V` | The same by whole lines — both ways, so extending upwards keeps the line you started on |
| `o` | While selecting: the other end of what you have, so you can fix the end you got wrong |
| `gv` | The selection you last gave up, back where it was |
| `iw` `aw` `ip` `ap` `i"` `i(` `i[` `i{` … | While selecting: the word, paragraph, string or brackets the cursor is in. `a` takes the delimiters |
| `y` | Copy the selection, and leave |
| `yy` `Y` | Copy the line |
| `y` + a motion | Copy what it covers: `yw`, `y$`, `y5j`, `yG`. Whole lines when it crosses rows |
| `yi` + an object | Copy one without selecting it first — `yiw`, `yi"` |
| `yc` | Copy the **code block** the cursor is in, without its indent or language line |
| `ym` | Copy the whole **turn** — the question and everything it produced |
| `yp` | Copy this conversation's **directory** — in a worktree, the worktree's path. The one `y` that is not a piece of the transcript |
| `ya` | Copy the entire transcript. Which is why `ya`*w* is the one thing here that is not Vim's — `viwy` is |
| `i` `a` `o` `⏎` | Back to the composer. While selecting, `i` and `a` open a text object and `o` swaps ends |
| `^S` | And back out, the way you came in |
| `Esc` | One thing per press: the selection, then the search highlight, then the mode |

There is no `^V`: a blockwise selection is a rectangle rather than a range, and every consumer of a
selection would have to grow a case for it. The key says so rather than doing nothing. Nothing here
edits, either — no `d`, `x` or `p`, and `c` is a motion here rather than a change. The transcript
is an artefact you take pieces out of.

**The card you are standing on is open.** Everything a fold hides, up to `chat.preview_lines`, for
as long as the cursor is in it — so `c c c` walks a turn's work one call at a time and the rows go
back as you leave. `⇥` is how you keep one. And **reading the last row keeps reading it**: a turn
still running writes below you, and if you are parked at the end you are carried along with it.
Anywhere else you stay exactly where you are — `G` starts following again. See ADR 0057.

## Answering a question — the panel over the composer

What an agent gets when it asks *you* something. One question at a time, `⇧⇥` to go back and change
an earlier answer, and **typing is answering** — it goes into the composer, where you can see and
edit it, and the numbered shortcuts disappear to say a digit is now a character. Every key here is
an ordinary binding against the `neosh.question` buffer kind, so `^Z` lists it and `init.ts` can
move it.

| Key | Does |
|---|---|
| `↵` | Take the option under the cursor, and go on to the next question. On one that takes several, confirm what is ticked. With something typed, send that instead |
| `1`–`9` | Take that option, while nothing has been typed |
| `⇥` | Tick or untick the option under the cursor, on a question that takes more than one |
| `^P` `^N`, `↑` `↓` | Move |
| `⇧⇥` | Back to the previous question |
| type | Your own answer, for when none of them is it. `⌫` back to the options, `^W` a word, `^U` all of it |
| `Esc` | Dismiss. The agent is told nobody answered — which is a thing it can act on, not an error |

A question for a conversation you are not looking at waits rather than taking the screen: the footer
says how many are asking, `^T` is where you go, and it opens when you get there.

## The project panel — `^T`

| Key | Does |
|---|---|
| `↵` | Open a conversation, fold a project, or add one from the `+` row |
| `j` `k` | Move. With a count, that many rows you can land on: `5j` |
| `^D` `^U` | Half a screen; `PgUp`/`PgDn` for a whole one. Not `^F`/`^B` — those stay the archive and hiding the panel |
| `gg` `G` | The ends of the list, and `5gg` / `5G` for the fifth row |
| `>` `<` `=` | Wider, narrower, back to the default. It is the `sidebar.width` setting, so `config.toml` and this key say the same number |
| `n` | New conversation — the same question `^N` asks, about the project the cursor is on |
| `o` | Add a project |
| `f` | Pin a project to the top |
| `J` `K` | Reorder within a group |
| `r` | Rename a conversation |
| `y` | Copy the row's directory — a worktree's path, ready to paste into a shell |
| `p` | Pull that repository from its remote (a git-plugin contribution) |
| `d` | Remove a worktree from disk — its branch stays, and it asks first (a git-plugin contribution) |
| `x` `X` | Archive, delete. On a project heading, `X` takes the project off the list — the only thing that does |
| `a` | The archive — the popup below. Nothing archived is ever a row in *this* panel, and by default not even a count. An `archive.action` contribution, not a key this panel owns |
| `⇥` | On the plan rows: how much of it to show — the limit that binds, every limit, or all of it with the account and the sentence. `usage.sidebar.style` is where it starts, `usage.sidebar` turns it off |
| `?` | The keys for whatever row you are on |
| `Esc` | Back to the composer |

## The archive — `^F`, or `a` in the project panel

What you have finished with, and the one place in the workspace where throwing things away is what
you came to do. **Nothing about it is on screen until you press the key**: no section, no row, no
count — `archive.sidebar = true` if you want the count in the project panel. A popup, and a panel
rather than a picker: it has marks, and every verb means what is marked or — with nothing marked —
the row under the cursor. The strip at the foot says which. See ADR 0060.

Conversations past `RESTORE_LIMIT` are in here too, and behave like any other row: the workspace
brings one in from its file the first time a key names it. The header says how many are `on disk
only` when any are.

| Key | Does |
|---|---|
| `↵` | Put this one back and go to it |
| `u` | Put these back, and stay here. Tidying and travelling are different intentions. Not `^U` — that is half a screen, here as everywhere |
| `X` | Delete these, for good. It asks, and says how many messages and which projects |
| `^X` | Empty it — everything the panel is **currently showing**, so a filter first is how you empty one project's worth. Asks like the delete it is |
| `<Space>` `⇥` | Tick this row, and step down. Tick a run of them with one key repeated |
| `a` | Tick everything showing, or untick it |
| `e` | Copy these conversations to the clipboard, as markdown |
| `y` | Copy this conversation's directory |
| `/` | Filter — on the title, the project and the path. Every word, in any order |
| `s` `S` | The next ordering along; the next grouping along. They are `archive.sort` and `archive.group`, so `config.toml` and these keys say the same thing |
| `j` `k`, `^D` `^U`, `gg` `G` | Move, with counts — `5j`, `12G` — exactly as in the project panel |
| `?` | The keys |
| `Esc` | One thing per press: the marks, then the filter, then the panel |
| `^F` `q` `^C` | Close it |

The settings: `archive.sidebar` (the count in the project panel, off), `archive.sort`,
`archive.group`, `archive.width`, `archive.height`, and three about time — `archive.auto_days` archives what has been idle that long (reversible, so it may happen on
its own), `archive.retention_days` is what counts as old, and `archive.remind` says so once a day.
`archive.sweep` deletes what is older than that, and only when you run it. Nothing in here deletes
on a timer.

## Pickers

| Key | Does |
|---|---|
| `↵` | Choose |
| `^N`/`^P`, `^J`/`^K`, `↑`/`↓` | Move |
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
