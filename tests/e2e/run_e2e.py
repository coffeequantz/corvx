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
import json
import os
import pty
import re
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


class _Screen(pyte.Screen):
    """pyte's screen, tolerant of the private status queries tmux sends (e.g. CSI ? 6 n)."""

    def report_device_status(self, mode, private=False, **kwargs):
        if not private:
            super().report_device_status(mode)


class Term:
    """A program running in a pseudo-terminal, with its screen read by a terminal emulator."""

    def __init__(self, argv, env, cols=110, rows=30):
        self.screen = _Screen(cols, rows)
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
        except ProcessLookupError:
            pass
        # On macOS a killed process waits for its terminal output to be read before it exits:
        # closing our end discards it.
        try:
            os.close(self.fd)
        except OSError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass

    def key(self, *keys: bytes):
        """Sends keys one at a time, so Esc followed by a key is not read as Alt+key."""
        for k in keys:
            self.send(k)
            self.pump(0.3)


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
    """Real Claude: a permission request becomes an approval item; approving it from the inbox
    (not in the session) lets the action happen and finishes the turn.

    Uses your Claude account for one short prompt. Opt in with CORVX_E2E_REAL_CLAUDE=1.
    """
    if os.environ.get("CORVX_E2E_REAL_CLAUDE") != "1":
        raise SkipTest("set CORVX_E2E_REAL_CLAUDE=1 to run against real Claude (uses your account)")
    # Manual permission mode, so the request below always asks, whatever your own defaults are.
    sid = h.start(runtime="claude", command=["claude", "--permission-mode", "manual", "--model", "haiku"])
    t = Term([str(CORVX), "session", "attach", sid], h.env)
    try:
        # Both backends show the detach hint: the native attach prints it, tmux's status bar shows it.
        assert t.wait_for("then d to detach", 15), "attach did not start"
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
        wait_until(lambda: item_for_session(h, sid, "approval"), 30, "approval item for the permission request")
        t.pump(8.0)  # past Claude's own reminder notification, which must not replace the item
        assert item_for_session(h, sid, "approval"), h.attention_text()
        approval = next(l.split()[0] for l in h.attention_text().splitlines() if " approval " in l)
        h.corvx("attention", "approve", approval)  # from the inbox, not the session
        wait_until(lambda: not item_for_session(h, sid, "approval"), 60, "approval item to close")
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


def claude_ready(t: "Term"):
    """Waits for real Claude's input line, answering the first-run folder trust question."""
    assert t.wait_for("then d to detach", 15), "attach did not start"
    if t.wait_for("trust this folder", 60):
        t.pump(3.0)
        for _ in range(3):
            t.send(b"\x1b[B")
            t.pump(1.0)
            if "❯ Yes, I trust this folder" in t.text():
                break
        assert "❯ Yes, I trust this folder" in t.text(), "could not select 'Yes':\n" + t.text()
        t.send(b"\r")
    assert t.wait_for("❯", 120), "Claude did not reach its prompt. Screen was:\n" + t.text()
    t.pump(2.0)


def events_of(h: Harness, kind: str):
    import sqlite3
    con = sqlite3.connect(str(h.home / "corvx.db"))
    try:
        return [json.loads(p) for (p,) in con.execute(
            "SELECT payload FROM events WHERE kind = ? ORDER BY seq", (kind,))]
    finally:
        con.close()


def real_claude_rules(h: Harness):
    """Real Claude: CORVX rules ask (even when Claude's settings allow the call), allow (no
    prompt where Claude would ask) and deny.

    Uses your Claude account for three short prompts. Opt in with CORVX_E2E_REAL_CLAUDE=1.
    """
    if os.environ.get("CORVX_E2E_REAL_CLAUDE") != "1":
        raise SkipTest("set CORVX_E2E_REAL_CLAUDE=1 to run against real Claude (uses your account)")
    # Claude's own settings allow echo, so without CORVX it would not ask.
    settings = h.project / ".claude" / "settings.local.json"
    settings.parent.mkdir(exist_ok=True)
    settings.write_text(json.dumps({"permissions": {"allow": ["Bash(echo:*)"]}}))
    rule = lambda *a: re.search(r"added rule (\S+)", h.corvx("rules", "add", *a, "--project", "e2e")).group(1)
    ask = rule("--command", r"\becho\s+corvx-probe\b", "--reason", "probe: ask")
    allow = rule("--command", r"\btouch\s+corvx_allowed\.txt\b", "--reason", "probe: allow",
                 "--action", "allow")
    deny = rule("--command", r"\bdate\b", "--reason", "probe: deny", "--action", "deny")

    sid = h.start(runtime="claude", command=["claude", "--permission-mode", "default", "--model", "haiku"])
    t = Term([str(CORVX), "session", "attach", sid], h.env)
    try:
        claude_ready(t)
        turns = lambda: len([i for i in events_of(h, "session.status")
                             if i.get("id") == sid and i.get("agent_state") == "waiting"])

        # ask: the rule sends it to the inbox although Claude's settings allow it.
        t.send(b"Use the Bash tool to run exactly: echo corvx-probe\r")
        wait_until(lambda: item_id(h, "approval", f"CORVX rule {ask}: probe: ask") is not None, 180,
                   "the ask rule to raise an approval. Screen:\n" + t.text())
        h.corvx("attention", "approve", item_id(h, "approval", "corvx-probe"))
        wait_until(lambda: item_id(h, "approval", "corvx-probe") is None, 60, "the approval to close")
        wait_until(lambda: turns() >= 1, 240, "the first turn to finish")

        # allow: runs without a prompt, where Claude would otherwise ask.
        t.send(b"Use the Bash tool to run exactly: touch corvx_allowed.txt\r")
        wait_until(lambda: (h.project / "corvx_allowed.txt").exists(), 180,
                   "the allow rule to let it run. Screen:\n" + t.text())
        assert not item_for_session(h, sid, "approval"), h.attention_text()
        wait_until(lambda: turns() >= 2, 240, "the second turn to finish")

        # deny: refused with the rule's reason, and no one is asked.
        t.send(b"Use the Bash tool to run exactly: date\r")
        wait_until(lambda: any(e.get("rule") == deny for e in events_of(h, "attention.auto_denied")),
                   180, "the deny rule. Screen:\n" + t.text())
        assert not item_for_session(h, sid, "approval"), h.attention_text()
        wait_until(lambda: turns() >= 3, 240, "the third turn to finish")
        assert any(e.get("rule") == allow for e in events_of(h, "attention.auto_approved"))
        t.send(b"\x1c")
        t.send(b"d")
    except Exception as e:
        raise AssertionError(f"{e}\nsession: {h.session_line(sid)}\nlast screen:\n{t.text()}") from e
    finally:
        t.close()
        h.corvx("session", "stop", sid, check=False)
        settings.unlink()
        for r in (ask, allow, deny):
            h.corvx("rules", "remove", r, check=False)


def output_of(h: Harness, sid: str):
    return h.corvx("session", "output", sid)


def approvals_from_the_inbox(h: Harness):
    """A permission request becomes an approval item; approving or denying it reaches the agent."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    h.corvx("session", "input", sid, "request", "--enter")
    wait_until(lambda: item_id(h, "approval", "Bash: make test") is not None, 10, "approval item")
    time.sleep(1.0)
    # Claude's own reminder does not add a second item.
    assert not item_for_session(h, sid, "blocked"), h.attention_text()
    h.corvx("attention", "approve", item_id(h, "approval", "Bash: make test"))
    wait_until(lambda: "allowed by hook" in output_of(h, sid), 10, "the agent to get the approval")
    wait_until(lambda: item_for_session(h, sid, "waiting"), 10, "waiting item after the turn")

    h.corvx("session", "input", sid, "request", "--enter")
    wait_until(lambda: item_id(h, "approval", "Bash: make test") is not None, 10, "second approval item")
    h.corvx("attention", "deny", item_id(h, "approval", "Bash: make test"), "use", "the", "Makefile")
    wait_until(lambda: "denied by hook: use the Makefile" in output_of(h, sid), 10, "the denial and its reason")


def answering_in_the_session_clears_the_approval(h: Harness):
    """The dialog stays usable: answering in the session closes the item and ends the wait."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    h.corvx("session", "input", sid, "request", "--enter")
    wait_until(lambda: item_id(h, "approval", "make test") is not None, 10, "approval item")
    h.corvx("session", "input", sid, "yes", "--enter")
    wait_until(lambda: "permission granted in session" in output_of(h, sid), 10, "the in-session answer")
    wait_until(lambda: item_id(h, "approval", "make test") is None, 10, "the approval item to close")
    wait_until(lambda: "hook returned after the dialog was answered" in output_of(h, sid), 10,
               "the waiting hook to return")


def approve_in_the_terminal_ui(h: Harness):
    """In the inbox, y approves; Y approves and passes Claude's own 'don't ask again' rules."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    t = Term([str(CORVX)], h.env)
    try:
        for key, expected in ((b"y", "allowed by hook\n"), (b"Y", "allowed by hook with 1 rule(s)")):
            h.corvx("session", "input", sid, "request", "--enter")
            wait_until(lambda: item_id(h, "approval", "make test") is not None, 10, "approval item")
            assert t.wait_for("1 to approve", 10), "top bar does not count the approval:\n" + t.text()
            # The request is the most urgent item: the top of the inbox.
            t.key(b"i", b"k", b"k", b"k")
            assert t.wait_for("asks to run", 5), "inbox does not show the request:\n" + t.text()
            assert "y approve" in t.text(), t.text()
            t.send(key)
            wait_until(lambda: expected in output_of(h, sid).replace("\r", ""), 10, f"{key!r} to reach the agent")
            t.send(b"\x1b")
            wait_until(lambda: item_id(h, "approval", "make test") is None, 10, "item to close")
        t.send(b"q")
        t.pump(0.5)
    finally:
        t.close()


def resume_after_a_restart(h: Harness):
    """tmux vanishing (a reboot) leaves the agent lost; resuming continues its conversation."""
    if not tmux_available():
        raise SkipTest("tmux is not installed")
    sid = h.start(runtime="claude", backend="tmux", command=[str(FAKE_CLAUDE), "--model", "haiku", "work"])
    wait_until(lambda: item_for_session(h, sid, "waiting"), 10, "the first turn")
    # What a reboot does to the agents: their tmux server is gone.
    subprocess.run(["tmux", "-S", str(h.home / "tmux.sock"), "kill-server"], capture_output=True)
    wait_until(lambda: " lost " in h.session_line(sid), 10, "the session to be marked lost")
    assert not item_for_session(h, sid, "waiting"), "a lost session's items are cleared"

    t = Term([str(CORVX)], h.env)
    try:
        assert t.wait_for("interrupted session", 10), "the TUI does not mention the lost session:\n" + t.text()
        t.send(b"s")
        assert t.wait_for("R resume", 5), t.text()
        for _ in range(40):  # ended sessions are listed last, in start order: ours is the last
            t.send(b"j")
        t.pump(0.5)
        selected = next((l for l in t.text().splitlines() if f"▸ " in l.split("││")[-1]), "")
        assert sid in selected and "lost" in selected, "the lost session is not selected:\n" + t.text()
        t.send(b"R")
        assert t.wait_for("resumed", 10), "no confirmation:\n" + t.text()
        t.send(b"q")
        t.pump(0.5)
    finally:
        t.close()
    wait_until(lambda: "running" in h.session_line(sid), 10, "the session to run again")
    wait_until(lambda: "resumed conversation fake-claude" in output_of(h, sid), 10, "--resume with the conversation")
    h.corvx("session", "input", sid, "work", "--enter")
    wait_until(lambda: item_for_session(h, sid, "waiting"), 10, "hooks to work after resuming")


def tmux_available():
    return shutil.which("tmux") is not None


def daemon_pid(h: Harness):
    h.corvx("project", "list")  # starts the daemon if needed; status does not
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


class Mcp:
    """`corvx mcp` over stdio, driven the way Claude Code drives an MCP server."""

    def __init__(self, env):
        self.proc = subprocess.Popen([str(CORVX), "mcp"], env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True)
        self.next_id = 1

    def request(self, method, params=None):
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}
        self.next_id += 1
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        reply = json.loads(self.proc.stdout.readline())
        if "error" in reply:
            raise AssertionError(f"{method}: {reply['error']}")
        return reply["result"]

    def notify(self, method):
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")
        self.proc.stdin.flush()

    def tool(self, name, args=None):
        result = self.request("tools/call", {"name": name, "arguments": args or {}})
        text = result["content"][0]["text"]
        if result.get("isError"):
            raise AssertionError(f"{name} failed: {text}")
        return json.loads(text)

    def wait_event(self, matches, what, attempts=4):
        """Calls wait_for until an event satisfies `matches`."""
        for _ in range(attempts):
            for ev in self.tool("wait_for", {"timeout_seconds": 20})["events"]:
                if matches(ev):
                    return ev
        raise AssertionError(f"wait_for never reported {what}")

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)


def orchestration_through_mcp(h: Harness):
    """An orchestrator's MCP tools: delegate into a worktree, wait, route attention, ask the human."""
    git = lambda *a: subprocess.run(["git", "-C", str(h.project), "-c", "user.name=e2e",
                                     "-c", "user.email=e2e@e2e", *a], check=True, capture_output=True)
    if not (h.project / ".git").exists():
        git("init", "-q")
        git("add", ".")
        git("commit", "-q", "-m", "init")

    orch = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])  # stands in for the orchestrator
    env = {**h.env, "CORVX_SESSION_ID": orch, "CORVX_AGENT_COMMAND": str(FAKE_CLAUDE)}
    m = Mcp(env)
    try:
        init = m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                        "clientInfo": {"name": "e2e", "version": "0"}})
        assert init["serverInfo"]["name"] == "corvx"
        m.notify("notifications/initialized")
        names = {t["name"] for t in m.request("tools/list")["tools"]}
        expected = {"list_projects", "start_agent", "list_agents", "read_screen", "send_message",
                    "wait_for", "stop_agent", "ask_human", "report_done"}
        assert expected <= names, names

        started = m.tool("start_agent", {"project": "e2e", "prompt": "work"})
        wid = started["id"]
        h.sessions.append(wid)
        assert started["worktree"] and Path(started["worktree"]).is_dir(), started

        ev = m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["id"] == wid, "the worker's first turn")
        assert ev["agent"]["state"] == "waiting", ev
        assert not item_for_session(h, wid, "waiting"), "a supervised agent's waiting must not reach the inbox"

        m.tool("send_message", {"id": wid, "text": "ask"})
        ev = m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["state"] == "blocked", "the permission request")
        assert item_for_session(h, wid, "blocked"), "permission requests must still reach the human"

        m.tool("send_message", {"id": wid, "text": "yes"})
        m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["state"] == "waiting", "the worker to finish")

        qid = m.tool("ask_human", {"question": "Which accent colour?"})["question_id"]
        assert item_id(h, "question", "Which accent colour?") == qid
        h.corvx("attention", "answer", qid, "electric", "blue")
        ev = m.wait_event(lambda e: e["type"] == "answer", "the human's answer")
        assert ev["answer"] == "electric blue", ev

        screen = m.tool("read_screen", {"id": wid})["screen"]
        assert "permission granted" in screen, screen
        assert any(a["id"] == wid for a in m.tool("list_agents"))

        m.tool("report_done", {"summary": "worker finished on its branch"})
        assert item_for_session(h, orch, "done"), "the orchestrator's report should reach the human"
        m.tool("stop_agent", {"id": wid})
    finally:
        m.close()


def orchestrator_decides_approvals(h: Harness):
    """An orchestrator decides its agents' permission requests, escalates one, and uses autopilot."""
    git = lambda *a: subprocess.run(["git", "-C", str(h.project), "-c", "user.name=e2e",
                                     "-c", "user.email=e2e@e2e", *a], check=True, capture_output=True)
    if not (h.project / ".git").exists():
        git("init", "-q")
        git("add", ".")
        git("commit", "-q", "-m", "init")

    orch = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    env = {**h.env, "CORVX_SESSION_ID": orch, "CORVX_AGENT_COMMAND": str(FAKE_CLAUDE)}
    m = Mcp(env)
    try:
        m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        m.notify("notifications/initialized")
        names = {t["name"] for t in m.request("tools/list")["tools"]}
        assert {"decide_approval", "set_autopilot"} <= names, names

        started = m.tool("start_agent", {"project": "e2e", "name": "worker", "prompt": "work"})
        wid = started["id"]
        h.sessions.append(wid)
        assert started["model"] == "sonnet", started  # cheaper model by default
        m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["id"] == wid, "the worker's first turn")

        # 1. The orchestrator approves.
        m.tool("send_message", {"id": wid, "text": "request"})
        ev = m.wait_event(lambda e: e["type"] == "approval", "the permission request")
        assert ev["decider"] == "you" and ev["request"].startswith("Bash: make test"), ev
        assert ev["agent"]["model"] == "sonnet", ev
        assert "(orchestrator deciding) Bash: make test" in h.attention_text(), h.attention_text()
        m.tool("decide_approval", {"id": wid, "decision": "allow"})
        wait_until(lambda: "allowed by hook" in output_of(h, wid), 10, "the orchestrator's approval")
        m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["state"] == "waiting", "the turn to end")

        # 2. It escalates; the human decides.
        m.tool("send_message", {"id": wid, "text": "request"})
        m.wait_event(lambda e: e["type"] == "approval", "the second request")
        m.tool("decide_approval", {"id": wid, "decision": "escalate", "message": "not sure about this one"})
        wait_until(lambda: item_id(h, "approval", "orchestrator: not sure about this one") is not None,
                   10, "the escalated item")
        h.corvx("attention", "deny", item_id(h, "approval", "not sure about this one"), "not", "now")
        wait_until(lambda: "denied by hook: not now" in output_of(h, wid), 10, "the human's denial")

        # 2b. Escalated again; this time the human approves it in the terminal UI.
        m.wait_event(lambda e: e["type"] == "agent" and e["agent"]["state"] == "waiting", "the turn to end")
        before = output_of(h, wid).count("allowed by hook")
        m.tool("send_message", {"id": wid, "text": "request"})
        m.wait_event(lambda e: e["type"] == "approval", "the third request")
        t = Term([str(CORVX)], h.env)
        try:
            assert t.wait_for("(1 with orchestrators)", 10), "top bar:\n" + t.text()
            m.tool("decide_approval", {"id": wid, "decision": "escalate", "message": "you decide"})
            assert t.wait_for("1 to approve", 10), "the escalation does not reach the top bar:\n" + t.text()
            t.send(b"i")
            assert t.wait_for("orchestrator: you decide", 5), "inbox:\n" + t.text()
            assert "y approve · Y always · n deny" in t.text(), "no keys in the detail box:\n" + t.text()
            t.send(b"y")
            wait_until(lambda: output_of(h, wid).count("allowed by hook") > before, 10,
                       "the approval from the terminal UI:\n" + t.text())
            t.key(b"\x1b", b"q")
            t.pump(0.5)
        finally:
            t.close()

        # 3. Autopilot: no one is asked.
        m.tool("set_autopilot", {"id": wid, "on": True})
        before = output_of(h, wid).count("allowed by hook")
        m.tool("send_message", {"id": wid, "text": "request"})
        wait_until(lambda: output_of(h, wid).count("allowed by hook") > before, 10, "autopilot's approval")
        assert item_id(h, "approval", "Bash: make test") is None, h.attention_text()
        assert "[autopilot]" in h.corvx("session", "list")
        m.tool("stop_agent", {"id": wid})
    finally:
        m.close()


def rules_reach_the_human(h: Harness):
    """CORVX's rules ask the human, allow or deny, whoever would decide; set per project."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    h.corvx("session", "autopilot", sid, "on")
    out = lambda: output_of(h, sid).replace("\r", "")
    allowed = lambda: out().count("allowed by hook")
    request = lambda cmd: h.corvx("session", "input", sid, f"request {cmd}", "--enter")

    # A built-in rule asks you: autopilot does not decide a push.
    request("git push origin main")
    wait_until(lambda: "pre-tool: ask (CORVX rule git-push" in out(), 10,
               "the PreToolUse hook to make Claude ask")
    wait_until(lambda: item_id(h, "approval", "CORVX rule git-push: pushes to a remote") is not None,
               10, "the push to reach the inbox:\n" + h.attention_text())
    assert allowed() == 0, out()
    h.corvx("attention", "approve", item_id(h, "approval", "git push"))
    wait_until(lambda: allowed() == 1, 10, "the human's approval")

    # Off for this project: autopilot approves it.
    h.corvx("rules", "set", "git-push", "off", "--project", "e2e")
    assert re.search(r"git-push\s+off\s+this project", h.corvx("rules", "--project", "e2e"))
    request("git push origin main")
    wait_until(lambda: allowed() == 2, 10, "autopilot to approve once the rule is off")

    # Allow and deny decide at once, with autopilot off, before Claude Code would ask.
    h.corvx("session", "autopilot", sid, "off")
    h.corvx("rules", "set", "git-push", "allow", "--project", "e2e")
    request("git push origin main")
    wait_until(lambda: "allowed before asking" in out(), 10, "the allow rule")
    h.corvx("rules", "set", "sudo", "deny")  # everywhere
    request("sudo make install")
    wait_until(lambda: "denied before asking: CORVX rule sudo: runs as root" in out(), 10,
               "the deny rule")
    assert re.search(r"sudo\s+deny\s+everywhere", h.corvx("rules", "--project", "e2e"))
    h.corvx("rules", "set", "sudo", "default")
    h.corvx("rules", "set", "git-push", "default", "--project", "e2e")
    assert re.search(r"git-push\s+ask", h.corvx("rules", "--project", "e2e"))

    # A rule of your own.
    added = h.corvx("rules", "add", "--command", r"\bterraform\s+apply\b",
                    "--reason", "changes infrastructure", "--project", "e2e")
    rule = re.search(r"added rule (\S+)", added).group(1)
    request("terraform apply -auto-approve")
    wait_until(lambda: item_id(h, "approval", f"CORVX rule {rule}: changes infrastructure") is not None,
               10, "your rule to send it to you:\n" + h.attention_text())
    h.corvx("attention", "deny", item_id(h, "approval", "terraform"), "not", "today")
    wait_until(lambda: "denied by hook: not today" in out(), 10, "the denial")

    # In the terminal UI: L on such a request allows its rule in the project and approves it;
    # the rules view changes rules too.
    t = Term([str(CORVX)], h.env)
    try:
        before = allowed()
        request("terraform apply")
        wait_until(lambda: item_id(h, "approval", "terraform") is not None, 10, "the request")
        t.send(b"i")
        assert t.wait_for(f"L allow {rule} in e2e from now on", 5), "inbox:\n" + t.text()
        t.send(b"L")
        wait_until(lambda: allowed() > before, 10, "L to approve it:\n" + t.text())
        assert re.search(rf"{rule}\s+allow\s+this project", h.corvx("rules", "--project", "e2e"))
        t.key(b"\x1b", b"l")
        assert t.wait_for("rules for e2e", 5), "rules view:\n" + t.text()
        t.send(b"n")  # the first rule, git-push: deny in this project
        wait_until(lambda: re.search(r"git-push\s+deny\s+this project",
                                     h.corvx("rules", "--project", "e2e")), 5, "n in the rules view")
        t.send(b"d")
        wait_until(lambda: re.search(r"git-push\s+ask", h.corvx("rules", "--project", "e2e")), 5,
                   "d in the rules view")
        t.key(b"\x1b", b"q")
        t.pump(0.5)
    finally:
        t.close()
    h.corvx("rules", "remove", rule)
    assert rule not in h.corvx("rules")


def autonomy_trust_and_journal(h: Harness):
    """The autonomy dial, suggestions from repeated approvals, and the mission journal."""
    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    time.sleep(1.0)
    out = lambda: output_of(h, sid).replace("\r", "")
    allowed = lambda: out().count("allowed by hook")
    request = lambda cmd: h.corvx("session", "input", sid, f"request {cmd}", "--enter")

    # Autonomous: approved without asking anyone; a push still asks.
    assert "guided" in h.corvx("project", "autonomy", "e2e")
    assert "autonomous" in h.corvx("project", "autonomy", "e2e", "autonomous")
    assert re.search(r"e2e\s+\S+\s+autonomous", h.corvx("project", "list"))
    request("cargo fmt --check")
    wait_until(lambda: allowed() == 1, 10, "the autonomous level to approve")
    assert item_id(h, "approval", "cargo fmt") is None, h.attention_text()
    request("git push")
    wait_until(lambda: item_id(h, "approval", "CORVX rule git-push") is not None, 10, "the push to ask")
    h.corvx("attention", "deny", item_id(h, "approval", "git push"), "not", "now")
    wait_until(lambda: "denied by hook: not now" in out(), 10, "the denial")

    # Guided again: you approve the same command three times; CORVX offers to allow it.
    h.corvx("project", "autonomy", "e2e", "guided")
    for n in range(3):
        request("cargo fmt --check")
        wait_until(lambda: item_id(h, "approval", "cargo fmt") is not None, 10, "the request")
        h.corvx("attention", "approve", item_id(h, "approval", "cargo fmt"))
        wait_until(lambda: allowed() == 2 + n, 10, "the approval")
    wait_until(lambda: item_id(h, "suggestion", "`cargo fmt …` 3 times in e2e") is not None, 10,
               "a suggestion:\n" + h.attention_text())

    # Accepting it in the terminal UI; the dial moves there too.
    t = Term([str(CORVX)], h.env)
    try:
        t.send(b"i")
        assert t.wait_for("y allow it without asking from now on", 5), "inbox:\n" + t.text()
        t.send(b"y")
        wait_until(lambda: item_id(h, "suggestion", "cargo fmt") is None, 10, "the suggestion to close")
        t.key(b"\x1b", b"l")
        assert t.wait_for("- / + to move", 5), "rules view:\n" + t.text()
        t.send(b"-")
        wait_until(lambda: "supervised" in h.corvx("project", "autonomy", "e2e"), 5, "- to lower the level")
        t.send(b"+")
        t.pump(1.0)
        wait_until(lambda: "guided" in h.corvx("project", "autonomy", "e2e"), 5, "+ to raise it:\n" + t.text())
        t.key(b"\x1b", b"q")
    finally:
        t.close()
    request("cargo fmt --check")
    wait_until(lambda: "allowed before asking" in out(), 10, "the accepted suggestion to allow it")
    assert re.search(r"you approved `cargo fmt …` before", h.corvx("rules", "--project", "e2e"))

    # The journal: an orchestrator's entries and its report go to .corvx/journal/.
    env = {**h.env, "CORVX_SESSION_ID": sid}
    m = Mcp(env)
    try:
        m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        m.notify("notifications/initialized")
        m.tool("journal", {"title": "Mission", "text": "Goal: format check.\nLearned: run cargo fmt first."})
        done = m.tool("report_done", {"summary": "Formatting is clean."})
        assert done["journal"], done
    finally:
        m.close()
    entries = list((h.project / ".corvx" / "journal").glob(f"*-{sid}.md"))
    assert len(entries) == 1, entries
    text = entries[0].read_text()
    assert "## Mission · " in text and "Learned: run cargo fmt first." in text, text
    assert "## Done · " in text and "Formatting is clean." in text, text


def the_knowledge_loop(h: Harness):
    """Knowledge files, proposals reviewed in the inbox, and what later agents read."""
    git = lambda *a: subprocess.run(["git", "-C", str(h.project), *a], check=True,
                                    capture_output=True, text=True).stdout
    if not (h.project / ".git").exists():
        git("init", "-q")
    git("config", "user.name", "e2e")
    git("config", "user.email", "e2e@e2e")
    git("add", "-A")
    git("commit", "-q", "--allow-empty", "-m", "before knowledge")

    out = h.corvx("knowledge", "init", "e2e")
    assert "wrote .corvx/decisions.md" in out and "committed" in out, out
    assert "@.corvx/conventions.md" in (h.project / "CLAUDE.md").read_text()
    assert "agents read it:          yes" in h.corvx("knowledge", "e2e")

    sid = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    m = Mcp({**h.env, "CORVX_SESSION_ID": sid})
    try:
        m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        m.notify("notifications/initialized")
        k = m.tool("read_knowledge")
        paths = [f["path"] for f in k["files"]]
        assert paths == [".corvx/brief.md", ".corvx/goals.md", ".corvx/decisions.md",
                         ".corvx/conventions.md", ".corvx/map.md"], paths
        assert "- test: `cargo test`" in k["files"][4]["text"], k["files"][4]

        propose = lambda file, title, text: m.tool("propose_knowledge", {
            "file": file, "title": title, "text": text, "evidence": f"mission {sid}"})
        fmt = propose("conventions", "Run cargo fmt first", "Run `cargo fmt` before committing.")
        redis = propose("decisions", "Use Redis", "Cache in Redis.")
        edit = propose("conventions", "Tests name the behaviour", "Name tests after what they show.")
        # The agent moving on does not close its proposals.
        h.corvx("session", "input", sid, "work", "--enter")
        time.sleep(1.5)
        inbox = h.attention_text()
        for p in (fmt, redis, edit):
            assert p["proposed"] in inbox, inbox

        # Rejected with a reason: it is not proposed again, and agents see why.
        h.corvx("knowledge", "reject", redis["proposed"], "no", "external", "services")
        try:
            propose("decisions", "use redis", "Again.")
            raise AssertionError("a rejected proposal was accepted again")
        except AssertionError as e:
            assert "no external services" in str(e), e

        # Reviewed in the terminal UI: y accepts one, e edits the other first.
        for line in h.attention_text().splitlines()[1:]:
            item = line.split()[0]
            if item not in (fmt["proposed"], edit["proposed"]):
                h.corvx("attention", "dismiss", item, check=False)
        env = {**h.env, "EDITOR": "printf 'Name each test after the behaviour it shows.\\n' >"}
        t = Term([str(CORVX)], env)
        try:
            t.send(b"i")
            assert t.wait_for("proposes to remember", 5), "inbox:\n" + t.text()
            t.key(b"k", b"k", b"k")
            assert t.wait_for("Run cargo fmt first", 5), t.text()
            t.send(b"y")
            wait_until(lambda: "Run `cargo fmt`" in (h.project / ".corvx/conventions.md").read_text(),
                       10, "y to accept:\n" + t.text())
            assert t.wait_for("Tests name the behaviour", 5), t.text()
            t.send(b"e")
            wait_until(lambda: "after the behaviour it shows" in
                       (h.project / ".corvx/conventions.md").read_text(), 10, "e to edit and accept:\n" + t.text())
            t.key(b"\x1b", b"q")
        finally:
            t.close()
        log = git("log", "-3", "--format=%s")
        assert "Knowledge: Run cargo fmt first" in log and "Knowledge: Tests name the behaviour" in log, log

        # What the next agent reads.
        k = m.tool("read_knowledge")
        conventions = k["files"][3]["text"]
        assert "Run `cargo fmt` before committing." in conventions, conventions
        assert "Name each test after the behaviour it shows." in conventions, conventions
        assert "Name tests after" not in conventions, conventions
        assert k["waiting_for_the_human"] == [], k
        assert k["rejected_before"] == [{"file": "decisions", "title": "Use Redis",
                                         "reason": "no external services"}], k
    finally:
        m.close()


def missions_cleanup_and_the_daemon(h: Harness):
    """Mission history, the journal committed at the end, cleaning up worktrees, editing a
    project, and stopping and restarting the daemon."""
    git = lambda *a, cwd=h.project: subprocess.run(
        ["git", "-C", str(cwd), "-c", "user.name=e2e", "-c", "user.email=e2e@e2e", *a],
        check=True, capture_output=True, text=True).stdout
    if not (h.project / ".git").exists():
        git("init", "-q")
        git("add", ".")
        git("commit", "-q", "-m", "init")

    # Renaming and moving a project keeps its id; a rescan picks up what changed.
    h.corvx("project", "set", "e2e", "--name", "E2E app", "--workspace", "clients")
    listed = next(l for l in h.corvx("project", "list").splitlines() if l.startswith("e2e "))
    assert "clients" in listed, listed
    (h.project / "Makefile").write_text("build:\n\ttrue\n")
    assert "test:  cargo test" in h.corvx("project", "rescan", "e2e")
    h.corvx("project", "set", "e2e", "--workspace", "default")

    orch = h.start(runtime="claude", command=[str(FAKE_CLAUDE)])
    env = {**h.env, "CORVX_SESSION_ID": orch, "CORVX_AGENT_COMMAND": str(FAKE_CLAUDE)}
    m = Mcp(env)
    try:
        m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        m.notify("notifications/initialized")
        merged = m.tool("start_agent", {"project": "e2e", "name": "merged", "prompt": "work"})
        unmerged = m.tool("start_agent", {"project": "e2e", "name": "unmerged", "prompt": "work"})
        h.sessions += [merged["id"], unmerged["id"]]
        for a, name in [(merged, "a.txt"), (unmerged, "b.txt")]:
            Path(a["worktree"], name).write_text("work\n")
            git("add", name, cwd=a["worktree"])
            git("commit", "-q", "-m", f"add {name}", cwd=a["worktree"])
        git("merge", "-q", "--no-ff", "-m", "merge", f"corvx/{merged['id']}")
        m.tool("stop_agent", {"id": merged["id"]})
        m.tool("stop_agent", {"id": unmerged["id"]})

        m.tool("journal", {"title": "Mission", "text": "**Goal:** Ship the widget."})
        done = m.tool("report_done", {"summary": "Merged one branch; the other needs review."})
        assert done["journal"], done
        # The journal entry is committed at the end of the mission, and nothing else is.
        assert git("log", "-1", "--format=%s").startswith("Journal: e2e mission"), git("log", "-3", "--oneline")
        assert git("status", "--porcelain", "--", ".corvx/journal") == ""

        history = h.corvx("missions", "e2e")
        assert "Ship the widget." in history and "2 agents" in history, history
        assert f"corvx session cleanup {orch}" in history, history
        assert "Merged one branch" in h.corvx("missions", "e2e", "--show", orch)

        # Cleaning up removes the merged worktree and branch, and keeps the unmerged one.
        out = h.corvx("session", "cleanup", orch)
        assert f"removed {merged['id']}" in out and f"kept    {unmerged['id']}" in out, out
        assert "not merged" in out, out
        assert not Path(merged["worktree"]).exists()
        assert Path(unmerged["worktree"]).exists()
        branches = git("branch", "--list", "corvx/*")
        assert merged["id"] not in branches and unmerged["id"] in branches, branches
    finally:
        m.close()

    # The daemon: status, a refused stop while a session without tmux runs, then a restart.
    assert "sessions:" in h.corvx("daemon", "status")
    pty = h.start(backend="pty", command=["cat"])
    p = subprocess.run([str(CORVX), "daemon", "stop"], env=h.env, capture_output=True, text=True)
    assert p.returncode != 0 and pty in p.stderr, p.stderr
    h.corvx("session", "stop", pty)
    old = daemon_pid(h)
    out = h.corvx("daemon", "restart")
    assert f"pid {old} →" in out, out
    assert daemon_pid(h) != old
    if tmux_available():
        assert "running" in h.session_line(orch), h.session_line(orch)
    # After the restart the ended agents are still listed under their orchestrator.
    assert "2 agents" in h.corvx("missions", "e2e")


def a_lead_starts_missions(h: Harness):
    """Goals, a project lead you talk to, and the missions it starts under itself."""
    git = lambda *a: subprocess.run(["git", "-C", str(h.project), "-c", "user.name=e2e",
                                     "-c", "user.email=e2e@e2e", *a],
                                    check=True, capture_output=True, text=True).stdout
    if not (h.project / ".git").exists():
        git("init", "-q")
        git("add", ".")
        git("commit", "-q", "-m", "init")
    h.corvx("knowledge", "init", "e2e")
    assert "@.corvx/goals.md" in (h.project / "CLAUDE.md").read_text()
    h.corvx("goals", "add", "e2e", "Ship the widget", "--detail", "Users can make widgets.")
    h.corvx("goals", "add", "e2e", "Dark mode", "--status", "next")
    listed = h.corvx("goals", "e2e")
    assert "active   Ship the widget" in listed and "next     Dark mode" in listed, listed

    env = {**h.env, "CORVX_AGENT_COMMAND": str(FAKE_CLAUDE)}
    corvx = lambda *a: subprocess.run([str(CORVX), *a], env=env, capture_output=True, text=True, timeout=60)
    out = corvx("lead", "e2e").stdout
    assert "started the lead of e2e" in out, out
    lead = out.split(": ")[1].split()[0]
    h.sessions.append(lead)
    started = [e for e in events_of(h, "session.started") if e.get("id") == lead][-1]
    assert "Read the project and give me a short status" in " ".join(started["command"]), started
    # Talking to a running lead sends the message; it does not start another.
    assert "sent to the lead of e2e" in corvx("lead", "e2e", "focus", "on", "the", "widget").stdout
    wait_until(lambda: "focus on the widget" in h.corvx("session", "output", lead), 5, "the message to reach the lead")
    assert sum("lead · e2e" in l for l in h.corvx("session", "list").splitlines()) == 1

    m = Mcp({**env, "CORVX_SESSION_ID": lead})
    try:
        m.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        m.notify("notifications/initialized")
        k = m.tool("read_knowledge", {})
        assert [g["title"] for g in k["goals"]] == ["Ship the widget", "Dark mode"], k["goals"]
        mission = m.tool("start_mission", {"goal": "Build the widget form", "serves": "Ship the widget"})
        orch = mission["id"]
        h.sessions.append(orch)
        started = [e for e in events_of(h, "session.started") if e.get("id") == orch][-1]
        command = " ".join(started["command"])
        assert 'serves the project goal "Ship the widget"' in command and "--model opus" in command, command
        assert any(a["id"] == orch for a in m.tool("list_agents")), "the lead supervises its mission"
    finally:
        m.close()

    # The mission's orchestrator reports; its journal entry is written and committed, and the
    # mission shows in the history (the lead does not).
    o = Mcp({**env, "CORVX_SESSION_ID": orch})
    try:
        o.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        o.notify("notifications/initialized")
        o.tool("journal", {"title": "Mission", "text": "**Goal:** Build the widget form."})
        done = o.tool("report_done", {"summary": "The form is built."})
        assert done["journal"], done
    finally:
        o.close()
    assert git("log", "-1", "--format=%s").startswith("Journal: e2e mission")
    history = h.corvx("missions", "e2e")
    assert "Build the widget form." in history and lead not in history, history


def setting_up_in_the_terminal_ui(h: Harness):
    """Add a project, set up its knowledge, write its brief and start a mission, all in the TUI."""
    proj = h.base / "newproj"
    (proj / "tests").mkdir(parents=True)
    (proj / "server.py").write_text("print('hi')\n")
    (proj / "tests" / "test_server.py").write_text("import unittest\n")
    git = lambda *a: subprocess.run(["git", "-C", str(proj), *a], check=True,
                                    capture_output=True, text=True).stdout
    git("init", "-q")
    git("config", "user.name", "e2e")
    git("config", "user.email", "e2e@e2e")
    git("add", "-A")
    git("commit", "-q", "-m", "init")

    env = {**h.env, "EDITOR": "printf '# New\\n\\nA small server.\\n' >",
           "CORVX_AGENT_COMMAND": str(FAKE_CLAUDE)}
    t = Term([str(CORVX)], env, cols=130, rows=36)
    orch = None
    try:
        assert t.wait_for("projects", 10), t.text()
        t.send(b"a")
        t.send(f"{h.base}/newp".encode())
        t.send(b"\t")
        assert t.wait_for("newproj/", 5), "Tab did not complete the path:\n" + t.text()
        t.send(b"\r")
        assert t.wait_for("add this project?", 5), t.text()
        assert t.wait_for("python3 -m unittest", 5), t.text()
        assert "[x] set up its knowledge files" in t.text(), t.text()
        t.send(b"\r")
        assert t.wait_for("knowledge set up and committed", 10), t.text()
        assert "newproj" in h.corvx("project", "list")
        assert "<!-- corvx:knowledge -->" in (proj / "CLAUDE.md").read_text()
        assert "CORVX: project knowledge files" in git("log", "-1", "--format=%s")
        assert t.wait_for("next: b writes the brief", 5), t.text()

        t.send(b"b")
        assert t.wait_for("brief saved and committed", 10), t.text()
        assert (proj / ".corvx" / "brief.md").read_text() == "# New\n\nA small server.\n"
        assert "Knowledge: update brief" in git("log", "-1", "--format=%s")
        assert t.wait_for("next: g adds a goal", 5), t.text()

        # g adds a goal; it shows on the board and in goals.md, committed.
        t.send(b"g")
        t.send(b"Health checks")
        t.send(b"\r")
        assert t.wait_for('goal "Health checks" added and committed', 5), t.text()
        assert "### Health checks · active" in (proj / ".corvx" / "goals.md").read_text()
        assert "Goals: Health checks (active)" in git("log", "-1", "--format=%s")
        assert t.wait_for("● Health checks", 5), t.text()

        # e renames the project and moves it to another workspace; its id stays.
        t.send(b"e")
        assert t.wait_for("edit newproj", 5), t.text()
        t.send(b" server")
        t.send(b"\t")
        t.send(b"\x7f" * 10)
        t.send(b"tools")
        t.send(b"\r")
        assert t.wait_for('renamed to "newproj server"', 5), t.text()
        assert "TOOLS" in t.text(), t.text()
        listed = next(l for l in h.corvx("project", "list").splitlines() if l.startswith("newproj "))
        assert "tools" in listed, listed

        t.send(b"o")
        assert t.wait_for("start a mission on newproj", 5), t.text()
        assert " Health checks " in t.text(), "the mission form should offer the goal:\n" + t.text()
        assert "0 guideposts · 0 decisions · 0 conventions · 0 past missions" in t.text(), t.text()
        t.send(b"Add a health check")
        t.send(b"\t")
        t.send(b"\t")
        t.send(b"\x1b[C")  # autonomy: guided -> autonomous
        t.send(b"\t")
        t.send(b"\t")
        t.send(b"\x1b[C")  # agents: sonnet -> haiku
        t.pump(0.5)
        t.send(b"\r")
        assert t.wait_for("started orchestrator", 10), t.text()
        line = next(l for l in h.corvx("session", "list").splitlines() if "orchestrator · Add a health check" in l)
        orch = line.split()[0]
        assert "autonomous" in h.corvx("project", "autonomy", "newproj")
        started = [e for e in events_of(h, "session.started") if e.get("id") == orch]
        command = " ".join(started[-1]["command"]) if started else ""
        assert "--model opus" in command and "start_agent uses haiku" in command, command
        assert 'serves the project goal "Health checks"' in command, command
        # The board selects the new mission and shows its screen live.
        assert t.wait_for("live · orchestrator · Add a health check", 5), t.text()
        assert t.wait_for("fake claude ready", 5), t.text()

        # t sends a message to the selected mission without attaching.
        t.send(b"t")
        assert t.wait_for("to orchestrator · Add a health check ›", 5), t.text()
        t.send(b"use fewer agents")
        t.send(b"\r")
        assert t.wait_for("sent to orchestrator · Add a health check", 5), t.text()
        wait_until(lambda: "use fewer agents" in h.corvx("session", "output", orch), 5, "the message to arrive")

        # From the projects column, t talks to the project's lead, starting it first.
        t.key(b"\x1b")
        t.send(b"t")
        assert t.wait_for("to new lead · newproj ›", 5), t.text()
        t.send(b"what next?")
        t.send(b"\r")
        assert t.wait_for("started the lead of newproj", 10), t.text()
        lead = next(l for l in h.corvx("session", "list").splitlines() if "lead · newproj" in l).split()[0]
        assert t.wait_for("lead newproj", 5), t.text()

        # m lists the project's missions, with the one just started (and not the lead).
        t.send(b"m")
        assert t.wait_for("missions · newproj", 5), t.text()
        assert "running" in t.text() and "Add a health check" in t.text(), t.text()
        t.key(b"\x1b", b"q")
    finally:
        t.close()
        for sid in h.corvx("session", "list").splitlines()[1:]:
            if "newproj" in sid:
                h.corvx("session", "stop", sid.split()[0], check=False)
        h.corvx("project", "remove", "newproj", check=False)


SCENARIOS = [
    attach_and_detach_pty,
    orchestration_through_mcp,
    orchestrator_decides_approvals,
    rules_reach_the_human,
    autonomy_trust_and_journal,
    the_knowledge_loop,
    missions_cleanup_and_the_daemon,
    a_lead_starts_missions,
    setting_up_in_the_terminal_ui,
    real_claude_rules,
    attach_from_the_terminal_ui,
    attach_and_detach_tmux,
    sessions_survive_a_daemon_restart,
    detach_resets_the_terminal,
    agent_states_reach_the_queue,
    failed_exit_raises_an_item,
    inbox_in_the_terminal_ui,
    approvals_from_the_inbox,
    answering_in_the_session_clears_the_approval,
    approve_in_the_terminal_ui,
    resume_after_a_restart,
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
