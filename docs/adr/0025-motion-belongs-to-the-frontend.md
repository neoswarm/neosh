# 0025 — Text that moves is a property of a highlight group

**Status:** accepted

## Context

The longest silence in the program is between pressing Enter and the first token. During it a still
screen and a wedged process look identical, and the only honest way to tell them apart is something
that moves.

The obvious implementation is for whoever wrote the text to animate it: a plugin sets an extmark per
character, recomputes the colours on a timer, and re-sets them. For a forty-character status line at
20 fps that is eight hundred API calls a second, each crossing the JS runtime boundary, to move a
highlight two columns. And it would be wrong anyway: how bright a band can get depends on what the
terminal can render, which is the one thing a plugin cannot know.

## Decision

`HighlightSpec` gained `animate: Option<Animation>` — `Shimmer { period_ms }` or
`Pulse { period_ms }`. The core stores it and forwards it. **The frontend animates whatever carries
it**, at whatever rate it likes.

A new `InputEvent::Repaint` lets the frontend ask for a frame. It carries no state and mutates
nothing: the core answers by flushing what it already had, which is what makes an animation unable to
desynchronise from the text it is drawn over.

## Why this does not reintroduce a frame loop

The ticker is armed by a frame that **actually drew** something animated, and the first frame without
any takes it down. So:

- An idle workspace sends nothing. There is still no clock anywhere in the core.
- An animated row scrolled off the screen stops costing frames — and the only code that knows it was
  skipped is the code that would have drawn it, which is why the flag is set during rendering rather
  than derived from what exists.
- The extra wakeups happen while a turn is streaming, which is exactly when the host is already being
  woken by tokens.

## Degradation

Without truecolor the same band renders as `BOLD`: one bit instead of twenty-four, and still
unmistakably a thing travelling left to right. Showing nothing would make "is it working?"
unanswerable on precisely the terminals where you are least sure.

A run whose colour is the terminal's default is not blended at all — that value is not something this
process can query portably, and blending from a guess gets the direction wrong on half of all themes.
Those runs take the attribute path.

## `ui.motion`

Applied by **removing** the animation from the groups that have one, not by telling the frontend to
ignore it. One decision in one place, and a frontend needs no concept of the setting at all: it
animates what carries an animation, and with motion off nothing does. The group keeps its colours
either way — turning movement off must not turn text invisible.

## Consequences

- Motion is reserved, by convention, for "something is happening and you cannot see it yet". Two
  groups carry it: `Status.Streaming` sweeps, `Status.Pending` pulses. A plugin can define more,
  and the same restraint applies for the same reason — a screen where several things move is a
  screen where none of them means anything.
- A shimmer emits one span per grapheme cluster. Uniform animations stay a single span, because
  splitting text that does not need splitting multiplies the span count of every line for nothing.
- The sweep is measured in clusters, not bytes, so it moves at a constant *visual* rate rather than
  crawling through CJK and sprinting through ASCII.
