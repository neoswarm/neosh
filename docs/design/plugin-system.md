# Plugins on top of plugins

**Status:** implemented, 2026-08-23 — see `docs/plugins.md` for how to use it. This document is the
audit that preceded the decision, kept because the gap analysis is the reasoning. Of the proposal, A–I landed
except: per-buffer/window *options* (vars have the scopes), the transcript card / footer / composer
as contribution *points* (they have kinds now, not points), and permission enforcement beyond
`vcs_write` and `notify`.

The question it answers: *if the sidebar is a plugin, how does a second plugin put git status on its
rows, and how does a third recolour its window — without either of them forking it?* That is the
thing Neovim does that nothing else does, and it is the whole reason "extensibility is the product"
is a thesis rather than a feature list.

## What Neovim actually has

"Everything is customisable" is not one mechanism. It is about nine, and a plugin that builds on
another plugin usually uses three or four of them at once. Mapped onto what neosh has:

| Neovim | What it is for | neosh today |
|---|---|---|
| `require("nvim-tree.api")` | Call another plugin, get an answer back | **Missing.** `cmd.exec` returns `void`; the host fires `CommandInvoked` and never awaits the handler (`plugins/api/src/index.ts:2460`). No `plugin:` import. |
| `filetype` + `FileType` autocmd | Name a kind of buffer so others can bind to it and decorate it | **Have:** `buf.create({ kind })`, `KeymapScope::BufKind`, `win.ofKind`. But the transcript, composer and status buffers have **no kind** (`host.rs:828/844/847`), and neither do the four shared widgets (`ui.ts:494, 982, 1345, 2164`). |
| Autocmds — `BufEnter`, `WinEnter`, `CursorMoved`, `ModeChanged`, `FocusGained`, `VimLeave`, `ColorScheme` | Hear that something happened in a plugin you do not import | **Mostly missing.** Eight fixed hooks, none about UI. `focus_changed`, `view_attached` and `shutdown` arrive on the wire and are **dropped** by `__dispatch` (`index.ts:2560-2562`). |
| `User` autocmds, `nvim_exec_autocmds` | Plugin-defined events | **Have**, and better: `event.emit` with host-stamped `from`. |
| `b:` / `w:` / `g:` / `t:` vars | Shared state about a *thing* | **Half.** `vars` is scoped to workspace, conversation, project — persisted, with change events. No buffer or window scope, and every write is a file write, so nothing per-cursor can live there. |
| `setlocal` | Per-buffer / per-window options | **Missing.** Flat registry (`options.rs:31`); the recorded plan was "a field on the declaration later". |
| Extmarks + `nvim_set_decoration_provider` | Draw on someone else's buffer at paint time | **Half.** Extmarks exist; no sign column, no per-window decoration, no range query, and a decorator has no stable row identity to key on. |
| `:hi default link`, `nvim_win_set_hl_ns`, `winhighlight`, colorschemes as plugins | Theme a window without owning it; ship a theme | **Weak.** `hl.define` is last-write-wins with no owner, never cleaned on unload (`editor.rs:650-679`), no `default`, no read-back, no per-window remap, and `ui.theme` is a two-member enum (`host.rs:9255`). |
| `vim.ui.select`, `vim.ui.input`, `vim.notify` as assignable functions | Replace *every* picker in one place (dressing.nvim, noice, nvim-notify) | **Missing.** `@neosh/api/ui` is a library each plugin calls; there is no slot to swap the implementation. |
| lazy.nvim spec: `dependencies`, `priority`, `event`/`cmd`/`ft`/`keys` | Ordering, and not paying for what you do not use | **Missing.** Load order is a filesystem sort (`host.rs:9488`), eager, no `requires`, and `plugins.disabled` only works on bundled plugins (`host.rs:9527`). |
| `:checkhealth`, `:Lazy` | See what is loaded and what each thing registered | **Missing.** Contribution points cannot be listed; a typo in a point name succeeds silently forever (`editor.rs:1170`). |

And the shape of it: the mechanisms neosh has are *good* — kinds, named-command keymaps, data
contributions, scoped vars, events with provenance are each at least as principled as the Neovim
equivalent. What is missing is (a) any way to *ask* another plugin something, (b) any way to
*decorate* or *restyle* what another plugin drew, and (c) the bundled surfaces being on the surface
at all. The sidebar is the one panel that was rewritten onto the primitives, and even there the
two points it reads are **additive only**: you can add a section above or below and a verb on a
row, and you cannot touch a built-in row's text, glyph, colour, order or placement
(`sidebar/main.ts:2631-2988`).

## The worked example, before and after

Three plugins a user might plausibly write, and what each costs today.

### 1. Git status on the sidebar's project rows

**Today:** fork the sidebar. `projectRow` is a private function; the only slot a third party has is
a whole *section* (`sidebar.section`), so the best you can do is a second list of the same projects
with `+3 ~2` beside each, under the real one.

**Proposed:**

```ts
// plugins/git-badges/main.ts
export async function activate({ neosh, subscriptions }: PluginContext) {
  const refresh = async () => {
    for (const p of await neosh.vars.get<string[]>({ scope: "global" }, "sidebar.projects") ?? []) {
      const s = await neosh.git.status(p);
      subscriptions.push(await neosh.ext.contribute("sidebar.decoration", `dirty:${p}`, {
        target: { project: p },                      // stable identity, not a row index
        right: { text: `+${s.staged} ~${s.modified}`, hl: "Git.Dirty" },
      }));
    }
  };
  neosh.event.on("neosh.ready", refresh);
  subscriptions.push(neosh.timer.every(4000, refresh));
}
```

Pure data, listable, disabled with the plugin, and the sidebar merges it at draw time with zero
round trips. This is nvim-tree's decorator API and neo-tree's `components`, as a contribution.

### 2. Recolour the sidebar's window

**Today:** redefine `Sidebar.Selected` globally. That changes every sidebar-styled thing, the
override is permanent for the session (`highlight.rs:120`), and survives disabling your plugin.

**Proposed:**

```ts
neosh.hl.define("Acme.Panel", { bg: { rgb: [0x1a, 0x1b, 0x26] } });
neosh.hl.define("Acme.Sel",   { link: "Visual" }, { default: true });   // only if nobody has
neosh.win.setHighlights({ kind: "neosh.sidebar" }, {
  Normal: "Acme.Panel",
  "Sidebar.Selected": "Acme.Sel",
});
```

`winhighlight` by kind: a remap the frontend consults before the global registry, owned by the
caller, gone when the plugin is.

### 3. A colorscheme

**Today:** 150 `hl.define` calls in `init.ts`, no name, not switchable, not listable.

**Proposed:**

```ts
neosh.ext.contribute("ui.theme", "gruvbox", {
  base: "dark",                       // which variant to fall back to for groups not named
  groups: { Normal: { fg: …, bg: … }, Comment: { link: "NonText" }, /* … */ },
});
```

`ui.theme` becomes an enum whose values are `dark`, `light` and every contribution on the point —
so `^E` and the model-picker-style sheet list it, and unloading the plugin removes it.

## Proposal

Nine pieces. The first three are what "plugins on plugins" actually needs; the rest are what makes
it honest. Each is framed the way the repository frames things — what it is, why the obvious
alternative is worse, what it costs.

### A. A command can answer: `cmd.call`, and `plugin:` imports

`cmd.call(name, args): Promise<unknown>` — the host awaits the handler and returns its JSON value.
`CommandInvoked` grows a `reply` id; the plugin side answers with `ApiCall::CmdReply`. `cmd.exec`
stays as the fire-and-forget spelling.

And, because every plugin lives in one isolate (`runtime.rs:438`), a direct import is cheap:
`import { api } from "plugin:sidebar"` resolves in the loader to the named plugin's entry URL
(generation query included, so reload keeps working), and a plugin exports whatever it likes under
`api`. `neosh init` writes `api.d.ts` for every installed plugin alongside `@neosh/api`, so the
import is typed against the plugin you actually have.

**Why both.** The panel-is-a-surface rule chose data over callbacks so that a contribution can be
*listed, described and disabled*, and that reasoning is right for registrations — rows, verbs, keys. It was never
about queries. "What row is the sidebar on?" has no representation as data that is not also a
cursor-move-per-keystroke file write; it is a function call, and faking it with `event.emit` plus a
correlation id plus a reply command is how people end up writing a worse RPC in every plugin.
`cmd.call` is the host-routed, listable form (it works for `agent.command` and over ASCP);
`plugin:` is the ergonomic one. A plugin that wants its API discoverable registers commands; one
that wants it typed exports a module; the sidebar should do both.

**Requires** dependency declaration (G) so that `plugin:sidebar` in a plugin that loads before the
sidebar is an error with a name in it, not `undefined`.

### B. Decorations and anchors: what a panel owes a plugin it has never heard of

Every bundled list panel — and `@neosh/api/ui` on behalf of every third-party one — reads three
points, not two:

- `<kind>.section` — as today, but placed by **name** rather than `above`/`below`:
  `{ before: "projects" }`, `{ after: "archived" }`. The panel publishes its own slot names
  (`sidebar.slots = ["above", "projects", "archived", "below"]`), and a contributor can sit between
  two of *another* contributor's sections the same way.
- `<kind>.action` — as today.
- `<kind>.decoration` — **new**: `{ target, hl?, spans?, right?, badge?, prefix? }` keyed on the
  row's stable identity (a project's cwd, a conversation's id), merged onto the built-in row at draw
  time. Upsert semantics already exist on `(plugin, id)` (`editor.rs:1173`), so a decorator that
  re-contributes is an update rather than a pile.

Which means a row needs an identity a plugin can name. The sidebar has one already (`Target`,
`sidebar/main.ts:75-87`); it is private. Publish it: `sidebar.cursor` as a **buffer var** (E), and
`rows()` on the `plugin:sidebar` API (A).

**Why data rather than a row-renderer callback.** neo-tree's `renderers` let a plugin replace the
function that draws a row. It is the most powerful option and the one that puts every contributor
on the panel's paint path — one slow decorator is a panel that lags on `j`. A decoration is
something the panel already knows how to draw, supplied by somebody else. The *shape* of a row
stays the panel's; what is *on* it is anyone's. Where that is not enough — someone wants
two-line rows — the answer is a panel of their own on the same widget, and the widget is the next
piece.

### C. `ListPanel`: the three mechanisms as a widget, not a convention

The rule says a panel that lacks any of kind, points and vars "is not finished", and then the audit
finds `model`, `palette`, `approvals`, `slash` and all four shared widgets missing most of them.
That is not five authors forgetting; it is the rule costing too much to follow by hand.

`@neosh/api/ui` grows `ListPanel` over `CursoredList`: give it a `kind` and it creates the buffer
with that kind, binds every verb as a named command at `buf_kind` scope, reads
`<kind>.section/.action/.decoration`, publishes `<kind>.cursor`, and subscribes to `ext.onChange`
for its own points. The sidebar becomes the first consumer and loses a thousand lines; the pickers
get a kind (`neosh.picker`) and named verbs (`neosh.ui.picker.next`, …) for free, so `^Z` lists
them and one picker can be rebound differently from another.

### D. Highlights become a registry like the others

Commands, tools and options have owners and are refused on conflict; keymaps have a tier rule;
highlights have nothing (`highlight.rs:118`). Give them:

- an **owner**, cleaned on unload — today a disabled plugin's colours outlive it;
- `hl.define(name, spec, { default: true })` — Neovim's `:hi default`, define-unless-defined, which
  is what a plugin providing a *look* uses so a user's `init.ts` wins without a load-order fight;
- `hl.get(name)`, `hl.list()`, `hl.onChange` — a plugin that wants "a little darker than `Normal`"
  can read `Normal`; a panel caching a colour hears the theme change (`ColorScheme`);
- `hl.reset(name)` — un-override, so `overridden` stops being monotonic;
- **per-window remap**, by window or by kind (`win.setHighlights`) — resolved in the frontend before
  the global registry, which is where colour is decided anyway (`neosh-tui/src/theme.rs`);
- **themes as contributions** on `ui.theme` (example 3 above). The palette contract
  (`palette.rs:270`) is what makes this safe: a theme that omits a group inherits its `base`
  variant's, so every name still resolves.

### E. Buffer and window scope, for vars and options

`VarScope` gains `{ scope: "buf", buf }` and `{ scope: "win", win }`, **in memory**, dropped with
the buffer or window, never written to `vars.json`. That is the answer to "a var write is a file
write": the things that change per keystroke are about a buffer or a window, and those scopes are
not persisted. `sidebar.cursor` lives there. `OptionSpec` gains `scope: "global" | "buffer" |
"window"` — the long-planned field on the declaration — which is what lets a plugin say `wrap` for
its panel and not for yours.

### F. UI events on the bus

Stop dropping what already arrives — `focus_changed`, `view_attached`, `shutdown` — and emit the
rest on the existing event bus with `from: "neosh"`: `neosh.win.enter`, `neosh.win.leave`,
`neosh.buf.kind`, `neosh.cursor.moved`, `neosh.mode.changed`, `neosh.viewport`, `neosh.theme`.
`event.on(name, cb, { kind })` filters on the buffer kind in the payload so a plugin interested in
the sidebar does not parse every cursor move in the transcript. Not new wire types; `neosh.ready`
set the precedent.

### G. The manifest says what a plugin needs, provides and costs

```toml
[plugin]
name = "git-badges"
requires = ["sidebar"]            # topological load order; `plugin:sidebar` is a hard error without it
after = ["usage"]                 # soft ordering, for "I want my section under theirs"

[activation]                      # lazy.nvim's event/cmd/ft/keys — absent means eager, as today
on_kind = ["neosh.sidebar"]
on_command = ["git.badges.refresh"]

[provides]
points = ["git.badge.source"]     # declared, so ext.list can enumerate and a typo can be reported
kinds = ["acme.tasks"]
vars = ["git.branch.scratch"]
```

`plugins.disabled` applies to every plugin, not only bundled ones. `deactivate` is called, as the
docs already claim. And a **tier** replaces the bundled/non-bundled special case: `builtin <
plugin < user`, applied uniformly to commands, keymaps, highlights, tools and options — a higher
tier overrides, a lower one is refused, and within a tier you say `override: true` or lose. Today
there are seven different conflict rules across those registries, and `init.ts` (loaded first, never
marked bundled) loses a key to any third-party plugin that binds it later — the exact failure the
bundled rule was written to prevent, one level out.

### H. The binary's own surfaces get kinds and points

`[chat]` is `neosh.transcript`, `[composer]` is `neosh.composer`, `[status]` is `neosh.status`.
At one stroke the reader's and composer's keys can be *bindings* rather than two `switch`
statements (`host.rs:3966`, `4361`), which retires the hand-maintained copies in the palette
(`palette/main.ts:255, 271`). Then, in order of how often someone will ask:

- `status.mode` is a segment like any other, so it can be relabelled or removed;
- `transcript.card` is a point: a plugin contributes a *classifier* (`{ tool: "acme_query", kind:
  "read", name: (args) => … }`) so its tool folds like a read and is named like one, and — the
  expensive version, behind `cmd.call` — a renderer by tool name that returns rows;
- `composer.footer` and `composer.chip` are points, so the plan rows and attachment chips are
  contributions the host happens to make;
- extmarks gain `sign` (a gutter column) so gitsigns-style marks are possible on the transcript.

### I. Introspection

`ext.points()` lists declared points with their contributors and consumers; a `plugins` panel (a
plugin, on `ListPanel`) shows per plugin what it registered — commands, keys, points, vars,
highlights, options — and `neosh plugin health` prints the same, plus "contributes to a point
nothing reads" and "links to a group nothing defines". `:checkhealth` is the thing that made
Neovim's ecosystem debuggable by people who did not write the plugins.

## What this does not propose

**A split tree.** `Dock` says it is not one on purpose; nothing above needs it.

**Per-plugin isolates.** One isolate is why `plugin:` imports are cheap and why a decorator costs
nothing; Neovim has one Lua state too. The cost is that a spinning plugin wedges them all, which is
the price Neovim pays as well. The one cheap fix worth taking: `__createContext`, `__dispatch` and
`__teardown` are public exports of the shared `@neosh/api` module (`index.ts:1603, 2401, 2568`), so
any plugin can tear down another or forge a context. Move them to a `neosh:internal` module only
`bootstrap.js` may import.

**Replacing contribution data with callbacks.** The opposite: everything that is a registration
stays data; `cmd.call` and `plugin:` are for *asking*.

## Order

What unlocks the most, soonest:

1. **A + G (`requires` only)** — `cmd.call`, `plugin:` imports, dependency ordering. Two days of
   work and every "how do I ask the sidebar…" question has an answer.
2. **B on the sidebar** — decorations, named anchors, published `Target`, `sidebar.cursor`. The
   git-status example works end to end. Fix the `RESERVED` list while there (it omits `gg`, `G`,
   `^D`, `^U`, `⇥`, digits — a contributed action on `G` silently replaces `neosh.sidebar.bottom`).
3. **D** — highlight ownership, `default`, read-back, per-kind remap, theme contributions.
4. **E + F** — buffer/window vars and options; UI events; stop dropping the three that exist.
5. **C** — `ListPanel`; rewrite the sidebar onto it, give pickers kinds and named verbs.
6. **H** — kinds on the host's buffers, then `transcript.card` and the footer as points.
7. **G (rest) + I** — lazy activation, tiers, `deactivate`, `plugins.disabled` for everyone,
   health.

## Defects found on the way

Worth fixing regardless of the above:

- `deactivate` is documented (`docs/plugins.md:42`) and never called (`bootstrap.js:93-94`).
- `buf.onChange`'s disposable never sends `buf_detach`; the host streams forever.
- Highlight definitions are not removed on unload (`editor.rs:650-679`).
- `plugins.disabled` does not apply to disk plugins (`host.rs:9527`).
- `keymap.set` can be refused and report success (`keymap.rs:244-252`); `keymap.del` has no
  ownership check.
- `permission.asking` is written by approvals (`approvals/main.ts:44`) and read by nobody — a
  conversation blocked on a permission prompt draws as an ordinary spinner.
- Seven of nine `plugin.toml` permissions are never checked; `fs_*`/`network` gate APIs that do not
  exist.
- Two plugins with one manifest `name` merge into one registry bucket (`bridge.rs:50-55`).
- Docs drift: `docs/config.md:1296` says there is no plugin manager.
