# 0017 — A window may claim the keys nothing else wanted

**Status:** Accepted.

## Context

Keys bind to command *names*, never to callbacks (ADR 0012). That indirection is what makes every
binding listable, remappable and invocable by other plugins, and it is worth keeping.

It is also not enough. A picker has a filter box; a text prompt has a text field. Both need every
printable character, and binding all of them to commands is absurd — and would still be wrong,
because those bindings would be global and would fight with everything else.

Building the picker into the host would have avoided the question. It would also have meant no
plugin could ever write one.

## Decision

`keymap.capture(win, command)`: while `win` is focused, every key the keymaps did **not** claim is
sent to `command`, with the `KeyContext` that already exists.

Precedence is the whole design:

1. keymaps resolve first, exactly as before;
2. only on `Unhandled` does the focused window's capture fire;
3. only then does the key fall through to the host's own default handling.

So a picker gets its filter keys and `<C-q>` still quits. A modal that swallowed every binding would
be a trap — you could open a picker and lose the key that closes the editor.

This is not a new mechanism so much as an exposed one: the host already routes unhandled keys to its
composer. Capture is that same fallback, made available to plugins rather than kept private.

## Lifetime

A capture is released when the window closes, when a `close_on_blur` float is dismissed, and when
the owning plugin unloads. The last one matters most: a capture naming a command that no longer
exists would answer every keystroke with "no such command", and the symptom is a keyboard that
appears to have stopped working with no indication why.

Capturing on a window that does not exist is an error rather than a silent no-op, because the
symptom of the alternative is "my picker ignores every key".

## Consequences

- Pickers, prompts, completion menus and modal lists are all writable as plugins.
- One capture per window; a second replaces the first. Two widgets competing for the same window's
  keyboard is never what was meant.
- Frontends gained `InputEvent::Command` at the same time, for the mirror-image problem: a frontend
  that can only send keys cannot have a menu, and synthesising whatever key an entry happens to be
  bound to breaks the moment the user rebinds it.
