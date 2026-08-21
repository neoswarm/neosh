# 0040 — A panel is a surface, not a program

**Status:** accepted

Extends [0016](0016-capabilities-in-core-interface-in-plugins.md), which put every capability on the
public API, to the thing 0016 did not cover: what a plugin can do to *another plugin's* interface.

## Context

The rule at the top of the repository is that anything neosh can put on screen, a plugin can put on
screen and take off it. That has been true of the *host* for a long time — the sidebar, the model
picker, the palette and the approvals dialog are all ordinary plugins, and `plugins.disabled` turns
any of them off.

It was not true of the plugins themselves, and the gap only shows up when you try to do the obvious
thing: add one key to the project panel.

**A panel's keys were private.** The sidebar took every key with `keymap.capture` and resolved it in
a `switch` statement four hundred lines into its own source. That worked and was fast, and it meant
`j`, `x`, `f` and `↵` were invisible to `keymap.list`, absent from `F1`, unavailable to `^K`, and
impossible to rebind. The one place in the workspace where a user had *no* say over a key was the
panel with the most keys in it. The help screen kept a hand-written copy of them, which is the
shape a fact takes just before it goes stale.

**A panel's window was private too.** `KeymapScope::Window` existed and resolved correctly, so in
principle a third party could have bound a key inside the sidebar — if they could name the window.
They could not. The id belongs to whoever opened it, is not published anywhere, and changes every
time the panel is toggled. There was no way to say "the sidebar, whichever window that is now".

**There was nowhere to put anything.** No way for one plugin to add a row to another's list, no way
to hear that something happened in a plugin you do not import, and nothing shared to write on: a
project's favourite flag lived in the sidebar's private state, so a second panel started with no
favourites and could not have been told about them.

The result was an extensibility story with a cliff in it. Small things — a colour, an option, a
status segment — were easy. Anything that touched an existing panel meant forking twelve hundred
lines, and a fork does not get the next version's bug fixes.

## Decision

Four primitives, and then the bundled panels are rewritten onto them so that the surface is the one
they actually use.

**A buffer has a kind, and keymaps can be scoped to it.** `buf.create({ kind: "neosh.sidebar" })`
publishes a name; `KeymapScope::BufKind` binds against it. Resolution is window → buffer → kind →
global, first match winning: a binding on one buffer is about one thing on screen and beats one
about every sidebar, and a binding on every sidebar beats a global default, so a panel can still
take a key back. `win.list()` reports the kind, which is how a plugin finds a panel it did not open.

This is Neovim's filetype, and it is here for Neovim's reason. A window id is an instance and dies
with the window; a kind is a name, and one call covers every window of that kind including the ones
opened tomorrow.

**`ext.contribute` is a contribution point.** A point is a name a plugin agrees to read —
`sidebar.section`, `sidebar.action` — and a contribution is a JSON item on it, conventionally
carrying a command name. The sidebar renders rows it did not write and invokes commands it has never
heard of; neither side imports the other. Contributions are owned by their plugin and withdrawn when
it unloads, so `plugins.disabled` takes somebody's rows with them and there is no way to leave a row
pointing at a command that has gone.

Data rather than a callback, deliberately. A contribution can be listed, described and disabled; a
function held inside somebody's closure cannot be any of those.

**`event.emit` is `User`.** Plugin-defined, broadcast, no reply. The absence of a reply is the
design: an emitter that could be blocked is an emitter with every listener on its critical path,
which is how one slow plugin wedges a panel. `from` is stamped by the host, so who said it is not
forgeable. Something that needs an answer registers a command or reads a point.

**`vars` is shared state, scoped to the thing it describes.** Plugin state stays private — right for
a fold set, wrong for "this project is a favourite", which four plugins have an opinion about. A var
is scoped to the workspace, a conversation or a project, and anybody may read or write it. A
conversation's vars are deleted with it. A project's outlive every conversation in it, because a
project is a directory and the directory is still there.

**And then the sidebar was rewritten against all four.** Every verb is a named command bound at
`buf_kind` scope. Favourites, order and fold state are project vars, migrated once from the old
private store. `sidebar.section` puts rows in the column; `sidebar.action` puts a verb on a row, and
the panel binds the key on the contributor's behalf so the row under the cursor can be passed as
arguments — a key press carries none of its own, and a plugin tracking the cursor separately would
be one race away from acting on the wrong conversation.

The help screen now generates the panel's section from the registry, and shows the panel you asked
from first.

## Consequences

`F1` lists the panel's keys, `^K` runs them, and rebinding one is a line in `init.ts`. That was the
motivating case and it is now the ordinary mechanism rather than a feature.

The hand-written `PANEL_KEYS` list in the palette is gone. What is left in that file is the keys
that genuinely are not bindings — the composer's, and the transcript reader's — which is now a
measure of how much is still not on the surface rather than a list nobody remembered to update.

**Contributed keys can collide, and the panel arbitrates.** `keymap.set` already refuses to let a
bundled default overwrite somebody's choice, but here the bundled plugin does the binding on a third
party's behalf, so that rule points the wrong way. The sidebar keeps a list of the keys it has spoken
for and refuses those, logging which one was refused. The command is still registered, so `^K` runs
it. This is a policy in one plugin rather than a rule in the core, because which keys a panel
considers its own is a question only that panel can answer.

**A var write is a file write.** Fine for `f` and `J`, wrong for anything on a cursor move — which
is why the sidebar's selection is not a var. A panel that wants to publish something per keystroke
emits an event.

**Two stores is a thing to explain.** `state` and `vars` differ only in who may look, and the
temptation will be to reach for `vars` for everything because it is the more capable one. The
guidance is the opposite: private until somebody else has a reason to see it. A workspace where every
plugin writes its scratch state into a shared table is a workspace where nobody can change a key
name.

**The registry is in the core, the vars are in the host.** Contributions are a registration owned by
a plugin, the same as a command or a keymap, and belong where those live. Vars are persisted and
scoped to conversations and projects, neither of which the core knows about. `Editor::handles` is a
deny-list, so the var and event calls had to be added to it — a new call that is not is silently
routed to the core and fails there.
