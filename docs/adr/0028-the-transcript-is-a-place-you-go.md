# 0028 — The transcript is a place you go, not a pane you focus

**Status:** accepted

## Context

The answer is the artefact. Everything a terminal agent produces that is worth keeping — a command,
a diff, a paragraph of reasoning you want to paste into a review — arrives as text in a transcript
that scrolls. And for a long time the only thing you could do with it was look at it: the keyboard
lived in the composer, and getting a command out meant selecting it with a mouse, across a wrapped
line, in a terminal that may not have a mouse.

The obvious fixes are all worse than they look.

**A "copy last message" key** answers one question well and every other question not at all. The
thing you want is rarely the whole message; it is the third code block, or the sentence with the
path in it, or the exchange from four turns ago.

**A mouse selection** is what the terminal already does, and it selects what is *drawn*: the
indent the renderer added, the bar down the left of a question, and whatever the pane beside it
happens to contain on the same rows. It also cannot reach anything that has scrolled off.

**Modeless bindings** — `^Y` copies the code block, `^]` goes to the next turn — is the design that
looks cheapest and gets expensive. Every new verb needs a chord nobody has spare, and the chords
have nothing in common with each other, so learning one teaches you nothing about the next.

## Decision

**Reading the transcript is a mode**, entered with `^S`, and inside it the keys are the ones an
editor already uses. `hjkl` and the arrows move, `w`/`b` go by word, `gg`/`G` reach the ends, `v`
starts a selection that motions extend, `y` takes it, `/` searches, `n` repeats. Two-key sequences
work the way they do in vi: `y` waits for what to yank, `g` for the second `g`, `z` for where to
put the line.

It is a **place**, not a focus change, and one boolean rather than an inference from which window
happens to be focused. That is the difference between a design you can reason about and one where
`j` means "down" or "the letter j" depending on state nobody wrote down.

Two verbs exist that no editor has, because a transcript has two things a file does not:

- **`[` and `]` step between turns.** The boundary is the bar drawn down the left of a question,
  read off the buffer each time rather than remembered in a list beside it. A remembered list is
  one more thing to keep true across a session switch, an answer being rewritten as it settles, and
  a transcript rebuilt from stored messages — and it would be wrong in exactly the case somebody
  needs it, which is scrolling back through a long conversation looking for something.
- **`yc` takes the code block the cursor is in**, without the two-space indent the renderer added
  or the language line above it. Found from the *marks*, not from the text: the rows of a fenced
  block are the ones the renderer coloured as code across their whole width, which is a fact it
  already recorded and which no amount of looking at the characters could recover — the back-ticks
  are gone by the time the buffer exists, because they were punctuation for the parser.

`ym` takes the whole exchange, `yy` the line, `ya` everything.

## Consequences

The mode says what its keys do, in the row under the composer, whatever `ui.hints` is set to — that
setting is about typing shortcuts, and this is not typing. Once an operator is down the row is
about what can follow it, because the general list has stopped being the live question.

A yank with a selection running leaves the mode. You came here to take something; staying would
mean pressing `Esc` every single time.

Searching borrows the composer to be typed into, the way entering a key does, rather than opening a
float — a float over the thing you are searching hides the answer at the moment it appears. Your
draft comes back when the search closes, however it closes.

The two verbs above are the ones this ADR is really about. Everything else here is vi's, and is
here so that the two new things sit in a grammar somebody already knows rather than being two more
chords to memorise.
