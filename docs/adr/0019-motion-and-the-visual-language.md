# 0019 — Motion, colour, and what a terminal is actually for

**Status:** Accepted.

## Context

neosh has a sidebar of projects and conversations, model and reasoning-effort switchers, a context
meter, worktrees and approvals — the feature set of a graphical agent workspace, in a terminal.

The trap is building it by imitating one. Fake easing on opacity a terminal cannot express, layout
tweens that flicker, hover affordances with no pointer, shadows made of shading characters: the
result is neither a good terminal program nor a good graphical one.

## Decision: keep the information architecture, drop the technique

What carries over is *structure* — what is grouped with what, what is one keystroke away, what is
visible at a glance. What does not is every technique that exists because a compositor does.

**Motion.** Three things survive: spinner frames, progressive disclosure, and cursor movement.
Reorder animations, height tweens, shimmer skeletons, scale-and-fade dialogs, toast choreography and
hover tooltips are all dropped.

This is not a compromise forced by the medium. Continuous repaint is a bad idea *everywhere* — it
costs power and attention for no information. A terminal simply makes that obvious, because it is a
discrete-frame medium with a diffing renderer: only changed cells are written.

So: **one clock**, module-global in `@neosh/api/ui` and therefore shared by every plugin, ticking at
100 ms. Twenty in-flight rows cost one repaint per tick rather than twenty. Ten braille frames at
100 ms is a one-second cycle. The clock starts on the first subscriber and stops on the last, so an
idle session has no timer at all.

Measured in a real pseudo-terminal, with the whole interface running:

| interval | frames/s | tty bandwidth | CPU |
|---|---|---|---|
| 120 ms | 8.2 | 0.5 KiB/s | 1.0 % |
| 80 ms | 12.2 | 0.6 KiB/s | 1.3 % |
| 33 ms | 29.0 | 1.3 KiB/s | 1.7 % |
| 16 ms | 40.7 | 1.8 KiB/s | 3.0 % |

Under 1 KiB/s at the default rate. That is why motion stays on over SSH rather than being disabled
there on suspicion — the measurement disagreed with the guess. `ui.motion = false` turns it off for
anyone who wants a still screen.

**Prominence** comes from the terminal's own primitives rather than weight and shadow: bold means
*needs you*, normal means *this is where you are*, dim means *receding*, and reverse is reserved for
selection and the one-shot "it moved there" flash, so it always means "look here now".

**Colour** is three hues — cyan in motion, amber act now, red broken — always paired with a glyph,
so a 16-colour terminal or `NO_COLOR` loses speed rather than meaning.

**Structure is drawn, not implied.** A side dock gets a rule column between it and what is beside
it, and each section of a panel gets a heading and a rule under it. Text running straight from a
panel into a transcript reads as one column of nonsense because the eye has nothing to stop at. The
rule is the first thing given up when the terminal is too narrow: a separator that costs you the
panel it separates is not a good trade.

## What this made us fix first

Designing the surface exposed four defects that made everything above unjudgeable, and they went
first:

1. **`UiEvent::Message` was collected and never drawn.** Every `notify` in the program was
   invisible, including the host's own "press ^C again to quit". A program whose only feedback
   channel is invisible appears not to respond.
2. **No wrapping.** A long answer clipped at the right edge.
3. **The transcript showed its first lines, not its last** — the wrong end of every conversation.
4. **No cursor.** Nothing indicated that typing went anywhere.

None of these are theming. All four are the difference between a usable terminal program and one
that looks broken.

## Consequences

- The bundled plugins carry the look; the core carries the vocabulary
  (`crates/neosh-core/src/palette.rs`). Every group a widget may link to is part of a contract with
  a test, because a dead link does not error — it renders plain, which is how an invisible selection
  highlight ships without anyone noticing.
- New motion goes on the shared clock or not at all. A plugin owning its own timer is how twenty
  rows become twenty repaints.
- `VirtTextPos::Right` exists because every list row is the same shape — flexible text left, fixed
  status right — and a plugin cannot align that itself without measuring display width, which is the
  frontend's job precisely so a plugin cannot get it wrong.
- Wrapping and tail-following apply to the main pane only. A list that wrapped would push its own
  last row off the bottom, and a picker that followed the tail would hide its title and filter line.
