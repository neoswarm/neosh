# 0035 — A diff is code that changed

**Status:** accepted

## Context

ADR 0033 gave a tool card a body: for an edit, the change itself, `+N -N` on the header, and `⇥` to
open it. That was the right shape and the wrong drawing. Put beside `codex`, which the card was
written to catch up with, four things were visibly worse and one was invisible and worse still.

1. **Green and red were the *text*.** An added line was green from column one to the end, and that
   is the whole of what the transcript said about it. Meanwhile the workspace had no syntax
   highlighting at all: `markdown.rs` refused it, in as many words, and the refusal was about doing
   it with a regex over keywords rather than about doing it. So the one colour a line of code had
   was the one fact you already knew from the `+` in front of it, and the colours that would have
   told you what the line *says* were not being spent.

2. **The frame was claude-code's, and it cost three columns.** `⎿` and five spaces of indent, on
   every row of every body. An elbow marks where a body starts, which is the one thing a body's
   position under its header already says; forty rows later there is nothing marking anything. And
   the three columns it took were the three columns at which a doc comment started being clipped.

3. **Long lines were cut, not wrapped.** `steering it if a message arrive…`. A clipped line of prose
   has lost an adjective. A clipped line of code has lost the half of it you were about to read, and
   there is no scrolling sideways in a transcript.

4. **`Edit(src/main.rs)`** reads as a function call, which is what it is on the wire and not what it
   is on screen.

5. **Tabs collapsed.** `Read` output of `     1\thello` arrived as `1hello`, because a terminal's
   tab is a motion to the next stop and a buffer line is characters. The one place a tool aligns its
   output deliberately was the one place the alignment vanished.

## Decision

**The two facts about a changed line are carried separately: the band says which way it went, the
foreground says what it says.** A new field on the wire, `ExtmarkOpts::line_hl_group`, is a group
behind the *whole rendered row* rather than behind a range of bytes on it. Every ranged group on the
row is patched over it, so a syntax colour keeps its foreground and inherits the band. This is the
only place in the mark vocabulary where two marks describe the same character at once, and it has to
be, because "this line changed" and "this word is a keyword" are both true and neither is the other.

The band is filled to the window's own right edge by the frontend, not padded where the row was
written. That is what makes it survive a resize, and what stops a green line ending in a ragged edge
forty columns in — a band that stops halfway reads as two lines, one of which changed.

**At equal priority, the narrower mark wins.** Needed for the same reason: a fenced block is code
and this word in it is a keyword, both set at ordinary priority. Resolving it by which was set last
would make the answer depend on a caller's loop order. This is a general rule in the frontend, not a
special case for diffs.

**Syntax highlighting is real grammars, in one crate that knows they exist.** `neosh-syntax`, over
syntect and the two-face grammar bundle — used as a *parser only*. Its thirty-odd themes are never
loaded; the scope stack is mapped onto nine `Syntax.*` groups and `neosh-core`'s palette decides
what they look like, exactly as it decides what a border looks like. A highlighter carrying its own
theme would be a second palette to keep in step with the first, and the two would disagree the first
time somebody switched to the light variant.

Seven hues, out of several thousand scopes. `palette.rs` says three hues do the work and a fourth
means memorising a legend; code is the exception and the module now says why. Every other colour in
the palette is a *state* — running, needs you, broken — and a state you have to look up is a state
you will misread. The colour of a string literal is not a state. It is a convention every editor has
already taught its reader, and one no glyph or attribute can carry instead. It is confined to the
inside of a diff or a fence and never leaks into the chrome.

`two-face` rather than syntect's bundled defaults for one specific reason: Sublime's default package
has no TypeScript, and this is a workspace whose plugins are written in it.

**A change is highlighted per change, not per line.** One pass over every line the change touches,
added and removed together, in diff order. A grammar is a state machine: line two of a doc comment
is only a comment because line one opened one, and highlighting the visible rows one at a time is
how half a comment ends up grey and the other half looks like code. That interleaving is not quite
the text of either file, and it is the closest a diff comes to being one — the two versions of a
changed line sit next to each other, so a construct opened before the change and closed after it is
still open in between.

For a fence in a streaming answer the same problem has a different shape, and `Incremental` is the
answer: the parser is kept between lines. It needs no state on `Md`, because a fence is only
*settled* by its closing marker — an open one is re-rendered whole from its opening marker on every
tick.

**Line numbers where somebody knew them, and nowhere else.** ADR 0033 declined them outright,
reasoning that an `old_string` edit does not say where in the file it sits. Half right. A unified
patch carries `@@ -142,7 +142,9 @@` and knows exactly; a whole file written from scratch is its own
line one. So `diff::Line` gained `at: Option<usize>`, `unified` fills it from the hunk header,
`whole_file` from the content, and `lines` — which is handed the two *arguments to an edit* and not
a file — leaves it empty. When nothing on a card knows, the column is not there at all rather than
being a stripe of blanks.

The alternative, resolving an edit's position by searching the file on disk for `old_string`, was
rejected: it is a different answer the moment the file has moved on, and no answer at all for a
conversation reopened somewhere else. A number that is sometimes a lie is worse than no number.

**A rule down the left, two columns in.** `  │ `, the same on the first row as on the last, which is
the point of it being a rule. It answers the question a long card actually leaves open — how far
does this go — and gives back three columns of code.

**The sign stays even though the band says the same thing.** The band is the first thing to go: a
sixteen-colour terminal, `NO_COLOR`, a screenshot pasted into a ticket. A diff whose two halves are
told apart by nothing at all is not a diff. Same rule as the rest of the palette — meaning survives
losing colour.

**Long lines wrap, with the continuation under the rule and nothing else.** No repeated sign, no
repeated number: both are facts about *a line*, and the continuation is still the same one. The
break is taken at a space when there is one in the last third of the chunk and anywhere otherwise —
code is mostly not prose, but the line most likely to overflow a card is a doc comment.

Wrapping is done where the row is written, which is a display-width question answered outside
`neosh-tui`, and deliberately: it is the same character-count approximation `clip` has always made
here. The frontend still owns width and re-wraps anything that overflows, so being slightly wrong
about a CJK line costs a re-wrap and nothing else. Leaving it entirely to the frontend was the
alternative, and it puts the continuation at column zero, under the rule rather than beside it.

**`⏺ Edit  src/main.rs`.** Colour already separates the name from the subject, and a gap is what a
column of these needs to be scannable: the eye finds the second column, not the open bracket.

**Tabs become four spaces on the way in.** A buffer line is characters and a terminal's tab is a
motion, so the two cannot both be right; the one that can be stored is the one that survives being
scrolled, searched, and copied out of `^S`.

**A fence keeps its whole-row `Markdown.Code` mark under the token colours.** It is what `yc` looks
for when it goes to copy a block, and it is the colour plain code is drawn in between the tokens
that have one. Losing it would have lost the feature silently, and only for the languages we know.

## Consequences

- A new crate, `neosh-syntax`, and the workspace's first grammar dependency. It is the only crate
  that knows syntect exists, as `neosh-script` is the only one that knows deno does. Guardrails are
  in `LIMITS`: past 256KB, 4000 lines, or a single 4KB line the answer is "draw it plain", which is
  what was drawn before the crate existed. The line bound is the one that fires — a minified bundle
  is one line of two hundred kilobytes.
- Nothing is cached. A card is highlighted once, when it is written; a transcript rebuild highlights
  what it rebuilds. If that ever shows up in a profile, the fix is a cache and not a smaller
  grammar.
- `cards::Row` is a struct rather than a `(String, Vec<Span>)` tuple, because it now carries a third
  thing. `host::Mark` likewise, so the replay path can carry a band alongside a span — the two are
  written together and have to arrive together, or a diff row would be green a frame after it was
  green.
- `Diff.Add` and `Diff.Delete` still exist and are still foregrounds: they colour the `+5 -1` on a
  header and in a turn's summary, where the number *is* the message. The bands are the new
  `Diff.AddLine` and `Diff.DelLine`, and a test asserts they set no foreground — the failure would
  otherwise be silent, since the code stays readable and merely stops being highlighted.
- `usize` comes back a keyword rather than a type. Rust's grammar scopes `let` and `usize`
  identically, under `storage.type.rust`, and no mapping table can tell apart what the grammar did
  not. Asserted in a test so the next person to notice reads the reason instead of fixing it.
- `scripts/shot.py` grew `--color`, which prints what colour every run of every row is. The plain
  dump cannot see a band or a token colour, and this whole ADR is about colour — "it looked right"
  is not something you can check by reading the renderer.
