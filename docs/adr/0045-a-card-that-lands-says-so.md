# 0045 — A card that lands says so

**Status:** accepted

## Context

ADR 0033 gave a running call a moving subject and ADR 0040 took the mark off a finished one. What
neither gave it was a moment of *arrival*. Watch a turn work and the transcript is a column of text
that grows: rows appear, and nothing about their appearing is drawn. The two things a person is
actually doing while they watch — checking that it is still going, and noticing when something
finished — were answered by the same slow colour pulse, and one of them was not answered at all.

Two specific gaps, and they turn out to be at opposite ends of the same event.

1. **The mark did not spin.** `Agent.ToolRunning` was `Pulse` — a glyph brightening and dimming in
   place. That is motion you have to be looking at to notice, and it is the one thing on screen
   whose whole job is to be noticed while you are looking somewhere else. Every terminal program in
   the world says "still going" by changing a glyph's *shape*, and the reason is not convention: a
   shape change survives peripheral vision and a brightness change does not.

2. **Nothing marked the landing.** A card came back and its row was simply different next time it
   was drawn. Half of what makes watching an agent work legible is watching things land, and the
   moment a call returns — the one instant in a card's life that is genuinely news — was the one
   thing the card never said.

The animation vocabulary could not express either. `Animation` had `Shimmer` and `Pulse`, both pure
functions of one global clock, both colour-only. A spinner needs to change what is *drawn*; an
arrival needs a *moment to count from*, which a function of a global clock does not have.

## Decision

Two new animations, and the awkward parts of each are where the design actually is.

### `Animation::Frames { set, period_ms }`

The run is replaced by successive glyphs. The only animation here that changes what is drawn rather
than what colour it is, which brings two constraints with it.

**The frontend clips or pads every frame to the display width of the text underneath.** A spinner
that shifts the column after it by one every tenth of a second is worse than no spinner, and the
guarantee has to be structural rather than a rule frame-set authors are asked to follow — so `fit`
enforces it and a badly-chosen set degrades to being cut off rather than to a broken layout.

**The buffer still holds the glyph the writer wrote.** A frame is what a cell *looks* like for a
sixteenth of a second, not what the line says. `^S` searches the glyph, a yank copies the glyph, and
the transcript that comes back tomorrow is the one you read today.

**Frames travel as a name, not as a list of codepoints.** This is the one place this codebase tells a
plugin what it may have rather than taking what it brings, and the reason is that the sets which read
well are the ones drawn from a font the terminal actually has — which is a question only the frontend
can answer and a plugin cannot. `FrameSet` is the closed list, adding to it adds it for everybody, and
a plugin picks from it exactly as it picks a highlight group. (`Animation` is `Copy` and lives inside
a `Copy` `HighlightSpec`; carrying a `Vec<String>` would also have rippled that away, but that is a
consequence of the decision rather than the reason for it.)

`Agent.ToolRunning` is now `Frames { Braille, 800 }`.

### `Animation::Flash { ms }`

One brightening, once. The row lifts and falls back over a third of a second, easing *out* rather
than in — a landing is bright immediately and then gets out of the way, where easing in would read
as the row arriving twice.

**The mark carries the timing, not the group.** "Once" needs a moment to count from, and a highlight
group is shared by every row that uses it, so a group cannot hold one. An extmark can: it is created
once. So the frontend keys the flash from the first time it saw *that mark*, which makes the
one-shot exactly as durable as the annotation it rides on. Scrolling the row away and back does not
fire it again, because it is the same mark.

**It belongs on `line_hl_group`.** A flash on a ranged group lights a quarter of a card and leaves
the rest, which reads as a highlight rather than as an arrival. The row's band is the only annotation
that is *about the row*, so it is the only one that can say the row just happened — and it is applied
to the result of every other decision, so a diff band, a syntax colour and a sweep all keep what they
agreed on and get lifted together.

**A burst is a redraw, and a redraw is not news.** Republishing a conversation on reattach mints every
mark in it at once. One row lighting up is an event; twenty is a strobe, and it is the same feature
that produces both. So more than three arrivals inside sixty milliseconds are recognised as a bulk
redraw and none of them flashes. This is the invariant most likely to be broken by accident later:
anything that starts re-creating card marks in bulk will make the transcript blink, and the burst
window is the only thing standing between that and the user.

`Agent.ToolLanded` is a band with **no colour of its own** — what it carries is timing, not
appearance — and `redraw_card` attaches it exactly once, guarded by `Card::flashed` rather than by
"is this the first time we settled", because the two are the same fact and only one of them survives
a card being opened and folded again.

### What was already there

The elapsed clock on a running card ticks already — `tick_cards`, one row replaced by one row, on the
same one-second deadline as the working line. It is named here only because it is the third of the
four things this change set out to add, and finding it already implemented is the reason the change
is as small as it is.

## Consequences

`ui.motion = false` turns both off, and both degrade rather than vanish: the mark draws as its static
glyph, the row draws as it would have anyway. Without truecolor a flash is the same bold/dim bit a
sweep already falls back to.

A finished flash stops setting the "something moved" flag, so a transcript full of landed cards costs
no frames. This is the property that makes the feature free when nothing is happening, and it is
tested rather than assumed.

**The stats do not count up.** `+0 -0` animating to `+12 -3` on landing was in the mockup this change
came from, and it is deliberately not here. It would need the header rewritten seven times over 350ms;
each rewrite clears and re-sets the row's marks, which would destroy the landing band the flash is
keyed to — the two features fight for the same rows. And a card's whole job is to report what changed,
so a number that is briefly wrong is a poor trade for a third of a second of delight. The flash
already marks the landing, which is what the count-up was for.
