#!/usr/bin/env python3
"""End-to-end tests for CORVX: the real binaries, a real daemon, real pseudo-terminals.

    cargo build --workspace
    pip install pyte            # terminal emulator used to read the screen
    python3 tests/e2e/run_e2e.py

Each run uses its own data directory under your home folder, starts its own daemon,
and removes everything it created when it finishes. Your real CORVX data is not touched.
Agents are the stand-in in fake_claude.py, so no model is called.
"""
import fcntl
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import traceback
from pathlib import Path

try:
    import pyte
except ImportError:
    sys.exit("pyte is missing: run `pip install pyte`")

ROOT = Path(__file__).resolve().parents[2]
CORVX = ROOT / "target" / "debug" / "corvx"
FAKE_CLAUDE = Path(__file__).resolve().with_name("fake_claude.py")


class Harness:
    """Owns one temporary CORVX home, its project, and its daemon."""

    def __init__(self):
        if not CORVX.exists():
            sys.exit("build first: cargo build --workspace")
        # Short path: Unix socket paths are limited in length.
        self.base = Path(tempfile.mkdtemp(prefix=".cxe2e-", dir=os.path.expanduser("~")))
        self.home = self.base / "data"
        self.project = self.base / "proj"
        self.project.mkdir()
        (self.project / "Cargo.toml").write_text("[package]\nname = \"e2e\"\n")
        self.env = {**os.environ, "CORVX_HOME": str(self.home), "TERM": "xterm-256color"}
        self.sessions = []  # every session a scenario starts, stopped after it
        self.corvx("project", "add", str(self.project), "--name", "e2e")

    def corvx(self, *args, check=True):
        p = subprocess.run([str(CORVX), *args], env=self.env, capture_output=True, text=True, timeout=60)
        if check and p.returncode != 0:
            raise AssertionError(f"corvx {' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
        return p.stdout

    def start(self, *args, runtime=None, backend=None, command):
        argv = ["session", "start", "e2e"]
        if runtime:
            argv += ["--runtime", runtime]
        if backend:
            argv += ["--backend", backend]
        argv += ["--", *command]
        out = self.corvx(*argv)
        sid = out.split()[1]  # "started <id> (pid N)"
        self.sessions.append(sid)
        return sid

    def stop_sessions(self):
        for sid in self.sessions:
            self.corvx("session", "stop", sid, check=False)
        self.sessions = []

    def session_line(self, sid):
        return next((l for l in self.corvx("session", "list").splitlines() if l.startswith(sid)), "")

    def attention_text(self):
        return self.corvx("attention")

    def close(self):
        try:
            status = self.corvx("status", check=False).splitlines()
            if status and "pid " in status[0]:
                pid = int(status[0].split("pid ")[1].split()[0])
                os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, ValueError):
            pass
        shutil.rmtree(self.base, ignore_errors=True)


class Term:
    """A program running in a pseudo-terminal, with its screen read by a terminal emulator."""

    def __init__(self, argv, env, cols=110, rows=30):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = b""
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if ready:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                self.raw += data
                self.stream.feed(data)

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def wait_for(self, needle, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.2)
            if needle in self.text():
                return True
        return False

    def send(self, data: bytes):
        os.write(self.fd, data)

    def close(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass


class SkipTest(Exception):
    """Raised by a scenario that cannot run here. It is reported as SKIP, not FAIL."""


def wait_until(check, timeout=10, what="condition"):
    end = time.time() + timeout
    while time.time() < end:
        if check():
            return
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


# ---------- scenarios ----------

def attach_and_detach_pty(h: Harness):
    """Typing reaches the session, and Ctrl+\\ then d detaches without stopping it."""
    for label, prefix in [("usual prefix", b"\x1c"), ("advanced prefix", b"\x1b[92;5u")]:
        sid = h.start(backend="pty", command=["cat"])
        t = Term([str(CORVX), "session", "attach", sid], h.env)
        try:
            assert t.wait_for("attached to", 10), "attach did not start"
            t.send(b"hello\r")
            assert t.wait_for("hello", 10), f"typed text did not reach the session ({label})"
            t.send(prefix)
            assert t.wait_for("press d to detach", 5), f"no visible hint after {label} ({label})"
            t.send(b"d")
            assert t.wait_for("detached from", 5), f"did not detach ({label})"
            t.pump(0.5)
            assert "running" in h.session_line(sid), f"session stopped on detach ({label})"
        finally:
            t.close()
            h.corvx("session", "stop", sid, check=False)


def detach_resets_the_terminal(h: Harness):
    """Pty backend: mouse reporting the session turned on is turned off again on detach."""
    sid = h.start(backend="pty", command=["sh", "-c", 'printf "\\033[?1000h\\033[?1006h"; cat'])
    t = Term([str(CORVX), "session", "attach", sid], h.env)
    try:
        assert t.wait_for("attached to", 10)
        t.pump(0.5)
        t.send(b"\x1c")
        t.send(b"d")
        assert t.wait_for("detached from", 5)
        t.pump(0.5)
        assert b"\x1b[?1000l" in t.raw and b"\x1b[?1006l" in t.raw, "mouse reporting was not switched off"
    finally:
        t.close()
        h.corvx("session", "stop", sid, check=False)


def agent_states_reach_the_queue(h: Harness):
    """Hook events move an agent through working, waiting and blocked, raising the right items."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)  # let the stand-in start and read its settings

    h.corvx("session", "input", sid, "work", "--enter")
    wait_until(lambda: item_for_session(h, sid, "waiting"), 10, "waiting item after a finished turn")

    h.corvx("session", "input", sid, "ask", "--enter")
    wait_until(lambda: item_for_session(h, sid, "blocked"), 10, "blocked item for the permission request")
    assert "Claude needs your permission" in h.attention_text(), h.attention_text()
    assert "blocked" in h.session_line(sid), h.session_line(sid)

    h.corvx("session", "input", sid, "yes", "--enter")
    wait_until(lambda: not item_for_session(h, sid, "blocked"), 10, "blocked item to clear")
    wait_until(lambda: "waiting" in h.session_line(sid), 10, "agent to wait for its next instruction")


def item_id(h: Harness, kind: str, fragment: str):
    """The id of the open attention item of this kind whose summary contains fragment."""
    for line in h.attention_text().splitlines()[1:]:
        parts = line.split(None, 4)
        if len(parts) == 5 and parts[1] == kind and fragment in parts[4]:
            return parts[0]
    return None


def item_for_session(h: Harness, sid: str, kind: str):
    """True if this session currently has an open attention item of this kind."""
    return any(
        len(parts := line.split(None, 4)) == 5 and parts[1] == kind and parts[3] == sid
        for line in h.attention_text().splitlines()[1:]
    )


def failed_exit_raises_an_item(h: Harness):
    sid = h.start(command=["sh", "-c", "echo boom; exit 3"])
    wait_until(lambda: item_id(h, "failed", "exited with code 3") is not None, 10, "failed item")
    h.corvx("attention", "ack", item_id(h, "failed", "exited with code 3"))
    assert item_id(h, "failed", "exited with code 3") is None, "acknowledged item is still open"


def inbox_in_the_terminal_ui(h: Harness):
    """The TUI shows the count, the inbox lists the item, and acknowledging clears it."""
    sid = h.start(command=["sh", "-c", "exit 4"])
    wait_until(lambda: "exited with code 4" in h.attention_text(), 10, "failed item for the inbox")
    t = Term([str(CORVX)], h.env)
    try:
        assert t.wait_for("1 failed", 10), "top bar does not count the failed agent"
        t.send(b"i")
        assert t.wait_for("exited with code 4", 5), "inbox does not list the item"
        t.send(b"a")
        assert t.wait_for("Nothing needs you", 5), "acknowledging did not clear the inbox"
        t.send(b"q")
        t.pump(0.5)
    finally:
        t.close()


def real_claude_attention(h: Harness):
    """Real Claude: a permission request is blocked in the queue, approving it finishes the turn.

    Uses your Claude account for one short prompt. Opt in with CORVX_E2E_REAL_CLAUDE=1.
    """
    if os.environ.get("CORVX_E2E_REAL_CLAUDE") != "1":
        raise SkipTest("set CORVX_E2E_REAL_CLAUDE=1 to run against real Claude (uses your account)")
    # Manual permission mode, so the request below always asks, whatever your own defaults are.
    sid = h.start(runtime="claude", command=["claude", "--permission-mode", "manual"])
    t = Term([str(CORVX), "session", "attach", sid], h.env)
    try:
        assert t.wait_for("attached to", 15), "attach did not start"
        # First run in a new folder: Claude asks whether to trust it. "Yes" is the second option.
        if t.wait_for("trust this folder", 60):
            # Let the question finish drawing, then move to "Yes" and confirm the highlight
            # before pressing Enter. Enter on the default choice exits Claude.
            t.pump(3.0)
            for _ in range(3):
                t.send(b"\x1b[B")
                t.pump(1.0)
                if "❯ Yes, I trust this folder" in t.text():
                    break
            assert "❯ Yes, I trust this folder" in t.text(), "could not select 'Yes' on the trust question:\n" + t.text()
            t.send(b"\r")
        ready = t.wait_for("❯", 120)  # the input line; Claude is ready for a prompt
        assert ready, "Claude did not reach its prompt. Screen was:\n" + t.text()
        t.pump(2.0)

        t.send(b"create a file named corvx_probe.txt containing the word ok\r")
        assert t.wait_for("Do you want to", 180), "Claude did not ask permission. Screen was:\n" + t.text()
        wait_until(lambda: item_for_session(h, sid, "blocked"), 30, "blocked item for the permission request")
        assert "Claude needs your permission" in h.attention_text(), h.attention_text()

        t.send(b"\r")  # approve the default option
        wait_until(lambda: not item_for_session(h, sid, "blocked"), 60, "blocked item to clear after approval")
        wait_until(lambda: item_for_session(h, sid, "waiting"), 240, "waiting item once the turn finishes")
        assert (h.project / "corvx_probe.txt").exists(), "the approved action did not happen"

        t.send(b"\x1c")
        t.send(b"d")
        assert t.wait_for("detached from", 10), "did not detach"
    except Exception as e:
        raise AssertionError(
            f"{e}\nsession: {h.session_line(sid)}\nlast screen:\n{t.text()}"
        ) from e
    finally:
        t.close()
        h.corvx("session", "stop", sid, check=False)


def tmux_available():
    return shutil.which("tmux") is not None


def daemon_pid(h: Harness):
    first = h.corvx("status").splitlines()[0]
    return int(first.split("pid ")[1].split()[0])


def attach_and_detach_tmux(h: Harness):
    """tmux backend: typing reaches the session, the prefix shows its hint, d detaches."""
    if not tmux_available():
        raise SkipTest("tmux is not installed")
    sid = h.start(backend="tmux", command=["cat"])
    t = Term([str(CORVX), "session", "attach", sid], h.env)
    try:
        assert t.wait_for("then d to detach", 10), "status bar hint missing:\n" + t.text()
        t.send(b"hello\r")
        assert t.wait_for("hello", 10), "typed text did not reach the session"
        t.send(b"\x1c")
        assert t.wait_for("press d to detach", 5), "no visible hint after the prefix:\n" + t.text()
        t.send(b"d")
        assert t.wait_for("detached from", 10), "did not detach:\n" + t.text()
        assert "running" in h.session_line(sid), "session stopped on detach"
    finally:
        t.close()


def sessions_survive_a_daemon_restart(h: Harness):
    """tmux backend: a daemon restart keeps the agent running, its item, and its hooks working."""
    if not tmux_available():
        raise SkipTest("tmux is not installed")
    sid = h.start(runtime="claude", backend="tmux", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    h.corvx("session", "input", sid, "work", "--enter")
    wait_until(lambda: item_for_session(h, sid, "waiting"), 10, "waiting item before the restart")

    old = daemon_pid(h)
    os.kill(old, signal.SIGTERM)
    wait_until(lambda: subprocess.run(["kill", "-0", str(old)], capture_output=True).returncode != 0, 10, "old daemon to exit")
    assert daemon_pid(h) != old, "a new daemon should have started"

    assert "running" in h.session_line(sid), "session did not survive: " + h.session_line(sid)
    assert item_for_session(h, sid, "waiting"), "attention item was lost in the restart"
    # Hooks from the surviving agent reach the new daemon.
    h.corvx("session", "input", sid, "ask", "--enter")
    wait_until(lambda: item_for_session(h, sid, "blocked"), 10, "blocked item after the restart")


def attach_from_the_terminal_ui(h: Harness):
    """The sessions view lists sessions; Enter attaches; detaching returns to the TUI."""
    sid = h.start(backend="pty", command=["cat"])
    t = Term([str(CORVX)], h.env)
    try:
        assert t.wait_for("projects", 10), "TUI did not start"
        t.send(b"s")
        assert t.wait_for(sid, 5), "sessions view does not list the session:\n" + t.text()
        t.send(b"\r")
        assert t.wait_for("attached to", 10), "Enter did not attach:\n" + t.text()
        t.send(b"hello-from-tui\r")
        assert t.wait_for("hello-from-tui", 10), "typed text did not reach the session"
        t.send(b"\x1c")
        t.send(b"d")
        assert t.wait_for("sessions ·", 10), "the TUI did not come back after detaching:\n" + t.text()
        assert "running" in h.session_line(sid), "session stopped on detach"
        t.send(b"\x1b")
        t.send(b"q")
        t.pump(0.5)
    finally:
        t.close()


SCENARIOS = [
    attach_and_detach_pty,
    attach_from_the_terminal_ui,
    attach_and_detach_tmux,
    sessions_survive_a_daemon_restart,
    detach_resets_the_terminal,
    agent_states_reach_the_queue,
    failed_exit_raises_an_item,
    inbox_in_the_terminal_ui,
    real_claude_attention,
]


def main():
    wanted = set(sys.argv[1:])
    chosen = [sc for sc in SCENARIOS if not wanted or sc.__name__ in wanted]
    results = []
    h = Harness()
    try:
        for scenario in chosen:
            name = scenario.__name__.replace("_", " ")
            try:
                scenario(h)
                results.append((name, None))
                print(f"PASS  {name}")
            except SkipTest as e:
                print(f"SKIP  {name}\n      {e}")
            except Exception as e:  # report and keep going
                results.append((name, e))
                print(f"FAIL  {name}\n      {e}")
                if os.environ.get("E2E_VERBOSE"):
                    traceback.print_exc()
            finally:
                h.stop_sessions()
    finally:
        h.close()
    failed = [n for n, e in results if e is not None]
    print(f"\n{len(results) - len(failed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
