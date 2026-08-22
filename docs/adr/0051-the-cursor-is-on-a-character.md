# 0051 — The cursor is on a character

## What happened

Three reports, and they turned out to be one thing said three ways: *I don't see the cursor when I
go into it*, *there are some obstacles when moving*, and *visual mode is not that good*.

**The cursor was not there.** The frontend placed the caret by taking `cursor_row - top_line` — a
count of *buffer* rows — while it drew the window by wrapping each of those rows into as many screen
rows as it needed. The two numbers agree exactly until the first line long enough to wrap. The
transcript is markdown prose; the renderer deliberately does not wrap it, because the terminal's
width is not known where the buffer is written. So every paragraph is one buffer row and four or
five screen rows, and the caret was drawn one row too high per wrap above it — until, a few
paragraphs in, it was outside the window, at which point the renderer stopped drawing it at all.
`^S` at the bottom of a real conversation put the keyboard into a mode with no cursor anywhere on
the screen. `follow_cursor` in the host had the same arithmetic and therefore agreed that everything
was fine.

**Moving was a text field's idea of moving.** Reading resolved its motions with
`ApiCall::WinMotion`, which is right for the composer and wrong here: the cursor sits *between* two
characters, `←` at column zero wraps to the end of the row above, and `End` goes one column past the
last character because that is where the next thing you type goes. Every one of those is the wrong
answer in a mode where each key is a verb. `^D` was half the window's *height* in buffer rows, which
in a transcript of paragraphs is three or four screenfuls. Paging reset the column to zero. `w` and
`W` were the same motion, `e` was not a key, and neither were `f`, `t`, `%`, `;`, `H`, `M` or `L`.

**And the selection was exclusive.** `v` then `y` — the two keys that mean "copy this" — printed
`nothing to copy — the selection is empty`. Linewise was not a shape but an invariant the host
re-established after every motion, by three API calls, from a list of the motions that knew to run
it. Nothing said you were selecting: the strip still said `reading`, and `Esc` and `y` had quietly
changed what they did.

## The decision

**The frontend draws the caret from the rows it drew.** `rendered_rows` returns each screen row
together with the buffer row and wrap segment it came from, and both the paint and the caret read
that one list. There is no second calculation of "how many rows is this" left to drift.

**A window never hides its own cursor.** Having found which rendered row the caret is on, the draw
shifts the slice it takes — minimally, the way a scroll that chases a cursor has to — so that row is
on screen. Wrapping is the frontend's knowledge and this is the only place it can be answered
correctly. `top_line` in the core stays what was *asked for*; what was *drawn* comes back through
`ViewportChanged`, which now also carries `rows`: how many buffer rows are on the screen. That is
the number `^D`, `^U`, `H`, `M` and `L` are counted in, and it is not the window's height.

**Reading has its own motions.** `crate::vim` is pure functions over `&[String]`: `h` and `l` stay
on their row, `$` is the last character rather than the space after it, nothing may park past the
end of a row, `w`/`W`/`b`/`B`/`e`/`E`/`ge` are Vim's five word motions rather than two, and `f`,
`t`, `;`, `,`, `%`, `^`, `g_`, `*` and `#` exist. `$` remembers *the end* rather than a column, so
`$jj` walks the right-hand edge of ragged text. Every position that leaves is a byte offset on a
grapheme boundary — word classification looks at `char`s, because a combining mark has no character
class, so results are snapped back before they are returned.

Not in the core, and this is the point rather than an omission: `CursorMotion` is the composer's
vocabulary and a plugin's text field's, and giving it a second set of answers to `h` would make
every caller ask which mode it was in. The reader reads rows with `buf.lines` and puts the cursor
back with `win.setCursor`, which is exactly what a third party writing this would have to write.

**A selection's shape is state on the window.** `SelectShape` — `exclusive`, `inclusive`, `line` —
set by `win.selectShape`, read by the one function that turns two positions into a range. So `v`
then `y` copies the character under the cursor, `V` takes whole rows in whichever direction it is
extended including upwards, and there is no frame in which the wrong thing is lit because no motion
can forget to re-establish anything. Dropping a selection drops the shape with it.

**A block cursor, and the strip says which mode you are in.** `win.cursorShape` is per window;
reading asks for `block` and gives the bar back on the way out. The frontend paints the cell under
the caret with `Cursor` — reverse video when no theme defines it, because a palette entry is how you
change what a block cursor looks like and never whether there is one — *and* asks the terminal for
`SteadyBlock`, which is what a screen reader and the terminal's own "where am I" follow. The strip
reads `visual` and `visual line` while a selection is running, for the same reason Vim's does: it is
the one state here where the same keys do different things.

**And the rest of the visual vocabulary, because half of it is worse than none.** `o` swaps which
end you are on; `gv` puts back the selection `Esc` gave up; `iw`, `aw`, `ip`, `ap`, `i"`, `i(`, `i[`,
`i{` and their `a` forms name what they name; `y` takes a motion (`yw`, `y$`, `y5j`, `yG` — linewise
when the motion crosses rows, as in Vim) and `yi<object>` copies one.

## What is deliberately not here

**`^V`.** Blockwise selection needs a third shape that is a rectangle rather than a range, and every
consumer — the highlight, the slice, the clipboard — would have to grow a case for it. The key says
so rather than doing nothing, because a chord with no effect reads as a broken keyboard.

**`ya<object>`.** `ya` is "copy the whole transcript", which is documented and older than this, so
`yaw` is the one shape of the operator that cannot be had. `viwy` is how you get it.

**`gj`/`gk`.** Moving by *display* line needs the wrap the frontend computed, and the reader is in
the host. It would have to either duplicate the wrap algorithm — the thing this ADR is about not
doing twice — or ask for a mapping the protocol does not carry. `j` is a buffer row, which is what
`j` is in Vim too.

**Editing anything.** No `d`, `c`, `x`, `p`, registers or macros. The transcript is an artefact you
take pieces out of; a mode where you can accidentally change what the agent said is a different
program.
