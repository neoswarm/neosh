# 0048 — A default binding is a key every terminal sends

## What happened

`F1` listed every binding neosh has. On a Mac out of the box it dimmed the screen: Apple's top row
is brightness and volume until somebody opens *System Settings → Keyboard → Function Keys*, so the
one key whose entire job was teaching the program the other keys was the one key a new user could
not press. The documentation's answer was a paragraph explaining where that setting lives.

`⌥↑` and `⌥↓` moved a rung up and down the capability ladder. macOS treats Option as a compose
key — `⌥p` is `π` — so unless the terminal has been told to send Option as Alt, those two keys are
`√` and a dead escape sequence. The documentation's answer was a table of five terminals and where
each of them hides that setting. `⌥V`, which took an attached image back off, was in the same
position.

Three settings, in two other programs, for three keys. A key the program never receives is a key no
amount of rebinding will fix, and a default that only works after a trip into somebody else's
preferences is not a default — it is a bug with instructions attached.

## What a terminal will actually send

The set is smaller than it looks, and it is worth writing down because it is what every future key
has to come out of:

- **Ctrl with a letter.** Every terminal, every platform, same byte. Minus `^I`, `^M` and `^H`,
  which *are* Tab, Enter and Backspace and cannot be told apart from them.
- **`⇧⇥`, `⏎`, `⌫`, `Esc`.** Named keys with their own sequences.
- **Printable characters**, in a mode that is not a text field.

And what is *not* in it:

- **Function keys**, for the reason above.
- **Alt/Option**, for the reason above.
- **Ctrl with punctuation.** In a plain terminal `Ctrl+/` and `Ctrl+7` are the same byte. The Kitty
  keyboard protocol distinguishes them and neosh asks for it (see `docs/config.md`), but a default
  cannot be conditional on a negotiation that half of terminals decline.
- **`⌘`**, which is never sent at all without that protocol.

Arrows, `PageUp`, `PageDown`, `Home` and `End` are a fourth category: real keys, sent reliably, and
absent from some keyboards or reachable only on a layer. They are fine to *bind* and wrong to
depend on.

## The decision

**A bundled default is a key from that set. Anything else is a key the user chooses.**

Concretely:

- The key list moved from `F1` to `^Z` — the one chord chat mode had left. Raw mode means it cannot
  suspend anything, so nothing was displaced.
- Taking an attached image off moved from `⌥V` to `⌫` **on an empty composer**, which is what a
  chip row does everywhere else and needs no modifier anybody's terminal has an opinion about.
- The capability ladder lost its keys and kept its commands. `model.upgrade` and `model.downgrade`
  are ordinary commands: `^K` runs them by name, one line in `init.ts` binds them.
- Arrows, `PgUp`, `PgDn`, `Home` and `End` stay bound everywhere they already were, and are never
  the only way to do anything. Every list moves on `^N`/`^P` as well.

## Why the ladder simply lost its key

There was an obvious alternative: a prefix layer. Free one rarely-used chord — `^O`, say, whose job
is also a row in the project panel — and spend it on `^O u`, `^O d`, `^O v`, `^O k`. Every homeless
verb gets a key and there is room for the next one.

It was the wrong trade. A prefix layer is a *concept*, paid for by everybody who ever reads the key
table, to save two keystrokes for the people who use the ladder — who are already one keystroke
from `^P`, which shows them the rungs with the current one lit. A capability that runs out of keys
loses its key and keeps its command. Nothing is unreachable: `^K` finds it by name, `^Z` lists what
is bound, and `init.ts` is where a key of your own goes.

This is also why nothing ships bound to `<leader>`. In chat mode the leader is a character you are
trying to type.

## A hint row is a promise about a keyboard

The second half, and the one that was quietly wrong for longer. `ui.keys.next` was
`"<Down> <C-n> <C-j>"`, and widgets print the *first* key in that list on their hint row — so every
picker in the program said `↑/↓ move` while `^N`/`^P` worked identically and went unmentioned.
Somebody reading that row on a keyboard whose arrows are on a layer is being told, by the program,
that they need a key they have to reach for.

The defaults now lead with the chord: `"<C-n> <Down> <C-j>"`. Same set, different order, and the
order is the part that gets read. `keyLabel` writes `^N` rather than `^n`, because nobody presses
shift to send it and the capital is how a terminal program has spelled a chord since curses.

Where a panel binds letters — the option sheet's `h j k l`, the project panel's `j k` — the letters
are what the row says, for the same reason.

## Consequences

- Anyone who was pressing `F1`, `⌥↑`, `⌥↓` or `⌥V` has to rebind them. They are four lines, and
  `docs/config.md` has them written out.
- `^Z` no longer reaches a shell job-control convention some people have in their fingers. In raw
  mode it never did — `ISIG` is off, so `^Z` was already an ordinary key press that nothing was
  listening for.
- One more chord is gone, and chat mode now has none left. The next capability that wants a global
  key has to take one, replace one, or live on `^K` — which is the constraint working, not the
  constraint failing.
- The sweep missed one, and the miss turned into a refinement. `session.copy.path` landed on `⌥Y`
  an hour before this was written — reachable on a Linux terminal, `¥` on a Mac, and mentioned
  nowhere on screen. Chat mode has no chord to give it, and the rule above says it loses its key.
  It keeps it instead, on the terms the fourth category already set for arrows: **Alt may be bound
  where it means something, and never as the only way.** What made `⌥↑`/`⌥↓` and `F1` bugs was
  that nothing else reached the same verb. Here everything does — `yp` in the reader beside `yc`,
  `ym` and `ya` (printable letters in a mode that is not a text field are in the set), `y` on any
  row of the project panel, `^K` and `/copy` by name — so a terminal that sends Alt gets a key in
  the composer and one that does not has lost nothing. The test for the next Alt binding is the
  same: name the route a Mac takes, or it does not ship.
