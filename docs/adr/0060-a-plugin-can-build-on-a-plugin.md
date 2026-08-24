# 0060 — A plugin can build on a plugin

**Status:** accepted

Extends [0040](0040-a-panel-is-a-surface-not-a-program.md), which said what a plugin may do to
another plugin's *panel*, to the rest of it: ask it something, decorate what it drew, recolour its
window, load after it, and find out it exists.

## Context

The thesis is that a third party could have written any of our panels and can replace any of them.
0040 made the sidebar a surface — a kind, two contribution points, shared vars — and that was true
as far as it went. An audit against the thing people actually mean by "extensible like Neovim"
found three gaps, each of which was a fork in practice:

**Nothing could ask another plugin anything.** `cmd.exec` returned `void`: the host fired
`CommandInvoked` and never awaited the handler. There was no import of another plugin's module.
"What row is the sidebar on" had no answer that was not an event, a correlation id and a reply
command — a worse RPC, written once per plugin.

**Nothing could decorate or restyle what another plugin drew.** The sidebar's points were additive:
a section above or below, a verb on a row. A built-in row's text, glyph, colour and placement were
private. Highlights were last-write-wins with no owner — a disabled plugin's colours outlived it,
an override deleted from `init.ts` survived `^R` — and there was no way to recolour one window
without redefining the groups every window draws with. A theme was a two-member enum.

**The bundled surfaces were not on the surface.** The transcript, the composer and the status line
had no kind. Neither did the shared pickers. `focus_changed`, `view_attached` and `shutdown` arrived
at the plugin runtime and were dropped. `deactivate` was documented and never called.
`plugins.disabled` only worked for bundled plugins. Seven registries had five different conflict
rules, and `init.ts` — loaded first, never marked bundled — lost a key to any plugin that bound it
later, which is the exact failure the bundled rule was written to prevent, one level out.

## Decision

### A command can answer, and a plugin can be imported

`cmd.call(name, args)` returns what the handler returned, through the host — so it works for the
host's own commands and does not care who owns the name. `cmd.exec` stays the key press. And
`import { api } from "plugin:sidebar"` resolves to the sidebar's *entry module*, generation query
included, so it is the very instance the host activated: one isolate for every plugin is what makes
this free, and it is Neovim's `require`.

0040 chose data over callbacks so a contribution could be *listed, described and disabled*. That
reasoning was about registrations and still holds for them. A query is not a registration; it gets
an answer.

`requires` in the manifest is what makes the import honest: the named plugin loads and activates
first, and a name nothing provides is refused at startup with the name in the message, never an
`undefined` three calls later. `after` is the soft form. Load order used to be a filesystem sort the
code leaned on; it still is, bent as little as possible to honour both.

### A decoration is a contribution

`<kind>.decoration`: `{ target, badge, hl, right }`, keyed by what the row is *about* — a project's
directory, a conversation's id — and merged onto the panel's own row at draw time. The git plugin's
dirty count is one. Data, not a row-renderer callback, because a renderer puts every contributor on
the panel's paint path and one slow decorator is a panel that lags on `j`; a decoration is a thing
the panel already knows how to draw, supplied by somebody else. The panel's own states — asking,
working, failed, unread — outrank it, because those are the rows the column exists to find.

Sections sit `before` or `after` a named slot or another section's id, because `above`/`below` gave
a contributor two places and no way to land between two of anybody else's.

### Highlights are a registry like the others

Owned, and gone when the owner is: back to the theme's definition or away. `default: true` is
Neovim's `:hi default` — define only if nobody has — which is what a plugin shipping a *look* for its
own groups uses so a user's `init.ts` wins whichever loaded first. Readable (`hl.get`, `hl.list`),
resettable, and announced (`hl.onChange`, every name on a theme switch).

A window, or every window of a kind, reads group names through a map — `winhighlight` — resolved in
the frontend before the global registry, one level deep so it cannot loop. That is how a panel is
recoloured without the rest of the workspace knowing.

A theme is a contribution on `ui.theme`: `{ base, groups }`, laid over a built-in variant so every
name in the contract still resolves. The option's list of values is the list of themes; a theme
withdrawn while in use puts the workspace back on a built-in, through the option path, so every
listener hears it.

### The workspace says what moved

`neosh.win.enter`, `.leave`, `.open`, `.close`, `neosh.cursor`, `neosh.mode`, `neosh.viewport` —
on the bus every plugin already listens to, `from: "neosh"`, read off the same batch the screen is
drawn from so the two cannot disagree. Not new wire types; `neosh.ready` set the precedent.
`event.on(name, cb, { kind })` filters on the buffer kind, which is the field a plugin decorating
one panel actually cares about.

`vars` gains buffer and window scopes, in memory only and dropped with the thing they describe.
That is the answer to "a var write is a file write": what changes per keystroke is about a buffer
or a window, and those two scopes are never written down.

### One tier rule

`builtin < plugin < user`, for keys, commands and highlights alike. A lower tier never takes a
name a higher one holds — silently, because a default finding its key taken is the ordinary case.
Within a tier, later wins, which is load order. A command name a higher tier holds is not refused
to the lower: the registration waits in the wings and comes back when the higher tier lets go, so
a bundled panel's `activate` does not fail over a verb the user had already decided to own.

### Everything else that was half there is now there

The host's buffers have kinds — `neosh.transcript`, `neosh.composer`, `neosh.status` — and the
composer is the *home* window, so a binding on its kind resolves at rest. The shared pickers have
kinds. `deactivate` runs, bounded. `plugins.disabled` disables any plugin. A plugin with
`[activation]` is held until a command, an event or a kind asks for it; the command is registered on
its behalf and the press replayed. `[provides]` says what a plugin reads, so `ext.points()` can say
who reads what and a contribution to a point nobody reads is reported at startup with the nearest
real name. `ext.plugins()` and the `plugins.list` panel are the `:checkhealth`.

`ListPanel` in `@neosh/api/ui` is all of this for a third party's panel, for the price of a kind
and a `rows` function — because 0040's "a panel that lacks any of these is not finished" was a rule
that cost too much to follow by hand, and five of our own panels proved it.

## Consequences

The three motivating cases work end to end and are tested: a git badge on the sidebar's project
row, a window restyled by kind, a theme as a plugin. So do the ones behind them: a plugin that
requires, imports and asks the sidebar; a lazy plugin woken by its key; a third-party panel on
`ListPanel` decorated by a plugin that has never heard of it.

**The sidebar writes `sidebar.projects` on load now.** It used to be written only when a project
*appeared*, so on a fresh workspace the var every decorator is told to read was not there. Found
by the first decorator.

**The reserved-key list is read off the registry.** The hand-kept one was missing `gg`, `G`,
`^D`, `^U`, `⇥` and the digits, so a contributed `G` silently replaced *go to the bottom*.

**`GitStatus` takes a `cwd`**, as `GitBranches` did, because a decorator asks about every project it
lists rather than the one the conversation is in.

**Not done, and recorded here so it is a known gap.** Per-buffer and per-window *options*
(`setlocal`) — vars have the scopes, options do not yet. The tool-card renderer, the footer and the
composer chrome are still the host's to draw; they are reachable as kinds now but not as points.
Five of nine manifest permissions are still unenforced, as before this. And a plugin that spins
still wedges the one isolate — the price `require` is paid in, and the one Neovim pays too.
