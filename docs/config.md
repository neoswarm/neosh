# Configuring neosh

```sh
neosh init
```

That writes a starter config, a `tsconfig.json`, and the API types — then tells you the path. Open
`init.ts` and start editing.

---

## The one thing to understand

**Your config is a plugin.** `init.ts` gets the entire public API, the same one every plugin uses,
with nothing held back. There is no config-only API and nothing a plugin can do that your config
cannot.

This is why there is no list of "supported config options" anywhere in this document. Anything in
[the API](../plugins/api/src/index.ts) is available to you — see [plugins.md](plugins.md).

## Where things live

```
~/.config/neosh/            $XDG_CONFIG_HOME, or $NEOSH_CONFIG_DIR
├── init.ts                 your config — this is the important file
├── config.toml             values only; the non-code path
├── tsconfig.json           generated, so your editor knows the API
├── types/                  generated, the API types for this exact binary
└── plugins/<name>/         drop a plugin here and it loads
                            plugin.toml + its entry module

~/.local/share/neosh/       installed plugins (a manager's business, not yours)
~/.local/state/neosh/       trust.json — projects you allowed to run code

<your project>/.neosh/      project config; see "Per-project" below
├── config.toml
└── init.ts
```

Same roots Neovim uses, for the same reason — `~/.config/neosh/init.ts` is to neosh what
`~/.config/nvim/init.lua` is to Neovim, and symlinking the config directory into a dotfiles repo
works exactly as you would expect.

```sh
neosh paths     # what those resolve to here, and which of them exist
```

Overridable with `NEOSH_CONFIG_DIR`, `NEOSH_DATA_DIR`, `NEOSH_STATE_DIR`, `NEOSH_CACHE_DIR`, or
`--config-dir`. `neosh --clean` reads and writes nothing at all — use it when reporting a bug.

## init.ts

```ts
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.opt.set("agent.model", "claude-cli/sonnet");

  await neosh.cmd.register("chat.clear", async () => { /* … */ }, {
    desc: "Clear the conversation",
  });
  await neosh.keymap.set("chat", "<C-l>", "chat.clear");
}
```

Keys bind to **command names**, never to callbacks. That indirection is what makes every binding
listable, remappable, and callable by other plugins.

Reload with **`<C-r>`** — no restart. It re-reads every layer from disk, unloads every plugin, and
resets settings back to their defaults first, so deleting a line actually removes its effect. If the
new config does not parse, the running one is left alone and you are told why.

Type checking, and testing what you wrote — see **[testing.md](testing.md)**:

```sh
cd ~/.config/neosh && npx tsc --noEmit
```

The types in `types/` were written by the neosh binary you are running and are refreshed at startup
if they drift, so they always describe the version you actually have.

**If `init.ts` throws, neosh still starts.** The error is reported and startup continues — you need
the editor to fix the editor.

### Built-in commands

The host registers its own commands through the same registry your plugins use, so they are
listable with `neosh.cmd.list()` and rebindable like anything else:

| command | default key | |
|---|---|---|
| `config.reload` | `<C-r>` | re-read config and reload plugins |
| `quit` | `<C-q>` | close neosh |

Both are shown in the status line at the bottom of the screen, which also reports the current mode
and model.

Binding the same key in your config replaces the default — these are ordinary bindings, not reserved
keys.

## The bundled plugins

The sidebar, the model switcher and the git actions ship inside the binary. They are **plugins**, not
features: same manifest, same API, same lifecycle, and nothing in their source that yours could not
do. They exist partly to make that claim testable — `scripts/check.sh` type-checks them against the
published `@neosh/api`, so the moment one reaches for something not public, the build fails.

| Plugin | What it does | Keys |
|---|---|---|
| `sidebar` | Projects, conversations, changes, model, usage | `<C-b>` hide, `<C-t>` thread list, `<C-n>` new |
| `model` | Model and reasoning-effort switchers, and the footer | `<C-p>`, `<C-e>` |
| `git` | Status, branches, commits, diffs, worktrees | `<C-g>`, `<C-d>` |
| `palette` | Everything by name, with its binding | `<C-k>`, `<F1>` |
| `approvals` | Asks before the agent does something gated | — |

Press `<F1>` for the complete list; it is read from the registry, so it is never out of date.

Turn one off:

```toml
[options]
"plugins.disabled" = ["sidebar"]
```

Or replace it: drop a plugin of your own in `~/.config/neosh/plugins/`. Yours loads *after* the
bundled ones, so registering the same command or key wins.

`neosh --clean` loads none of them, which is what makes it useful for answering "is this neosh or is
it my setup".

### Conversations and projects

The sidebar groups conversations by the directory they were opened in. There is no project registry
to maintain: opening a conversation somewhere else *is* how a second project appears, and
`git.worktree.list` opens one in another worktree. `neosh.git` answers about whichever conversation
is active, so switching conversation switches which tree you are looking at.

Conversations are saved as you go — one file each under `~/.local/state/neosh/sessions/` — and
restored at startup, in the one you were last in. `--clean` neither reads nor writes them.

Pin the projects you live in with `f` and they move to the top of the list, marked with a heart;
`J`/`K` reorder within a group, and a pinned project stays listed after its last conversation is gone,
so `Enter` on it starts a new one. The arrangement is saved under
`~/.local/state/neosh/plugin-state/` — not in your config, because an editor that rewrites the file
you hand-edited because you pressed a key is one you stop trusting with it.

While a turn is running the row shows how long it has been running, next to the spinner. "Working"
with no clock is indistinguishable from wedged.

In the panel (`<C-t>`): `↑`/`↓`, `j`/`k` or `^N`/`^P` move, `Enter` opens or folds, `Space` folds,
`f` pins, `J`/`K` reorder, `n` new conversation here, `x` archive, `u` unarchive, `X` delete,
`r` rename, `?` every binding, `Esc` leaves. `J`/`K` work from a conversation row too — they move
the project it is in, because you are looking at the project when you are looking at what is inside
it. The foot of the panel lists the keys for whatever the cursor is on; set `sidebar.hints = false`
once they are in your fingers.

### Archiving, and the one verb that deletes

`x` archives. Everything is kept — every message, the file on disk — it simply leaves the list you
work in and appears under `ARCHIVED` at the foot of the panel, which only exists while there is
something in it and starts shut. `u`, or opening it, brings it back to the top.

It asks nothing, because it takes nothing away. Charging a confirmation for a reversible action is
what teaches you to dismiss confirmations, which is how the one that matters stops working.

`X` deletes: the file goes and there is no undo, so it asks — unless the conversation has nothing in
it to lose, or you have set `ui.confirm_destructive = false`. Archiving the conversation you are in
moves you to the most recently used other one, or starts a fresh one if there is no other.

For plugins: `session.archive(id)`, `session.archive(id, false)`, and
`session.list({ includeArchived: true })`. Plain `list()` leaves them out.

```toml
[options]
"sidebar.open" = true        # show it at startup
"sidebar.width" = 34
"sidebar.hints" = true       # the contextual key strip at the foot of the panel
"sidebar.refresh_ms" = 4000  # a workspace re-read per tick
```

### Keys are yours

Every binding in neosh — including the ones that ship — is an ordinary entry in the same table your
config writes to, bound to a *command name* rather than a callback. Setting the same key replaces
what was there.

```ts
await neosh.opt.set("mapleader", ",");            // set it before you use it
await neosh.keymap.set("chat", "<leader>pa", "project.open");
await neosh.keymap.set("chat", "<leader>pp", "sidebar.focus");
await neosh.keymap.del("chat", "<C-o>");           // and drop the default if you want
```

`<leader>` is substituted when the binding is made, not when the key is pressed — the same as
Neovim, and for the same reason: a binding that silently moved when you changed `mapleader` halfway
down your config would be impossible to reason about.

Nothing ships bound to `<leader>`, which is deliberate: the default leader is `\`, and a leader with
bindings on it is a character you can no longer type immediately. When you do bind a sequence, a
prefix you start and do not finish is typed literally after `timeoutlen` milliseconds — so `,pa`
does not cost you the comma key.

```toml
[options]
mapleader = ","
timeoutlen = 500   # how long to wait for the rest of a sequence
```

`^K` searches every command by name, and `F1` lists every binding — both read the live registry, so
a plugin loaded five minutes ago is in them without anything having been written down.

### Adding a project

`^O` anywhere, `o` in the project panel, or `Enter` on the `+ Add project` row. It offers the
worktrees of the repository you are in that are not open yet, and drops through to a path field for
anything else. It is the `project.open` command, so `^K` finds it and `<leader>pa` can be it.

The path field completes as you type: the list under it is the directories matching what you have
so far. `<Tab>` takes the highlighted one into the field so you can keep descending, `<C-w>` goes
back up a whole segment rather than a character, and `<CR>` accepts either the highlighted row or
exactly what you typed — because the directory you want may not be one it offered. `~` expands, and
a path with no `/` completes against the conversation's own directory.

### Confirmations

Anything that cannot be undone asks first: deleting a conversation removes its file from disk, and
`git worktree remove` does not put a directory back. The cursor starts on the answer that changes
nothing, because `<CR>` is reflex by the second time you have seen a dialog.

It asks *conditionally*. A conversation you have not said anything in yet has nothing to lose, so
`x` just closes it — a dialog for that is friction that teaches you to dismiss dialogs. Nothing that
only changes what is on screen ever asks.

```toml
[options]
"ui.confirm_destructive" = true
```

### Picker keys

Every picker, prompt and completion list reads the same settings, and they take Neovim notation.
More than one key can mean the same thing:

```toml
[options]
"ui.keys.next" = "<Down> <C-n> <C-j>"
"ui.keys.prev" = "<Up> <C-p> <C-k>"
"ui.keys.page_down" = "<PageDown>"
"ui.keys.page_up" = "<PageUp>"
"ui.keys.first" = "<C-Home>"
"ui.keys.last" = "<C-End>"
"ui.keys.accept" = "<CR>"
"ui.keys.dismiss" = "<Esc> <C-c>"
"ui.keys.complete" = "<Tab> <Right>"
"ui.keys.clear" = "<C-u>"
"ui.keys.delete_word" = "<C-w>"
```

A widget claims these **window-scoped** while it is open, so they outrank global bindings and are
released the moment it closes. That is what makes `^N` move in a picker even though `^N` is
otherwise "new conversation" — without it, filtering a model list by typing `n` with control held
would drop you into a new conversation with the picker gone.

### Writing, selecting and copying

The composer is a text field, not a line that gets appended to. `←`/`→` move by character and
`^←`/`^→` by word; `Home`/`End` reach the ends of the line and `^Home`/`^End` the ends of the draft.
Hold shift with any of them to select. `S-CR` breaks the line instead of sending, so a pasted
snippet stays one message. `^W` deletes the word behind the cursor and `^U` clears back to the start
of the line.

`^C` copies when something is selected and clears the draft when nothing is — the two cases cannot
both be true, which is what lets one key carry both jobs. `^X` cuts, `^A` selects everything. Your
terminal's own paste works: it arrives as a bracketed paste and lands at the cursor.

### Reading the transcript

`^S` moves the keyboard into the transcript, which is where the text you actually want to keep
lives. It is a mode, not a focus change, because the keys mean different things there. The status
line says `reading` while you are in it, and the row under the composer says what the keys do —
including, once an operator is down, what can follow it.

The keys are vi's, chosen rather than invented:

| | |
|---|---|
| `hjkl`, arrows | move |
| `w` `b` | by word |
| `0` `$` | ends of the line |
| `gg` `G` | ends of the transcript |
| `^D` `^U` | half a screen |
| `^F` `^B`, `PgUp`/`PgDn` | a screen |
| `zz` `zt` `zb` | put the cursor's line in the middle, at the top, at the bottom |
| `[` `]` | previous / next **turn** |
| `{` `}` | previous / next **block** |
| `/` `?` | search forward / backward |
| `n` `N` | next / previous match |
| `v` | start a selection that motions extend |
| `V` | select this whole line |
| `y` | copy the selection, and leave |
| `yy` `Y` | copy the line |
| `yc` | copy the **code block** the cursor is in |
| `ym` | copy the whole **turn** — the question and everything it produced |
| `ya` | copy the entire transcript |
| `i` `a` `o` `⏎` | back to the composer |
| `Esc` | drop the search highlight, then leave |

Two of those are not in any editor, because a transcript has two things a file does not. `[`/`]`
step between turns, found from the bar drawn down the left of a question rather than from a
remembered list — a remembered one would be wrong in exactly the case you need it, which is
scrolling back through a long conversation. And `yc` takes the code block the cursor is in, without
the indent the renderer added or the language line above it. That one is the reason the mode
exists: an answer with a command in it is worth very little if getting the command means selecting
it by hand across a wrapped line.

Searching is incremental — hits light up as you type — and case-insensitive unless the query has a
capital in it. The composer is borrowed to type into, and your draft comes back when the search
closes, however it closes.

Copying uses OSC 52, which travels back through the terminal connection — so it reaches the
clipboard on the machine you are sitting at, not the one neosh is running on. Terminals that do not
implement it ignore it silently; there is no way to detect support.

### The strip along the bottom

```
 chat  ask ⇧⇥  main  ███░░░░░ 38% of 200k  ↑12k ↓4k          claude-opus-5 ^P  High ^E
```

Left is what the conversation is *spending* — the permission mode, the branch, how full the context
window is, tokens in and out. Right is what it is spending it *on*. They are separated because they
change at different rates, and interleaving two things that move at different speeds makes both
harder to read.

The context meter is drawn from the moment a conversation opens, empty, rather than appearing once
a turn has been spent. The one time you most want to know how much room a model has is *before*
deciding what to do with it, and a 200k window and a 1M one are different tools. The bar is for the
shape of the answer — plenty of room, getting full, nearly out — and the number beside it for the
rest; the colour changes past 70% and again past 90%.

When the strip is too narrow for everything in it, whole segments are dropped, least important
first. Nothing is truncated: half a token count is a wrong token count, and a bar cut short reads
as a level nothing is at.

Every entry carries the key that changes it, immediately after it. That is the whole rule: a key is
memorable once it has been seen beside the thing it does, and a legend somewhere else is a second
place to look for something that is already on screen. It also means those keys are *not* repeated
on the shortcut row below — saying it twice costs a row and teaches nothing the first place did not.

There is no shortcut row below by default, for the same reason. It carried `^T`, `^N` and `^K`,
which are in the sidebar's own footer two rows away, and `F1`, which is on the row it points at.
What the duplication actually bought was one fewer line of transcript and a composer pressed
against the status strip. `ui.hints = true` brings it back if you want it; the exception is reading
mode, which draws its own row whatever the setting says, because those keys are live, different,
and written down nowhere else.

Plugins own the strip's contents:

```ts
await neosh.status.set("model", { text: "opus", keys: "^P", align: "right", priority: 10 });
```

### The row under the composer

Under the field you type into is a row of shortcuts: `⏎ send`, `^P model`, `^T conversations`, and
so on. It is not a hard-coded list. Every entry is registered by whoever owns the feature:

```ts
await neosh.hint.set("model", { keys: "^P", label: "model", priority: 10 });
```

Which means the row is always true. A plugin in `plugins.disabled` takes its shortcut with it,
rather than leaving a key advertised that no longer does anything — and a plugin you install adds
its own without neosh knowing it exists.

Entries sort by `priority` and are dropped from the end when the terminal is too narrow, so the
lowest priority is the one you would most want kept. Nothing is ever cut in half: half a shortcut
reads as a key that exists and does something else. `ui.hints` is off by default, so the row is
there for a plugin that has something worth putting on it rather than by default; the full list is
on the help key either way.

### Saying something while it is working

Typing while a turn is running does not start a second turn and is not refused. It is **steering**:
the message is held, shown under the composer, and taken into the running turn at the next gap —
between one round of tool calls and the next, or in place of the turn ending. The model sees it
while it is still working, and can change what it does next.

```
  ⏵ read_file  src/main.rs
✳ Working…  8s  ·  1 queued  ·  esc to interrupt
```

It is not injected into the provider stream, because a stream in flight cannot be interrupted
without discarding what has already been generated. The gap between rounds is the earliest honest
moment.

It does not appear in the transcript until it has actually been said — a transcript that shows a
question nobody has been asked is a transcript that is lying. And if the turn ends before there is
a gap, the message becomes the next turn rather than being quietly dropped.

Drivers that run their own loop (`claude-cli`, `codex-cli`) have exactly one round trip per turn,
so steering them means the message lands as the next turn. That is a property of those CLIs, not a
setting.

### What the agent may do

The footer says what the agent is allowed to do without asking, and `⇧⇥` changes it:

| | |
|---|---|
| `ask` | prompt before writing, running or connecting. The default |
| `allow-listed` | only what `config.toml` already permits |
| `full access` | no prompts |
| `deny` | refuse everything; read-only |

**Workspace containment applies in every mode, including full access.** A path outside the
workspace is refused whatever the setting says: "full access" means not being asked, not reaching
the rest of the disk.

`⇧⇥` opens a list rather than cycling, because full access is one keystroke from `ask` in any cycle
and arriving there by holding a key down is exactly the accident worth designing against.
`permission.cycle` is bound to nothing by default, for people who want it.

The mode is for this session only and is never written to a file. A mode you switched on to get
through one task should not still be on next week — the way to make it permanent is
`[permissions] mode = "…"`, which is a thing you did on purpose.

### How an answer is drawn

Answers are rendered as markdown as they arrive: headings lose their hashes, `- ` becomes a bullet,
fenced code is indented under its language, and `**bold**` / `*italic*` / `` `code` `` /
`[text](url)` have their markers **removed** rather than shown. A terminal can draw bold; drawing
the asterisks as well is showing the same thing twice.

The rendering is incremental, and the rule is that a block is settled once it cannot change — a
paragraph followed by a blank line, a heading with its newline, a fence with its closing
back-ticks. Settled blocks are drawn once and never touched again; only the trailing partial block
is redrawn per tick. Re-parsing the whole answer on every token is quadratic in its length, and the
place that would show is the tail of the long answer you actually care about.

Tables are laid out in columns, with alignment honoured and a rule under the labels rather than a
box around everything — the job is separating the labels from the data, and a full grid spends four
times the ink saying it. A table too wide to line up becomes one block per row instead:

```
  Provider  Kind                 Key
  ──────────────────────────────────
  Claude    plan                none
  OpenAI    api key  $OPENAI_API_KEY
```

Columns running off the right edge would mean guessing which value belongs to which label, which is
worse than not having a table.

One thing is deliberately not done: **no syntax highlighting inside fences**. Doing it properly is a
grammar per language; doing it with a regex over keywords colours the wrong words in exactly the
code you are reading closely. A fence gets one colour and its language in the corner.

`chat.markdown = false` shows exactly what the model sent, which is what you want when the question
is "what did it actually say".

### Theme and motion

```toml
[options]
"ui.theme" = "dark"       # or "light"
"ui.motion" = true        # text that moves while something is happening
"ui.hints" = false        # a shortcut row under the composer; off, the sidebar says the same keys
"ui.ascii_only" = false   # for terminals without a decent font
"ui.nerd_font" = false    # brand glyphs for providers, where the font has them
```

Motion is reserved for one thing: *something is happening and you cannot see it yet*. While a turn
is in flight the working line sweeps — a band of brightness travelling along the word — because a
still screen and a wedged process look identical, and a sweep reads as aliveness at the edge of
vision without asking to be looked at. `Status.Streaming` sweeps; `Status.Pending` pulses, for
waiting on something outside the program.

The **frontend** does this, on its own clock, over whatever the core last wrote — so a shimmer costs
nothing above the terminal boundary and stops the moment the animated row scrolls out of sight.
Without truecolor the same band renders as bold rather than as nothing. `ui.motion = false` removes
the movement and keeps the colour; it never makes text disappear. See
[ADR 0025](adr/0025-motion-belongs-to-the-frontend.md).

The theme is a set of semantic groups — `Status.Working`, `Git.Added`, `Meter.Fill` — that plugins
link to rather than choosing colours. Override any of them from `init.ts` and a theme switch will
leave yours alone:

```ts
await neosh.hl.define("Status.Working", { fg: "#ff00ff", bold: true });
```

Motion is one shared 100 ms clock and costs about 1% of a core and under 1 KiB/s, which is why it
stays on over SSH. See [ADR 0019](adr/0019-motion-and-the-visual-language.md) for the measurements.

### Approvals

`permissions.mode = "ask"` prompts before the agent runs a command or reaches the network. Reading
files inside the workspace is allowed in every mode but `deny` — the point of `ask` is the things
that leave the project.

An answer of "allow for this session" is held **in memory only**; making it permanent is a line in
`config.toml`, not a keystroke. With the `approvals` plugin disabled, `ask` mode refuses anything it
would have prompted about rather than allowing it.

### Choosing a model

`<C-p>` opens a picker in two panes: your providers on the left, the models each one serves on the
right.

```
Model                                                              ⇧⇥ providers
> 
 PLANS                  │ ❯ Claude Opus 5         Frontier  Most capable for complex work
  ✳ Claude            ✓ │   Claude Fable 5        Frontier  Long-form writing and voice
  ⬢ Codex             ✓ │   Claude Sonnet 5       Balanced  Best for everyday tasks
 API KEYS               │   Claude Haiku 4.5      Fast      Fastest, for quick answers
  ✳ Anthropic         ! │   ▸ 5 superseded
 LOCAL                  │
  ▲ Ollama              │
────────────────────────┴──────────────────────────────────────────────────────────
 ↵ use   ^S sign in   ^R refresh   ^A add   ^D remove   esc close
```

The rail is grouped by **what a turn costs you**, which is the distinction one flat list could not
make:

| | | |
|---|---|---|
| **PLANS** | a subscription you already pay for | no key, nothing stored |
| **API KEYS** | billed per token | see [API keys](#api-keys) |
| **LOCAL** | on this machine | costs nothing, needs nothing |

The marker at the right edge of each rail entry: `✓` it will work, `!` it needs a key or its CLI is
missing, `⨯` no driver provides it. Superseded models fold behind a count — reachable, out of the
way, and opened automatically while you are filtering.

Typing filters the model list. `⇧⇥` and `←` reach the provider rail, `⇥` and `→` come back, and the
title row says which of the two the key would take you to right now — a legend can only tell you a
key exists, and a second pane with no visible way into it is a pane nobody finds.

The picker's own actions are **chords**, not letters:

| | |
|---|---|
| `^S` | sign in to whatever the rail is on |
| `^R` | ask that endpoint again — after adding a key, gaining access, or starting a local server |
| `^A` | add a model by id, for something the catalogue has never heard of |
| `^D` | remove one you added |

Chords because every bare letter the picker takes is a letter the filter can never contain. These
used to be `s`, `r`, `n` and `d`, which between them made it impossible to search for "sonnet".

`^A` asks twice — the id, which goes on the wire and has to be exact, and the name, which is what
you will read in the list forever afterwards. Nothing validates the id, because nothing here can:
whether an endpoint serves it is a question only the endpoint can answer, and it answers on the
first turn. What you add is kept per provider in plugin state rather than in configuration — it is
a note about *this machine's* access, not a decision worth committing to a repository. `^D` takes
it back out, and refuses on a model the provider serves, which is not yours to delete.

`ui.nerd_font = true` swaps the geometric provider marks for real brand glyphs. Off by default and
deliberately not detected: a terminal cannot be asked what its font contains, and guessing wrong
draws a row of boxes, which reads as a broken program rather than as a missing font.
`ui.ascii_only = true` reduces them to letters.

#### What you see before you sign in

Every provider lists models whether or not you have a key for it, because the list is *how you
decide whether to sign in*. Those entries come from a written-down catalogue: enough to pick from,
and never claimed to be complete.

The moment a key is present the endpoint's own `/v1/models` decides what exists, and the catalogue
keeps only the part it is authoritative about — the display name, the rung and the one-line
description, none of which appear in any `/v1/models` response. A model the endpoint does not
return is dropped, even if the catalogue lists it: offering one whose first turn is a 404 is worse
than offering nothing.

Providers that serve other people's models — Groq, Cerebras, Together, Fireworks — have no written
catalogue on purpose. Their lineups change weekly, so a list here would be wrong faster than it was
useful, and the picker says "discovery needs a key" rather than showing an empty pane.

#### When the model you used last has stopped working

Reopening a conversation restores the model it was using. If that model can no longer authenticate
— an API-key provider, on a machine that does not have the key — neosh moves to one that can, keeps
the product line where it can (`anthropic/claude-opus-5` becomes the Opus on your plan, not
whatever sorts first), and says so in one line.

A stored selection is a *record of what was used*, not an instruction. What you put in
`agent.model` is an instruction, and is left alone even when it cannot authenticate: quietly using
a different model would be worse than the error you get when you send.

### Moving along the ladder

Every model in the catalogue sits on one of three rungs — **Frontier**, **Balanced**, **Fast** —
because every lineup worth switching between has three: Opus/Sonnet/Haiku, gpt-5/mini/nano,
Pro/Flash/Flash-Lite.

```
<A-Up>      model.upgrade      one rung up, same provider
<A-Down>    model.downgrade    one rung down
            model.line opus    the current model in a line, wherever it is reachable
```

`upgrade`/`downgrade` hold the provider fixed on purpose: "give me something cheaper" is a question
about the model, and answering it by also changing your billing would answer a question nobody
asked. Neither wraps — at the top, `upgrade` says so rather than dropping you to the cheapest thing
in the catalogue.

`model.line` resolves a *product line* to the one in it that has not been superseded, which is what
people mean by "use Opus": not a pinned id that goes stale.

These are ordinary commands. Rebind them like anything else:

```toml
# in init.ts
await neosh.keymap.set("chat", "<C-A-k>", "model.upgrade");
```

### Reasoning effort

`<C-e>` picks reasoning effort and whatever else the driver exposes. Those are not a fixed list:
they arrive as `ProviderOptionDescriptor`s attached to the model, so a provider plugin that invents
a knob gets a working picker for it with no change to the switcher.

Switching between two models that share an option keeps your setting; switching to one without it
drops the setting rather than sending a value the driver would reject.

### Plans

Two plans ship: `claude-cli` and `codex-cli`. Both are the vendor's own CLI, driven as a provider.
Neither needs an API key, neither stores a token, and neither is offered unless the program is
actually on your `$PATH` — a provider whose every turn would fail with "no such file" is worse than
one that is not listed.

They are **agent drivers**: `claude -p` and `codex exec` run their own loop, with their own tools,
their own sandbox and their own approval policy. So neosh's tool registry is not in play on those
turns and `tool.pre` hooks observe rather than gate. Two consequences worth knowing before you pick
one:

- Neither driver overrides the CLI's sandbox. If a turn stops to ask for approval it wants, the
  place to change that is `codex`'s or `claude`'s configuration, not neosh's. Loosening somebody
  else's security policy to make turns run more smoothly is not a decision a wrapper should make.
- `codex exec --json` has no token deltas — the protocol reports an assistant message when it is
  complete. So a Codex turn shows its tool activity live and then its answer all at once. That is
  the CLI's shape, not a shortcut.

Three more come from one driver, because Cursor's `cursor-agent`, xAI's `grok` and Google's
`gemini --experimental-acp` all speak the **Agent Client Protocol** — JSON-RPC over stdio. A fourth
ACP agent is a line in the catalogue rather than a file of code.

ACP has one limitation worth knowing before you pick one. An ACP agent asks its *client* for
permission, which means neosh has to answer, and it currently answers from the permission mode
alone: under **full access** it takes the agent's allow-once option, and under anything else it
refuses and says so. Routing those prompts into the same approval you get for a built-in tool call
is the next step and needs the request to travel out of the driver and an answer to travel back.
Until then the behaviour is conservative and stated rather than quietly permissive.

Adding a plan behind a CLI that speaks neither is a driver plus a line in the catalogue.
`AuthRef::Cli { program, login }` is the whole contract: name the program, name the command that
logs in, and the account split, the rail grouping and the "not installed — run this" message all
follow.

A provider whose CLI is not installed still lists what it offers, greyed out, with the command that
would fix it at the top. What a provider serves is how you decide whether installing it is worth
your afternoon, and an empty pane answers that with silence.

### API keys

Only for the **API KEYS** group. A plan signs itself in — if the Claude entries are missing, the
`claude` CLI is not on your `$PATH`, and `provider.auth` says so rather than hiding the provider.

`provider.auth` lists every configured provider and the state of its account; `provider.key` (built
in, so it works with `--clean`) takes a key for the provider you are on. Picking a model that has no key
offers the same prompt rather than failing later, when the failure is between you and the question
you were asking.

**The prompt is the host's, not a plugin's.** Nothing is echoed — you get one bullet per character —
and no plugin ever sees what you typed. There is no API call that returns a key, only ones that
report where it came from, so a plugin cannot leak what it cannot read. See
[ADR 0022](adr/0022-credentials-never-cross-the-plugin-boundary.md).

Where it goes:

| | Survives a restart | |
|---|---|---|
| OS keychain | yes | used automatically when `secret-tool` or `security` is installed |
| this session | no | the fallback when neither is — say, over SSH with no session bus |

neosh tells you which happened. It never writes a key to a file it controls.

Four places a key is looked for, in order — one you typed this session, the keychain, a helper
program, then the environment. Typed wins, because you typed it after seeing what the environment
gave you.

A helper is the answer if you already keep secrets somewhere:

```toml
[[providers]]
id = "anthropic"
driver = "anthropic"
display_name = "Anthropic"
base_url = "https://api.anthropic.com"
auth = { kind = "command", argv = ["pass", "show", "anthropic/api-key"] }
```

Anything on `$PATH` works — `pass`, `op read`, `bw get`, `gopass`, your own script. Only the first
line of its output is used, so tools that print metadata after the secret are fine.

### Git, and the prompts behind it

`git.branch.new` asks what you are about to work on, has a model name the branch, and shows you the
name **before** creating it. `git.commit` writes a message from the staged diff and shows you that
before committing.

Every prompt is a setting, in two layers:

```toml
[options]
"git.branch.prefix" = "feature/"
"git.branch.instructions" = "Start with the Jira key when the message mentions one."
"git.commit.instructions" = "Follow Conventional Commits."
```

`*.instructions` is appended to the built-in prompt — the common case. `*.prompt` replaces it
entirely, for when appending is not enough; it must still ask for JSON with the documented keys
(`branch`, or `subject`/`body`). Instructions still apply on top of a replaced prompt, so a
per-project rule in `.neosh/config.toml` layers onto your own prompt.

```toml
[options]
"gen.model" = "anthropic/claude-haiku-4-5"   # what writes names and messages
```

`gen.model` is separate from `agent.model` on purpose: naming a branch does not need a frontier
model. Empty uses the conversation's model.

Repository *writes* — branch, checkout, stage, commit, worktree — need `permissions = ["vcs_write"]`
in a plugin's `plugin.toml`. That is a plugin permission, not the agent's permission mode: those
govern what the **model** may do, and the model does not reach git this way. It reaches it through a
registered tool, gated like every tool.

## config.toml

For setting values without writing code. It can express a subset of what `init.ts` can; it exists so
that "use this model" is one line, and so a settings UI has a file it can rewrite.

```toml
[agent]
model = "anthropic/claude-opus-5"

[permissions]
mode = "ask"                 # ask | allow_listed | deny
allow_commands = ["cargo"]

[options]
"chat.show_thinking" = true

[[providers]]
id = "local"
driver = "openai-compat"
display_name = "llama.cpp"
base_url = "http://localhost:8080/v1"
auth = { kind = "env", var = "MY_API_KEY" }

plugin_dirs = ["~/src/neosh-plugins"]
```

Unknown keys are an **error**, not ignored. A typo that is silently skipped looks exactly like a
setting that does not work.

`auth` names *where a key is*, never a key: `{ kind = "env", var = "…" }`,
`{ kind = "command", argv = [...] }`, `{ kind = "cli", program = "codex", login = "codex login" }`
for a plan behind a vendor CLI, or `{ kind = "none" }` for a local endpoint that wants none.
Nothing writes a secret to this file, and nothing reads one from it.

## Options

Options are declared, typed, and owned. You cannot set one that does not exist, and you cannot set
the wrong type — both are errors that name the problem.

```ts
await neosh.opt.set("chat.show_thinking", true);
const n = await neosh.opt.get<boolean>("chat.show_thinking");
await neosh.opt.all();                    // everything declared, with types and defaults
neosh.opt.onChange((e) => { /* … */ });   // fires for every option, not just yours
```

Built in:

| Option | Type | Default | Effect |
|---|---|---|---|
| `agent.model` | string | `""` | `instance/model`. Empty picks the first that works. |
| `agent.system_prompt` | string | `""` | Replaces the built-in prompt when set. |
| `chat.show_thinking` | bool | `false` | Stream reasoning into the chat buffer. |
| `chat.show_tools` | bool | `true` | Show a line when a tool runs. Errors always show. |
| `gen.model` | string | `""` | Model for branch names, commit messages and titles. Empty uses `agent.model`. |
| `plugins.disabled` | list | `[]` | Bundled plugins not to load. |

That list is short because every entry does something. Plugins declare their own, through the same
call the built-ins use:

```ts
await neosh.opt.declare({
  name: "myplugin.enabled",
  type: { type: "bool" },
  default: true,
  description: "…",
});
```

Once declared, it is settable from `config.toml` like any other — including by a user who set it
*before* your plugin loaded. Held values are applied the moment the declaration arrives.

## Plugins

Anything in `~/.config/neosh/plugins/<name>/` with a `plugin.toml` loads automatically. To load from
elsewhere, from `init.ts`:

```ts
await neosh.rtp.add("~/src/my-plugins");
```

This works because **your config runs before plugin discovery** — it is the thing that decides what
else loads. `rtp.add` after discovery is an error rather than a silent no-op.

## Per-project

`<project>/.neosh/config.toml` is committable and applies to anyone who opens the repository.

Because a config file in a repository you cloned is code someone else wrote, it is split by what it
can do:

**Applies immediately.** `model` — it can only name a provider instance *you* already configured, so
the worst it achieves is picking a different one of your models. `[permissions]` — and only to make
things **stricter**; a repository cannot grant itself `curl`.

**Waits for `neosh trust`.** `.neosh/init.ts`, `plugin_dirs`, `providers`, `[options]`,
`system_prompt`. Anything that runs code or widens what the agent may do.

```sh
neosh trust            # prints the files, then approves them
neosh trust --list
neosh trust --revoke
```

Trust is keyed on the **contents of every file under `.neosh/`**, not on the path. Editing any of
them — including a module `init.ts` imports — revokes it automatically, so a `git pull` cannot
quietly change what runs on your machine. You will be told, and you re-approve after reading the
change.

`neosh init --project` scaffolds `.neosh/` in the current directory.

## Precedence

Later wins:

1. built-in defaults
2. `~/.config/neosh/config.toml`
3. `<project>/.neosh/config.toml`
4. `~/.config/neosh/init.ts`
5. `<project>/.neosh/init.ts`, if trusted
6. command-line flags

Flags last, so `--model` always takes effect.

## Timers

```ts
const stop = neosh.timer.every(1000, () => { /* … */ });
ctx.subscriptions.push(stop);          // cancelled when your plugin unloads

const redraw = neosh.timer.debounce(150, () => render());
neosh.agent.onToken(redraw);           // runs once, 150ms after the tokens stop
```

`setTimeout`, `setInterval`, `clearTimeout` and `clearInterval` exist as globals too, for ported
code. Prefer `neosh.timer` — the globals cannot know who called them, so they are **not** cancelled
when your plugin unloads, and an interval that outlives its plugin doubles up on every reload.

Repeating timers have a 1 ms floor, so `setInterval(f, 0)` is `setInterval(f, 1)`. One-shots are
not clamped.

## What the runtime gives you

The plugin runtime is a bare `deno_core` — a sandbox by construction, not by policy. You get the
ECMAScript built-ins, `console`, `queueMicrotask`, `Deno.core`, and the timers above. You do **not**
get `fetch`, `TextEncoder`, or any filesystem access. Anything with an effect goes through the neosh
API, which is what makes hooks and permissions meaningful.

## Not yet

- **`:set` writing back to disk.** The machinery knows which options you changed; nothing persists
  them.
- **Buffer- and window-local options.** Global only for now.
- **A plugin manager.** `rtp.add` and `plugins/` are the manual path; a manager is a plugin, and
  nothing in the core needs to change for one to exist.
- **A command line.** Commands are invoked by key, from the palette (`<C-k>`), or from code; there
  is no `:` prompt yet.
- **Reload closing what a plugin opened.** A float opened during `activate` opens again on reload,
  and an in-flight turn keeps streaming. Use `ctx.subscriptions` for anything that should not
  outlive your plugin.
