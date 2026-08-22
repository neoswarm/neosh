# 0044 — What the plan has left is not a number you can count

Accepted.

## The problem

neosh could tell you three things about what a conversation was costing: how full the context window
was, how many tokens the conversation had spent, and what those tokens would have cost at API rates.
All three are *derived* — counted here, from things that happened here.

None of them is the number that stops you working.

On a subscription, what stops you is a rolling allowance the vendor enforces and will not itemise: a
five-hour session window, a weekly window, and — increasingly — a weekly window scoped to one model.
It is opaque by construction. Nothing on this machine can compute it, because it is not a function of
anything on this machine: a turn you ran in `claude` directly, in another editor, or from a script
spent the same allowance, and the exchange rate between tokens and percent is the vendor's business
and changes without announcement.

So it cannot be counted. It can only be *reported*, and then *kept*.

The consequence of not having it is specific and expensive. You start something long, twenty minutes
in the agent stops, and the first you hear of the limit is the refusal. The information existed the
whole time — `claude` had been printing it on a line in its own output stream that neosh parsed and
discarded — and the one moment it was not available was the moment before you committed to the work.

## The decision

**Quota is reported, kept, and drawn where you look before you start.** Three parts.

### A window is whatever the vendor says it is

`QuotaWindow::id` is a string, not an enum. Anthropic's `/api/oauth/usage` answers with a `limits`
array of `session` / `weekly_all` / `weekly_scoped`; the same account's `rate_limit_event` line spells
those `five_hour` / `seven_day`; `codex` reports `primary` and `secondary` and names neither. The
array grew three kinds between two releases.

A fixed enum would mean a limit that actually binds — the per-model weekly cap that is, in practice,
the one people hit — being dropped for not being on our list, silently, in the release where it
started mattering. So anything with a percentage gets a row, and a kind nobody here recognises is
titled from its own name.

The two Anthropic spellings *are* canonicalised, because they are the same allowances and the store
patches by id: left alone, the session limit would appear twice at different percentages, and the
fuller of the two would win a gauge it should never have been in.

Severity is graded **by the vendor**, not by a threshold here. Anthropic calls 94% of a weekly window
`critical` and 55% of a session window `normal`. A client that decided at 80% for both would be
shouting about the wrong one.

### Patch or replace, decided by where it came from

- A driver mid-turn **patches**. `claude`'s line carries exactly one window and says nothing about
  the others; treating it as a snapshot deletes the weekly row every time the session row moves.
- A poll or a plugin **replaces**. Both are complete by construction, and only a replace can express
  a limit that has *gone* — a scoped cap that expired, an overage switched off. Patched, those sit
  there forever at whatever they last read.

### Polling reads the vendor CLI's own login, and is a setting

At rest nothing reports anything, and at rest is when the question gets asked. `codex app-server`
answers `account/rateLimits/read` for free. Anthropic has no such door: the only way to ask is the
OAuth usage endpoint, and the only credential for it on the machine is the `claude` CLI's own.

That is the trade this codebase already makes — the `claude-cli` driver exists because reusing a
login you have is what makes neosh useful the minute it is installed, and demanding a second sign-in
for a number the first one already entitles you to would not be worth the ceremony. The rules do not
bend for it: the token is a `SecretString`, read at request time, dropped with the request, never
written, never logged, and structurally absent from every type in `neosh_proto::quota`. What crosses
a boundary is percentages.

And it is `usage.poll`, because a machine where reaching into another program's state directory is
the wrong answer should be able to say so and still get the mid-turn numbers.

## What this is not

**It is not joined to the token history.** They are two types, two calls and two blocks of the panel,
and they never share an axis. A percentage of an opaque allowance and a token count do not convert
into each other, and a chart that implied they did would be inventing an exchange rate. The
temptation to add them is constant and it is always wrong.

**The history is a different question, read from a different place.** "Where did the week go" is
answered by scanning the vendor CLIs' own transcripts — `~/.claude/projects/**/*.jsonl`,
`~/.codex/sessions/**/*.jsonl` — and not from neosh's conversations, for the same reason the quota
cannot be counted: a turn run elsewhere spent the same allowance, and a history that could not see it
would answer a question nobody asked. This is what `ccusage` does and it is the right approach.

Every part of that scan is bounded, because it walks directories somebody else fills: files outside
the span are never opened, a line is parsed only after a substring test says it might carry usage, and
the whole scan is capped — and a scan that hits the cap **says so**, because a truncated history drawn
without a caveat reads as a quiet month.

**Percentages have no history on disk anywhere**, so one exists only because neosh writes each answer
down as it arrives. Sampled on change rather than on a timer: a flat line does not need a point every
thirty seconds saying it is still flat. The gaps are left as gaps — a straight line across the
fourteen hours the machine was asleep is usage that did not happen.

## The shape it lands in

`ApiCall` + generated TS + a plugin that uses it, in that order, as every capability does. The strip
is a `sidebar.section` contribution; the panel is a buffer with a `kind`; every key is a named
command. `quota.report` lets a provider written as a plugin publish its own vendor's allowance and be
kept, sampled, broadcast and drawn exactly like a built-in one — gated on having registered the
driver that serves the instance, because a figure anybody could spoof is not one to draw as fact next
to a decision about spending an afternoon.

## A measurement without a name is not information

The strip shipped as three bars, three percentages and three countdowns, and every one of them was
correct. It was still not readable, for two reasons that are the same reason.

It never said **whose** allowance it was. `QuotaSnapshot::instance` is a configuration key, and the
strip drew none of it — so on a machine with a Claude plan and a Codex one it was two stacks of
identical bars with nothing between them, and on a machine with one it answered "how much is left"
without ever saying *of what*. The account row fixes it with the [`Brand`] the model picker already
draws: same mark at the same three fidelities, same brand colour, same display name. The point is
not decoration. It is that the thing you chose in `^P` and the thing being spent here are visibly
one thing, and nothing had ever said so.

And it never said **which way round** any of it went. A bar that fills as an allowance is spent
reads as "how much is left" to about half of everyone; a bare `3h` beside a percentage is as easily
"running for" as "back in". So one row per account is a sentence — `78% left · back in 3h` — about
the window the vendor marked `active`, with `▸` on the row it belongs to so the sentence never has
to name it twice. The panel does the same job with a column legend (`limit / used / last 7 days /
refills`) and two lines saying what a rolling allowance is, because a percentage of something opaque
is exactly the number people otherwise plan around as though it were money.

Two smaller things follow from the same rule. A label is shortened by giving up its separator, then
by abbreviating its *base*, and only then by clipping its scope — `Weekly · Opus 5` clipped from the
right became `Weekly ·…`, which loses the only characters that distinguish a per-model cap from the
plain weekly one and leaves two rows with the same name and different numbers. And a countdown
counts in days once there are days: `elapsed` measures a turn, which never runs for a week, so a
weekly window came back as `resets in 164h 34m` — a number you have to do arithmetic on to find out
means Tuesday.

The way in is on the heading. `sidebar.section` grew an optional `hint`, drawn dim at the right of
the title, because a summary with no way out of it is one you read and then have to go and find the
rest of by guessing — and `^L` pressed from the composer is a different affordance from a row you
have to focus the sidebar and arrive at. It is on the contribution point rather than special-cased
for this plugin, so the git panel or anybody else's section can say the same thing.

## Consequences

- Two numbers now exist that neosh cannot verify and did not compute. Both carry their age, and the
  panel draws it, because a percentage with no age on it is trusted for longer than it should be.
- The cost figure in the history is API-equivalent, not money spent. A subscription bills separately
  and a plan turn costs nothing extra at all. It is there because it is the only common unit that
  puts Opus and Haiku on one axis, and `fully_priced` is what a heading checks before it writes a
  currency symbol.
- A model with no published rate still appears: its tokens count and its cost does not. Dropping it
  would make the busiest week look like the quietest.
- Reading another program's credential store is now something neosh does. It is one file, one
  endpoint, one setting, and it is written down here so that the next thing to propose it has to
  argue from this precedent rather than establish it.
