# 0057 — A terminal is a place you are, not a copy of somebody else's screen

**Status:** accepted

Finishes [0042](0042-however-many-terminals-you-open.md), which allowed several terminals and made
them mirrors, and named this as the work it was not doing.

## Context

ADR 0036 made the workspace a process and the terminal a viewer of it. ADR 0042 then allowed
several viewers — but as mirrors: one `Editor`, one cursor, one composer, one conversation on
screen. Every attached terminal drew the same frame, and typing in either was typing in both.

That was the honest half of the ask and it says so. The other half was in the same sentence and has
been in the "what this still does not do" section ever since:

> Independent views — a transcript here, a diff there, two people in one workspace — need `Host`
> split into a shared `Workspace` and a per-view `View`, `Agent`'s notion of "the conversation on
> screen" moved out of shared state, and an `ApiCall` that names the view it draws into.

The reason to want it is the reason ADR 0042 gives for allowing a second terminal at all. **You
open the second window because the first one is busy.** A mirror of the busy window is precisely
the thing you were trying to get away from: you wanted to keep watching the turn *and* go and look
at something else, and a copy gives you the first without the second.

It is also the shape everything else in neosh already had. A conversation is not the program's
(ADR 0030); a turn belongs to a conversation rather than to the screen; the transcript is a place
you go (ADR 0028). The one thing that was still singular was *where you are*.

## Decision

**What the agent produced is the workspace's. Where you are looking is yours.**

That line decides every case below, and it is already how `neosh-core` is built: a `Buffer` holds
the text and a `Window` holds the cursor, the anchor, the selection and the scroll offset. The
window map had to gain an owner; nothing had to be invented.

**A client and a view are different things.** A `ClientId` is a terminal on the end of a
connection: it has a size, it reports the geometry it resolved, and `^Q` closes it. A `ViewId` is a
set of windows. They are one to one today, and they are two ids because the relationship is not
forced to be — several clients on one view is exactly the mirror ADR 0042 shipped, and keeping the
distinction is what let this arrive as four changes rather than one.

**`ViewId` is on the wire.** ADR 0042 kept it out deliberately, and gave the reason: every view saw
the same frame, so the number bought nothing and cost a field to keep in step. A float that has to
land in *this* terminal is what changed.

**Windows belong to a view; buffers do not.** Focus, mode and a half-typed chord go with the
window, because all three are facts about one keyboard. Keymaps, commands, options, highlights and
contributions stay shared: a binding is registered once and means the same thing wherever you press
it, and only where you pressed it differs.

**A frame is routed, not broadcast.** Every event is tagged with the view it is about, `None`
meaning everybody, and `push_ui` reads the tag off the window the event names rather than taking it
at two dozen call sites — where the one that was forgotten would be a panel drawn on somebody
else's screen. Two cases cannot be derived and say so by name: a window that has just been closed
is no longer in the map to be asked, and focus becoming nothing names no window. A terminal whose
share of a frame is only the trailing flush is not woken up: a flush is a full repaint, and
twenty of them a second because something happened in somebody else's conversation is what the
routing exists to avoid.

**The transcript is per view, not per conversation.** Two terminals reading one conversation render
the same messages twice, which costs a render and buys the thing that matters: folding a card open
is *navigation*, so `⇥` on a diff must not move the rows under somebody else who happens to be in
the same conversation. The rows say the same thing because the rows are what the agent produced;
where you are in them is yours. The composer goes the same way, and for the same reason.

**Which conversation is on screen is a fact about a terminal.** `SessionStore` held one session out
of the map and called it active, which is a fair description of a workspace with one screen and a
false one the moment there are two. It is flat now — every conversation, and the order they were
last arrived in — and *who is looking at what* is the host's, one answer per view. `switch` became
`enter`, told which conversation is being left rather than working it out, because leaving a
placeholder is what drops it and a placeholder somebody else is sitting in is not one anybody left.
`step_away` became `next_after`, a question rather than a move: two views on the conversation being
deleted each have to be told, and the store has no idea how many there are.

What survives of the old idea is `current`, the most recently arrived in anywhere. That is a real
thing to want — a question asked outside any turn is about somewhere, and a workspace with nothing
attached still has a most recent conversation — but it is a fallback and never a place somebody is.

**"Is this on screen" became "which terminals is this on".** A turn's output is drawn once in each
terminal showing its conversation, which may be none and may be two, and the mark saying a turn
ended where you could not see it (ADR 0042's other half) is set when that list is empty. A clock
tick is nobody's key press, so it says all of them.

**A terminal attaching lands in the most recent conversation nobody else is reading.** You opened
the second window because the first is busy; landing in the same place is the one behaviour
guaranteed not to help. When every conversation is already on somebody's screen — or there is only
one — it lands in the most recent, and two views of one conversation is a perfectly good thing to
be: same messages, own scroll, own draft.

**The last screen of all is kept.** Zero views is an ordinary state — closing the last terminal
leaves the turns running — but it must not mean losing where you were, so the last one is handed to
whoever attaches next. That is what makes leaving mid-answer and coming back find the answer still
arriving into the same transcript, rather than a rebuild that is missing everything the turn has
not committed yet. It is out of `view.list` while nobody is at it, and everything a plugin put in
it is closed when the last client leaves rather than left for the plugin to close asynchronously —
a panel closed a frame late is a panel still on screen when the next terminal arrives and its
replacement opens beside it.

**A plugin mostly does not have to say where it is drawing.** Three rules, in order, and the first
two are exact:

1. **A window names its own view.** A float anchored to one goes where that panel is, and a plugin
   tidying up after a terminal that has gone is the ordinary case rather than a mistake.
2. **A buffer only one terminal is showing names it.**
3. **Otherwise, the terminal being served.** For anything done in answer to a key that is exact;
   for a plugin acting on its own — a timer, an agent event it subscribed to — it is the last
   terminal to have pressed anything, which is a guess, and is why the next paragraph exists.

**And it can always be said outright.** `KeyContext` carries the view the key was pressed in;
`win.open`, `float.open` and `session.switch` take one; `view.at(id)` is the whole `neosh`
namespace bound to a terminal. The ergonomic layer makes all of it invisible for the ordinary case:
a command handler's third argument *is* that bound namespace, so `here.win.open(...)` is a panel
where the person pressing the key is looking and every other call on it is the call it always was.

**A dock is the one thing that has to be per view**, because one that exists once exists in one
place. `view.onOpen` is when to make it — fired for the terminals that were already here as well as
the ones still to come, so a plugin does not have to work out whether it was here first — and
`view.onClose` is when to let go.

**The workspace stopped being the size of its smallest terminal.** That rule existed because a tool
card was one row of content every view shared, and content measured for the wide window wrapped in
the narrow one. There is nothing left to share: the wide window fits the card to itself and the
narrow one next door elides it. What is still merged is the geometry of one window two terminals
are looking at, where the old reasoning holds exactly.

## Consequences

- **A question is put in front of the person reading the conversation that asked it.**
  `SessionChanged` says which terminal moved, because "the active conversation" is a question with
  as many answers as there are screens. The panel opens in a terminal showing that conversation,
  waits if none is, and closes again if that terminal switches away. One panel: a question is
  answered once.
- **Two terminals on one conversation are still a good thing to be.** Same messages, two scroll
  positions, two drafts, two sets of folded cards. That is a mirror of the *work* and not of the
  screen, which is what ADR 0042 was reaching for.
- **A plugin written before any of this still works.** Every float in the workspace — the palette,
  the model sheet, git status — builds its window per invocation, so rule 1 or rule 3 puts it where
  the key was pressed without a line changing. What needed rewriting was the one dock.
- **The same conversation rendered twice costs a render.** Measurable only with several terminals
  in one conversation, which is the case that used to be free and is now the unusual one. If it
  ever matters, the fix is to share the buffer and give the fold state a per-window home — not to
  put the cursor back in the workspace.
- **`Editor::apply` still means "the local view".** Every call site that ought to name a terminal
  says `apply_in`, and the split is deliberately visible so the ones that were not migrated are
  greppable rather than silently correct-looking.
- **Nothing here is about two *people* in one workspace.** It is the mechanism they would need, and
  the parts that are still shared — one permission layer per conversation, one composer per view
  and no notion of who typed what — are what that would have to revisit.
