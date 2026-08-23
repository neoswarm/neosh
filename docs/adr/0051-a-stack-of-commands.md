# 0051 — A stack of commands

**Status:** accepted

## Context

ADR 0040 made a run of reads one row. A turn that reads three files, greps twice and lists a
directory is one line naming what it looked at, and `⇥` opens it into a row per call. The argument
was that the answer to a read is "yes, and it was what you would expect", so the six bodies were
six answers nobody had asked for and the six headers were the thing you actually wanted.

It explicitly stopped there: *"Only calls that looked at something ever share. A command's output
is the answer and an edit's diff is the answer; there is no summarising either into a list of
names."*

Half of that is still true. The other half turns out to describe a card, not a run of them. A real
turn does this:

```text
  Ran  git add -A                     1 line
  └ done
  Ran  git commit -m 'fix the thing'  4 lines
  └ [main 6759f10] fix the thing
     1 file changed, 2 insertions(+)
  Ran  git push                       3 lines
  └ To github.com:neoswarm/neosh
     6759f10..c280938  main -> main
```

Eight rows, of which the last two are the answer and the first six are the sound of somebody
getting there. And `git add` printing `done` under its own header is the same complaint ADR 0040
opened with, one kind of call along: a body that says what you would have guessed, drawn at full
price, holding apart the headers that were the information.

What is different about a command is that *one* of those outputs is load-bearing, and it is almost
always the last one: the earlier commands are how you arrive at the one you were waiting for.
`cargo fmt`, `cargo build`, `cargo test` — the test result is the answer and the other two are
setup. So a stack of commands cannot fold to nothing the way a stack of reads does, and it does not
have to keep everything either.

## Decision

**A run of commands shares a card, the same way a run of reads does — and it keeps the last one's
output.**

```text
  Ran  git add, git commit, git push
  └ To github.com:neoswarm/neosh
     6759f10..c280938  main -> main
```

The same rule for what a run *is*: calls with nothing drawn between them, asked the same way by the
live path and the replay. The same fold: `⇥` opens it.

**Reads with reads, commands with commands, and never one of each.** A run is a row saying what a
stretch of the turn was doing, and `Explored  a.rs, cargo test` is two activities wearing one verb.
`groups_with` now asks whether both calls are the same kind and that kind is one that folds — read
off the arguments, as ever, so a shell a plugin registers tomorrow stacks for the same reason it
gets a colour.

**A command's name in the list is what it *is*, without its flags.** `cargo test -p neosh-core --
--test-threads 2` is forty-two columns of a row that has to hold four of them, and thirty of them
are how the run was tuned. So the folded row says `cargo test`: the leading run of plain words, up
to three, dropping a `VAR=value` prefix. No ellipsis — everything on that row is short for
something, and `git add…, git commit…, git push` is punctuation saying what the row already says.
It is loose on purpose and it is safe to be loose — the whole command line is on the row this call gets when the
stack is opened, and on the card it gets when it is alone. This is [`short_subject_of`]'s existing
job (a path folds to its last component there) and it is the same trade.

**A run in flight is fitted from the back.** ADR 0040 fills the row from the front and counts what
did not fit. That is right for a run that is over — it is an account of what the turn did, and an
account starts at the beginning — and wrong for one still going, where the row is a report of what
is *happening*: nine commands in, the front-fitted row named what the model was doing forty seconds
ago and `+4 more`. So while any call on the card is still out, the list is fitted from the end and
what fell off the front is `+N earlier`. The one being waited on is the one name that can never be
the name that did not fit.

**`⇥` opens the stack into a row per command with the whole of its output**, which is exactly what
each of them would have had as a card of its own:

```text
  └ Ran  cargo fmt --check
        all good
    Ran  cargo test -p neosh-core -- --test-threads 2
        running 142 tests
        test result: ok. 142 passed
```

One indent deeper than the command it belongs to, because at the same indent the next command's
name reads as another line of the output above it. This is where a stack differs from a run of
reads, which stops at the names: the *contents* of six files is what that fold existed to keep out
of the transcript, and the output of three commands is what this one is holding.

**A run does not continue past a failure.** A stack shows the last output, so a failed call in the
middle of one would be a failure that folded away — and the fold is about noise, which a failure is
not. `Card::takes` refuses to grow a card that already holds a failure, so the failed call is the
last leg on its card and its output is the one showing, and whatever came next starts a new card.
Calls made in parallel can still land out of order, so the fold shows any failed output wherever it
sits, and names each one when there is more than one thing to name.

## Consequences

- `cards::Head` gains the `ToolResult` it came back with. Only a stack of commands reads it; a
  header is a sentence about a call and never about its output, which is why the line count stayed
  a separate field.
- `group_body` takes `Limits`. The replay grew `set_body`, because a card now *loses* a body as
  well as gaining one: the moment a second command joins, the first one's output comes off the
  screen. The live path already redrew the whole card and needed nothing.
- **A folded stack costs two or three rows for a stretch of the turn that used to cost eight.** The
  `git add`/`git commit`/`git push` example above goes from eight rows to three.
- The verb on a stack is coloured by its kind, so `Ran` is `Agent.ToolRun` where `Explored` is
  `Agent.ToolRead`. The old code hard-coded the read colour, which nobody had noticed because
  nothing but reads could get there.
- A command whose output you were reading can be folded away by the *next* command starting. That
  is the cost, it is one `⇥` away, and it is the same bargain ADR 0040 struck for reads: the run is
  a summary and the key is the detail. What it is not allowed to do is hide a failure, which is
  what the two rules above are for.
- A stack of one is not a stack. A single command is an ordinary card with an ordinary body, and
  nothing about it changed.
