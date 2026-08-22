#!/usr/bin/env python3
"""Watch candidate tool-card layouts, animated, in your own terminal.

    python3 scripts/mock.py

The same scripted turn — five reads, a grep, two edits, a test run that fails and
then passes, a file written — played through five different layouts. Keys:

    1..5   pick a layout        space  replay from the start
    ,  .   slower / faster      q      quit

Nothing here imports from neosh. It is a drawing board: the colours are copied
from `crates/neosh-core/src/palette.rs` (DARK) and the maths of the sweep is
copied from `crates/neosh-tui/src/shimmer.rs`, so what you see is what the real
thing would look like — but no decision has been made and nothing is wired up.
Delete this file when the argument is over.
"""

import math
import select
import shutil
import sys
import termios
import time
import tty

# ---------------------------------------------------------------------------
# Palette — DARK, from crates/neosh-core/src/palette.rs
# ---------------------------------------------------------------------------

FG = (0xD6, 0xDB, 0xE4)
MUTED = (0x8B, 0x94, 0xA3)
FAINT = (0x4B, 0x52, 0x63)
ACTIVE = (0x38, 0xBD, 0xF8)
ATTENTION = (0xFC, 0xD3, 0x4D)
DANGER = (0xFC, 0xA5, 0xA5)
SUCCESS = (0x6E, 0xE7, 0xB7)
ACCENT = (0xA5, 0xB4, 0xFC)
DIFF_ADD_BG = (0x14, 0x2B, 0x1E)
DIFF_DEL_BG = (0x38, 0x1B, 0x1C)
CURSOR_BG = (0x2A, 0x30, 0x3C)

KIND_COLOUR = {"read": ACTIVE, "edit": SUCCESS, "bash": ATTENTION, "write": SUCCESS}


def mix(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def paint(text, rgb=None, *, bold=False, dim=False, bg=None):
    if not text:
        return ""
    codes = []
    if bold:
        codes.append("1")
    if dim:
        codes.append("2")
    if rgb:
        codes.append("38;2;%d;%d;%d" % rgb)
    if bg:
        codes.append("48;2;%d;%d;%d" % bg)
    if not codes:
        return text
    return "\x1b[" + ";".join(codes) + "m" + text + "\x1b[0m"


# ---------------------------------------------------------------------------
# Motion
# ---------------------------------------------------------------------------

SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"


def spinner(t):
    return SPINNER[int(t / 0.08) % len(SPINNER)]


def sweep(text, base, t, period=2.0, strength=0.85):
    """A raised-cosine band travelling along a run of text. See shimmer.rs."""
    n = len(text)
    if n == 0:
        return ""
    half, pad = 4.0, 5.0
    centre = ((t % period) / period) * (n + pad * 2) - pad
    out = []
    for i, ch in enumerate(text):
        d = abs(i - centre)
        lit = 0.0 if d > half else 0.5 * (1.0 + math.cos(math.pi * d / half))
        out.append(paint(ch, mix(base, (255, 255, 255), lit * strength)))
    return "".join(out)


def flash(rgb, landed, t, ms=0.45):
    """One-shot brighten the instant a row commits, easing back out."""
    if landed is None or not (0 <= t - landed < ms):
        return rgb
    e = 1.0 - (t - landed) / ms
    return mix(rgb, (255, 255, 255), 0.75 * e * e)


def count_up(n, landed, t, ms=0.35):
    if landed is None:
        return 0
    return int(round(n * min(1.0, max(0.0, (t - landed) / ms))))


def took(sec):
    if sec < 1:
        return "%dms" % int(sec * 1000)
    if sec < 60:
        return "%.1fs" % sec
    return "%dm %02ds" % (sec // 60, sec % 60)


# ---------------------------------------------------------------------------
# The turn being played
# ---------------------------------------------------------------------------

QUESTION = "make the tool cards smaller and give them some life"

FAIL_OUT = [
    "   Compiling neosh-core v0.1.0 (/work/crates/neosh-core)",
    "error[E0308]: mismatched types",
    "   --> crates/neosh-core/src/palette.rs:291:30",
    "    |",
    "291 |         (\"Agent.ToolRunning\", spec(pulse(fg(r.attention), 2600))),",
    "    |                               ^^^^ expected `(&str, &str)`, found `&str`",
    "    |",
    "error: could not compile `neosh-core` (lib test) due to 1 previous error",
]

PASS_OUT = [
    "   Compiling neosh-core v0.1.0 (/work/crates/neosh-core)",
    "    Finished `test` profile in 11.9s",
    "     Running unittests src/lib.rs",
    "test cards::a_run_of_reads_is_one_row ... ok",
    "test cards::a_finished_card_carries_no_mark ... ok",
    "test palette::every_group_a_card_uses_exists ... ok",
    "test result: ok. 42 passed; 0 failed; 0 ignored",
]

EDIT_DIFF = [
    ("-", 154, "    fn mark(&self, state: ToolState) -> &'static str {"),
    ("+", 154, "    fn mark(&self, state: ToolState) -> (&'static str, &'static str) {"),
    (" ", 155, "        match state {"),
    ("-", 156, "            ToolState::Running => self.live,"),
    ("+", 156, "            ToolState::Running => (self.live, \"Agent.ToolRunning\"),"),
    ("-", 157, "            ToolState::Failed => self.fail,"),
    ("+", 157, "            ToolState::Failed => (self.fail, \"Agent.ToolFailed\"),"),
    (" ", 158, "        }"),
]

EDIT2_DIFF = [
    ("-", 291, "        (\"Agent.ToolRunning\", spec(pulse(fg(r.attention), 2600))),"),
    ("+", 291, "        (\"Agent.ToolRunning\", spec(frames(fg(r.attention), 80))),"),
]

WRITE_DIFF = [("+", i + 1, line) for i, line in enumerate([
    "# 0042. A card folds when it lands",
    "",
    "## Status",
    "",
    "Accepted.",
    "",
    "## Context",
    "",
    "A card's size is a function of the call. It should be a function of how",
    "long ago the call happened: what is in flight is what you are watching,",
    "and what has landed is a receipt you scan.",
])]


class Act:
    def __init__(self, kind, verb, subject, start, end, **kw):
        self.kind = kind
        self.verb = verb
        self.subject = subject
        self.start = start
        self.end = end
        self.ok = kw.get("ok", True)
        self.legs = kw.get("legs", [])          # (short name, start, end) for a read run
        self.out = kw.get("out", [])            # what a command printed
        self.diff = kw.get("diff", [])          # what an edit changed
        self.added = kw.get("added", 0)
        self.removed = kw.get("removed", 0)
        self.lines = kw.get("lines", 0)         # how much a read came back with
        self.exit = kw.get("exit", None)

    def state(self, t):
        if t < self.start:
            return "todo"
        return "running" if t < self.end else ("done" if self.ok else "failed")

    def colour(self):
        return KIND_COLOUR[self.kind]


ACTS = [
    Act("read", "Read", "cards.rs, host.rs, palette.rs, shimmer.rs, +1 more",
        0.4, 3.1, lines=1840,
        legs=[("cards.rs", 0.4, 1.1), ("host.rs", 0.9, 1.8), ("palette.rs", 1.4, 2.0),
              ("shimmer.rs", 1.7, 2.4), ("Agent.ToolLive", 2.1, 3.1)]),
    Act("edit", "Edited", "crates/neosh/src/cards.rs", 3.5, 4.3,
        diff=EDIT_DIFF, added=3, removed=3),
    Act("bash", "Ran", "cargo test -p neosh-core", 4.7, 9.4,
        ok=False, out=FAIL_OUT, exit=101),
    Act("edit", "Edited", "crates/neosh-core/src/palette.rs", 9.8, 10.5,
        diff=EDIT2_DIFF, added=1, removed=1),
    Act("bash", "Ran", "cargo test -p neosh-core", 10.9, 16.2, out=PASS_OUT, exit=0),
    Act("write", "Wrote", "docs/adr/0042-a-card-folds-when-it-lands.md", 16.6, 17.3,
        diff=WRITE_DIFF, added=12),
]

ANSWER_AT = 17.8
ANSWER = [
    "Cards fold when they land now, and the run of reads is one row. The mark",
    "only shows while a call is out or when it went wrong.",
]
LOOP = 24.0

CHANGED = [("crates/neosh/src/cards.rs", 3, 3),
           ("crates/neosh-core/src/palette.rs", 1, 1),
           ("docs/adr/0042-a-card-folds-when-it-lands.md", 12, 0)]


# ---------------------------------------------------------------------------
# Pieces every layout shares
# ---------------------------------------------------------------------------

def facts(a, t, *, live_clock=True):
    """The tail of a row: what it changed, how it ended, how long it took."""
    st = a.state(t)
    out = []
    if st == "running":
        if live_clock and t - a.start > 0.35:
            out.append(paint(took(t - a.start), MUTED, dim=True))
        return out
    if st == "todo":
        return out
    landed = a.end
    if a.added or a.removed:
        add = count_up(a.added, landed, t)
        rem = count_up(a.removed, landed, t)
        piece = paint("+%d" % add, flash(SUCCESS, landed, t))
        if a.removed:
            piece += " " + paint("-%d" % rem, flash(DANGER, landed, t))
        out.append(piece)
    if a.lines:
        out.append(paint("%d lines" % a.lines, MUTED, dim=True))
    if a.exit is not None:
        if a.exit == 0:
            out.append(paint("ok", flash(SUCCESS, landed, t)))
        else:
            out.append(paint("exit %d" % a.exit, flash(DANGER, landed, t), bold=True))
    out.append(paint(took(a.end - a.start), MUTED, dim=True))
    return out


def plain_len(s):
    """Visible width of a string that has SGR in it."""
    n, i = 0, 0
    while i < len(s):
        if s[i] == "\x1b":
            i = s.index("m", i) + 1
            continue
        n += 1
        i += 1
    return n


def justify(left, right, width):
    gap = width - plain_len(left) - plain_len(right)
    return left + " " * max(2, gap) + right


def mark_of(a, t, g_live="▸"):
    st = a.state(t)
    if st == "running":
        return paint(spinner(t), ATTENTION)
    if st == "failed":
        return paint("✗", DANGER, bold=True)
    return " "


def subject_span(a, t):
    """The subject: sweeping while the call is out, quiet once it has landed."""
    st = a.state(t)
    if st == "running":
        return sweep(a.subject, ACTIVE, t)
    return paint(a.subject, flash(MUTED, a.end, t))


def run_names(a, t):
    """A read run's names, each coloured by its own call."""
    out = []
    for name, start, end in a.legs:
        if t < start:
            continue
        if t < end:
            out.append(sweep(name, ACTIVE, t))
        else:
            out.append(paint(name, flash(MUTED, end, t)))
    return out


def body_rows(a, t, width, *, limit_out=3, limit_diff=12, margin="  │ "):
    """What a call came back with, under a rule."""
    rows = []
    pre = paint(margin, FAINT, dim=True)
    room = width - len(margin)
    if a.diff:
        for sign, no, text in a.diff[:limit_diff]:
            bg = DIFF_ADD_BG if sign == "+" else (DIFF_DEL_BG if sign == "-" else None)
            col = SUCCESS if sign == "+" else (DANGER if sign == "-" else MUTED)
            body = "%s %4d %s" % (sign, no, text)
            body = body[:room].ljust(room)
            rows.append(pre + paint(body, col if sign != " " else MUTED, bg=bg, dim=sign == " "))
        if len(a.diff) > limit_diff:
            rows.append(pre + paint("⋮ +%d lines (^S ⇥ to expand)"
                                    % (len(a.diff) - limit_diff), MUTED, dim=True))
        return rows
    if not a.out:
        return rows
    shown = a.out if a.state(t) == "running" else a.out
    if a.state(t) == "running":
        # arriving over time, tail-following
        n = max(1, int((t - a.start) / (a.end - a.start) * len(a.out)))
        shown = a.out[:n][-limit_out:]
        for line in shown:
            rows.append(pre + paint(line[:room], MUTED, dim=True))
        return rows
    if len(a.out) <= limit_out:
        for line in a.out:
            rows.append(pre + paint(line[:room], DANGER if not a.ok else MUTED,
                                    dim=a.ok))
        return rows
    head, tail = a.out[:1], a.out[-(limit_out - 1):]
    for line in head:
        rows.append(pre + paint(line[:room], MUTED, dim=True))
    rows.append(pre + paint("⋮ +%d lines (^S ⇥ to expand)"
                            % (len(a.out) - len(head) - len(tail)), MUTED, dim=True))
    for line in tail:
        rows.append(pre + paint(line[:room], DANGER if not a.ok else MUTED, dim=a.ok))
    return rows


def summary_rows(t, width):
    if t < ACTS[-1].end + 0.2:
        return []
    landed = ACTS[-1].end + 0.2
    add = sum(f[1] for f in CHANGED)
    rem = sum(f[2] for f in CHANGED)
    rows = ["", paint("  changed %d files  " % len(CHANGED), MUTED)
            + paint("+%d" % count_up(add, landed, t), flash(SUCCESS, landed, t))
            + " " + paint("-%d" % count_up(rem, landed, t), flash(DANGER, landed, t))]
    for path, a, r in CHANGED:
        left = paint("    " + path, MUTED, dim=True)
        right = paint("+%d" % a, SUCCESS) + " " + paint("-%d" % r, DANGER)
        rows.append(justify(left, right, width))
    return rows


def answer_rows(t, width):
    if t < ANSWER_AT:
        return []
    out = [""]
    for i, line in enumerate(ANSWER):
        chars = int(max(0, (t - ANSWER_AT - i * 0.5)) / 0.012)
        if chars <= 0:
            break
        out.append("  " + paint(line[:chars], FG))
    return out


def question_rows(width):
    return [paint("▌ ", ACCENT, bold=True) + paint(QUESTION, FG), ""]


# ---------------------------------------------------------------------------
# The five layouts
# ---------------------------------------------------------------------------

def layout_today(t, width):
    """1 — what neosh draws now: a blank, a header, and a body that never leaves."""
    rows = question_rows(width)
    for a in ACTS:
        if a.state(t) == "todo":
            continue
        rows.append("")
        head = mark_of(a, t) + " " + paint(a.verb, a.colour()) + "  "
        if a.legs:
            head = mark_of(a, t) + " " + paint("Read", ACTIVE) + "  " + ", ".join(run_names(a, t))
        else:
            head += subject_span(a, t)
            tail = facts(a, t)
            if tail:
                head += "  " + "  ".join(tail)
        rows.append(head)
        if a.state(t) != "todo" and not a.legs:
            rows.extend(body_rows(a, t, width))
    rows.extend(summary_rows(t, width))
    rows.extend(answer_rows(t, width))
    return rows


def layout_fold(t, width):
    """2 — generous while it is out, one row of receipt the moment it lands."""
    rows = question_rows(width)
    for a in ACTS:
        st = a.state(t)
        if st == "todo":
            continue
        if a.legs:
            rows.append(mark_of(a, t) + " " + paint("Read".ljust(8), ACTIVE)
                        + ", ".join(run_names(a, t)))
            continue
        left = mark_of(a, t) + " " + paint(a.verb.ljust(8), a.colour()) + subject_span(a, t)
        right = "  ".join(facts(a, t))
        rows.append(justify(left, right, width))
        # A body only while it is out, or for ever if it failed.
        if st == "running" or st == "failed":
            rows.extend(body_rows(a, t, width, limit_out=4, limit_diff=6))
            rows.append("")
    rows.extend(summary_rows(t, width))
    rows.extend(answer_rows(t, width))
    return rows


def layout_stage(t, width):
    """3 — nothing in flight is in the transcript; it commits a row when it lands."""
    rows = question_rows(width)
    for a in ACTS:
        if a.state(t) in ("todo", "running"):
            continue
        if a.legs:
            rows.append("  " + paint("Read".ljust(8), ACTIVE) + ", ".join(run_names(a, t)))
            continue
        left = "  " + paint(a.verb.ljust(8), a.colour()) + subject_span(a, t)
        if not a.ok:
            left = paint("✗ ", DANGER, bold=True) + paint(a.verb.ljust(8), a.colour()) + subject_span(a, t)
        rows.append(justify(left, "  ".join(facts(a, t)), width))
        # A failure always shows, in every layout: the setting is about noise,
        # not about hiding failures.
        if not a.ok:
            rows.extend(body_rows(a, t, width, limit_out=3))
    rows.extend(summary_rows(t, width))
    rows.extend(answer_rows(t, width))
    return rows


def stage_rows(t, width):
    """The pane above the composer that layout 3 puts everything live into."""
    live = [a for a in ACTS if a.state(t) == "running"]
    if not live:
        return []
    a = live[0]
    rows = [paint("─" * width, FAINT, dim=True)]
    left = paint(spinner(t), ATTENTION) + " " + paint(a.verb, a.colour()) + "  " + sweep(
        a.subject if not a.legs else ", ".join(n for n, s, e in a.legs if s <= t), ACTIVE, t)
    rows.append(justify(left, paint(took(t - a.start), MUTED, dim=True), width))
    rows.extend(body_rows(a, t, width, limit_out=3, margin="  ")[:3])
    return rows


def layout_ledger(t, width):
    """4 — one row, always. The body is somewhere you go, never something you scroll past."""
    rows = question_rows(width)
    for a in ACTS:
        st = a.state(t)
        if st == "todo":
            continue
        if a.legs:
            rows.append(mark_of(a, t) + " " + paint("Read".ljust(8), ACTIVE)
                        + ", ".join(run_names(a, t)))
            continue
        left = mark_of(a, t) + " " + paint(a.verb.ljust(8), a.colour()) + subject_span(a, t)
        right = "  ".join(facts(a, t))
        if st == "failed":
            right += "  " + paint("⇥", ACCENT, bold=True)
        rows.append(justify(left, right, width))
    rows.extend(summary_rows(t, width))
    rows.extend(answer_rows(t, width))
    return rows


RAIL_GLYPH = {"read": "◇", "edit": "✎", "bash": "⟩", "write": "✎"}


def layout_rail(t, width):
    """5 — the verb is a glyph, so every subject starts in the same column."""
    rows = question_rows(width)
    live_any = any(a.state(t) == "running" for a in ACTS)
    for a in ACTS:
        st = a.state(t)
        if st == "todo":
            continue
        rail = paint("┃ ", mix(a.colour(), (0, 0, 0), 0.45))
        glyph = RAIL_GLYPH[a.kind]
        if st == "running":
            g = paint(spinner(t), a.colour())
        elif st == "failed":
            g = paint("✗", DANGER, bold=True)
        else:
            g = paint(glyph, flash(mix(a.colour(), (0, 0, 0), 0.25), a.end, t))
        if a.legs:
            left = "  " + rail + g + " " + ", ".join(run_names(a, t))
            right = paint("%d lines" % a.lines, MUTED, dim=True) if st == "done" else ""
        else:
            left = "  " + rail + g + " " + subject_span(a, t)
            right = "  ".join(facts(a, t))
        rows.append(justify(left, right, width))
        if st == "failed":
            for r in body_rows(a, t, width - 6, limit_out=3, margin="│ "):
                rows.append("  " + rail + r)
    rows.extend(summary_rows(t, width))
    rows.extend(answer_rows(t, width))
    return rows


LAYOUTS = [
    ("Today", "what neosh draws now — a blank, a header, a body that never leaves", layout_today, False),
    ("Fold", "generous while it is out, one row of receipt the moment it lands", layout_fold, False),
    ("Stage", "nothing live is in the transcript; the pane above the composer is", layout_stage, True),
    ("Ledger", "one row, always. ⇥ opens the body somewhere else", layout_ledger, False),
    ("Rail", "the verb is a glyph, so every subject starts in the same column", layout_rail, False),
]


# ---------------------------------------------------------------------------
# Player
# ---------------------------------------------------------------------------

def working_line(t, width):
    if t >= ANSWER_AT or t < 0.2:
        return []
    return ["", paint("✳", ATTENTION) + " " + sweep("Working", ATTENTION, t, period=2.6)
            + "  " + paint(took(t), MUTED, dim=True)
            + paint("   Esc to interrupt", FAINT, dim=True)]


def composer(width):
    return [paint("─" * width, FAINT, dim=True),
            paint("› ", ACCENT, bold=True) + paint("", FG),
            paint("  claude-opus-5 · full access · ^K palette", FAINT, dim=True)]


def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    sys.stdout.write("\x1b[?1049h\x1b[?25l")
    pick, speed, t0 = 1, 1.0, time.monotonic()
    try:
        tty.setcbreak(fd)
        while True:
            cols, lines = shutil.get_terminal_size((100, 34))
            width = min(cols - 2, 96)
            t = ((time.monotonic() - t0) * speed) % LOOP
            name, blurb, fn, staged = LAYOUTS[pick - 1]

            tabs = []
            for i, (n, _, _, _) in enumerate(LAYOUTS, 1):
                label = " %d %s " % (i, n)
                tabs.append(paint(label, (20, 24, 30), bg=ACCENT, bold=True)
                            if i == pick else paint(label, MUTED))
            head = ["", "  " + "".join(tabs), "  " + paint(blurb, FAINT, dim=True), ""]

            foot = []
            if staged:
                foot += stage_rows(t, width)
            else:
                foot += working_line(t, width)
            foot += [""] + composer(width)
            foot += [paint("  1-5 layout · space replay · , . speed %.1fx · q quit"
                           % speed, FAINT, dim=True)]

            body = ["  " + r if r else "" for r in fn(t, width)]
            room = lines - len(head) - len(foot) - 1
            above = max(0, len(body) - room)
            if above:
                body = body[above:]
                body[0] = paint("  ↑ %d rows above" % above, DANGER, dim=True)

            out = ["\x1b[H"]
            foot = ["  " + r if r else "" for r in foot]
            for row in head + body + [""] * max(0, room - len(body)) + foot:
                out.append(row + "\x1b[K\r\n")
            out.append("\x1b[J")
            sys.stdout.write("".join(out))
            sys.stdout.flush()

            if select.select([sys.stdin], [], [], 0.05)[0]:
                key = sys.stdin.read(1)
                if key in "12345":
                    pick = int(key)
                elif key == " ":
                    t0 = time.monotonic()
                elif key == ",":
                    speed = max(0.25, speed - 0.25)
                elif key == ".":
                    speed = min(3.0, speed + 0.25)
                elif key in ("q", "\x03", "\x1b"):
                    break
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        sys.stdout.write("\x1b[?25h\x1b[?1049l")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
