# 0010 — MCP and ACP as the RPC door

**Status:** Accepted as direction. Not built in Phase 0.

## Context

The brief: treat MCP as "a well-known case of the JSON-RPC door, do not invent a competing tool
protocol". While surveying prior art we found `packages/effect-acp` in t3code — Zed's **Agent Client
Protocol**, a JSON-RPC-over-stdio standard for the editor↔agent boundary — alongside a Codex
app-server integration. The same "don't invent a competitor" logic plausibly extends to it.

## Decision

Three boundaries, deliberately distinct, and only one of them is ours to invent.

1. **Tools — MCP.** An MCP server is a `PluginRequest::RunTool` over stdio with different framing.
   `ToolSource::Mcp { server }` already exists in the protocol and `ToolRegistry` needs no change to
   accept it: MCP tools land in the same namespace with the same shape, so a policy hook intercepts
   them without knowing what they are.

2. **Agent sessions — ACP, probably.** This is the boundary `claude-cli` and a future Codex driver
   sit on. Adopting ACP rather than growing our own would make neosh usable as an ACP *client* for
   any conforming agent, and eventually as an ACP *server*. Deferred because it affects the agent
   boundary, not the plugin boundary the Phase 0 definition of done is about, and because committing
   to someone else's protocol shape is a decision worth making with the agent layer more built out
   than it is.

3. **UI — ours.** ACP does not model retained UI state (buffers, extmarks, floats, focus), and MCP
   does not either. `UiEvent`/`InputEvent` is the one protocol here that had to be invented, which
   is why ADR 0006 spends effort on getting its shape right.

## What Phase 0 did to keep this cheap

`PluginInbound`/`PluginOutbound` is a full-duplex JSON-RPC-shaped envelope, and the in-process
TypeScript runtime is already just one transport for it. Adding a subprocess transport is a new
implementation of the same messages, not a new protocol.
