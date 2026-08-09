#!/usr/bin/env python3
"""
localecho.py - a prototype of client-side line editing over a PTY.

Sits between your terminal and a child process (a local shell, or `ssh host`).
While the child is NOT in the alternate screen, it takes over line editing
entirely: keystrokes are echoed locally and instantly, and nothing is sent
over the wire until you press Enter. The moment a full-screen app takes over
(vim, htop, less -- anything emitting \\e[?1049h) it drops to raw passthrough.

This is deliberately NOT mosh. There is no prediction and no server-side
emulator. Inside the alternate screen it is exactly as slow as plain SSH,
which is the honest ceiling for a client-only approach.

Usage:
    # Feel it against a local shell with a simulated 150ms RTT:
    ./demo/localecho.py --latency 150

    # Against a real host (no synthetic latency):
    ./demo/localecho.py --ssh myhost

    # Real host, plus extra simulated latency on top:
    ./demo/localecho.py --ssh myhost --latency 100

Press Ctrl-] at any time to toggle local line editing on/off, so you can
A/B the same connection at the same latency. That toggle is the whole point:
run it, type a few commands with it on, hit Ctrl-], type the same commands
again.

Known boundaries, which you should go and hit on purpose:
  * Tab completion / Ctrl-R / arrow keys can't work locally -- we don't know
    the remote's history or filesystem. Pressing them flushes the buffer and
    falls back to raw mode for the rest of the line.
  * Inside vim/htop/less: no benefit at all, by design.
  * Password prompts: see the PW_HINTS heuristic below. Read that comment
    before typing a real password into this thing.
"""

import argparse
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time
import tty

# DEC private modes for the alternate screen buffer. 1049 is what basically
# everything modern uses; 1047/47 are legacy but cost nothing to watch for.
ALT_ENTER = (b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h")
ALT_EXIT = (b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l")
MAX_SEQ = max(len(s) for s in ALT_ENTER + ALT_EXIT)

# When the remote disables echo we have no way to know -- there is no channel
# for it over SSH. This heuristic sniffs the output stream for something that
# looks like a password prompt and disarms local echo for the next line. It
# catches sudo/ssh/su, which is most of what matters in practice, but it is a
# heuristic and not a guarantee. A prompt this doesn't match will get your
# input painted on screen.
PW_HINTS = (b"assword", b"assphrase", b"PIN", b"Verification code")

TOGGLE_KEY = 0x1D  # Ctrl-]

# Keys we cannot service locally: they need remote state (history, completion,
# cursor movement within a line the remote owns).
BAILOUT_KEYS = {
    0x09,  # Tab      - completion
    0x12,  # Ctrl-R   - reverse search
    0x1B,  # ESC      - arrows and everything else escape-prefixed
    0x01,  # Ctrl-A
    0x05,  # Ctrl-E
    0x02,  # Ctrl-B
    0x06,  # Ctrl-F
    0x0B,  # Ctrl-K
    0x10,  # Ctrl-P
    0x0E,  # Ctrl-N
}


class DelayLine:
    """A one-way pipe that releases bytes after a fixed delay."""

    def __init__(self, millis):
        self.delay = millis / 1000.0
        self.queue = []

    def push(self, data):
        self.queue.append((time.monotonic() + self.delay, data))

    def drain(self):
        now = time.monotonic()
        out = bytearray()
        while self.queue and self.queue[0][0] <= now:
            out += self.queue.pop(0)[1]
        return bytes(out)

    def next_deadline(self):
        return self.queue[0][0] if self.queue else None


class Session:
    def __init__(self, child_fd, latency, enabled):
        self.fd = child_fd
        self.to_child = DelayLine(latency / 2.0)
        self.to_user = DelayLine(latency / 2.0)

        self.enabled = enabled          # local line editing armed?
        self.alt_screen = False         # child is in a full-screen app
        self.suspended = False          # bailed out for the rest of this line
        self.disarm_next_line = False   # password prompt sniffed

        self.line = bytearray()         # what the user has typed but not sent
        self.pending_echo = bytearray()  # bytes we drew that the remote will re-echo
        self.scan_tail = b""            # carry for sequences split across reads

    # -- helpers ---------------------------------------------------------

    def line_mode(self):
        return (
            self.enabled
            and not self.alt_screen
            and not self.suspended
            and not self.disarm_next_line
        )

    def draw(self, data):
        """Write straight to the user's terminal, bypassing the delay line."""
        os.write(sys.stdout.fileno(), data)

    def notice(self, text):
        self.draw(b"\r\n\x1b[7m " + text + b" \x1b[0m\r\n")

    def send(self, data):
        self.to_child.push(data)

    # -- output from child ----------------------------------------------

    def on_child_output(self, chunk):
        # Track alternate-screen transitions. We scan a small carry buffer
        # prepended to the chunk so a sequence split across two reads is still
        # caught. Setting the flag is idempotent, so overlap is harmless.
        scan = self.scan_tail + chunk
        for seq in ALT_ENTER:
            if seq in scan:
                if not self.alt_screen:
                    self.alt_screen = True
                    # Anything typed but unsent is meant for the shell, not for
                    # the app that just launched. Flush it.
                    self.flush_line()
        for seq in ALT_EXIT:
            if seq in scan:
                self.alt_screen = False
        self.scan_tail = scan[-(MAX_SEQ - 1):] if MAX_SEQ > 1 else b""

        if any(h in scan for h in PW_HINTS):
            self.disarm_next_line = True

        # Swallow the remote's echo of a line we already painted locally.
        # Byte-for-byte match; the first mismatch abandons suppression so a
        # syntax-highlighting shell that repaints the line just wins.
        if self.pending_echo:
            i = 0
            while i < len(chunk) and self.pending_echo:
                if chunk[i] == self.pending_echo[0]:
                    self.pending_echo.pop(0)
                    i += 1
                else:
                    self.pending_echo.clear()
                    break
            chunk = chunk[i:]

        if chunk:
            self.to_user.push(chunk)

    # -- input from user -------------------------------------------------

    def flush_line(self):
        """Send whatever is buffered, and expect the remote to echo it back."""
        if self.line:
            self.send(bytes(self.line))
            self.pending_echo.extend(self.line)
            self.line.clear()

    def on_user_input(self, data):
        for byte in data:
            b = bytes([byte])

            if byte == TOGGLE_KEY:
                self.flush_line()
                self.enabled = not self.enabled
                self.notice(
                    b"local line editing ON" if self.enabled
                    else b"local line editing OFF (raw passthrough)"
                )
                continue

            if not self.line_mode():
                # Raw passthrough. Enter re-arms line mode for the next line.
                if byte in (0x0D, 0x0A):
                    self.suspended = False
                    self.disarm_next_line = False
                self.send(b)
                continue

            if byte in (0x0D, 0x0A):
                # Commit. We do NOT echo a newline -- the remote's own echo of
                # CRLF comes back and lands in the right place.
                self.flush_line()
                self.send(b"\r")
                continue

            if byte in (0x7F, 0x08):
                if self.line:
                    # Pop one character, not one byte: discard any UTF-8
                    # continuation bytes (0b10xxxxxx) first, then the lead
                    # byte, so multibyte input erases cleanly.
                    while len(self.line) > 1 and (self.line[-1] & 0xC0) == 0x80:
                        self.line.pop()
                    self.line.pop()
                    self.draw(b"\b \b")
                continue

            if byte == 0x15:  # Ctrl-U
                if self.line:
                    self.draw(b"\b \b" * len(self.line))
                    self.line.clear()
                continue

            if byte == 0x17:  # Ctrl-W
                n = 0
                while self.line and self.line[-1:] == b" ":
                    self.line.pop()
                    n += 1
                while self.line and self.line[-1:] != b" ":
                    self.line.pop()
                    n += 1
                self.draw(b"\b \b" * n)
                continue

            if byte in (0x03, 0x04):  # Ctrl-C, Ctrl-D
                self.line.clear()
                self.send(b)
                continue

            if byte in BAILOUT_KEYS:
                # We can't service this locally. Hand the remote everything
                # we've buffered and get out of the way until the next line.
                self.flush_line()
                self.suspended = True
                self.send(b)
                continue

            # Ordinary printable byte (or a UTF-8 continuation): buffer it and
            # paint it immediately. This is the entire point of the program.
            self.line.append(byte)
            self.draw(b)


def set_winsize(fd):
    try:
        packed = fcntl.ioctl(sys.stdout.fileno(), termios.TIOCGWINSZ, b"\0" * 8)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, packed)
    except OSError:
        pass


def main():
    ap = argparse.ArgumentParser(
        description="Prototype client-side line editing over a PTY.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="Ctrl-] toggles local line editing while running.",
    )
    ap.add_argument("--ssh", metavar="HOST",
                    help="run `ssh HOST` instead of a local shell")
    ap.add_argument("--latency", type=float, default=None, metavar="MS",
                    help="simulated round-trip latency in ms "
                         "(default: 150 for a local shell, 0 with --ssh)")
    ap.add_argument("--off", action="store_true",
                    help="start with local line editing disabled")
    ap.add_argument("rest", nargs=argparse.REMAINDER,
                    help="extra args passed to ssh")
    args = ap.parse_args()

    if args.latency is None:
        args.latency = 0.0 if args.ssh else 150.0

    if args.ssh:
        argv = ["ssh", "-t", args.ssh] + [a for a in args.rest if a != "--"]
    else:
        argv = [os.environ.get("SHELL", "/bin/bash"), "-i"]

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(argv[0], argv)
        os._exit(1)

    set_winsize(fd)
    signal.signal(signal.SIGWINCH, lambda *_: set_winsize(fd))

    sess = Session(fd, args.latency, enabled=not args.off)

    stdin_fd = sys.stdin.fileno()
    saved = termios.tcgetattr(stdin_fd)
    tty.setraw(stdin_fd)

    banner = (
        b"localecho: %dms simulated RTT, line editing %s. Ctrl-] toggles."
        % (int(args.latency), b"ON" if sess.enabled else b"OFF")
    )
    sess.notice(banner)

    try:
        while True:
            timeout = 0.02
            for dl in (sess.to_child, sess.to_user):
                d = dl.next_deadline()
                if d is not None:
                    timeout = min(timeout, max(0.0, d - time.monotonic()))

            r, _, _ = select.select([stdin_fd, fd], [], [], timeout)

            if stdin_fd in r:
                data = os.read(stdin_fd, 4096)
                if not data:
                    break
                sess.on_user_input(data)

            if fd in r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                sess.on_child_output(data)

            out = sess.to_child.drain()
            if out:
                os.write(fd, out)
            out = sess.to_user.drain()
            if out:
                os.write(sys.stdout.fileno(), out)
    finally:
        termios.tcsetattr(stdin_fd, termios.TCSAFLUSH, saved)
        try:
            os.close(fd)
        except OSError:
            pass
        os.waitpid(pid, 0)
        print()


if __name__ == "__main__":
    main()
