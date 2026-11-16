#!/usr/bin/env python3
"""A stand-in for Claude Code, for end-to-end tests.

It reads the hook settings CORVX passes with --settings, and sends the same hook
events Claude Code would. It accepts the other flags CORVX passes (--mcp-config,
--permission-mode, --model, --append-system-prompt) and an optional first prompt.

Prompts, typed or given as the first argument:

  work   a turn that finishes            (UserPromptSubmit, PreToolUse, Stop)
  ask    asks permission                  (Notification); the next line answers it
  request [command]  asks permission the way Claude Code 2 does: runs the PreToolUse hook
         (printing "pre-tool: <decision>" when a CORVX rule decides; allow runs the tool and deny
         refuses it without asking), then the PermissionRequest
         hook, and waits for its decision, while the next line typed answers it "in the
         session". The command defaults to "make test".
  exit   quits with status 3

With --resume <id> it prints "resumed conversation <id>" first.

No model is called, so it costs nothing and runs the same every time.
"""
import json
import subprocess
import sys

import threading

FLAGS_WITH_VALUES = {"--settings", "--mcp-config", "--permission-mode", "--model",
                     "--append-system-prompt", "--resume"}

args = sys.argv[1:]
flags, positional = {}, []
i = 0
while i < len(args):
    if args[i] in FLAGS_WITH_VALUES and i + 1 < len(args):
        flags[args[i]] = args[i + 1]
        i += 2
    else:
        positional.append(args[i])
        i += 1

with open(flags["--settings"]) as f:
    hooks = json.load(f)["hooks"]
hook_cmd = hooks["Stop"][0]["hooks"][0]["command"]
permission_cmd = hooks.get("PermissionRequest", [{"hooks": [{}]}])[0]["hooks"][0].get("command")
with open(flags["--settings"]) as f:
    status_cmd = json.load(f).get("statusLine", {}).get("command")
spent = 0.0


def status_line():
    """Runs the status line as Claude Code does after a turn: the session's cost so far."""
    if status_cmd:
        payload = {"session_id": "fake-claude", "cost": {"total_cost_usd": spent},
                   "model": {"display_name": "Fake"}, "context_window": {"used_percentage": 12},
                   "rate_limits": {"five_hour": {"used_percentage": 37, "resets_at": 4102444800}}}
        out = subprocess.run(status_cmd, shell=True, input=json.dumps(payload).encode(),
                             capture_output=True).stdout.decode().strip()
        print(f"status: {out}", flush=True)


def hook(event, **fields):
    payload = {"hook_event_name": event, "session_id": "fake-claude", **fields}
    return subprocess.run(hook_cmd, shell=True, input=json.dumps(payload).encode(),
                          capture_output=True).stdout.decode().strip()


pending = False
# A permission request whose hook is waiting: answered by the hook or by the next line typed.
request_open = threading.Event()

SUGGESTIONS = [
    {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "make test"}],
     "behavior": "allow", "destination": "localSettings"},
    {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
]


def finish_tool(how):
    request_open.clear()
    print(how, flush=True)
    hook("PostToolUse", tool_name="Bash")
    hook("Stop")


def ask_hook(command):
    """Runs the PermissionRequest hook, as Claude Code does, and acts on its decision."""
    payload = {"hook_event_name": "PermissionRequest", "session_id": "fake-claude",
               "tool_name": "Bash",
               "tool_input": {"command": command, "description": "Run the tests"},
               "permission_suggestions": SUGGESTIONS}
    out = subprocess.run(permission_cmd, shell=True, input=json.dumps(payload).encode(),
                         capture_output=True).stdout.decode().strip()
    if not request_open.is_set():
        print("hook returned after the dialog was answered", flush=True)
        return
    if not out:
        print("hook returned no decision", flush=True)
        return
    decision = json.loads(out)["hookSpecificOutput"]["decision"]
    if decision["behavior"] == "allow":
        rules = decision.get("updatedPermissions")
        finish_tool(f"allowed by hook with {len(rules)} rule(s)" if rules else "allowed by hook")
    else:
        request_open.clear()
        print(f"denied by hook: {decision.get('message')}", flush=True)
        hook("Stop")


def handle(text):
    global pending
    if request_open.is_set():
        finish_tool("permission granted in session")
    elif text == "request" or text.startswith("request "):
        command = text[len("request"):].strip() or "make test"
        hook("UserPromptSubmit", prompt=text)
        out = hook("PreToolUse", tool_name="Bash", tool_input={"command": command})
        if out:
            decision = json.loads(out)["hookSpecificOutput"]
            verdict, reason = decision["permissionDecision"], decision["permissionDecisionReason"]
            print(f"pre-tool: {verdict} ({reason})", flush=True)
            # As Claude Code does: allow runs the tool without asking, deny refuses it.
            if verdict == "allow":
                finish_tool("allowed before asking")
                return
            if verdict == "deny":
                print(f"denied before asking: {reason}", flush=True)
                hook("Stop")
                return
        request_open.set()
        threading.Thread(target=ask_hook, args=(command,), daemon=True).start()
        print("permission dialog shown", flush=True)
        # Claude Code also notifies when its dialog has waited a few seconds.
        hook("Notification", notification_type="permission_prompt",
             message="Claude needs your permission to use Bash")
    elif pending:
        pending = False
        hook("UserPromptSubmit", prompt="permission answered")
        hook("PreToolUse", tool_name="Bash")
        hook("PostToolUse", tool_name="Bash")
        hook("Stop")
        print("permission granted", flush=True)
    elif text == "work":
        global spent
        hook("UserPromptSubmit", prompt="work")
        hook("PreToolUse", tool_name="Read")
        hook("Stop")
        spent += 0.25
        status_line()
        print("turn finished", flush=True)
    elif text == "busy":
        # A turn that goes on: working until the next line.
        hook("UserPromptSubmit", prompt="busy")
        hook("PreToolUse", tool_name="Read")
        print("working on it", flush=True)
    elif text == "ask":
        hook("UserPromptSubmit", prompt="ask")
        hook("Notification", notification_type="permission_prompt",
             message="Claude needs your permission to use Bash")
        print("waiting for permission", flush=True)
        pending = True
    elif text == "exit":
        sys.exit(3)


if "--resume" in flags:
    print(f"resumed conversation {flags['--resume']}", flush=True)
print("fake claude ready", flush=True)
if positional:
    handle(positional[-1].strip())
for line in sys.stdin:
    handle(line.strip())
