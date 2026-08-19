# 0008 — One event shape, several transports

**Status:** Accepted. Expanded well beyond the original plan.

## Decision

`ProviderEvent` mirrors the Anthropic Messages SSE shape, and every driver adapts *into* it.

## Why that shape

It is the most expressive of the formats we target — separate thinking blocks carrying replayable
signatures, incrementally streamed tool-argument JSON, explicit per-block indices and lifecycle.
Every other format maps into it without loss; the reverse is lossy. The agent loop therefore has
exactly one stream shape to handle.

## The discovery that shaped this

`claude -p --output-format stream-json --include-partial-messages` emits Anthropic SSE payloads
**verbatim**, wrapped as `{"type":"stream_event","event":{…}}`. Verified against a live run, and the
captured output is pinned as a test fixture.

So the subprocess driver and the HTTPS driver share one parser rather than each maintaining its own
understanding of the wire format. That is why `sse::anthropic` is a free function over
`serde_json::Value` instead of a method on a client.

## Drivers

| driver | kind | credentials | covers |
|---|---|---|---|
| `anthropic` | model | `ANTHROPIC_API_KEY` | Claude, first-party |
| `openai-compat` | model | per instance | OpenAI, OpenRouter, Groq, DeepSeek, xAI, Mistral, Together, Fireworks, Cerebras, Ollama, llama.cpp, LM Studio, vLLM |
| `google` | model | `GEMINI_API_KEY` | Gemini |
| `claude-cli` | **agent** | an existing `claude` login | Claude, no API key |
| `mock` | model | none | recorded fixtures, for CI |

## Model drivers versus agent drivers

Most drivers are **model drivers**: they expose a completion API and *our* agent loop runs on top,
so our tool registry, hooks and permission layer all apply.

`claude-cli` is an **agent driver**: it runs its own loop with its own tools. It exists because it
needs no API key at all — a fresh install is useful immediately. The difference is surfaced through
`Provider::delegates_agent_loop`, and the host acts on it (it does not send our tool list to a
driver that cannot call those tools) rather than pretending the two kinds are interchangeable.
Wiring our tool registry into it through `--mcp-config` is the natural next step and needs no change
to the trait.

## The mock is not optional

Without it the definition of done is untestable in CI. With it, the entire stack — plugin
activation, tool dispatch, hook veto, streaming, coalescing — runs from a recorded fixture with no
key, no network, and no timing dependence.
