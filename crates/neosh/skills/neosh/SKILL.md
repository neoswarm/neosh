---
name: neosh
description: Run and manage long-lived coding agents in a neosh workspace from the command line — start conversations in any directory or git worktree, fan work out across many at once, watch and steer them while they run, read what they produced, and clean up. Use whenever the user asks to delegate work to neosh, run something "in the background", work on several branches or tasks in parallel, check on or collect results from agents already running, or mentions `neosh`, a neosh workspace, or a neosh conversation.
---

# Driving neosh

neosh is an agent workspace that runs as **one long-lived process per config directory**. Terminals
attach to it and detach from it; the conversations, and the turns running in them, belong to the
workspace and outlive every viewer. That is the property this skill is for: you can start ten
agents, walk away, come back, and they are still there.

`neosh agent …` is the workspace from a script. Every verb is one call on the same public API a
neosh plugin would use, over the same socket a terminal attaches to — so a conversation you start
here is an ordinary conversation the user can open with `^T` and read.

## Before anything else

```sh
neosh status            # is a workspace running, and what is in it
neosh agent ls          # every conversation, with its state
```

`neosh status` exiting with "no workspace running" is normal — the first `neosh agent start`
starts one. Verbs that only *ask* (`ls`, `read`, `watch`, `wait`, `models`, `commands`) will not
start a workspace; they say there is none and exit 1.

**A control connection is not a terminal.** Nothing you do here takes over the user's screen,
moves the conversation they are reading, or makes the workspace think somebody is watching.

## The verbs

| Command | Does |
|---|---|
| `neosh agent start [PROMPT…]` | Start a conversation and say something in it. Prints its id |
| `neosh agent ls` | Every conversation: id, state, project, label, directory |
| `neosh agent send <id> TEXT…` | Say something. While a turn runs this **steers** it |
| `neosh agent read <id>` | The transcript |
| `neosh agent watch [id]` | Follow, live |
| `neosh agent wait <id>` | Block until the turn ends; the exit status says how |
| `neosh agent interrupt <id>` | Ask the turn to stop. The conversation survives |
| `neosh agent rename <id> [TITLE…]` | Name it, or take the name off |
| `neosh agent archive <id>` | Put it away. `--restore` brings it back |
| `neosh agent rm <id>` | Delete it and its file. Irreversible, so it asks — `-y` to mean it |
| `neosh agent models` | Every model this workspace can reach |
| `neosh agent model <id> inst/model` | Change which model a conversation uses |
| `neosh agent commands` | Every command registered — neosh's own and every plugin's |
| `neosh agent run <name> [args…]` | Run one of them |
| `neosh agent call '<json>'` | One raw API call, for anything with no verb yet |

Ids are uuids. Every listing prints the first eight characters and every verb takes them back, so
`neosh agent read 3f2a1b0c` works. `.` means whatever the user's terminal is currently looking at.
An ambiguous prefix is refused with the candidates rather than guessed at.

## Machine-readable output

**Always pass `--json` when you are going to parse the output.** The plain output is laid out for a
person and is allowed to change; `--json` is the wire type verbatim, one object per line.

```sh
neosh agent ls --json | jq -r 'select(.active_turn != null) | .id'
neosh agent read "$id" --json | jq -r 'select(.role=="assistant") | .content[] | select(.type=="text") | .text'
```

## The one-shot: ask, wait, read the answer

```sh
neosh agent start --wait "summarise what changed on this branch and why"
```

`--wait` blocks until the turn ends and prints what the agent said. The exit status is the answer:

| Status | Means |
|---|---|
| `0` | The turn finished |
| `1` | It failed, or the model refused |
| `2` | It was interrupted |
| `124` | `--timeout` ran out. The turn is **still running** |

So `neosh agent start --wait "…" && ./deploy.sh` means what it looks like. Always set `--timeout`
in a script — an agent with a hard problem will happily think for an hour.

## Running many at once

This is what the workspace is for. Nothing below spawns a copy of neosh; they are all sockets onto
one process.

```sh
# Fan out — one conversation per crate, each in its own directory.
for c in crates/*/; do
  id=$(neosh agent start -C "$c" --title "audit $(basename "$c")" \
        "Audit this crate for unwrap() on plugin input or network responses. Report findings only.")
  echo "$id $c" >> /tmp/audit.ids
done

# Join — wait for each, note which failed, then collect.
while read -r id dir; do
  neosh agent wait "$id" --timeout 900 || echo "unfinished: $id ($dir)" >&2
done < /tmp/audit.ids

while read -r id dir; do
  printf '\n===== %s =====\n' "$dir"
  neosh agent read "$id" --text
done < /tmp/audit.ids
```

Start every conversation **before** waiting on any of them, or you have made a queue. `wait` on a
conversation with nothing running returns `0` immediately, so waiting on one that already finished
is safe.

### Worktrees: parallel work on one repository

Several agents editing one checkout will fight. Give each its own git worktree and each has its own
files, its own branch and its own index:

```sh
git worktree add ../wt-login -b fix/login
neosh agent start -C ../wt-login "fix the login redirect loop"
```

neosh's sidebar nests a worktree under the repository it belongs to, so the user sees these grouped
rather than as unrelated projects.

## Watching and steering

```sh
neosh agent watch                 # everything happening in the workspace
neosh agent watch "$id" --json    # one conversation, as events
```

Events are the same ones a plugin sees. The ones worth acting on: `turn_started`, `token`,
`tool_started`, `tool_finished`, `turn_ended`. Each names the conversation it came from.

Steering does not need the turn to stop:

```sh
neosh agent send "$id" "actually, leave the tests alone — just the parser"
```

The message is held and taken in at the next gap between tool calls. Several sent into one gap
arrive as one message.

## When a conversation stops making progress

Check in this order:

1. `neosh agent ls --json | jq 'select(.id|startswith("'"$id"'"))'` — is `active_turn` set?
2. If it is set and nothing is arriving on `watch`, it is **probably waiting on a person.** An
   agent can ask the user a question, and a question is answered at a terminal — there is no
   headless answer for it. Tell the user, and name the conversation.
3. `neosh agent interrupt <id>` asks the turn to stop. The conversation survives it, so the
   transcript and everything the agent did are still there. Follow with `send` to redirect.

## Reporting back to the user

They can see all of this. Give them the id and the directory and let them go and look:

> Started three audits — `3f2a1b0c` (neosh-core), `7d4e9a21` (neosh-tui), `bb10f3e8` (neosh-agent).
> `^T` in neosh to read any of them; the first has already finished.

## Rules

- **`--json` for anything you parse.** The pretty output is for people.
- **`--timeout` on every `wait` in a script.** A turn has no natural end.
- **Never `rm` without asking the user.** It deletes the conversation and its file, and there is no
  undo. `archive` is the reversible one, and is almost always what is wanted.
- **`-C` on every `start`.** The conversation's directory is where its tools read and write. The
  default is your own current directory, which is rarely what you meant.
- **Don't run `neosh` (bare) or `neosh --serve` to "check" something.** Bare `neosh` takes over
  the terminal; `neosh status` and `neosh agent ls` are the questions.
- **`neosh stop` kills every running turn** in the workspace, including the user's own. Ask first.
- A conversation you started is the user's too. Give it a `--title` so their sidebar is readable.

## Reference

- `neosh agent <verb> --help` for the flags on any verb.
- `neosh agent commands` lists every command the workspace has, plugins included, and
  `neosh agent run <name>` runs one — that is how a capability with no verb here is reached.
- `neosh agent call '{"call":"session_list","include_archived":true}'` makes any API call at all.
  The call names are the `ApiCall` variants in `@neosh/api`.
- `neosh paths` says which config, state and socket are in play.
