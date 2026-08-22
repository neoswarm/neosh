# 0043 — A question is not a permission

**Status:** accepted

## Context

ADR 0032 gave an agent driver somewhere to put a question it could not answer alone. The shape it
built was `PermissionRequest` — a title, a capability, a list of options the asker worded itself —
and it was exactly right for what it was built for: *may this happen*. Every path an agent driver
could ask down was pointed at it.

Then `claude` grew `AskUserQuestion`, and it came down the same pipe.

It arrives as an ordinary `can_use_tool` control request, distinguished only by
`requires_user_interaction: true` and the tool's name, and it is not a permission request in any
sense that matters:

```json
{"subtype":"can_use_tool","tool_name":"AskUserQuestion","requires_user_interaction":true,
 "input":{"questions":[
   {"question":"Which database should this use?","header":"Database","multiSelect":false,
    "options":[{"label":"Postgres","description":"…"},{"label":"SQLite","description":"…"}]},
   {"question":"Which of these should be enabled?","header":"Features","multiSelect":true,
    "options":[…]}]}}
```

Three questions in one request, one of them taking several answers, and the reply it wants is not a
yes:

```json
{"behavior":"allow","updatedInput":{"questions":[…],
 "answers":{"Which database should this use?":"SQLite",
            "Which of these should be enabled?":["Logging","Tracing"]}}}
```

Read as a permission request, this fails in three ways that compound.

1. **Policy answers it.** `PermissionLayer::check` sees `Capability::Exec { command:
   "AskUserQuestion" }` — the fallback for an unrecognised tool — and the configured default is full
   access (ADR 0038), so it returns `Allow` without anybody being asked anything.

2. **A yes carries no answers.** `PermissionAnswer::Allow` becomes `{"behavior":"allow"}`, which is
   a perfectly valid response and means "run the tool". The tool runs with no answers in it.

3. **The agent is told you ignored it.** The CLI turns that into a tool result reading, verbatim:
   *"The user did not answer the questions."*

So a turn asked you something, never drew it, and then reported to its own model that you had
declined to reply. Nothing errored. Nothing logged. The only symptom was a model that occasionally
said *"since you didn't answer, I'll assume…"* about a question you were never shown.

That is a silent failure produced by an *abstraction*, not by a bug in it: every individual step did
the correct thing with the type it was handed. The type was wrong.

Why it is wrong is worth being precise about, because "add a field to `PermissionRequest`" was the
tempting fix:

- **A permission has one question; this has a list.** They are answered in one sitting, and going
  back to change the second answer is an ordinary thing to want.
- **A permission's options are decisions *about the request*.** `Yes`, `Yes and don't ask again for
  cargo test`, `No`. A question's options are the *subject matter*, one of them may be several of
  them at once, and any of them may be something nobody listed.
- **A permission is answerable by policy, and this is not.** That is the load-bearing difference.
  `permissions.mode = allow` means "do not stop to ask me before writing files". There is no reading
  of it under which it also means "and choose a database for me". A mode, a rule and an allow-list
  are all statements about *risk*, and a question is not about risk.

## Decision

**A question is its own family, from the wire to the panel.**

`UserQuestion`, `QuestionOption` and `QuestionAnswer` in `neosh-proto`; `QuestionAsker` in
`neosh-provider`, beside `PermissionAsker` and independent of it; `HookName::AskUser` for a plugin
to serve; `ApiCall::AskUser` for a plugin to raise. `claude_control::can_use_tool` now *refuses* a
request with `requires_user_interaction` set, and `claude_control::ask_user_question` claims it, so
neither path can quietly take the other's traffic.

Five things follow, and each was a decision in its own right.

**The question is its own key.** `UserQuestion` has no id. The CLI looks answers up by the *text of
the question they answer* — `updatedInput.answers` is a map keyed on it — so a generated id is an
answer to a question that does not exist, and the turn comes back saying nobody answered. One field
for both, and a doc comment saying why, because the temptation to add an id is permanent.

**The questions go back exactly as they came.** `updatedInput.questions` is the array neosh
received, not one rebuilt from `Vec<UserQuestion>`. The CLI re-reads it to draw its own record of
the exchange, and a round trip through neosh's types would silently drop every field added to the
tool after this was written.

**Nobody answering is a denial with a sentence in it, not an empty yes.** An `allow` carrying an
empty `answers` map is what produced *"The user did not answer the questions"* — a statement about
you, in words nobody chose, with no reason attached. A `deny`'s `message` is passed to the model
verbatim, so the difference between *"the user dismissed this"* and *"there is nobody at this
workspace to ask"* survives, and those are two different things for a model to do next.

**Policy is never consulted.** `DriverQuestioner` does not hold a `PermissionLayer` — not "does not
call it", does not *have* one. The failure this ADR exists for was one line of policy check in the
wrong place, and the way to make it un-reintroducible is for the type to have nothing to check
with.

**Failing closed here means "nobody answered", which is an ordinary outcome.** A permission hook
that times out is a veto, because failing open on a permission is unsafe. A question hook that times
out is a fact the agent is told in a sentence and goes on from. No blocking hook at all — the panel
disabled, a headless workspace — is answered immediately rather than after a timeout, because the
answer is known and a turn that hangs for thirty minutes on a workspace nobody is looking at is
worse than one that is told the truth at once.

## The panel

Everything a bundled plugin draws, a third party must be able to replace (ADR 0040), so the panel is
`plugins/builtin/questions/` and it is served by the `ask_user` hook. Turning it off is
`plugins.disabled`; replacing it is registering the same hook from a plugin that loads later.

**One question at a time.** Four at once is a form, and a form over a transcript is a modal you must
finish before you can read what prompted it. A rail at the top right says which one this is —
`◉○ 1/2` — and `⇧⇥` goes back to change an earlier answer, because the second question is often what
tells you the first one's answer was wrong.

**The composer is the field** (ADR 0037). Typing is not filtering, it is *answering*: the option
nobody listed, which is the one people reach for most. It goes into the composer, where it is
visible and editable, and the panel echoes it on a `✎` row. A digit is a shortcut while nothing has
been typed and a character once something has, and the panel says which by wearing the numbers or
not — the badges disappearing is the notice that the keyboard's meaning changed.

**A field is a field, and the row is one.** The `✎` row was drawn with the same `clip` as an
option's label, so an answer wider than the panel became its first sixty characters and an ellipsis
— and stayed that way however much more was typed. The caret stopped, the words went somewhere
invisible, and the only way to read back what you had written was to send it: the one row on this
panel whose entire job is to show you what you are writing was the one row that would not. So it
wraps, onto as many lines as the sentence takes, and past six it keeps the *end* — the end is where
the caret is, and a field that scrolled the other way would show you everything except the word you
are typing. The wrap keeps every character it was given, spaces included, because a line that
rewrites what you typed is one the caret can no longer be placed on honestly.

**What you are writing belongs to the question, not to the panel.** Which question you are on and
which row the cursor is on live on the ask, which outlives the panel; what was in the field lived in
the panel, and the panel is closed and built again from nothing every time the screen moves to
another conversation. Go and check something next door — which is very often exactly what the
question sent you to do — and the sentence was gone, with no key having been pressed that could have
deleted it. It is kept on the ask, and restored into the composer as well as onto the row, because
switching conversations empties the composer and an answer on the panel that is not in the field
below it is two answers that disagree.

**Every key is a named command** bound against the `neosh.question` buffer kind, so `F1` lists them
and `init.ts` can move them. Only printable characters come through the raw capture, and only
because they are text rather than verbs.

**A question belongs to its conversation.** Several turns run at once and one of them asking is not
a reason to take the screen away from the one you are reading, so a question for a conversation that
is not on screen waits, says so in the footer, and opens when you switch to it. A turn that ends
takes its unanswered question with it — the driver has already told the agent, and a panel with
nothing behind it is one whose `↵` writes into a pipe that has moved on.

**Which conversation is asking is a fact about the conversation.** The footer can only ever say
*how many* — it is one line and it is shared — so on its own it turns "somebody is waiting on you"
into a hunt through the conversations for the one that is. Worse, the row you are hunting for is the
one that looks least like it needs you: a conversation blocked on a question still has its turn in
flight, so `active_turn` is set and every list draws it as thinking, identically to the twenty above
it that really are.

So the panel writes `question.asking` — the session ids currently waiting — as a **workspace var**,
and the sidebar reads it. A var rather than this plugin's state for the ordinary reason (ADR 0040):
it is about the conversations, not about us, and a question panel of your own writes the same key and
lights up the same rows without either plugin knowing the other exists. Three things follow.

*It outranks working.* Anything drawing a conversation has to check this before `active_turn`, or the
spinner wins and the mark is never seen — which is the whole bug.

*It moves, and `unread` does not.* `Status.Pending`, the palette's group for waiting on something
outside the program, which is what the footer's `Question.Waiting` already links to. ADR 0042 argues
a mark for news must be still, because news does not stop being true if you ignore it; this is the
other case. A question is a *block* — nothing in that conversation goes on until you answer — and it
ends the moment you do.

*It is on disk and the queue is not.* `vars.json` is written through, and the asks live in memory, so
a workspace that stopped with a question on screen would come back claiming one is still open. The
panel therefore announces once at startup, when the queue is empty, and that write is what clears it.

**Layout is in columns and marks are in bytes**, and confusing the two is not theoretical: the first
build padded rows with `byteLength`, so `❯` — one column, three bytes — made every row with the
cursor on it two columns short, and the shortcut column zig-zagged. The screenshot showed it
immediately and no test would have.

## Consequences

`ApiCall::AskUser` means a plugin can ask you a question through the same door an agent does, and
gets the same panel. That is the test from `CLAUDE.md` — could a third party have written this, and
can they replace it — answered in both directions, and it is also how the end-to-end tests raise a
question without a live agent.

ACP has no question channel in its base protocol, and the vendors that have added one have each
added their own. `set_question_asker` defaults to doing nothing, so those drivers are unaffected
until there is something concrete to map; when there is, it is a driver change and nothing above it
moves.

The `AskUserQuestion` card reads `Asked  Database, Features` rather than `AskUserQuestion(…)`, off
the arguments and by shape, like every other verb in `cards.rs` (ADR 0040). A plugin that asks the
same way gets the same card without asking for one.
