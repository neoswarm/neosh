# 0050 — A card is attached to what it did

## What happened

A tool card was drawn with a blank row above it and a rule down the left of everything under it:

```text
Let me look at the config and run the tests.

  Read  host.rs, cards.rs

✗ Ran  cargo test -p neosh-core
  │ no such tool: bash

All green.
```

Three actions and a sentence cost nine rows, three of which are empty and about forty columns of
which are a vertical bar repeating a fact the indent already carries. On a diff the bar runs down
the side of the code it is meant to be showing.

Both were deliberate, and both are written down in ADR 0040 and in `cards.rs`: air is the cheapest
structure there is, and a rule answers "how far does this go" where an elbow only says "it starts
here". The arguments are not wrong. They are answers to a question that stopped being open once a
card became *a header at column zero with its body indented*: the shape says where an action begins
without spending a row on it, and the extent of a body is legible from the indent because nothing
else in the transcript is indented that way.

## The decision

**No blank row above a card.** Prose keeps its paragraph break — a sentence and a command are
different kinds of thing — but two commands are not, and a run of actions reads as a list because it
*is* one, not because there is air in it.

**A corner rather than a rule.** The first row of a body wears `└` in the same four columns every
other row of it is indented by; the rest are plain indent. That marks the one moment the eye needs
telling — where the body starts, which now matters more, because the row above it is another card
rather than a blank. `⇥` still opens the whole thing, and an opened diff is four columns of code
wider than it was.

**Compaction moves the meter.** Unrelated to the layout and found in the same place: `Activity::
Compacted { after }` was drawn as a card and thrown away. A driver that has once answered "how full
is it" turns off the estimate `Session::add_usage` derives from a request's prompt, so nothing else
was going to correct it — the footer reported the pre-compaction number for the rest of the
conversation, directly under a card that said `180k → 12k`. The driver said `post_tokens`; that is
what it is for.

## What was considered instead

**Keeping the rule and dropping only the air.** The two are the same argument: both spend a
dimension of the layout on saying where a thing starts and stops, and the header and the indent
already say it. Half the change would have left the transcript denser and still walled.

**No mark at all under the header**, only the indent. That is the failure the rule was written
against in the first place, and removing the blank row above makes it worse rather than better: with
another card on the row above and nothing to say a body has begun, a result reads as loose text that
happens to be indented. The corner costs one glyph on one row and answers it.

## Consequences

- `Glyphs::rule` is gone; `Glyphs::elbow` replaces it, `Glyphs::margin` is four spaces and
  `Glyphs::opener` is the corner. A body is laid out from `margin` as before, and `attach` swaps the
  first row's four columns — one place, so nothing downstream has to know there are two margins.
- The live path and the replay both draw no gap, which is ADR 0040's other rule: a conversation that
  changes shape when you switch away and back reads as two programs having written it.
- Ten card unit tests and two integration tests carried the old layout in their expected values.
  What they were about — folding from the middle, limits, tabs, a run opening into a row per call —
  is unchanged.
