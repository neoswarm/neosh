# 0040 — A turn that read six files is one row

**Status:** accepted

## Context

ADR 0033 gave a tool call a card: a state dot, the tool's name, its subject, and — for a call that
only *looked* at something — nothing under it, because the answer to "did the read work" is almost
always "yes, and it was what you would expect". That fixed the bodies. It left the headers.

A real turn opens by reading three files, grepping twice and listing a directory. That is six
cards, which is six rows, which on a forty-row terminal is a sixth of the screen — and every one of
those rows opens with the same filled circle in the same colour saying the same thing. The
information in the block is six names. The block is six rows tall.

Two specific complaints, and they turn out to be the same one:

1. **The dot.** `⏺` is `U+23FA`, a record button. It is wide, it is the leftmost thing on the row,
   and it appears on every card whatever happened. A mark that is on every row of a column is a
   mark the eye stops reading by the third one — so it costs two columns and a lot of ink to say
   "this happened", which is what a card being there already says.
2. **The height.** Six rows for six names.

Put beside the references: `codex` groups a run of reads under one `Explored` header with `└`
continuation rows and merges consecutive reads onto one of them; `claude-code` collapses the whole
run to a single dim line — `Read 3 files, searched for 1 pattern` — with a spinner and a live hint
of the current file while it is going; `pi` draws no marker at all, just `read src/main.rs` in the
tool's colour, and hides the result entirely unless you expand it. Three different programs, one
agreement: **the per-call row is not the unit a reader wants for calls that only looked at
something.**

## Decision

**A mark appears only when the state is news.** A call still out gets `▸`, pulsing, and it is then
the one moving thing in the transcript. A call that failed gets `✗` in red. A call that finished
gets two blanks of the same width. Nothing is lost: the kind is still the colour of the name, the
failure still shows, and the column of names still lines up — the body's rule now sits directly
under the name it belongs to, which it always should have. `Agent.ToolDone` is gone from the
palette, because there is no longer anything for it to paint.

**A run of calls that only looked at things shares one card.**

```text
  Explored  host.rs, cards.rs, diff.rs, fn header, **/*.rs
```

The names rather than a count. `Read 3 files` is one row and `codex`'s `└ Read a, b, c` is two, and
the thing you actually wanted from those six rows was the six names — so the row carries as many as
fit, whole, and `+N more` for the rest. Never half a name: `cards.r…` is a file that does not
exist, and `+2 more` is a fact.

It is called what the tools call themselves when they agree — three `Read`s are still reads, and
inventing a friendlier word for a tool is the thing ADR 0033 already refused to do. `Explored` only
for a mixture, where no one name would be true, and `Exploring` while any of it is still out.

**A run is calls with nothing drawn between them.** Not "calls in the same message", not "calls of
the same kind": if a sentence, a diff, a command's output or another card has been drawn since,
the reader has been told something else and the run is over. Both the live path and the replay ask
the same question the same way — *is this card still the last row of the transcript* — which is why
switching away and back cannot re-fold a conversation differently from how you left it.

**Only calls that looked at something ever share.** A command's output is the answer and an edit's
diff is the answer; there is no summarising either into a list of names. The rule is the one from
ADR 0033, unchanged: [`cards::looks_at_something`], read off the arguments rather than a list of
tool names, so a plugin's tool joins a run tomorrow without anybody adding it to anything.

**`⇥` opens the run into a row per call**, with the whole subject and how much came back:

```text
  Explored  one.rs, two.rs, fn main
  │ Read  crates/neosh/src/one.rs  120 lines
  │ Read  crates/neosh/src/two.rs  86 lines
  │ Grep  fn main  4 lines
```

The short names on the folded row are the loose answer and this is where it is exact. It stops
there — the *contents* of the six files is what the fold existed to keep out of the transcript, and
a single read that was never part of a run is still an ordinary card whose `⇥` gives you the file.

**A call is about what it looked for before where it looked.** A grep is `{pattern, path}` and the
subject used to be the path, so a turn that grepped three different things showed `src`, `src`,
`src`. The pattern is the answer to "what was that call"; the path is a directory the model picked,
usually the one you are already in. Reading and writing name no pattern, so they still get a path.

## Consequences

- `Card` holds `legs: Vec<Leg>` rather than one call. One leg is the ordinary case and renders
  exactly as before; several is a run. `finish_card` resolves `(card, leg)` by id, and a result
  arriving for a leg of a folded run draws nothing, which is the point.
- **A run costs no rows while it happens.** The header is rewritten in place as each call starts,
  so a turn that reads six files never pushes six rows and takes them back. It used to.
- Nothing says on the row that a run can be opened. The `^S` hint strip already says `⇥ expand`
  when the cursor is on a card that has something to show, and a permanent `(⇥ to expand)` on every
  run would be noise on the row this ADR exists to shorten.
- `Glyphs::dot` is gone, replaced by `live` and `fail`. `compaction` draws with a finished call's
  blank, which is what it is.
- The mark and the subject animate differently and deliberately: `Agent.ToolRunning` pulses a
  single glyph, `Agent.ToolLive` sweeps a run of text. A band travelling along the list of things
  being read is legible from across the room; a glyph brightening is not, and a glyph is all the
  mark is.
- Two calls to the *same* file in one run are named twice. `codex` de-duplicates. Left alone: two
  reads of one file is a fact about the turn, and a list that quietly drops one is a list you
  cannot count.

## What was added afterwards: a card says what happened

The rows got shorter and still did not read. `Bash`, `Agent`, `Grep`, `Broken`, `Edit` down the
left of a turn is a list of *mechanisms*, and it asks the reader to know what each of those is
before the transcript says anything. Beside `codex` — `Explored`, `Ran`, `Read`, `Search` — the
difference is not density, it is that one of them is written in English and the other is a call
log.

Three changes, and none of them shows more:

**A card leads with what the call did, when its arguments say.** `Ran cargo test`, `Searched fn
header`, `Edited src/cards.rs`, `Wrote src/glyph.rs`, `Fetched https://…`. Read off the arguments
in the same pass and by the same rule [`Kind::of`] reads the colour — so nothing is looked up in a
table of tool names, and a tool registered tomorrow gets a verb for the same reason it gets a
diff.

This reverses half of ADR 0033, and the other half stands: [`did`] returns `None` for a call
nothing here understands, and then the tool's own name is what shows. A plugin's `frobnicate` is
`frobnicate` for ever. What that ADR was protecting against was neosh *inventing* a friendlier word
for a tool and then disagreeing with it; stating what a call did, from what it was handed, invents
nothing.

**A command's output is folded from the middle.** One line of head, the count of what fell out,
and the rest of the budget spent on the end:

```text
  Ran  cargo test -p neosh-core -- --test-threads 2      9.0s
  │    Compiling neosh v0.1.0
  │ … +5 lines (^S ⇥ to expand)
  │ test cards::body ... ok
  │ test result: ok. 12 passed
```

The head of a command's output is how it started and the tail is what it decided. Folding from the
end kept `Compiling`, `Finished` and a blank line, and put `test result: ok` behind `⇥` — three
rows spent proving the command had begun. Blank rows are skipped at either edge for the same
reason: in a budget of three, a blank line is a third of the card spent on nothing. They are still
counted as elided, because they were there.

**A row of air between one action and the next.** The cheapest structure there is, and the thing
that makes `codex` scannable. Butted together, a header, four rows of output and the next header
are one block with no edges in it. This is the one change here that makes the transcript *taller*,
and it is worth more than the rows it costs — the fold above pays for most of them anyway.

### Consequences

- `… +N more lines` became `… +N lines`. The row now appears in the middle of an output as often as
  at the end of one, and "more" is only true at the end.
- The elision mark is a glyph now (`Glyphs::elision`), because `…` was hardcoded and is not ASCII —
  in `ui.ascii_only` the one row whose job is to say "there is more" was drawn with a character the
  terminal had been told it could not draw. `clip` still has the same leak for a clipped subject,
  which is smaller and not fixed here.
- `Read` and `Searched` are told apart by which argument the call is *about*, the same question
  [`subject`] answers — a grep is not a read of its directory.
- Two tools of the same kind now read identically: a call to `Bash` and a call to `Shell` are both
  `Ran`. Accepted. Which of two shells ran a command is not a question a transcript is asked, and
  the exact name is in the conversation.
