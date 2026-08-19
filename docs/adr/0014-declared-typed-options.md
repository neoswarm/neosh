# 0014 — Options are declared, typed, and owned

**Status:** Accepted.

## Context

A configuration system needs a settings mechanism. The default design — a `HashMap<String, Value>`
that anyone can write anything into — is what most editors ship, and it has a specific, chronic
failure: **a typo is indistinguishable from a setting that does not work.** You write
`chat.show_thinkng = true`, nothing happens, and there is no signal anywhere that would tell you
which of the two problems you have.

## Decision

An option must be **declared** before it can be set. A declaration carries a name, a type, a default,
a description, and an owner.

```rust
OptionSpec { name: "chat.show_thinking", ty: Bool, default: false, description: … }
```

- **Setting an undeclared name is an error**, and the error carries the nearest declared name by
  edit distance — the cause is almost always a typo and the fix is one character away.
- **Setting a wrong type is an error**, not a coercion. The single normalization is integer → float,
  because JSON has one number type and `1` is a perfectly good `1.0`. A config language that turns
  `"true"` into `true` teaches people to write nonsense that happens to work.
- **Names are validated** as dot-separated lowercase segments, so the namespace stays greppable and
  a future `:set` grammar has something regular to parse.
- **Options have owners.** A plugin cannot declare over a name someone else owns; unloading a plugin
  removes its options rather than leaving settable names nothing reads. Re-declaring your own option
  keeps the user's value — a plugin reload must not silently reset settings — unless the value no
  longer fits the new type.

The built-in options are declared through `ApiCall::OptDeclare` under the plugin id `neosh`, at
startup, using the same call a plugin uses. There is no built-in table. If the built-ins had needed
a private path, that would have been evidence the option API was wrong.

## Values are stored, not interpreted

The registry says what exists, what type it is, and what it holds. Acting on a change is the
declarer's job, signalled by `CoreEffect::OptionChanged` and broadcast to every plugin as
`PluginEvent::OptionChanged`.

Broadcast, not delivered to the owner: a setting is shared state, and the plugin that declares
`ui.theme` is rarely the one that changes it.

## Deferred values

`config.toml` can set an option a plugin declares, which means the value is written before the
option exists. Those are held and retried after each plugin activates; anything still unapplied once
every plugin has loaded is reported as *"is not declared by anything — is the plugin that provides
it installed?"*.

Without this, configuring a plugin declaratively would be impossible and every plugin would invent
its own JSON file, which is exactly the fragmentation this registry exists to prevent.

## `OptionValue` is untagged

A config file says `border = "rounded"`, not `border = { kind = "str", value = "rounded" }`.
Deserialization tries variants in declaration order so `true` is a bool and `3` is an integer rather
than both landing in the JSON catch-all. Type checking happens against the declaration, which is
where the type actually is.

One wire detail worth recording: the `Json` variant is typed as `{ [key: string]: unknown }` in
TypeScript rather than `unknown`. A bare `unknown` in a union swallows every other member, which
would make `opt.set(name, anythingAtAll)` type-check — the precise drift this project exists to
avoid.

## What is deliberately not here

**Scopes.** Neovim has global, buffer- and window-local options. Real, and not needed yet; adding a
scope later is a field on the declaration, not a redesign.

**Persistence.** There is no `:set` that writes back to disk, so nothing writes `config.toml`. The
`modified` flag on `OptionEntry` exists so that when something does, it knows what is worth
recording — a persister that rewrites defaults produces enormous diffs of nothing.

**A large built-in catalogue.** Four options ship: `agent.model`, `agent.system_prompt`,
`chat.show_thinking`, `chat.show_tools`. Every one of them is read by something. Declaring options
nothing acts on would be worse than having none — it is a lie in the config file, and the user has
no way to tell.
