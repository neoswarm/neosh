# 0033 — A card shows what happened

**Status:** accepted

## Context

After ADR 0031 a tool card was a dot, a name, a subject and the first three lines of whatever came
back. For a `Read` that is about right. For an edit it is `⏺ Edit(src/host.rs)` and, under it,
`edited` — which is the tool telling you it did the thing, and nothing about what the thing was.
Put beside `codex` or `claude`, which show the diff, colour the added and removed lines and say
`+117 -40`, the transcript reads as one that asks to be trusted.

Three specific gaps, and one that was not a gap but looked like one:

1. **An edit showed no diff and no numbers.** The card had the tool's arguments — the old string,
   the new string, the path — and threw them away in favour of three lines of result text that
   said `edited`.
2. **A result went at the end of the transcript, not under its header.** Harmless for a model
   driver, which runs one call at a time. An agent driver runs several at once, so the result of
   the first arrived after the header of the third and was drawn under it.
3. **Nothing could be opened.** The first three lines and `… +40 more lines` was all you would
   ever see of a result, short of asking the model to repeat itself.
4. **`^S` was invisible.** The vim-style reading mode — `hjkl`, `v`, `y`, `yc`, `/` — had existed
   since ADR 0028 and was advertised in exactly one place: the `ui.hints` row, which is off by
   default. The first draft of this change was going to build a reading mode. It already existed;
   what it lacked was a door.

And the welcome screen said `neosh — a terminal-first agent workspace` in one accent colour, which
is accurate and forgettable.

## Decision

**What counts as an edit is read off the call's arguments, not its name.** `old_string` and
`new_string`; an `edits` list of them; `old_text`/`new_text`; or `content` headed for a path. Those
are the shapes every editing tool in practice takes — Claude's, ACP's, a plugin's — and reading
them means a tool registered tomorrow gets a diff without asking for one. A name-based list would
be right for the tools we have heard of and wrong for every other.

**The diff is computed here, by a small LCS over the two strings.** `crates/neosh/src/diff.rs`:
trim the common prefix and suffix, align the middle, fold unchanged runs to two lines of context
around each change. No dependency, because what gets diffed is the two sides of one edit — a few
dozen lines with long matching ends — and a real diff library earns its weight on repositories.
Past four million table cells it degrades to "all of one, then all of the other", which is still a
correct diff and never stalls the host loop.

**The stats go on the header, and only once the edit has landed.** `⏺ Edit(src/host.rs)  +3 -1`.
A running call has changed nothing yet and a failed one changed nothing at all, so neither carries
a number. The subject gives up room to the stats before it is clipped: the number is the new
information, the path you mostly knew.

**A body goes under its own header.** The host keeps a registry of every card on screen — id,
call, result, row, how many rows its body takes, whether it is open. Finishing a card inserts its
body at `header + 1` and moves every card below by that many rows. The transcript rebuilt from
messages on a switch does the same insertion, so the two agree. The registry replaces the per-round
map of "running call → row", which could only ever answer the narrower question.

**Any card opens, while reading.** `Tab` or `za` in `^S` mode toggles the card under the cursor
between its folded body and the whole of it — the full diff, the full output. The trailer under a
folded body says so: `… +40 more lines (^S ⇥ to expand)`. Opening lives in reading mode rather than
on a global key because it is an operation on *a row*, and the composer has no cursor in the
transcript to say which. `chat.diff_lines` (default 12) is the fold point for diffs, alongside
`chat.tool_output_lines` (default 3) for everything else.

**A turn ends with what it changed.** `changed 3 files  +117 -40`, then one row per file. Computed
from the turn's own successful edit calls and not from `git diff`, which would count work done
outside the turn — by you, or by a turn in another conversation on the same checkout — and would
say nothing for a conversation whose directory is not a repository. Live, the tally accumulates on
the `Round` as calls finish and is drawn at `TurnEnded`; on replay it is recomputed from the
messages at each turn boundary, so the two are the same numbers by construction. A turn that only
read and answered has no summary: a row saying "nothing changed" on every turn is a row.

**ACP's diff is folded into the call's input.** An ACP agent reports an edit's `oldText`/`newText`
in `content`, separately from `rawInput`. The driver copies them into the input as
`old_text`/`new_text`/`path` where the input has nothing of its own, so the card reads them like
any other edit. A diff that arrives later in a `tool_call_update` is still lost — the block is
closed by then — and that stays open.

**The welcome screen says `^S`.** Under the model and the directory: `⏎ send   ^S browse & copy
^P model   ^T projects   F1 every key`. The list is the five keys a new user needs before the first
question, and it is fixed rather than read from the binding table — a welcome that rebinds itself
is correct and unhelpful.

**And it has the word on it.** NEOSH in the block font every Neovim dashboard uses, six rows by
forty-three columns, lit like brushed metal: a grey ramp down the faces, one dark grey for the
shadow. Six palette groups (`Logo.Face0..5`, `Logo.Edge`) rather than a gradient the frontend
computes, so a theme can repaint the word — in its own metal, or flat — without the frontend
knowing it is a word. Greys and not the accent, so the one coloured thing on that screen is still
the key that does something. Below 45 columns, or in `ui.ascii_only`, it is the old single line:
half of those glyphs would be boxes. The welcome is redrawn on resize for the same reason.

## Consequences

- `cards.rs` owns every rendering decision about a tool card — header, body, summary — and both
  the live path and `transcript()` go through it. `host.rs` lost the four functions that used to
  do this and gained the registry.
- Two new options: `chat.diff_lines` (rows of a diff before it folds, default 12), and the command
  `chat.toggle_card`. Two new keys in reading mode, `⇥` and `za`, in the `CLAUDE.md` table.
- Paths in a live card are now relative to the *conversation's* directory, as the replay's always
  were. They used to be relative to where neosh was started, which is the same thing until you
  open a second project.
- A write (`content` to a path) is shown as all additions. There is no "before" to diff against,
  and `+N` is the number you would check.
- No line numbers in the gutter, which `codex` and `claude` both have. An `old_string` edit does
  not say where in the file it sits and the card does not read the file to find out; the numbers
  would be right for writes and absent for edits, which is worse than absent.
- The fixture for the integration tests is a plugin provider whose one call is an edit — the same
  pattern as the `chatty` fixture of ADR 0031 — so the diff, the stats, the summary and the fold
  are all proved by driving the real binary, not by reading the renderer.
- **A sub-agent's traffic is not the conversation's.** Real captures of a turn that ran `Agent`
  show the CLI streaming `stream_event` for the top-level agent *only*, and reporting the
  sub-agent as whole `assistant` and `user` messages stamped with `parent_tool_use_id`. neosh read
  the `user` half and not the `assistant` half, so every call the sub-agent made arrived as a
  result with no call: 73 of one real session's 243 tool results had no `tool_use` anywhere in the
  conversation, and rendered as a column of `⎿ …` with no card above them. `claude_cli_line` now
  ignores any line carrying a parent, whatever its type. The sub-agent's own account of what it
  did comes back as the result of the `Agent` call, under that call's card, folded like any other
  — which is both the honest shape and the one the CLI's own interface shows.

## What was added afterwards: the card says how it is going

The first version said what a call *was* and what it came back with, and nothing about the gap
between the two — which is where all the waiting happens. Three additions, and one thing
deliberately left out.

**The subject moves while the call is out.** `Agent.ToolLive` is a shimmer on the command, the path,
the query — the same sweep the working line uses, on the run of text you are actually waiting on.
The dot beside it already pulsed and that was not enough: a single glyph brightening and dimming is
invisible from a foot away, and the question a card with no body has to answer is "is this still
going or is it wedged". Nothing new ticks for it; the animation lives on the highlight group and
the frontend owns the frames, as [ADR 0027] has it. `ui.motion` takes it away with everything else.

**A clock, and then a duration.** Ticked once a second by the same deadline that moves the working
line's number, replacing the header row in place — one row for one row, so nothing below moves.
`redraw_card` cannot do this job: it settles the open answer, because a body appearing shifts every
row after it, and doing that once a second would end the answer a dozen times while it was still
arriving.

Under 100ms nothing is printed. A duration answers "how long did I wait", and for a call that came
back before you could look at it the answer is "you didn't" — `0ms` beside every read and grep puts
a number on the one thing about them nobody wondered, and buries the `40s` that mattered in a column
of them.

Durations are **not persisted**, and a restored card has no clock. The messages record what
happened, not how long it took; a stopwatch is a property of watching something happen, and
reopening a conversation is not watching it happen again. The alternative — timing from the moment
the buffer was drawn — would put "how long ago you pressed `^T`" where a duration goes.

**`exit 2`, read out of the output.** No protocol here carries it: `ToolResult` says *whether* a
call failed and never *how*. `Exit code 101` is the first line of a failed command in every driver
in the tree, so it is read from the text, and when the convention does not hold the card says less
rather than inventing a number.

**And the name is coloured by what the call was handed**, not by what it is called — `Kind::of`
reads `command`, `url`, an edit pair, a path, in that order, exactly as `edits_of` does. A tool a
plugin registers tomorrow is a `Run` because it was given a command. The name itself stays whatever
the tool calls itself: mapping `Bash` to `run` would be a second vocabulary for the one the model is
already using, and the moment they disagree the transcript describes a tool that does not exist.

Left out: a live tail of a command's output while it runs, which `codex` has. Its exec tool streams
and ours do not — for `claude`, `codex exec` and every ACP agent, output arrives whole, once, at the
end. A pane that could only ever fill in one go is a worse lie than an empty one.
