# 0053 — An orchestrator is a plugin

## What happened

The test for a feature here is "could a third party have written this, and can they replace it?".
Applied to the thing this program actually is — a workspace that runs several agents at once, across
several machines — the answer was no, and in a way that is easy to miss because the *remote* half
was right.

`swarm.command` names a node and a session and carries an `AgentCommand`: send, interrupt, approve,
set model, rename, archive, start. A plugin could steer, re-point and start conversations on every
other machine in the swarm, by id. On this one it had `agent.send`, which takes no session and acts
on whatever is on screen — and the same for cancelling and for the model selection.

Meanwhile everything that *observes* a conversation was already addressed: `PluginEvent::Token`,
`ToolStarted`, `ToolFinished` and `TurnEnded` all say which conversation they came from, and
`session.list`/`session.messages` read any of them. So the local API could watch every conversation
in the workspace and drive exactly one of them. An orchestrator that fans work out over several
conversations and joins the results was writable against a fleet of other people's machines and not
against the laptop it runs on. The workaround was `session.switch` before every message, which moves
the transcript out from under whoever is reading it.

Three more things were the same shape — a capability that existed on paper and not in the program.

`PluginPermission` has listed `tools`, `hooks_blocking`, `providers`, `raw_cells` and the rest since
it existed, and `vcs_write` was the only one ever checked. A plugin that declared nothing could
register a tool the model calls, take a blocking hook and veto every turn, or stand in as a model
provider. The manifest read like a boundary and was a description.

`HookName` had six hooks and every one of them was about a turn already under way: what the prompt
says, which tool may run, what it cost. None of them could answer the question an orchestrator
exists to answer — *who should do this one*. Routing policy could only be written in Rust.

And `installed_plugin_dir` had a doc comment reading "where a *future* plugin manager installs". The
instructions for installing somebody's plugin were to clone it into a directory yourself. An
ecosystem is not a plugin API; it is a plugin API plus a way for one person's plugin to end up on
another person's machine.

## The decision

**One vocabulary for doing something to a conversation, and it works locally.** `agent.command`
carries the same `AgentCommand` `swarm.command` does, at a conversation named by id, defaulting to
the one on screen. `swarm.command` addressed at this node short-circuits to the same code rather
than dialling itself — a fleet plugin iterating over `swarm.nodes()` reaches its own node like any
other, and needing the swarm to be running before a plugin can steer the conversation it is sitting
in would be a strange thing to require. One body means remote and local cannot drift, and that a
refusal is refused for the same reason on both paths.

It grants no authority a plugin did not have: switching and sending was always allowed. It stops the
screen being the cursor.

**A routing hook.** `turn.route` fires once a turn knows what it is going to say and before it knows
who to say it to — after the words are settled, so the decision is about the prompt that will
actually be sent, and before the driver is resolved, because that is the last moment the answer is
still a question. A blocking hook may re-point it, or veto with a reason. The selection is an
`Option`, so a conversation with no model chosen is a routable state rather than an error and a hook
may supply one.

Once per turn rather than once per round, because the selection is read before the tool loop and
used for every round in it — that is where the decision is actually made. A hook that wants to
change models mid-answer is asking for `set_model`, which exists. What the hook chooses is written
back to the conversation, so the footer, the model strip and a `^P` opened mid-answer agree with the
turn rather than reporting the model it was pointed at before.

**The declared permissions are enforced.** Four calls are gated on what the manifest says:
registering a tool, registering a provider driver, taking a *blocking* hook, and claiming a raw cell
surface. A non-blocking hook needs nothing, because an observer cannot refuse anything — which is
exactly the distinction `hooks_blocking` names. Everything else stays free: drawing a window,
reading a buffer and binding a key are no more privileged than what the person sitting there can
already do. A refusal names the word to put in the manifest, so the fix is one line rather than a
capability name and a guess about where it goes.

**A plugin arrives as a git clone.** `neosh plugin add <url>`, `list`, `update`, `remove`. No
registry and no index: a URL is already a globally unique name that works for a fork, a private
repository and the branch you are writing, and a registry is a thing to run forever where this is a
thing to have now. The manifest names the directory, because a plugin's id is `name` in its
`plugin.toml` and installing it anywhere else would let two directories claim one id. The manifest
is validated *before* anything is moved into place, so a plugin built against another protocol
version fails at install with a sentence rather than at the next startup as a line in a log. What is
installed is separate from what you put in your config directory by hand: `remove` refuses to touch
the latter, because `add` did not put it there.

`list` shows the bundled plugins too. What loads here is one question and should have one answer —
and a list that showed only the installed ones would make the sidebar and the model switcher look
like they were not plugins, which is the opposite of what they are.

## Consequences

An orchestrator is now an ordinary plugin: `session.create` or `agent.command({ command:
"new_session" })` to make the conversations, `agent.command({ command: "send" }, id)` to give each
one work, `onTurnEnd` — which already carried the conversation — to join. Nothing about that reaches
into the core, and nothing about it moves the screen.

`new_session` answers with the conversation it made. It used to read the active one back off the
store, so a peer that asked this machine to start something was handed the id of whatever happened
to be in front of us, and its next command steered a conversation nobody had asked it to touch. The
title it carries is kept too, rather than accepted on the wire and dropped — a caller making three
conversations at once knows what each is for at the moment it asks, and that is the only moment it
is cheap to say.

Enforcing the permissions is a breaking change for any plugin that was relying on a capability it
never declared. That is the point, and the cost is one line in a manifest. The bundled plugins
already declared what they use.

`plugin update` is `--ff-only`. A plugin directory is a checkout neosh manages, a merge commit
written by an editor into somebody's plugin is a thing nobody asked for, and a divergence is a
question for a person.
