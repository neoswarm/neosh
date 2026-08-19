# 0005 — Embed deno_core directly, not rustyscript

**Status:** Accepted. Supersedes the original plan, which chose `rustyscript`.

## Context

The plan chose `rustyscript` 0.12.3 as a façade over `deno_core`: it bundles TypeScript
transpilation, a module loader and op plumbing, and its default feature set (no `deno_fs`,
`deno_net` or `deno_fetch`) is a sandbox by construction. The plan explicitly noted the risk — it
pins `deno_core ^0.355` against an upstream at 0.410 — and put the runtime behind a façade crate so
swapping it would be one crate rather than a refactor.

The risk materialised immediately, before any runtime code was written.

## What actually happened

`rustyscript` pins `deno_ast =0.49.0`, which pins `swc_common` 9.x and `swc_config` 3.x. Both import
`serde::__private`, which serde removed in 1.0.225. Working backwards:

- serde ≥ 1.0.225 — `swc_common` and `swc_config` fail to compile.
- serde 1.0.224 — `swc_config` compiles against different internals and fails differently.
- serde < 1.0.220 — other crates in the tree require ≥ 1.0.220.

The window that satisfies both is empty. Staying on `rustyscript` meant pinning the *entire
workspace* to a months-old serde indefinitely, taxing every other dependency, with no upstream fix
available (`swc_common` 9.2.0 is the latest on that line).

## Decision

Depend on `deno_core` 0.410 directly, and on `deno_ast` 0.53 for TypeScript transpilation.

`deno_core` has no swc dependency at all and requires only `serde ^1.0.149`. `deno_ast` 0.53 is on
the swc 17 line, which builds against current serde. The workspace runs on serde 1.0.229 with no
pins.

## What we gave up, and what it cost

We hand-write the module loader (`loader.rs`, ~150 lines) and the op surface (`runtime.rs`, two
ops). That is less than it sounds: the loader is mostly the `@neosh/api` virtual module and a
`MediaType` match, and both are directly tested.

We also gave up rustyscript's sandbox-by-default feature set. In practice we get the same property
more cheaply: `deno_core` on its own has no filesystem, network or process access — those live in
the `deno_*` extension crates we do not depend on. A plugin's only capability is the two ops, which
means every effect it can have passes through `ApiCall` and is therefore hookable and auditable.
That is a *stronger* guarantee than feature-flag hygiene.

## Lesson recorded

The plan's instinct to isolate the runtime behind one crate was right, and it is why this swap cost
an afternoon rather than a rewrite. The judgement that was wrong was treating a known version lag as
a future problem: a transitive pin that far behind is a present problem.
