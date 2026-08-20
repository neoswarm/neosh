# 0027 — One driver for every agent that speaks ACP

**Status:** accepted

## Context

Five terminal agents are worth reaching from neosh, and each of them is a plan somebody already
pays for: Claude, Codex, Cursor, Grok, Gemini. Written one at a time that is five drivers, five
wire formats, five sets of bugs and five things to keep up with.

Two of them have their own shape and are already written: `claude -p --output-format stream-json`
emits Anthropic SSE verbatim, and `codex exec --json` emits its own documented `ThreadEvent` stream.
The other three — `cursor-agent`, `grok`, `gemini --experimental-acp` — all speak the **Agent Client
Protocol**: JSON-RPC 2.0 over stdio, newline-delimited.

## Decision

One driver for ACP, parameterised by program and arguments. Adding a fourth ACP agent is a line in
the catalogue.

This is the same bet [ADR 0010](0010-mcp-and-acp.md) makes about MCP, arriving from the other
direction: a protocol worth adopting is one that turns "support vendor X" into configuration.

## The part that is not solved

An ACP agent asks its **client** for permission — `session/request_permission` — so neosh has to
answer, synchronously, from inside a driver that has no route to the permission layer or to a
person.

What it does today: under `full access` it takes the agent's `allow_once` option; under any other
mode it refuses and the turn reports how many times it did. Conservative, and said out loud rather
than quietly permissive.

What it should do: arrive at the same approval prompt a built-in tool call goes through, so
`tool.pre` hooks see it and the answer is a person's. That needs the request to travel out of the
driver as an event and an answer to travel back, which is a protocol addition — and, done
carelessly, a second approval path with its own rules. It is deliberately not being done in a hurry.

`allow_once` is preferred over `allow_always` where both are offered. "Always" is a decision with a
lifetime, and a wrapper taking it on somebody's behalf is taking one they cannot see.

## Sessions

One child process per turn, with the agent's session id kept between them. `session/load` is tried
first so the conversation continues; agents without the `loadSession` capability refuse it, and then
a new session is made and the whole conversation goes in the prompt. Both are correct; the first is
merely cheaper.

## Why not a long-lived process

It would be cheaper still, and it is what a mature implementation does. It also means owning a
child's lifetime across turns, session switches, config reloads and crashes — a supervisor, in other
words. Per-turn spawn is the version whose failure mode is "this turn failed" rather than "the agent
process has been wedged since Tuesday", and it can be replaced without changing anything above it.
