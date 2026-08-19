# 0002 — ratatui + crossterm for the terminal frontend

**Status:** Accepted. Pre-decided in the brief.

## Decision

`ratatui` for widgets, `crossterm` for terminal control and input.

## Why

ratatui is immediate-mode, which is the right fit for a frontend that is a *fold over an event
stream*: state lives in the mirror, and each frame is a pure function of it. A retained-mode widget
toolkit would duplicate the state we already keep, and the two copies would drift.

crossterm because it is the backend ratatui is best supported on, it handles bracketed paste and
the Kitty keyboard protocol, and it does not require a curses dependency.

`ratatui::backend::TestBackend` matters more than it looks: it makes the renderer assertable
without a terminal, which is why the layout and annotation tests are real tests instead of
eyeballing.

## Constraint this imposes

ratatui 0.30 is a façade over `ratatui-core` / `ratatui-crossterm`, and the latter supports both
crossterm 0.28 and 0.29 through renamed dependencies. The workspace pins crossterm explicitly so
the direct dependency and the one ratatui links against cannot diverge — a mismatch there produces
type errors that read as if the API changed.

## Scope

Confined to `neosh-tui`. Nothing above it names ratatui, which is what ADR 0006 is about.
