# 0007 — Extmarks live on lines; columns are byte offsets

**Status:** Accepted.

## Context

The brief is specific: extmarks "must shift when text above it changes", because "naive
`(line, col)` decorations desync within seconds of streaming output".

## Decision (a): a mark is stored inside the line it annotates

```rust
struct Buffer { lines: Vec<Line> }
struct Line   { text: String, marks: SmallVec<[Mark; 2]> }
```

`set_lines(start, end, lines)` is a `Vec::splice`. A mark's row *is* its line's index.

## Why

The obvious design — a map keyed by `(row, col)` plus a fix-up pass that shifts every mark below an
edit — is O(marks) per edit. Streaming output edits the buffer once per token, so it degrades
exactly when the editor is busiest. That is the same failure the brief warns about, arriving as a
performance cliff rather than a correctness bug.

Storing marks on lines makes "virtual text survives an append above it" a **structural property**.
There is no shift pass, so there is no shift pass to get wrong. Inserting 500 lines above a mark is
500 splices and zero mark updates — there is a test for exactly that.

## The rule that makes streaming work

For rows inside a replaced range, a mark **survives with its column clamped** if the new content has
a line at the same offset from `start`; otherwise its `on_delete` policy applies. Without this,
appending one token to the last line — which is `set_lines(last, last+1, [longer_text])` — would
orphan every mark on that line, once per token.

## Cost

Finding a mark by id is O(lines). That is the right trade: the renderer only ever asks "what marks
are on the lines I am about to draw", which this layout makes trivial, while by-id lookup is a rare
plugin operation. When buffers get large enough for the scan to matter, the replacement is a marker
tree — and because the access is behind `Buffer`'s methods, that is a contained change.

## Decision (b): columns are UTF-8 byte offsets

Not character counts, not display columns. Same as Neovim, for the same reason.

The core does **zero** `unicode-width` work. The frontend converts byte offset to screen column at
draw time, in one function. Since agent output is full of emoji and CJK, this puts all the width
math in one place where it is tested, and makes the "count chars, corrupt layout" bug unwritable in
the core.

Both ends clamp to character boundaries, so a desynced or mid-codepoint offset degrades to a
slightly-left position instead of panicking.
