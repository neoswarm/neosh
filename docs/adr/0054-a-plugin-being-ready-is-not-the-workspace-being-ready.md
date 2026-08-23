# 0054 — A plugin being ready is not the workspace being ready

## What happened

CI went red on twenty-four of the hundred and forty-seven tests in `builtin_plugins.rs` at once,
almost all of them in code the branch had not touched, and every one of them passed on a laptop.
The screen a failure printed said what was wrong:

```text
  model      mock/mock  ·  no key needed
  directory  /tmp/neosh-builtin-7725-cardfold/work

▌ go

✗ hello_greet
  └ no such tool: hello_greet
```

The test had written `agent.model = "editor/editor"` into `config.toml`, installed a plugin that
serves it, waited for that plugin to say it was ready, and typed. The mock answered.

Nothing about that is a test artefact. A configured `agent.model` naming a plugin's provider is
held — `Startup::pending_model` — and applied when the *last* plugin finishes loading, because
until then "no provider serves lab/brainy" would be the wrong thing to say about a plugin that is
about to register one. Between the plugin registering and the tenth bundled plugin finishing, the
selection is still the fallback. A turn sent in that gap goes to a model the user did not choose,
and the welcome above it says so in a line nobody reads twice.

The same window has two other doors in it. `session.new` is a command the sidebar registers, so
before the sidebar has loaded it is a name nothing answers to and the host says `no such command`
and moves on — which is how a test that cycled the permission mode and then asked for a new
conversation ended up asserting about the conversation it was already in. And `^E`, `^L` and `^T`
are keys nothing has bound yet, so pressing one does nothing at all.

None of the three is reachable by a human on an idle machine; all three are reachable by anything
that acts on the workspace as soon as it can, which is every test and any script.

## The decision

**A held `agent.model` is applied the moment something can serve it**, not when the last plugin
loads. A provider registering is the event that changes the answer, so that is where the question
is asked again. `retry_pending_model` keeps its job — it is still where a spec nothing *ever*
serves is reported and where the fallback is chosen — but by then it usually has nothing to do.

**A command nothing has registered yet is held, not refused.** Same shape as a deferred option and
a queued reload, and for the same reason those exist: a command belongs to a plugin, plugins load
in parallel with the terminal becoming usable, and "no such command" is a wrong answer to a right
question. They run in the order they arrived, once startup settles. After that a name nobody claims
is genuinely unknown and is reported exactly as before.

**The host says when the workspace is up.** `neosh.ready`, on the ordinary event bus, `from:
"neosh"`, broadcast once every plugin has loaded and again after every reload. Not a new wire type:
a plugin listens for it with the same `neosh.event.on` it uses for anything else, and a fact this
small does not earn a `UiEvent` of its own.

Which leaves the rule the three of them are the same rule. **Your own `activate` returning tells
you nothing about the workspace.** The others are still loading alongside you: a command you would
call is not registered, a contribution point you would read is empty, a model a plugin registers is
not selectable, and a key you would expect to be taken is free. Anything that depends on the rest of
the workspace goes in a `neosh.ready` listener.

## Consequences

The mock plugins in `builtin_plugins.rs` announce themselves from `neosh.ready` instead of from the
end of `activate`, which is what makes every `wait_for("… ready")` in that file mean what it has
always been read as meaning. Twenty-one of the twenty-four failures were that one line.

A plugin that wants the old timing still has it — `activate` is where you register what *you* own,
and registering is exactly the thing that must not wait for anybody. `neosh.ready` is for the
second half: the work that reads somebody else's.

The held-command queue is bounded by how long startup takes, and it is emptied by the same two
places that already apply a pending model and run a queued reload. A plugin whose `activate` never
returns still wedges those — it always has — and that is the argument for a load timeout, not
against holding.

One test in that suite was red for a different reason and is worth writing down beside these,
because it is the same shape: the last-30-days block is read from `~/.claude/projects` and
`~/.codex/sessions` — files another program wrote, which is the whole point of ADR 0044 — and the
test asserted on whatever was in them. On a laptop that is a real month of work and the assertion
passes; on a fresh runner it is `nothing in this span`. The sandbox now sets `CLAUDE_CONFIG_DIR`
and `CODEX_HOME` the way it already set `NEOSH_STATE_DIR`, and the test writes the one transcript
line it wants to count. Which also stops a hundred and forty-seven neoshes scanning somebody's real
`~/.claude` on every run.

Not fixed here: a *key* pressed before anything has bound it still does nothing. There is no name to
hold — an unbound key produces no command — and holding raw keys would mean deciding what `^Q`
means during startup, which is a different question with a worse failure mode.
