#!/usr/bin/env python3
"""Drive neosh in a pty and print what the screen looks like.

Usage: shot.py [--cols N] [--rows N] [--wait MS] [--after MS] [--color] key [key ...]

Keys are literal text, or a name in angle brackets: <cr> <esc> <tab> <c-p> <up> <pageup> ...
A key of the form <wait:400> sleeps that many milliseconds, and <paste:text> arrives as a real
bracketed paste rather than as the keystrokes that would spell it.

--color prints what colour every run of every row is, instead of the plain screen. A band behind a
diff line and a syntax colour on top of it are both invisible to the plain dump, and "it looked
right" is not a thing you can check by reading the renderer.
"""
import os, pty, sys, time, select, signal, argparse

NAMED = {
    "cr": "\r", "esc": "\x1b", "tab": "\t", "bs": "\x7f", "space": " ",
    "up": "\x1b[A", "down": "\x1b[B", "right": "\x1b[C", "left": "\x1b[D",
    "s-tab": "\x1b[Z",
    # Shifted arrows, in the modifier form every terminal this runs under emits. Named here
    # because a binding on one is a binding no screenshot could otherwise reach.
    "s-up": "\x1b[1;2A", "s-down": "\x1b[1;2B",
    "s-right": "\x1b[1;2C", "s-left": "\x1b[1;2D",
    "f1": "\x1bOP", "f2": "\x1bOQ", "f3": "\x1bOR", "f4": "\x1bOS",
    # Bound keys, so a screenshot has to be able to press them. Paging is what a long transcript is
    # read with, and `home`/`end` are what a composer's line ends are reached with.
    "pageup": "\x1b[5~", "pagedown": "\x1b[6~",
    "home": "\x1b[H", "end": "\x1b[F",
    "delete": "\x1b[3~", "insert": "\x1b[2~",
}

def encode(tok: str) -> str:
    if not (tok.startswith("<") and tok.endswith(">")):
        return tok
    name = tok[1:-1].lower()
    if name in NAMED:
        return NAMED[name]
    if name.startswith("c-") and len(name) == 3 and name[2].isalpha():
        return chr(ord(name[2].upper()) - 64)
    if name.startswith("a-"):
        rest = name[2:]
        return "\x1b" + (NAMED.get(rest, rest))
    # A bracketed paste, which is a different thing from typing the same characters: neosh binds
    # `/`, reads a pasted image path as an attachment, and treats a pasted `<esc>` as text rather
    # than an interrupt. None of that is reachable by typing.
    if name.startswith("paste:"):
        return "\x1b[200~" + tok[len("<paste:"):-1] + "\x1b[201~"
    # Raw bytes, as hex: <raw:1b5b313b3341>. The escape hatch for keys whose encoding is the
    # question — a control character no name covers, or the sequence one particular terminal sends
    # for a chord. Without it, "does neosh see Ctrl+/" is unanswerable except by pressing it.
    # A mouse wheel notch, in SGR encoding: <wheel:up>, <wheel:down>. A wheel is not a key and
    # cannot be typed — and until neosh captured the mouse, a terminal faked one by sending three
    # arrows per notch, which fired three of somebody's bindings. Testing that it no longer does
    # means sending the real thing.
    if name.startswith("wheel:"):
        button = {"up": 64, "down": 65}.get(name[6:])
        if button is None:
            raise SystemExit(f"unknown wheel direction {tok}")
        return f"\x1b[<{button};10;10M"
    if name.startswith("raw:"):
        return bytes.fromhex(name[4:]).decode("latin-1")
    raise SystemExit(f"unknown key {tok}")

def desc(key):
    fg, bg, bold, italics, reverse = key
    bits = []
    if fg != "default":
        bits.append(fg)
    if bg != "default":
        bits.append("on " + bg)
    for on, name in ((bold, "bold"), (italics, "italic"), (reverse, "reverse")):
        if on:
            bits.append(name)
    return "[" + ",".join(bits) + "]" if bits else "[-]"


def runs(screen, y, cols):
    """One row as (style, text) runs, with the untouched tail of the terminal dropped."""
    row = screen.buffer[y]
    out, cur, key = [], "", None
    for x in range(cols):
        c = row[x]
        k = (c.fg, c.bg, c.bold, c.italics, c.reverse)
        if k != key and cur:
            out.append((key, cur))
            cur = ""
        key, cur = k, cur + c.data
    if cur:
        out.append((key, cur))
    while out and out[-1][0][:2] == ("default", "default") and not out[-1][1].strip():
        out.pop()
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=100)
    ap.add_argument("--rows", type=int, default=32)
    ap.add_argument("--wait", type=int, default=1500, help="ms to settle after boot")
    ap.add_argument("--after", type=int, default=700, help="ms to settle after the last key")
    ap.add_argument("--cmd", default="./target/debug/neosh")
    ap.add_argument("--arg", action="append", default=[], nargs="?", const="")
    ap.add_argument("--color", action="store_true", help="print colour runs instead of plain text")
    ap.add_argument("--grep", default=None, help="with --color, only rows containing this")
    ap.add_argument("keys", nargs="*")
    a = ap.parse_args()

    import pyte
    screen = pyte.Screen(a.cols, a.rows)
    stream = pyte.ByteStream(screen)

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = str(a.cols)
        os.environ["LINES"] = str(a.rows)
        os.execvp(a.cmd, [a.cmd] + a.arg)

    import fcntl, termios, struct
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))

    def drain(ms):
        end = time.time() + ms / 1000
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], max(0, end - time.time()))
            if not r:
                break
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            stream.feed(data)

    drain(a.wait)
    for tok in a.keys:
        if tok.startswith("<wait:"):
            drain(int(tok[6:-1]))
            continue
        os.write(fd, encode(tok).encode())
        drain(120)
    drain(a.after)

    # Where the terminal caret is, which the plain dump cannot show and which is the only way to
    # check "is the cursor visible" at all. `hidden` is a real answer: a caret the renderer could
    # not place is one it turned off, and that reads on screen as a mode with no cursor in it.
    cur = screen.cursor
    print(f"cursor: row {cur.y} col {cur.x}{' hidden' if cur.hidden else ''}")
    if a.color:
        for y in range(a.rows):
            text = screen.display[y]
            if not text.strip() or (a.grep and a.grep not in text):
                continue
            print(f"{y:3} " + "  ".join(f"{desc(k)}{v!r}" for k, v in runs(screen, y, a.cols)))
    else:
        print("┌" + "─" * a.cols + "┐")
        for line in screen.display:
            print("│" + line + "│")
        print("└" + "─" * a.cols + "┘")

    try:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    except OSError:
        pass

main()
