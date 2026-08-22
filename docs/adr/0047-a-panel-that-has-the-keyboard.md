# 0047 — A panel that has the keyboard

## What happened

`^E` opens the model's option sheet. Pressing `^N` over it opened a new conversation *behind* it and
left the sheet on screen. `^T` opened the project panel underneath and moved focus into it, with the
sheet still drawn on top and still holding the focus stack's opinion about where keys go. `^G`, `^L`,
`^J`, `^D`, `^O`, `^B` and `^F` all did their own version of the same thing. The model picker leaked
identically; so did every widget in `plugins/api/src/ui.ts`.

None of this was an oversight in any one panel. It is what the routing rule says: `active_scopes`
resolves window, then buffer, then kind, then **global**, and global is always on the end.

## Why "bind over the keys you care about" was never going to work

That was the existing answer, and it is written down in `bindWidgetKeys`: a widget claims its
navigation keys window-scoped, so `<C-n>` means "next" in a picker rather than "new conversation".
It is correct and it is still there. It solves a different problem.

A widget can enumerate the keys it *wants*. It cannot enumerate the keys it merely does not want to
happen, because that list is every key the program binds now plus every key it binds next year plus
every key a plugin the author has never heard of binds at load time. A panel that is on screen for
two seconds cannot be asked that question.

The question it *can* answer is "does anything else reach me". For a control sheet you are in the
middle of using, a picker you are filtering, or a confirmation you are answering, the answer is no.

## The decision

`FloatConfig::modal`. While a modal float has focus, `KeymapScope::Global` is not in the scope chain.

Three consequences, and each is load-bearing:

**Nearer scopes still resolve.** Window, buffer and kind bindings are unaffected, which is what keeps
a modal exactly as extensible as any other panel: a third party binds against the buffer **kind** and
that binding is nearer than a global one, which is the ordering `KeymapScope` already promised. ADR
0040's rule survives intact — every key in a modal panel is still an ordinary binding pointed at a
named command, `F1` still lists it, `init.ts` can still move it.

**A key nothing claimed is swallowed.** `unclaimed` stops at a modal instead of emitting
`UnhandledKey`. Otherwise the host puts characters into the composer behind the float and reads
`<Esc>` as "interrupt the turn" — typing into a field you cannot see, which is worse than a key that
does nothing. A `KeymapCapture` on the modal's own window still receives them, so a filter box inside
a modal works exactly as before; modality is about what happens *after* the panel has declined a key.

**The escape hatches survive.** `ui.modal_escape_keys` — `<C-q>` and `<C-r>` — resolve globally
anyway, and are consulted only once the panel's own scopes have declined the key, so a modal that
binds `<C-r>` for something of its own keeps it. This is what makes the whole thing safe: a panel
that failed to bind a way out would otherwise be a terminal somebody has to kill, and "the panel is
buggy" must never be the same event as "the program is gone".

It is an option and not a constant because these are keys, and every key here is the user's. **Absent
is not empty**: an undeclared option falls back to the built-in pair, because the one thing standing
between a broken panel and a lost terminal must not be contingent on somebody having remembered to
declare a default. An explicitly empty list is honoured, because that is a person saying so.

## What is modal, and what deliberately is not

Modal: the `^E` option sheet, and the four float widgets in `@neosh/api/ui` — `picker`, `confirm`,
`prompt`, `railPicker`. Between them that is the model picker, `^N`, the archive, the palette, every
confirmation, and anything a plugin builds on the same helpers.

Not modal: hints, hover cards and anything else non-focusable, which never intercepted keys in the
first place; and the docked panels, which are places you *are* rather than things you are in the
middle of. `modal` defaults off, so a float that says nothing behaves exactly as it always did.

The question panel is not modal either, and that is the interesting exclusion. A question for a
conversation you are not looking at waits, the footer says how many are asking, and `^T` is how you
go and answer it — a modal question panel would take away the key whose whole job is getting to the
other question. Answering is not the same shape of activity as filtering a list.

## The cost

`^E` no longer toggles itself shut from outside, because the global `^E` does not arrive. So the
panel binds `<C-e>` to close, the way `^S` leaves reading the transcript. Any modal that borrows a
global key to open itself now owes the same binding — which is a real obligation, and cheaper than
the alternative, where the key that opened a panel opens a second one behind it.
