# 0021 — Motions are verbs the core resolves

**Status:** accepted

## Context

The composer began as a string with characters appended to the end of it. `Backspace` popped one
off. There was no cursor, so there was nothing to move, nothing to select, and no way to copy
anything — which is a peculiar limitation for a program whose entire output is text you want to
keep.

Fixing that means deciding where cursor motion and text editing live. Three options:

1. **In the frontend.** It already measures display width and draws the caret.
2. **In each caller.** Give plugins `buf.setLines` and let them compute positions.
3. **In the core**, as calls that name a verb rather than a position.

## Decision

The core, as `WinMotion { win, motion, select }` and `WinEdit { win, edit }`. A motion names
`WordLeft`, not a column. An edit names `DeleteWordBack`, not a range.

Selection is a per-window anchor, drawn as an extmark in a namespace the core reserves at startup
and links to `Visual`.

## Why not the frontend

The frontend is replaceable — that is the whole point of `--ui-protocol=stdio`. Putting motion there
means every frontend reimplements grapheme clusters and word boundaries, and they disagree. It also
means the core cannot offer editing to plugins at all, because a plugin talks to the core.

## Why not each caller

Grapheme and word boundaries are hard, and getting them wrong is invisible until someone types an
emoji. `←` must step over `👩‍💻` as one thing — four code points, eleven bytes, one cluster — and
`char`-wise motion puts the cursor inside it. A plugin author should not have to know that, and if
they each solve it separately, `<C-w>` means something slightly different in every text field.

The consequence worth having: the composer, a plugin's text field and a future normal mode behave
identically because they are asking the same question, not because three people implemented the
same rules.

## Columns stay byte offsets; motion is by cluster

These are different things and both are necessary. A column is a UTF-8 byte offset because that is
the protocol (ADR 0007) and it keeps width math in the frontend. Motion steps by grapheme cluster so
a keystroke never lands inside a character.

Vertical motion remembers a **cluster index**, not a byte column: the same byte offset is a
different place on a line with different content, so moving down past a short line and back up would
not return you where you started. Neovim remembers a display column, which we cannot — the core does
not know how wide anything is. A cluster index is the closest thing computable without measuring,
and it is exactly right for every line that does not mix character widths.

## Copying is OSC 52, not a clipboard library

neosh is used over SSH. A clipboard library talks to the X11 or Wayland display on the machine the
process runs on, which is the wrong machine. OSC 52 travels back through the stream the UI is drawn
on and reaches the terminal the human is sitting at.

The cost is honest and unavoidable: support cannot be detected, so a terminal that does not implement
it ignores the escape and the copy silently does nothing. Refusing to copy on the chance it will not
work is worse.

## Consequences

- `<C-c>` carries two jobs: copy when something is selected, interrupt or clear when nothing is. They
  cannot both apply, so it is never ambiguous — and it matches what the key does everywhere else.
- Reading the transcript is a *place*, not a focus change, entered with `<C-s>`. The keys mean
  different things there — `j` moves rather than types — and pretending otherwise is how modeless
  editors end up with a dozen chords nobody remembers.
- One selection namespace serves every window. Two views of one buffer both selecting is a real case
  and the last refresh wins; a per-window namespace nobody can enumerate would be worse.
- `TextEdit::DeleteSelection` cannot be planned by `neosh-core::text`, which is a pure function over
  lines and knows nothing about anchors. The editor turns it into a `DeleteRange` first. That seam is
  deliberate: the moment `text` needed window state it would stop being testable without one.
