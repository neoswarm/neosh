# 0011 — Provider drivers are open, and models are discovered

**Status:** Accepted.

## Context

The requirement was "support every model", explicitly modelled on t3code. Reading t3code's contracts
showed the load-bearing idea: `ProviderDriverKind` is an **open branded slug**, not a closed enum,
and drivers advertise per-model option descriptors that the UI renders generically.

## Decision

Three separate ideas, none of which is a hardcoded model list.

**1. Drivers are registered, not enumerated.** `DriverKind` is a free-form slug. The host registers
the built-ins at startup; a plugin registers its own through `ApiCall::ProviderRegisterDriver`.
Nothing in the codebase knows the set of vendors that exist. Supporting a new one is a plugin.

**2. Instances are configured drivers.** A `DriverKind` is an implementation; an `InstanceId` is a
configured one — base URL, credentials, model list. "OpenAI and OpenRouter and Groq and a local
llama.cpp, simultaneously" is four instances of `openai-compat`, which is why that single driver
covers thirteen of the built-in presets and every future endpoint that speaks the same shape.

**3. Models are discovered at runtime.** Every OpenAI-compatible endpoint answers `GET /v1/models`;
Anthropic and Google have equivalents. A baked-in model list is wrong within weeks. Verified: with
zero API keys configured, `neosh --list-models` returns 423 models, because OpenRouter's catalogue
endpoint needs no auth.

## The exception, and why

The Anthropic catalogue *is* written out — not for the model ids, which are discovered, but for the
**option descriptors**: which effort levels a generation accepts, whether fast mode exists, whether
thinking can be disabled. None of that is discoverable from any endpoint, and getting it wrong
produces a 400 rather than graceful degradation. `budget_tokens` on a current model is rejected
outright; `xhigh` effort does not exist before the 4.7 generation.

## Per-model options are declared, not hardcoded

Every vendor spells reasoning depth differently and the set changes per model. Rather than a union
of every vendor's knobs, a driver advertises `ProviderOptionDescriptor`s per model — select or
boolean, with labels — and the UI renders them without knowing what any of them mean. A new vendor
knob appears in the picker with no UI change. `prompt_injected_values` is carried through because
some vendors gate depth on magic words in the prompt rather than a request parameter.

## Consequence for the agent loop

`ProviderRegistry::default_selection` takes the first *usable* instance, and the catalogue orders
`claude-cli` first because it needs no API key. A fresh install with an existing `claude` login is
useful with no configuration at all.
