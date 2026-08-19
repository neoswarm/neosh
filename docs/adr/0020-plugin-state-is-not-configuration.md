# 0020 — Plugin state is not configuration

**Status:** accepted

## Context

The script runtime is a bare `deno_core`: no filesystem, no subprocess, no network. That is
deliberate — it is what makes hooks and permissions mean anything. But a panel that lets you pin a
project has to still be pinned tomorrow, and a plugin has nowhere to put that.

The obvious answer was to reuse `neosh.opt`. Options are already declared, typed, persisted through
`config.toml`, and broadcast on change. Pinning a project would be `opt.set("sidebar.favorites",
[...])` and the work would be done.

## Decision

A separate, per-plugin key/value store: `neosh.state.get/set/remove`, one JSON object per plugin
under `$STATE/plugin-state/<plugin>.json`, keyed by the *calling* plugin id — which the host knows
from the request and the caller cannot forge.

Options stay what they are: settings the user writes.

## Why not options

An option is configuration. `config.toml` is a file the user hand-edits, comments, and keeps in a
dotfiles repository. Writing to it on their behalf means:

- **Their comments and formatting go.** Serialising a `BTreeMap` back to TOML loses everything that
  is not a key and a value, and the file they get back is not the file they wrote.
- **The provenance is destroyed.** `config.toml` answers "what did I ask for". Once an editor writes
  to it, it answers "what did I ask for, plus whatever I have clicked since", and you can no longer
  read it to find out why the editor behaves the way it does.
- **Layering breaks.** Project config already overrides global config. A write has no correct
  destination: writing to the project layer leaks a preference into a repository, and writing to the
  global layer silently loses to the project one on the next read.

The one-line version: an editor that rewrites the file you hand-edited because you pressed a key is
an editor you stop trusting with the file.

## Consequences

- Two mechanisms to learn, and a genuine judgement call at the boundary. The rule is: if the user
  would reasonably write it in `config.toml`, it is an option; if it is the residue of using the
  program, it is state. Sidebar *width* is an option. Which projects are *pinned* is state.
- State is not typed or declared. That is the right trade for arrangement — a schema for "which rows
  are folded" is ceremony — but it means a plugin must tolerate its own old shapes. The bundled
  sidebar filters what it reads back to strings rather than trusting it.
- It is plain JSON on disk with no encryption and no redaction. Secrets stay in the environment,
  resolved per request, exactly as before. This is documented at every entry point rather than
  enforced, because a store that tried to detect secrets would mostly produce false confidence.
- The host, not the core, answers these calls: the core has no filesystem access and should keep it
  that way. `Editor::handles` is a deny-list, so adding a capability means naming it there too —
  see `crates/neosh-core/tests/routing.rs`, which exists because that was got wrong once already.
