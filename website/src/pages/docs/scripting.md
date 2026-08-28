---
layout: ../../layouts/Docs.astro
title: Scripting
description: neosh agent is the workspace without a screen. Start conversations, watch them, steer them, read what they produced — from a shell loop, from CI, or from another coding agent.
---

`neosh "why is the composer eating my paste"` starts a conversation in this directory, sends that, and draws the answer arriving. It is the terminal, with the first question already asked. Quote it when the first word is also a subcommand name: `neosh status of the build` runs `neosh status`.

`neosh agent` is the headless half. It sends the **same `ApiCall` a plugin sends**, over the **same socket a terminal attaches to** — so what a script can do is exactly what a plugin can do, and a conversation started this way is an ordinary one that `^T` lists and you can take over.

A control connection is not a terminal. It does not attach, it does not take an attached one over, and `neosh status` does not count it as somebody watching.

## The verbs

| Command | Does |
| --- | --- |
| `neosh agent start [PROMPT…]` | Start a conversation and say something in it. Prints its id |
| `neosh agent ls` | Every conversation: id, state, project, label, directory |
| `neosh agent send <id> TEXT…` | Say something. While a turn runs this steers it |
| `neosh agent read <id>` | The transcript |
| `neosh agent watch [id]` | Follow, live |
| `neosh agent wait <id>` | Block until the turn ends |
| `neosh agent interrupt <id>` | Ask the turn to stop. The conversation survives it |
| `neosh agent rename <id> [TITLE…]` | Name it, or take the name off |
| `neosh agent archive <id>` | Put it away. `--restore` brings it back |
| `neosh agent rm <id>` | Delete it and its file. Irreversible, so it asks |
| `neosh agent models` | Every model this workspace can reach |
| `neosh agent model <id> inst/model` | Change which model a conversation uses |
| `neosh agent commands` | Every command registered — neosh's own and every plugin's |
| `neosh agent run <name> [args…]` | Run one of them |
| `neosh agent call '<json>'` | One raw `ApiCall`, for anything with no verb yet |

Ids are uuids; every listing prints the first eight characters and every verb takes them back. `.` means whatever your terminal is looking at. An ambiguous prefix is refused with the candidates rather than guessed at.

Verbs that only *ask* — `ls`, `read`, `watch`, `wait`, `models`, `commands` — will not start a workspace. They say there is none and exit 1.

## Asking one question

```bash
neosh agent start --wait --timeout 600 "summarise what changed on this branch and why"
```

`--wait` blocks until the turn ends and prints what the agent said. The exit status is the answer: `0` finished, `1` failed or refused, `2` interrupted, `124` the timeout ran out and the turn is **still running**. So `neosh agent start --wait "…" && ./deploy.sh` means what it looks like.

## Running many at once

Nothing here starts a second copy of neosh — they are all sockets onto one workspace, which is what makes twenty of them cheap.

```bash
: > /tmp/ids
for c in crates/*/; do
  neosh agent start -C "$c" --title "audit $(basename "$c")" \
    "Audit this crate for unwrap() on plugin input. Report findings only." >> /tmp/ids
done

while read -r id; do neosh agent wait "$id" --timeout 900; done < /tmp/ids
while read -r id; do neosh agent read "$id" --text; done < /tmp/ids
```

Start everything before waiting on anything, or you have written a queue. `wait` on a conversation with nothing running returns `0` immediately.

Several agents editing one checkout will fight. A git worktree each gives every one its own files, branch and index — and the sidebar nests them under the repository they belong to:

```bash
git worktree add ../wt-login -b fix/login
neosh agent start -C ../wt-login "fix the login redirect loop"
```

## Parsing the output

Pass `--json` for anything you parse. The plain output is laid out for a person and is allowed to change; `--json` is the wire type verbatim, one object per line — `SessionInfo` for `ls`, `Message` for `read`, `PluginEvent` for `watch`.

```bash
neosh agent ls --json | jq -r 'select(.active_turn != null) | .id'
neosh agent read "$id" --json | jq -r 'select(.role=="assistant") | .content[] | select(.type=="text") | .text'
```

## When nothing is happening

Check whether `active_turn` is set. Set, with nothing arriving on `watch`, usually means the agent is **waiting on a person**: an agent can ask you a question, and a question is answered at a terminal. `neosh agent interrupt <id>` asks the turn to stop; the transcript survives, so `send` can redirect it.

## Anything with no verb

```bash
neosh agent commands                                              # every command, plugins included
neosh agent run git.pull                                          # run one
neosh agent call '{"call":"session_list","include_archived":true}'
```

`call` takes a JSON-encoded `ApiCall` — the variants are the ones in `@neosh/api` — and prints the `ApiOk` it got back. `-` reads it from stdin. This is the whole plugin API with no verb in front of it.

## The skill, for coding agents

If you drive neosh from Claude Code, Codex, Cursor or Gemini, install the skill rather than pasting this page:

```bash
neosh skill install                  # for you, every agent it knows about
neosh skill install --project        # checked into this repository instead
neosh skill install --agent claude   # just one
neosh skill where                    # say where it would go
neosh skill show                     # print it, for somewhere this does not know about
```

It writes `SKILL.md` into `.claude/skills/neosh/`, `.agents/skills/neosh/` (Codex and anything else reading the shared Agent Skills location), `.cursor/skills/neosh/` and `.gemini/skills/neosh/`. Skills are read when a session starts, so restart your agent afterwards.

Claude Code can also take it as a plugin:

```
/plugin marketplace add neoswarm/neosh
/plugin install neosh@neosh
```

## Security

The socket lives in your own runtime directory at `0600`, and a Unix socket does not leave the machine — so reaching it means already being the person at the keyboard. Control calls are therefore not gated: a permission layer between somebody and their own workspace would be ceremony rather than security.

The permissions that matter are unchanged. A tool call made by a conversation started from a script is asked about exactly as one started by typing, under that conversation's permission mode. Steering agents on *other* machines is a different question with a different answer — see [machines](/docs/machines).
