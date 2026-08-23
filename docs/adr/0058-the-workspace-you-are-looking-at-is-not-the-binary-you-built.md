# 0058 — The workspace you are looking at is not the binary you built

## What happened

Several merged pull requests' worth of sidebar work — the favourite marker moved off the left
column, every indent re-cut two columns narrower — did not appear on screen. The repository was
right, the branch was right, `cargo run` was run in it, and the panel drew exactly what it had
drawn the day before. Nothing said why, because from the terminal's point of view nothing was
wrong.

The workspace was a process that had been up for four hours and forty-six minutes:

```text
/proc/1598725/exe -> '/home/meszmate/projects/neosh/target/debug/neosh (deleted)'
```

`cargo run` had built a new binary, unlinked the old one and written a new file at the same path.
The workspace went on executing the inode it had started with — the one now marked `(deleted)` —
and the terminal that had just been built attached to it and drew what it was sent. That is ADR
0036 working as designed: the workspace is a process and the terminal is a viewer of it, closing a
terminal does not stop turns, and starting a terminal does not start a workspace. Every plugin runs
in the workspace. The sidebar *is* a plugin. So the code that draws the panel was in another
process, loaded from a file that no longer existed, and no amount of rebuilding was going to reach
it.

`PROTOCOL_VERSION` already refuses the case where the two cannot talk, and the refusal even names
this scenario: *"it has been running since before neosh was upgraded"*. But the protocol had not
changed. The two processes understood each other perfectly. The only thing wrong was that one of
them was five hours out of date, and that is not a thing the wire could express.

The failure mode is worse than a crash for being survivable. A crash is investigated. This looks
like a change that did not work, so what gets debugged is the change — and it is not the change,
it is the process.

## The decision

**Two neoshes that can talk are still allowed to disagree about which build they are, and the one
with a screen says so.**

`BuildId` — a version string and the executable's modification time — travels on `ClientMessage::
Attach` and comes back on `ServerMessage::Attached`, and sits in `WorkspaceStatus` so `neosh
status` can be asked the same question without attaching. A mismatch is a notice, never a refusal:
the protocol matches, everything works, and refusing to attach to a running workspace because a
file on disk changed would take somebody's live turns away over a cosmetic difference.

Four things follow, and each of them is the whole point of one line of code.

**The stamp is captured at startup, not when it is first needed.** A workspace outlives the file it
was loaded from. Asked at the first attach — hours later, which is exactly when it is interesting —
`current_exe` is a path with `(deleted)` on the end and there is nothing to stat. A workspace that
cannot say what it is is one nobody can be warned about, so `build::capture()` is the first thing
`run` does.

**The terminal decides, because the workspace cannot.** A stale workspace is running the only code
it has ever had; there is nothing in it that knows it is behind. Being out of step is a fact about
the pair, and only the newer of the two can see both halves of it.

**An unreadable stamp is unknown, not old.** Zero predates every build, so compared as a number it
would warn on every attach on any platform that cannot stat its own executable — which is the
fastest way to teach somebody that this message means nothing. `same_as` answers *yes* when either
side is unknown.

**Different is not the same question as behind.** An installed neosh and a `target/debug` one
written in the same second differ with neither behind the other, and that is worth a milder
sentence. Being *behind* is the case that gets `neosh stop` in it, because it is the only one where
there is something newer you meant to be running.

It is `NoticeKind::Alert` rather than `Reply` for the reason `ALERT_TTL` already gives: six seconds
would start from a moment nobody chose, and this particular moment is one where the screen is still
filling in. It stays inside the terminal regardless — leaving one is `UiEvent::Alert`, which is the
host's to send, and a rebuild is not news worth a desktop notification.

The notice is one row and nothing wraps it, so it carries the two facts that cannot be worked out
from the screen — how far behind, and what to do — and `neosh status` carries the paragraph.

## Consequences

Development stops having a silent failure in it. The two ways out are both said on screen:
`neosh stop` and start again, or `--no-daemon`, which is one process that begins and ends with the
run and therefore cannot be stale.

`neosh status` grows a row, but only when the builds differ. A version string on every status is a
line nobody asked for.

It does not detect a workspace whose *plugins* are stale while its binary is current, because that
cannot happen: the plugin tree is embedded with `include_dir!`, so plugins and binary are one
artefact. What it does not cover is `include_dir!` failing to notice an edit under `plugins/` —
that is a build-time problem with a build-time answer (`touch crates/neosh-script/src/loader.rs`),
and a stamp on the binary cannot see it.
