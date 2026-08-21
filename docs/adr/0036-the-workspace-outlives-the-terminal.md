# 0036 — The workspace outlives the terminal

**Status:** accepted

## Context

Everything expensive in neosh outlives the reason you opened a terminal. A turn takes minutes. A
plugin runtime takes a second to boot, and then holds a JS heap, a model catalogue and whatever
state a panel has arranged. A conversation is the artefact — it is the thing you came for.

All of it was tied to a tty. Closing the window killed the work; so did the ssh connection
dropping, the laptop sleeping badly, or a `tmux` server being restarted. And the one thing you can
reliably predict about a window is that it will be closed.

The workaround people already use is to run the whole thing inside `tmux`, which works and is an
admission: the program cannot keep its own state, so a second program is asked to keep the terminal
that keeps it. That buys survival at the cost of everything `tmux` costs — a nested mux, its own
key prefix, its own scrollback, its own copy mode fighting `^S`.

There was also nothing to ask. `neosh` had no way to say what was running, because "what is
running" was only ever a property of a process you were already looking at.

## Decision

**The workspace is a process, and the terminal is a viewer of it.** `neosh` with no arguments finds
the workspace for your config directory and attaches; if there is none, it starts one and attaches
to that. What survives a terminal closing is *everything*: the turns in flight, the plugins, the
sessions, the catalogue, the scroll position. What the terminal owns is the screen and the keyboard
and nothing else.

**This is not a new protocol.** The boundary already existed and had been load-bearing since ADR
0001: `Frontend` takes `UiEvent`s, something else sends `InputEvent`s back, and `StdioFrontend` has
been writing exactly that over a pipe for as long as there have been end-to-end tests. All this adds
is a socket in the middle. The client needed no changes to `TerminalUi` at all — which is the test
of whether a boundary was real, and it passed.

What is new is one envelope, `ClientMessage` / `ServerMessage`, for the things a *connection* has to
say and a view does not: attaching, detaching, being told somebody else took over. Those are facts
about the wire, so they live on the wire's own type and `InputEvent` stays what it was — the same
events arrive from a terminal that owns the process, where there is no connection to talk about.

**A delta stream had to learn to say everything.** `UiEvent` is deltas, which is right for a
frontend that has been listening since the process started and useless for one connecting half a
day in: a delta into an empty mirror is nothing. So `Editor::republish` says the whole state —
highlights, then buffers with their marks, then windows with their cursors and scroll positions,
then focus — and a client that folds it in has exactly the mirror a client that had been there all
along would have. That is asserted directly, by building both and comparing them.

Two things had to be *kept* to make that possible, having previously only been forwarded: a
window's `top_line`, and a surface's rectangle. Reattaching to a transcript that dropped back to
the top would be a poor kind of survival.

A surface's *cells* are still not kept, and are not republished. They are a grid a plugin owns, and
holding a second copy would mean holding one that is wrong the moment the plugin draws again. The
claim is re-announced and `PluginEvent::ViewAttached` tells the plugin to paint.

`InputEvent::Attached` is separate from `InputEvent::Ready` for one reason: the answer is
different. `Ready` means "the frontend is up" and gets a delta stream; `Attached` means "and it has
never seen any of this" and gets everything. Folding the two together and always re-announcing
would make the in-process case send its whole state twice at startup, into a mirror that has it.

**`quit` and `stop` became different words.** They were the same word because there was only one
place to be. Now `^Q` closes the *terminal* — the turns keep running, the plugins stay loaded,
reattaching puts you back where you were — and `neosh stop` stops the workspace. A key you press by
reflex must not be able to kill a turn you were waiting on.

**One viewer at a time, and the newest wins.** A second `neosh` takes over and the first is told
why. Not because sharing is undesirable — it is the next thing to build, and see below — but
because it is the only honest thing this shape can do: there is one `Editor`, so there is one
cursor, one scroll position and one terminal size, and fanning that out to two windows of different
sizes draws the wrong layout in at least one of them. Taking over rather than refusing is
deliberate: a client that died without saying so is indistinguishable from one that is merely
quiet, so refusing would let a crashed terminal lock you out of your own work until you noticed and
killed it.

**One workspace per config directory.** Not per project: `^T` already lists conversations across
checkouts, and a socket per directory would mean a window in one repository could not see an agent
running in another. Keying on the *config* directory rather than on the user is what keeps a
`--config-dir` sandbox — every integration test, and `--clean` — out of the workspace you are
actually using. Without that, running the test suite would attach to the developer's real workspace
and stop it.

**A socket file is not a lock.** It outlives the process that made it, so a crash leaves one
behind, and treating its presence as "something is running" would mean one hard kill locks the
directory forever. The test is behavioural: *connect* to it. Something answers, and the socket is
not ours to take. Nothing answers, and it is a leftover, which is removed and rebound.

**`neosh status` answers from a snapshot, not from the run loop.** A status query has to be
answerable *while* the workspace is busy, because that is exactly when it is asked — so the host
publishes what it is holding and the accept loop reads it without disturbing anything. Published on
every path that could have changed the answer, rate-limited to once a second, *except* when the set
of running turns changes: whether something is running is the one thing the query is about, and
answering it a second late is answering the wrong question.

**Three ways to stay in one process**, because not everything wants a workspace:
`--ui-protocol=stdio` (a pipe has nobody to hand a running workspace to), `--clean` (it means "read
and write nothing", and attaching to a workspace running somebody's real config is the opposite of
that), and `--no-daemon` for when you want the old shape on purpose.

## Consequences

- `neosh status` and `neosh stop` are new. `neosh --serve` runs a workspace in the foreground,
  which is how you watch one boot when it will not: started for you, its errors go to `/dev/null`.
- A workspace we start is `setsid`, so it survives the shell that started it, the terminal that
  shell is in, and the ssh session that terminal is in. That is the whole point, and it is one line
  that is easy to lose.
- The socket lives under `$XDG_RUNTIME_DIR/neosh/` — the directory the operating system promises to
  clear when the session ends, which is exactly a socket's lifetime — with the state directory as
  the fallback, `0700` on the directory and `0600` on the socket. It is a way into a process that
  can read your files and spend your API credits.
- **Protocol mismatch is refused with a sentence**, naming `neosh stop`. The failure this exists
  for is neosh being upgraded while a workspace from the old one is still running, and it will
  happen to everybody exactly once.
- The mock provider can be paced (`NEOSH_MOCK_DELAY_MS`). A test that needs a turn to still be
  running while the terminal is closed under it has no other way to arrange one.
- **Windows is not covered.** Unix sockets only; a named pipe is the equivalent and is not written.
  Everything above the transport is transport-agnostic, so it is one module.
- **Nothing shuts a workspace down on its own.** It runs until `neosh stop`. An idle timeout is a
  reasonable thing to want and is deliberately not guessed at here.

## What this does not do yet, and what it costs to finish

The ask this came from was two things: work that survives the terminal, and *however many windows
you open, you see everything*. This is the first. The second needs `Host` split into a shared
`Workspace` — sessions, turns, providers, plugins — and a per-client `View` holding the editor, the
buffers, the cursor, the composer and the cards. Three things make that more than a mechanical
refactor, and they are worth writing down before somebody starts:

1. **The agent's event stream is single-consumer.** `Agent::new` hands back one `mpsc` receiver.
   Several views need every event, so it wants a router fanning out to a channel per view — not a
   broadcast channel, whose lagging consumers drop messages, which here would mean a view silently
   missing a tool result.
2. **`Agent` holds "the conversation on screen".** That is per-view state living in shared state.
   Only a handful of call sites, but the concept has to move to the view.
3. **Plugins draw, and would have to say where.** A plugin calling `ui.float.open()` today lands in
   *the* editor. With several, an `ApiCall` has to name the view it belongs to, defaulting to the
   one whose input triggered the plugin — which means threading a view through the plugin RPC, and
   through `@neosh/api`.

None of that is blocked by this ADR; all of it is easier because of it, since the wire between a
view and the workspace now exists and is exercised.
