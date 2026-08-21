# 0041 — A question can have a picture in it

**Status:** accepted

## Context

Two things went wrong in the same screenshot, and they are not the same bug — but they were reported
as one, because from the outside they are one: *"I send something and it doesn't respond
correctly."*

### What was actually happening

A conversation with five `/compact` rows, four `Compaction canceled.` lines that belonged to none of
them, and then an ordinary question — *"and when I open neosh in another terminal…"* — whose only
answer was `Not enough messages to compact.` The question had been asked. Something else answered
it.

**The first cause is one process, one pipe, and a turn that was walked away from.** The `claude`
driver keeps one CLI per conversation and outlives every turn in it, which is the whole reason it
exists — the conversation lives on the CLI's side. `Live::lines` therefore outlives the turn too.
When a turn was abandoned — `<Esc>`, or switching to another conversation while this one worked —
the agent dropped the stream, the driver's next `tx.send` failed, and it returned. The CLI was
still mid-answer. Everything it had not written yet stayed in the pipe, and nothing else reads that
pipe, so the *next* turn read it: the tail of the abandoned turn, its `result` line, and a `break`
before the new prompt had produced a word of its own.

The next turn therefore reported someone else's answer, and then reported nothing. Both symptoms,
from one return statement.

**The second cause is that steering wrote one message per keystroke of yours.** Everything queued
while a turn ran was taken into the conversation at the next gap, as *N separate user messages* —
and `prompt_from` hands a delegating driver the newest user message and nothing else, because the
driver already holds the history. So of two things queued into the same gap, the older one was
drawn in the transcript as asked and then never put to anybody. There is no way to tell that apart,
from a chair, from an agent that read your message and ignored it.

### And the thing that was missing

You cannot paste a picture into a terminal. Bracketed paste is a *text* protocol: `⌘V` on a
screenshot arrives as nothing at all, or as a file path if you dragged the file. Every terminal
agent that supports images therefore does the same two things — a **key** that goes and asks the
system clipboard, and **a pasted path** read as the file it names — and neosh did neither, so the
answer to "look at this" was to describe a screenshot in words.

## Decision

### An abandoned turn is drained, not dropped

When the receiver is gone, the driver stops *forwarding* and starts *draining*: it asks the CLI to
stop, then reads to the `result` line that ends the turn, discarding as it goes, and leaves the
process at a turn boundary where the next prompt starts clean. The five-second deadline an interrupt
already arms is the bound — past that the process is killed and the slot emptied, exactly as before.

Two corollaries, both of which would otherwise undo it: a permission question that arrives during a
drain is answered `Deny` rather than put to somebody who has already left, and the `result` line
does not trigger the end-of-turn context question, which would re-arm the interrupt deadline at two
seconds and kill the process that draining exists to preserve.

### Everything queued into one gap is one message

`take_steering_into` joins what is waiting into a single user message, in the order it was typed —
the same join the end-of-turn path already did with what it was left holding. Nothing is drawn as
asked that was not asked.

### A picture is a block, and the block names a file

`ContentBlock::Image { path, media_type }` — not the bytes. A screenshot is a megabyte of base64, a
conversation holds as many as you paste into it, and a message is written to the session file, read
back on every attach and sent down the socket to whoever is looking. The bytes are written once
into the workspace's own directory and every message that mentions them says where; a driver reads
the file when it builds its request, which is the only moment anything needs them. A transcript on
disk that is mostly pictures is one that cannot be read back quickly, and this is the difference.

The media type is read **off the bytes**, because it has to be right or the provider rejects the
turn, and a `.png` that is really a JPEG is a thing that exists.

Every driver carries it, in its own vocabulary: Anthropic's `image`/`source.base64`, OpenAI's
`image_url` part array, Gemini's `inlineData`, ACP's `image`/`mimeType`, and `claude`'s stdin, which
takes the same `content` the API does. Codex is the exception and says why — it is a process on this
machine reading a file this machine wrote, so it gets `localImage` and the *path*, and base64 down a
pipe would be a megabyte of encoding to save nothing.

An image whose file has gone is **dropped from the request**, not a failed turn: the rest of the
message is still a question worth asking, and failing it would mean an old conversation could never
be replayed at all.

### Three ways in, and only one of them is a key

* **`^V` asks the clipboard.** This is the one that has to be a key, for the reason above. Nothing
  here knows how to talk to a clipboard — `wl-paste`, `xclip`, `pngpaste`, `osascript`, `powershell`,
  first one present that produces bytes. Every desktop already ships a program whose whole job this
  is; linking X11 and Wayland into a program that spends its life on a pipe would be the wrong
  trade, and would still be wrong on the machine where the workspace is headless.
* **A pasted path is the file it names.** Dragging a file onto a terminal window pastes its path,
  which is the only gesture a terminal has for "this file". Four gates before anything is swallowed
  — one line, one path, a file that exists, and bytes that are an image — because inserting what
  was typed is what somebody is expecting and eating it is not.
* **`agent.attach()`**, because the API is the product. `ChatAttach`, `ChatAttachments`,
  `ChatDetach`, `ChatDetachAll` and an `images` argument on `agent.send`, so a plugin that
  *produces* a picture — a rendered chart, a screenshot it took — puts it in a message the same way.

### It is a chip above the field, not a token in it

The attachment row sits between the rule and the queue, above the composer, for the reason the queue
does: it is part of the message you are writing rather than chrome, and it is the one part of that
message you cannot read off the field itself. `png 1920×1080 · 412 KB`, because what is worth
knowing about something you cannot see is what kind it is and how big. `⌥v` takes the last one off,
next to where you would have put it.

A placeholder token in the text was the alternative, and it means the message you send is no longer
the message you typed. See ADR 0037: the composer is the field.

### Shrunk on the way in, and not otherwise touched

Every provider we target scales an image down on arrival, so past 1568px on the long edge full
resolution buys a slower request and nothing else. An image already inside it is written through
**untouched** — re-encoding a PNG that did not need it is a way to make a screenshot of text
blurrier for nothing, and a screenshot of text is what people paste.

## Consequences

`Prompt` — the words and what was attached to them — replaces the bare `String` that used to travel
from the composer to the turn, because the two travel together the whole way and two arguments
everywhere is how they get out of step.

The clipboard is read by the **workspace**, which today is a process on the same machine as the
terminal viewing it. When that stops being true — a workspace on another host — `^V` will be reading
the wrong clipboard, and the fix is for the viewer to send the bytes rather than for the workspace
to go and look. That is a protocol change, and it is not this one.

`scripts/shot.py` grew `<paste:…>`, which sends a real bracketed paste rather than the keystrokes
that would spell it. Nothing about any of this was reachable by typing.
