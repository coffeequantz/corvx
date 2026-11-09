#!/usr/bin/env python3
"""A stand-in for Claude Code, for end-to-end tests.

It reads the hook settings CORVX passes with --settings, and sends the same hook
events Claude Code would, driven by the lines typed into it:

  work   a turn that finishes            (UserPromptSubmit, PreToolUse, Stop)
  ask    asks permission                  (Notification); the next line answers it
  exit   quits with status 3

No model is called, so it costs nothing and runs the same every time.
"""
import json
import subprocess
import sys

args = sys.argv[1:]
settings = args[args.index("--settings") + 1]
with open(settings) as f:
    hook_cmd = json.load(f)["hooks"]["Stop"][0]["hooks"][0]["command"]


def hook(event, **fields):
    payload = {"hook_event_name": event, "session_id": "fake-claude", **fields}
    subprocess.run(hook_cmd, shell=True, input=json.dumps(payload).encode())


print("fake claude ready", flush=True)
pending = False
for line in sys.stdin:
    text = line.strip()
    if pending:
        pending = False
        hook("UserPromptSubmit", prompt="permission answered")
        hook("PreToolUse", tool_name="Bash")
        hook("PostToolUse", tool_name="Bash")
        hook("Stop")
        print("permission granted", flush=True)
    elif text == "work":
        hook("UserPromptSubmit", prompt="work")
        hook("PreToolUse", tool_name="Read")
        hook("Stop")
        print("turn finished", flush=True)
    elif text == "ask":
        hook("Notification", message="Claude needs your permission to use Bash")
        print("waiting for permission", flush=True)
        pending = True
    elif text == "exit":
        sys.exit(3)
