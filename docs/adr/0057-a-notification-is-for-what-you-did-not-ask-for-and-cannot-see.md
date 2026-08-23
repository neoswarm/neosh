# 0057 — A notification is for something you did not ask for and cannot see

**Status:** accepted

## Context

There is one notification channel and everything is on it. `neosh.notify(text, level)` becomes
`ApiCall::Notify`, becomes `UiEvent::Message`, becomes a six-second toast in the bottom-right
corner: newest at the bottom, three at a time, two hundred kept. It is the only feedback channel
in the program, so every part of the program uses it — around a hundred and seventy call sites,
thirty-eight in the sidebar, thirty-three in git, sixty-six in the host itself.

`MessageLevel` is `info | warn | error`, which says *how bad a thing is* and never *whether you
need to know about it*. Those are different questions and only the second one decides whether
something should interrupt you. So the channel cannot sort itself, and what comes out of it is a
corner of the screen that is almost always saying something and almost never saying anything.

Read the call sites and they are three different things wearing one coat.

**Most of them are replies to a key you just pressed, restating what visibly changed.**
`favourited ~/proj` — the row just grew a pin and moved to the top of the panel. `archived — 'a' in
the panel finds it` — the row is gone. `new conversation in <worktree>` — you are in it, the
transcript is empty, the footer says the branch. `on <branch>` — the footer says that too. The
message is true, and it is the same fact the screen is already showing, printed a second time in a
corner. It is not news. It is an echo.

**Some of them are progress, and progress is a state rather than a message.** `pulling…`,
`naming the branch…`, `asking <node>…`. Each is true for about a second and then is not, and each
is pushed onto a stack that has no idea it has been superseded. The message that replaces it is
also pushed. So a pull draws two rows where it meant to draw one that changed.

**A few of them are the real thing** — an error, a node lost, a pairing request — and they are
indistinguishable from the other hundred and sixty.

Meanwhile, the two events that most deserve to reach a person have no notification at all. A turn
finishing sets `SessionInfo::unread`. A question waiting sets the `question.asking` var. Both are
marks in the sidebar, and both require that you are already looking at neosh. There is no OSC
sequence, no bell and no desktop notification anywhere in the tree. The channel is loud about
things you can see and silent about things you cannot.

### What ADR 0042 already decided, and why this is not a reversal of it

ADR 0042 considered a notification for exactly this case and rejected it, in three sentences worth
quoting because they are still correct:

> A toast is gone in four seconds and the wait is measured in minutes, so it is precisely the
> message you are not there for. A bell says something finished and not which thing. And neither
> survives closing the terminal, which is the case that needs it most.

All three hold. None of them is an argument against notifying; they are an argument against a
notification being the *record*. `unread` is the record — durable, per conversation, cleared by
arriving, and correct whether you were there or not. What 0042 did not solve is the case where you
are not looking at neosh at all: a mark in a program that is not on your screen is a mark nobody
reads. The notification is not a replacement for `unread`. It is the thing that points at it, once,
at the moment it becomes true, and then gets out of the way and lets the mark do the remembering.

Which also answers "a bell says something finished and not which thing": a bell does, and a
notification that names the conversation does not.

## Decision

### The rule

**A notification is for something you did not ask for and cannot see.** Three tests, and a message
must pass all three or it is not a notification:

1. **You did not ask for it.** A message that answers a key you pressed is a *reply*. It belongs
   where you were looking, it is about that keystroke, and it dies with the next one.
2. **You cannot already see it.** If the row moved, the panel closed, the footer changed — the fact
   is on screen. Saying it in the corner is the same fact twice, and the corner is where you learn
   to stop looking.
3. **It is still true when it arrives.** `pulling…` is a state that gets replaced, not a message
   that accumulates.

### Kind is not level

`MessageLevel` stays exactly what it is and keeps meaning how bad. A second, orthogonal field says
what *kind* of thing this is, and the kind is what decides where it goes and how long it lives.

| Kind | Is | Lives | Can leave the terminal |
|---|---|---|---|
| `Reply` | feedback for a key you pressed | until the next one, or 6s | only if it is an error |
| `Progress` | a state, keyed, replaced in place | until `done`, or 60s | never |
| `Alert` | something happened that you did not cause | until seen | yes |

`Reply` is the default, so `neosh.notify(text)` keeps meaning what it means and none of the
existing call sites change behaviour by being left alone.

**A `Reply` does not stack.** One at a time, newest replaces previous. Two keys pressed quickly are
two keys, and the reply you want is the one for the second. The three-high stack was the corner
trying to be a log, and there is already a log — `ApiCall::Log`, which is what a message nobody
needs to read at the time belongs in.

**A `Progress` is keyed and idempotent.** `notify.progress(key, text)` writes the row with that
key; a second call with the same key replaces it; `notify.done(key)` removes it. Progress draws
above replies and never expires on a timer — but it is dropped after sixty seconds regardless,
because a plugin that failed to call `done` must not be able to leave a permanent row on screen.

**An error is an error whatever kind it is.** A `git push` that fails is a `Reply` — you pressed
the key — and it still has to reach you, because a push takes thirty seconds and you may not be
there any more. So the escalation rule below reads `level` for errors and `kind` for everything
else, rather than making `Failure` a fourth kind that duplicates `Error`.

### Where a notification goes is decided by where you are

This is the whole system. One question — *can they see this already?* — asked against the
conversation the thing happened in, and answered against where the person actually is.

| Where you are | Turn done | Question / permission | Error | Reply / Progress |
|---|---|---|---|---|
| In that conversation, terminal focused | nothing — it is on screen | the panel opens | the corner | the corner |
| Another conversation, focused | the sidebar mark | mark + footer count | the corner + mark | the corner |
| Terminal not focused | **OSC** + mark | **OSC** + mark | **OSC** | nothing |
| Nothing attached | **desktop** + mark | **desktop** + mark | **desktop** | dropped |

Nothing ever leaves the terminal about a thing you are looking at. That single line is what kills
the noise, and it is not a heuristic — the host knows which conversation is on screen, which views
are attached, and whether any of them is focused.

### The notification belongs to the terminal, not to the workspace

The workspace is a process and the terminal is a viewer of it (ADR 0036), and the person is at the
terminal. So the raise is done by the *view*, as an escape sequence on the stream the UI is already
drawn on — which is precisely the argument `UiEvent::Clipboard` already makes for OSC 52 over a
clipboard library:

> a library talks to the X11 or Wayland display on the machine the process is running on — which is
> the wrong machine.

A `notify-send` from the workspace has the identical bug, and it is the common case rather than an
edge one: a coding agent runs on the big machine and you are SSH'd into it from the laptop you are
sitting at. The notification has to come out where your eyes are.

So `UiEvent::Alert { title, body, level }` is sent only to views, only when the host has already
decided you are away, and the view decides how to raise it: OSC 9, OSC 777 or kitty's OSC 99,
wrapped in the tmux passthrough when `$TMUX` is set. A terminal that implements none of them
ignores all of them silently — the same honest failure OSC 52 already accepts, and for the same
reason: we cannot ask a terminal what it supports, and refusing to notify on the chance it will not
land is worse than a sequence that does nothing.

**The desktop fallback is for when there is no terminal to send to.** With nothing attached there is
no view, no stream and no wrong machine to be on — the workspace's own machine is the only one
there is. Then, and only then, it shells out: `notify-send`, `osascript`, `powershell`, tried in
order, exactly as `images.rs` already does for the clipboard.

### Away is focus, and where there is no focus it is idleness

`EnableFocusChange` and `Event::FocusGained` / `FocusLost` are what a terminal that supports mode
1004 will tell us, and that is the honest answer. A terminal that never reports is not assumed to
be focused: assuming focus means you get no notifications at all on terminals that cannot say, which
is the failure that matters. It falls back to the last keystroke — no input in this view for
`notify.idle_after` (60s by default) is away.

Focus is per view, and a workspace is away only when *every* attached view is. Two terminals open
and one of them focused means you can see it.

### A turn you were there for is not news

A turn that took three seconds finished while you were still looking at the key you pressed to
start it, and a notification for it is a notification for something you watched happen.
`notify.min_turn` (10s by default) is the floor: below it, a finished turn sets `unread` like
always and raises nothing.

This is also the fix for a bug ADR 0042 left behind. `s.unread = !on_screen` counts *detached* as
on-screen, on the reasoning that reattaching lands you in that conversation with the answer in it.
That is true for the mark and false for the notification — with nothing attached, a finished turn is
the single most newsworthy thing the workspace can produce. Attachment and visibility are separated
here rather than folded together.

### Bursts are one notification

Three conversations finishing while you are at lunch is one notification that says three, not three
notifications. Coalescing is per view on a short window, and identical text within it is one row
with a count rather than two rows. At most one alert per conversation per turn, always.

### Raising one is a capability

A plugin that can put a notification on your desktop unprompted is a plugin that can interrupt you,
so `ApiCall::Alert` needs the word `notify` in `plugin.toml` and `ApiCall::Notify` does not.
Drawing in the corner stays free, like every other kind of drawing.

## Consequences

`neosh.notify` keeps working and keeps meaning `Reply`, so nothing breaks by being left alone — but
the call sites that restate what visibly changed are deleted rather than reclassified, because the
right number of messages for `favourited` is zero. The ones ending in `…` become `Progress`.

`unread` and `question.asking` are untouched and stay the record. Everything here is a pointer at
them.

Terminals differ in what they raise, and we cannot detect it. `notify.osc` names the sequence when
`auto` guesses wrong, and `notify.desktop = "always"` is the way out for someone whose terminal
does nothing at all — at the cost of being the wrong machine over SSH, which is their call to make
and not ours to make for them.

Independent per-view alert routing — this terminal notifies, that one does not — is not here. It
needs the same `Host` split into a `Workspace` and a `View` that ADR 0042 deferred, and everything
above is correct without it.
