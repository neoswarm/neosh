# 0048 — A list is a place you move in

## What happened

The sidebar had `j` and `k`. That is the whole of it: no count, no page, no way to the top, and no
way to make the column wider than whatever `sidebar.width` said when the panel opened. On a list of
four conversations that is enough. On a list of thirty it is thirty presses, and the keys everybody's
hands already know — `5j`, `^D`, `gg`, `G` — did nothing at all, which reads as a panel that has not
been finished rather than one that made a choice.

Two other things came from the same place. A generated title is a sentence, the column is a little
over thirty cells wide with seven of them spent on an indent, and what a row said was
`Rework the sidebar so…` — for every row, including the conversation you were sitting in. And the
plan gauge, contributed at the foot, floated directly under the last project: a strip whose position
depended on how many projects were unfolded, which means it is somewhere different every time you
look for it.

## The decision

**Motions are Vim's, counts included, wherever there is a cursor over a list.** That is two places
and only two: the project panel and the transcript reader. A count is `1`–`9` then a motion; it
counts *rows you can land on*, so the headings and rules the cursor already skips do not silently eat
two of the five. `^D`/`^U` are half a screen, measured from the panel's real height rather than a
guess, and `PgUp`/`PgDn` are a whole one — deliberately not `^F`/`^B`, which are the archive and
hiding the panel, and a key that means something else only while the cursor happens to be in this
column is the worst kind of key. `gg` and `G` are the ends, and `5gg`/`5G` are the fifth row,
because that is the one place a count is a destination rather than a repetition.

**A half-typed count is on screen.** It sits at the right of the key strip in the panel and at the
head of the hint row in the reader. A count is two keystrokes with nothing in between, and a first
press with no visible effect is indistinguishable from a key that does nothing — which is exactly
what teaches somebody the feature is not there.

**A count is spent by the motion after it, and ended by anything else.** Every other verb in the
panel clears it, so a `5` you thought better of does not turn the next `j` into five. The one
exception is the first key of a pair: `5gg` is one motion with a number in front of it, so `g` puts
the count back for the key that finishes it.

**The row you are in stays open.** `ListRow.expand` unfolds a row's full text whether or not the
cursor is on it, and the sidebar sets it on the active conversation and nothing else. The rule that
*the cursor is what asks* is what keeps a column scannable, and this is the one row it is wrong for:
where you are is not a row you are considering. There is only ever one of them, which is what stops
this becoming "wrap everything".

**The foot is held against the bottom edge.** `render({ pinned })` takes a number of trailing rows
and pads with blank lines so they end at the bottom of the window — measured in *drawn lines*, not
rows, because an unfolded row is several lines tall. When the list is longer than the window there is
nothing to pin against and the rows are simply the end of the list, which is where scrolling finds
them.

**A dock can be resized without being reopened.** `ApiCall::WinResize` changes a docked window's
extent in place. Reopening was the previous answer and it is not the same thing: the window id
changes and the focus stack drops it, so a panel resized from inside itself threw the cursor back to
the composer on every press. `>` and `<` in the panel move the *setting*, so `config.toml`,
`init.ts` and the key all say the same number, and a width set in a file resizes the open panel
rather than closing and reopening it.

**And what the foot *says* is on the same budget.** The plan strip was an account row, a bar per
limit and a sentence under them: five rows of a column whose job is your conversations, four of
which answer a question nobody asked. It is now the limit that would refuse the next request, plus
any other already graded `critical` or `exhausted` — one row for one account, with the brand mark on
the bar since there is no account row left to carry it. `⇥` on a plan row steps up to every window
and then to the whole block, `usage.sidebar.style` is where it starts, `usage.sidebar` takes it off
the column, and `^L` is all of it whichever is showing.

The mark on that row keeps the *provider's* colour while the bar is graded by how close the limit
is — two accounts are then two rows you tell apart without reading a word — which needed
`sidebar.section` rows to carry `spans` as well as one highlight group. A contributed row that can
only be one colour is a row nobody can put a mark on.

That key needed one small thing: `sidebar.action` grew `on: "custom"`. A plugin could already
contribute *rows* to the column and had nowhere to put a key that belonged to them — `any` binds it
on every row in the panel and advertises it over conversations it means nothing for.

**And the titles got shorter.** `session.title.prompt` asks for two to five words and at most 32
characters, and the plugin clamps at a word boundary rather than mid-syllable. A title generated to
fit a column that cannot show it is a wasted request, and `Fix the authenticati` reads as a broken
panel rather than a long name.

## What was considered instead

**Counts in the core, so every panel gets them.** The scope rule is what kills it: the panel keys
live in `chat` mode, which is also the mode the composer types in, and a digit that becomes a count
globally is a digit you can no longer type into a message. The two surfaces that want counts are the
two with a cursor and no text field — the sidebar, whose keys are ordinary bindings on a buffer kind,
and the reader, which has its own key path. Pickers are the third list in the program and they
deliberately do not want this: typing in a picker filters, so `5` there is the character `5`.

**A wider default sidebar.** It is somebody else's screen. The keys are the answer, and the default
stays where it was.

**Gravity.** `WindowLayout::Docked` already has `Gravity::End`, which settles short content against
the bottom — of the whole window, which would put `PROJECTS` down there too. The foot is a *part* of
one list, so it is the list that has to place it.

## Consequences

- `ListRow.expand` is public API, and a third-party panel built on `CursoredList` gets both it and
  `pinned` for free.
- `sidebar.action`'s `on: "custom"` means anybody contributing a section can give its rows verbs,
  and `sidebar.section` rows take `spans`, so they can carry a mark in its own colour — the two
  halves of the contribution model that were missing.
- `win.resize` is public API: any plugin's dock can be resized, by its own keys or by anybody's.
- The count lives in the panel rather than in the key table, which means a plugin that binds its own
  verb into the sidebar gets the count cleared for it and does not have to know it exists.
- `2j` in the sidebar is two conversations, and `2j` in a file tree somebody else writes is whatever
  they make it. Nothing here reaches into a panel it does not own.
