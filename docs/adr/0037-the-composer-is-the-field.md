# 0037 — The composer is the field

**Status:** accepted

## Context

ADR 0034 gave the workspace a `/` menu: two vocabularies, neosh's commands and the driver's, offered
in one list because from where you are sitting they are the same thing. The idea was right. The
widget it was built out of was a **picker** — the same modal, focused, fuzzy-matched float that `^P`
and `^K` open — and every one of that widget's properties turned out to be wrong here, in ways that
compounded into the worst kind of bug: the program silently doing something other than what was
typed.

Typing `/compact` into a fresh conversation, on the driver that implements it:

1. The `/` opened a float **in the middle of the screen**, over the answer being read, pointing at
   nothing.
2. The float took the keyboard. `compact` went into the float's own filter line, and the composer
   kept the lone `/` that had opened it. **What you had typed was not anywhere you could see.**
3. The filter was fuzzy, over the label *and the description*. `c`…`o`…`m`…`p`…`a`…`c`…`t` occur in
   that order in a great deal of ordinary English, and a command's description is a sentence — so
   `compact` matched a dozen unrelated commands and put one of them under the cursor.
4. `↵` **ran** whatever was under the cursor. Usually `chat.read`, which is how you ended up in the
   transcript reader having asked for a compaction.
5. And `/compact` itself was not in the list at all. A driver reports its commands at its handshake,
   which for `claude` means *after a turn has run*; a conversation you have just opened knows none
   of them. So the one thing typed was the one thing that could not be chosen.

The user's report of all of this was "the compact process is really bad, there's no animation or
anything" — which is exactly what it looks like from outside. Nothing ran, so nothing moved.

## Decision

**A completion is a suggestion about what is in the composer, and the composer stays the field.**

Concretely, four things.

**What is typed goes into the composer.** `PickerOptions.onQuery` fires on every change to the
filter, and the slash menu's implementation of it is one line: put `/` + the filter back into the
draft. So the message says what you typed, at every keystroke, and dismissing the menu leaves it
there mid-word — because it may have been the first word of an ordinary sentence, and a menu is not
entitled to an opinion about that.

**Matching is on the name, anchored at the start.** `PickerOptions.match: "name"` scores a prefix
match above a substring match and never looks at `keywords` at all. Fuzzy matching is right for a
search box, where you half-remember a word from a description and the list *is* the point. It is
wrong for a menu whose accept key executes, because there the cost of a bad match is not a bad row —
it is the wrong thing happening.

**An unknown name is sent, not refused.** When nothing matches, `↵` takes the freeform value, which
here is "send `/whatever` to the agent as typed". This is not a fallback for a rare case: on a
conversation that has not run a turn it is the *only* path a driver command can take, and it is what
makes `/compact` work the first time you ask for it. The row says so in as many words rather than
saying "no matches", which is a sentence that describes the list rather than answering the person.

**A float can be anchored to a dock.** `Anchor::Dock { dock }` places a float flush against a dock's
strip, on the main region's side of it. The composer is whatever is docked at the bottom, and a
plugin has no way to name that window — so before this the only choices were the middle of the
screen or the cursor, and the cursor is *inside* the field the menu would then cover.

The float's near edge meets the dock, not its top-left corner. Anchoring the corner would make every
caller subtract its own height first, and the day one of them is a row out, the menu sits on top of
the field it is a menu for. The same reasoning as `Extent` measuring content rather than the box
drawn around it: the number a caller has is what it wants to show, not where the frame goes.

## Consequences

Three of these are additions to the *public* widget vocabulary — `onQuery`, `match`, and
`Anchor::Dock` — and that is the point rather than an accident. An `@file` menu, a mention menu, a
snippet menu are all the same shape, and none of them should have to be built inside the host to be
placed correctly. `slash/main.ts` is an ordinary plugin using ordinary API, which is the test this
workspace applies to every feature it ships.

The picker also gained a guard it should always have had: a burst of keystrokes arrives as several
concurrent invocations of its key command, any of which may be the one that accepts and closes, and
the rest were drawing into a window that no longer existed. That surfaced as `not found: window 5`
in the log three times per fast-typed word.

What is deliberately *not* done: the menu does not close itself when the filter stops matching. It
is the obvious behaviour and it drops keystrokes — closing mid-burst hands the rest of the word to a
composer that receives it out of order, so `/compact` typed at speed arrives as `/cmpocat`. The menu
holds the keyboard until it is dismissed or accepted, and says clearly what `↵` will do.
