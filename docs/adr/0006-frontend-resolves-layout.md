# 0006 — The frontend resolves layout

**Status:** Accepted.

## Context

The brief contains a genuine tension: "the core translates that retained state into ratatui widgets
each frame" and "do not let the core assume a terminal". Those pull in opposite directions, and the
answer determines whether a non-terminal frontend is a new consumer or a rewrite.

## Decision

The core emits **declarative geometry**; the frontend resolves it.

- Core sends: a dock (`left`/`right`/`bottom`/`main`), or a float's anchor, offset, preferred
  extent, z-index and border. Plus buffer text, extmarks and highlight *group names*.
- Frontend does: measuring, wrapping, scrolling, display-width math, colour down-conversion, and
  drawing. It reports realized geometry back via `InputEvent::ViewportChanged`.

## Why

If the core emitted concrete cell rectangles, a character grid would be baked into the protocol, and
every non-terminal frontend would inherit terminal layout assumptions — exactly the retrofit the
brief warns about. The deciding input was the intent to have a possible TypeScript or web frontend:
that is only cheap if layout is the consumer's business.

## Cost

Cursor-anchored floats need viewport feedback, so there is a small back-channel from frontend to
core. That is one event type and it is the only realized geometry the core ever learns.

## How this is kept honest

`--ui-protocol=stdio` emits the same event stream as JSON lines, and the end-to-end tests assert on
it. If the core ever grew a path that reached past the protocol to draw something, the JSON stream
would stop containing everything needed to render, and those tests would fail. The boundary is
*exercised*, not merely designed.

## Deliberate limit

The protocol assumes a monospace character grid for raw-cell surfaces (`SurfaceCell`), which are
the escape hatch for diffs and sparklines. A web frontend renders those as an opaque grid. Text,
annotations and layout carry no such assumption.
