# 0057 — A turn is something you can watch

**Status:** accepted

## Context

ADR 0033 made a card a row until you ask for more. ADR 0040 made a run of calls of one kind one
row. ADR 0051 made a stack of commands keep the last one's output. Every one of them was about the
same thing — a transcript you read *afterwards*, where the turn is over and the question is "what
did it do", and the answer is the shortest true account of it.

None of them is about the other half, which is a turn that is still going and a person watching it.
Three things go wrong there, and they compound:

**Every command names the directory it is already in.** A vendor CLI that cannot keep a working
directory between calls writes each of them as `cd /home/you/projects/thing && cargo test`.
`subject_of` takes the first line and clips it to whatever the header has left, so the card reads
`Ran  cd /home/you/projects/th…`. Folded into a stack it is worse: `short_command` stops at the
`&&`, so a stretch of the turn that ran six different things folds to `cd, cd, cd`. The one part of
that row the reader already knows — the conversation has a directory, the sidebar names it, and
every path on every other card is drawn relative to it — is the only part that fits.

**Reading is a place, and the agent walks away from it.** `^S` is a cursor in the transcript and
`reading_jump` pins `top_line` on every motion, which is what makes it a place. A turn still
running writes rows below that place: they land off the bottom of the window and nothing says so.
So watching a turn while reading it was `^S`, `Esc`, look, `^S` again — per tool call.

**Nothing steps card to card.** `[`/`]` is a whole turn, which is far too coarse to inspect what a
turn *did*. `{`/`}` is a blank-line block, and since ADR 0050 took the blank row off the top of a
card, a run of nine cards is one block. That leaves `j`, and `⇥` on each one, and `⇥` again to put
it back.

Together those are a transcript that reports a turn accurately and cannot be used to *follow* one.

## Decision

**A command is named by what it does, never by how it got there.** A leading `cd <somewhere> &&`
— or `;`, and repeated, because `cd a && cd b && make` is the same preamble said twice — is taken
off before the command is named, everywhere a command is named. Only that shape: `cd src` on its
own is genuinely a `cd` and says so, `cd x || y` is a decision rather than a preamble, and a `cd`
anywhere but the front is part of what was run. Nothing is lost — an opened stack gives every
command back in full, which is where you go when the question is *which* directory.

**Reading at the end follows the end; reading anywhere else does not move.** The same distinction
an unscrolled window already makes, and for the same reason: parked on the last row you are
watching, half way up a diff you are reading, and the second one must not be dragged away from what
it is looking at. There is nothing to turn on and one `G` to start following again. Not while
selecting or searching — both of those are two positions, and moving one of them on the agent's
behalf changes what the reader is holding without anything on screen having been aimed at.

**The card the cursor is in is open.** ADR 0049's rule, one surface along: the row you are *in*
stays unfolded, and only ever one row — the cursor is what asks, everywhere else. A turn that ran
nine commands is nine rows you walk down, each showing what it printed as you arrive and giving the
rows back as you leave. `⇥` still exists and now means something it could not mean before: *keep*
this one, all of it, when the cursor goes.

Which needs a third state rather than a second one, and `cards::Open` is it — `Folded`, `Preview`,
`Full`. *How much does a card cost when nobody is looking at it* is a setting and the answer has to
be small. *How much does the card I am looking at right now show* is the cursor's question, it is
asked about exactly one card, and the answer costs nothing that moving off the card does not give
straight back.

**A preview is bounded.** `chat.preview_lines`, default 16. A card that read a nine-hundred-line
file would otherwise drop nine hundred rows under the cursor on the way past, and `j` would stop
being a key you can predict — which is the whole reason the fold exists. And never *fewer* rows
than the folded card showed: a preview is a floor on top of the setting rather than a ceiling under
it, so `chat.tool_output_lines = 40` does not lose thirty rows the moment you step onto the card.

**A preview is a bigger budget, not an opening.** Which is the difference between a card and a
*stack* of them. Opening a stack of commands means a row per command *and* an output under each
one, and at `preview_lines` apiece a nine-command stack would drop a hundred and fifty rows under
the cursor on the way past — the flood the bound exists to stop. So the cursor buys more of the one
output that was already showing, which by ADR 0051 is the one that is the answer, and `⇥` buys the
other eight. A run of *reads* is not this and does open: a row per call is one row per call, and
that is bounded by the card.

**`c` and `C` step to the next tool call and the one before it.** The transcript's third structure,
after the turn and the block, and the one that says what the turn actually *did*. `c` because both
bracket pairs are spent and it is free here — nothing in this mode edits, so Vim's `c` has nothing
to collide with — and it takes a count like every other motion, so `3c` is three calls on. It lands
on the header, which opens the card, so `c c c` reads a turn's work one call at a time.

## Consequences

- `cards::body`, `group_body`, `run_body` and `diff_rows` take `Open` where they took
  `expanded: bool`, and `result_rows` takes a plain limit with `usize::MAX` for "all of it". The
  replay path draws `Open::Folded` explicitly, which is what `false` meant there.
- `Card::takes` refuses to grow a card that is previewed as well as one that is opened, for the
  reason it already refused the second: growing a card under the person reading it moves what they
  were reading.
- **A preview redraws, and a redraw settles the streaming answer.** So it fires only when the card
  under the cursor actually *changes* — moving within a card redraws nothing — and never while the
  reader is being carried by the tail, because following is watching and a body nobody aimed at is
  not something to open. That is the one place the two halves of this ADR had to be told apart.
- Leaving reading folds the preview and leaves a pinned card open. A preview is the cursor's and
  there is no cursor in the composer; a pin is yours.
- `cd crates && cargo test` folds to `cargo test` rather than `cd crates`, which is a change to a
  documented example in ADR 0051. It is the same rule that ADR said it was applying — the name is
  what a command *is* — arriving at the case that ADR did not have in front of it.
