---
layout: ../../layouts/Docs.astro
title: Models
description: Any model, through the driver that suits it. A subscription you already pay for, an API key, or a local endpoint. Adding an endpoint is a config entry, never code.
---

## Drivers

| Driver | Needs |
| --- | --- |
| `claude-cli` | Your existing `claude` login |
| `codex-cli` | Your existing `codex` login |
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai-compat` | Any endpoint speaking the OpenAI shape |
| `google` | `GEMINI_API_KEY` |
| ACP agents | `cursor-agent`, `grok` and `gemini` speak the Agent Client Protocol; a fourth ACP agent is a catalogue line, not code |

## The picker

`^P` opens two panes: your providers on the left, the models each serves on the right. It works mid turn too; the running agent is told and thinks the rest with the new model.

```
 PLANS                  │ ❯ Claude Opus 5         Frontier  Most capable
  ✳ Claude            ✓ │   Claude Sonnet 5       Balanced  Everyday tasks
  ⬢ Codex             ✓ │   Claude Haiku 4.5      Fast      Quick answers
 API KEYS               │   ▸ 5 superseded
  ✳ Anthropic         ! │
 LOCAL                  │
  ▲ Ollama              │
```

The rail is grouped by what a turn costs you: **PLANS** are subscriptions you already pay for, **API KEYS** bill per token, **LOCAL** costs nothing and needs nothing. The marker at the right edge: `✓` it will work, `!` it needs a key or its CLI is missing, `⨯` no driver provides it.

Typing filters. `⇧⇥` and `←` reach the provider rail, `⇥` and `→` come back. The picker's own actions are chords, because every bare letter a picker takes is a letter the filter can never contain:

| Key | Does |
| --- | --- |
| `^S` | Sign in to the provider the rail is on |
| `^R` | Ask that endpoint again, after adding a key or starting a local server |
| `^A` | Add a model by id, for one the catalogue has never heard of |
| `^D` | Remove one you added |

Every provider lists models whether or not you have a key, because the list is how you decide whether signing in is worth it. Once a key is present, the endpoint's own `/v1/models` decides what exists; a model the endpoint does not return is dropped even if the catalogue lists it.

## Plans

`claude-cli` and `codex-cli` drive the vendor's own CLI as a provider. Neither needs an API key, neither stores a token, and neither is offered unless the program is actually on your `$PATH`. Two things worth knowing:

- They are agent drivers, running their own loop with their own tools and sandbox, so neosh's tool registry is not in play on those turns, and neither driver overrides the CLI's own approval policy. The place to change that is the CLI's configuration.
- `codex exec --json` has no token deltas, so a Codex turn shows its tool activity live and then its answer all at once. That is the CLI's shape, not a shortcut.

A provider whose CLI is not installed still lists what it offers, greyed out, with the command that would fix it at the top.

## The ladder

Every catalogue model sits on a rung: Frontier, Balanced or Fast. Three commands move along it, none with a default key; run them from `^K` or bind your own:

```
model.upgrade      one rung up, same provider
model.downgrade    one rung down
model.line opus    the current model in a product line, wherever it is reachable
```

`upgrade` and `downgrade` hold the provider fixed on purpose: "give me something cheaper" is a question about the model, and answering it by also changing your billing would answer a question nobody asked.

## Model options

`^E` shows everything the current model can be told, on one panel: effort, thinking, fast mode, context, and whatever the driver invented. Options arrive as descriptors attached to the model, so a provider plugin that invents a knob gets a working picker for free. Each change applies as you move; switching to a model without an option drops the setting rather than sending a value the driver would reject.

## Adding an endpoint

```toml
[[providers]]
id = "local"
driver = "openai-compat"
display_name = "llama.cpp"
base_url = "http://localhost:8080/v1"
auth = { kind = "none" }
```

## Keys and secrets

A key is looked for in four places, in order: one you typed this session, the OS keychain, a helper program, then the environment. Typed wins, because you typed it after seeing what the environment gave you.

```toml
auth = { kind = "env", var = "MY_API_KEY" }
auth = { kind = "command", argv = ["pass", "show", "anthropic/api-key"] }
auth = { kind = "cli", program = "codex", login = "codex login" }
auth = { kind = "none" }
```

Anything on `$PATH` works as a helper: `pass`, `op read`, `bw get`, your own script. Only the first line of its output is used.

The key prompt itself is the host's, not a plugin's: nothing is echoed, and no plugin ever sees what you typed. There is no API call that returns a key, only ones that report where it came from, so a plugin cannot leak what it cannot read. Keys go to the OS keychain where one is available, or are held for the session where not; neosh tells you which happened and never writes a key to a file it controls.

## What the plan has left

`^L` opens live gauges per limit, with a sparkline of how each has moved, and under them where the time actually went: tokens and their API-equivalent cost by day or hour, per model, read from the agents' own transcripts, so a turn you ran outside neosh is counted too.

An allowance cannot be counted, only reported, so neosh keeps whatever the vendor last said and shows how old that is. One row per limit window, never collapsed to the worst of them, with `▸` marking the one that binds and the colour being the vendor's own grade.

```toml
[options]
"usage.sidebar" = true      # the strip at the foot of the sidebar
"usage.warn_at" = 90        # say something the first time a limit passes this
"usage.days" = 30           # the span the panel opens on
"usage.poll" = true         # ask while nothing is running
"usage.show_tokens" = true
```

`usage.poll` is the one worth a sentence: at rest is exactly when you ask "how much is left", and nothing reports it at rest, so neosh asks the vendor with the CLI's own login. The token is read at request time, never written anywhere, never logged, and structurally absent from every event a plugin can see. What crosses that boundary is percentages.
