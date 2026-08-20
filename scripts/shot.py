#!/usr/bin/env python3
"""Drive neosh in a pty and print what the screen looks like.

Usage: shot.py [--cols N] [--rows N] [--wait MS] [--after MS] key [key ...]

Keys are literal text, or a name in angle brackets: <cr> <esc> <tab> <c-p> <up> ...
A key of the form <wait:400> sleeps that many milliseconds.
"""
import os, pty, sys, time, select, signal, argparse

NAMED = {
    "cr": "\r", "esc": "\x1b", "tab": "\t", "bs": "\x7f", "space": " ",
    "up": "\x1b[A", "down": "\x1b[B", "right": "\x1b[C", "left": "\x1b[D",
    "s-tab": "\x1b[Z",
    "f1": "\x1bOP", "f2": "\x1bOQ", "f3": "\x1bOR", "f4": "\x1bOS",
}

def encode(tok: str) -> str:
    if not (tok.startswith("<") and tok.endswith(">")):
        return tok
    name = tok[1:-1].lower()
    if name in NAMED:
        return NAMED[name]
    if name.startswith("c-") and len(name) == 3:
        return chr(ord(name[2].upper()) - 64)
    if name.startswith("a-"):
        rest = name[2:]
        return "\x1b" + (NAMED.get(rest, rest))
    raise SystemExit(f"unknown key {tok}")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=100)
    ap.add_argument("--rows", type=int, default=32)
    ap.add_argument("--wait", type=int, default=1500, help="ms to settle after boot")
    ap.add_argument("--after", type=int, default=700, help="ms to settle after the last key")
    ap.add_argument("--cmd", default="./target/debug/neosh")
    ap.add_argument("--arg", action="append", default=[], nargs="?", const="")
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
