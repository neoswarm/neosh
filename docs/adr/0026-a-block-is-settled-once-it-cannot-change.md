# 0026 — A block is settled once it cannot change

**Status:** accepted

## Context

An answer arrives a few characters at a time, and the whole of it has to look right after every one
of them. Markdown makes that awkward: what a line *means* depends on lines above it (is this inside
a fence?) and sometimes on lines below it (is that dash a list item or the underline of a setext
heading?). The obvious implementation — re-parse the answer, redraw all of it — is quadratic in the
length of the answer. It is fine for a paragraph, and the place it stops being fine is the tail of
the thousand-line answer somebody is actually reading.

The other obvious implementation — parse each line as it arrives and never revisit it — is wrong in
a way that is worse than slow. A half-written `**bold` is not bold, and a fence's opening
back-ticks change the meaning of everything after them. Committing to a reading of a line before
the line is finished means the transcript flickers between two interpretations on every token.

## Decision

**A block is settled once it cannot change**, and settling is the only thing that is permanent.

Concretely: a paragraph followed by a blank line, a heading with its newline, a fence with its
closing marker. Settled blocks are rendered once, written into the transcript once, and never
looked at again. The trailing partial block is re-rendered from scratch on every tick and replaces
itself as a range.

The cost per token is therefore bounded by the size of *one block*, not by the size of the answer,
and the interpretation of a line is only committed to when no further input could change it.

## Consequences

The streaming fast path (`BufAppendText`, "never resend the document") is no longer what the
transcript uses for an answer. It is still the right primitive and still there; it is simply not
enough on its own, because rendering is not append-only. The replacement is a range write of the
trailing block, which is the smallest thing that is actually correct.

Two renderers would defeat the point, so there is one: a stored message is rendered by pushing it
through the same incremental type and then finishing it. The test that matters is that streaming a
source character by character produces the same rows *and the same marks* as rendering it in one
go — because a conversation that changes shape when you switch away and back reads as two different
programs having written it.

Marks are cleared over the rows a patch replaces before the new ones are set. Without that, a mark
from the previous reading of the trailing block clamps onto whatever replaced it, and a half-written
bold run colours the wrong words for the rest of the answer.

## What is deliberately not rendered

**Syntax highlighting inside fences.** Doing it properly means a grammar per language. Doing it
improperly — a regex over keywords — colours the wrong words in exactly the code being read most
carefully, which is worse than one honest colour for the whole block.

**Tables.** Column widths depend on the terminal, the content and the wrap policy, and a table that
reflows badly is harder to read than the pipes it came from.

Neither is a permanent decision. Both are things to add when there is a way to do them that does not
make the common case worse.
