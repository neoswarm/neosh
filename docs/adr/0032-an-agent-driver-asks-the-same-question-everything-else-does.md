# 0032 — An agent driver asks the same question everything else does

**Status:** accepted

## Context

Three complaints arrived as one sentence: *I switch to another conversation while it is working,
come back, and everything the agent did is gone — and it refuses things even on full access, and it
never asks me anything.* They turned out to be three faces of one fact about agent drivers.

An **agent driver** — `claude`, `codex`, anything speaking ACP — runs its own loop. It has its own
tools, its own sandbox and its own idea of what needs approving. neosh does not execute its calls
and neosh's tool registry never sees them. ADR 0027 said that out loud and treated it as a
property to be surfaced rather than hidden. What was not worked through is what it costs at three
other seams.

### One: a turn that is not in the conversation yet

The transcript is rebuilt from `Session::messages` on every switch, and a turn's messages land when
its stream closes. For a model driver that is the end of each round — a gap of milliseconds, and
`Round.text` covered it. For an agent driver it is **the entire turn**: one connection carries
text, tool calls and tool results for as long as the agent works, and neosh records none of it
until the connection closes.

So `enter_session` rebuilt the transcript from a conversation that contained the user's question
and nothing else. Worse, the moment a tool started, the host cleared its copy of the streamed text
— correctly, for a model driver, whose assistant message had just landed — leaving nothing anywhere.
You came back to your own sentence and a spinner, with an agent visibly working on something you
could no longer see.

### Two: a mode that reached nothing

`claude -p` and `codex exec` were spawned with **no permission arguments at all**. Both then apply
their own default, which is to refuse anything that would need asking, because in non-interactive
mode there is nobody to ask. All four of neosh's permission modes therefore produced identical
behaviour, and the footer said "full access" over a turn that could not write a file. The ACP
driver did read the mode, but the config-reload path never pushed a changed mode out to it.

For codex this was deliberate: the original comment said a wrapper that silently passed
`--dangerously-bypass-approvals-and-sandbox` would be making a security decision on somebody else's
behalf. That is true, and it was solving the wrong half. neosh *has* a setting, the user sets it on
purpose, and a wrapper that ignores it is also deciding on their behalf — it just decides the more
restrictive way, silently, while the UI says otherwise.

### Three: a question nobody could hear

An ACP agent asks its client for permission. neosh answered from the mode alone: take the allow
option under `full access`, refuse otherwise. So `ask` — the default — meant *refuse everything
without asking anybody*, which is the opposite of what the word says. Meanwhile the approvals
plugin, its picker, and the whole `permission_pre` hook existed and were reachable only by neosh's
own two built-in tools.

The first pass at this ADR concluded that `claude` simply had no channel to ask over, on the
evidence that `--help` lists no prompt flag. That was wrong, and worth recording as the specific
kind of wrong it was: **the flag is `.hideHelp()`**. Reading the CLI binary rather than its help
output shows what the Claude Agent SDK passes whenever its caller supplies a `canUseTool` callback:

```js
if (canUseTool) { push("--permission-prompt-tool", "stdio") }
...
function makeCanUseTool(name, transport) { if (name === "stdio") return transport.createCanUseTool() }
```

"No public flag" is not the same fact as "no channel", and the difference was one grep.

## Decision

**The host keeps a record of the running turn, and clears it when the conversation takes over.**
`Round.said` is an ordered list of what the turn has produced — text and tool calls with their
results — in the same three shapes `transcript()` draws from a finished conversation. A new
`AgentEvent::Committed` is emitted the moment messages land in the session, and that, rather than
"a tool started", is when the record is dropped. The two are the same instant for a model driver
and a whole turn apart for an agent driver, which is exactly the distinction that was missing.

`replay_round` draws the record through the same two helpers the live path uses. One function per
thing drawn, not two near-identical ones: near-identical is how a card acquires a blank line in one
path and not the other.

**The permission mode reaches every driver that has a policy.** A new `drivers::AgentDriver` trait
carries `set_permission_mode`, implemented by all three, and the host pushes on every change
including a config reload. Each driver translates the four modes into its own vocabulary, and each
translation is documented where it lives, because they are not the same lever:

| neosh | `claude -p` | `codex exec` |
|---|---|---|
| deny | `--permission-mode plan` | `--sandbox read-only` |
| ask | `--permission-mode manual` | `--sandbox read-only` |
| allow-listed | `--permission-mode acceptEdits` | `--sandbox workspace-write` |
| full access | `--dangerously-skip-permissions` | `--dangerously-bypass-approvals-and-sandbox` |

For `claude`, `ask` passes **no** mode flag: its own default is the one that prompts, and those
prompts now reach a person (below). For `codex exec` there is nobody to reach, so `ask` maps to the
strictest option rather than to "edits are fine" — failing closed is the direction ADR 0009 already
makes a blocking hook time out in, and quietly widening a mode the user set to "ask" would be neosh
answering a question addressed to them.

**`claude` asks over its control protocol.** `--permission-prompt-tool stdio` turns every decision
the CLI would have made alone into a `control_request` on stdout, and blocks until a
`control_response` comes back on stdin:

```text
→  {"type":"control_request","request_id":"…","request":{"subtype":"can_use_tool",
    "tool_name":"Bash","input":{"command":"touch marker.txt && chmod 700 marker.txt"},
    "tool_use_id":"toolu_…","permission_suggestions":[{"type":"addRules","rules":[…],
    "behavior":"allow","destination":"localSettings"}]}}
←  {"type":"control_response","response":{"subtype":"success","request_id":"…",
    "response":{"behavior":"allow","updatedPermissions":[…],"toolUseID":"toolu_…"}}}
```

Its precondition is `--input-format stream-json`, so the prompt stops being a positional `-p`
argument and becomes a message written to stdin. That is the entire cost.

Two details that only a real capture settles, both in [`claude_control`](../../crates/neosh-provider/src/drivers/claude_control.rs):

* **`permission_suggestions` is the "don't ask again" row.** It is the exact rule the CLI would
  write, in the CLI's own words, and it is handed straight back as `updatedPermissions`. neosh
  neither authors nor interprets it. Suggestions that *deny*, or that change the permission mode as
  a side effect of saying yes once, are not offered — the second would leave the CLI accepting edits
  under a footer still saying "ask", which is the bug this ADR opened with.
* **The prompt shows the command, not the `description`.** That field reads better and is the wrong
  thing: it is the *model's* account of what its own command does ("Create marker.txt and set
  permissions to 700"), and a permission prompt is the one place a model's account of its own
  actions must not be what you read. `decision_reason` *is* appended, because that one is the CLI's
  own voice — it is why the ask escalated — and it is sanitized first, because the CLI's schema says
  it may carry ANSI and a terminal that pasted it through would let a warning about a dangerous
  command repaint the dialog asking about it.

**An ACP agent's request goes to the same prompt a tool call does.** `PermissionAsker` is a trait
in `neosh-provider` — a request, an answer, and nothing about hooks or plugins, because a driver has
no business knowing which is on the other end. `neosh-agent::DriverAsker` implements it over the
`permission_pre` blocking hook, so the approvals plugin answers, and every hook watching that point
sees an agent driver's effects for the first time.

**Policy runs before the person, on the driver's request too.** `DriverAsker` checks the permission
layer first and only a `Prompt` reaches a prompt. That is what makes `full access` still silent,
a workspace read still unremarked — the same answer neosh's own `read_file` gets — and a path
outside the workspace still refused in every mode.

**The agent's own options travel with the request, and the chosen one travels back.**
`HookPayload::PermissionPre` gained `title`, `options` and `chosen`. An agent that offers *"Yes,
and don't ask again for `cargo`"* is offering something no client-side yes/no can reproduce, so the
prompt shows its wording and hands its `optionId` back through a `Modify` outcome. Built-in tools
offer no options and get the old three rows.

Finally, `ProviderRegisterDriver` gained `agent_loop`, so a plugin can register an agent driver
rather than only a model one. That is the thesis applied to this change — the driver kind neosh
ships three of is now a kind a plugin can ship — and it is also what makes the switching bug
testable end to end, since reproducing it needs a driver that commits nothing until it finishes.

## Consequences

- Switching away from a running agent turn and back shows everything it has done, once.
- The permission mode does what it says on all three agent drivers.
- `ask` mode works with ACP agents and with `claude`, in both cases offering the agent's own
  options. Verified against the real CLI end to end: the prompt appears, "Yes" lets the write
  through, and a refusal comes back to the model as a `deny` it explains rather than a hang.
- **`codex` still fails closed, and cannot be fixed in this transport.** Not a limitation to work
  around: `codex exec` answers every approval request with
  `"command execution approval is not supported in exec mode"` and forces
  `approval_policy: Never`. The only surface that prompts is `codex app-server` — `initialize`,
  `thread/start`, `turn/start`, `item/*` notifications, and the three server→client requests
  (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`,
  `item/tool/requestUserInput`, answered with `{"decision":"accept"|"acceptForSession"|"decline"
  |"cancel"}`). That is a replacement for the driver's transport and event mapping, not an extra
  argument, and it is the next piece of work rather than part of this one.
- An unrecognised ACP tool-call kind is gated as `Exec`. Deliberately the strictest of the four: an
  unknown effect waved through is the mistake that cannot be taken back.
- A driver with no asker installed behaves exactly as before — policy answers, and where policy
  wanted a person the answer is no. That is what tests and headless runs get.
