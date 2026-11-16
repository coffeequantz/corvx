# CORVX — Product & Technical Specification

**Status:** Draft v0.3 · **Date:** 2026-11-09 · **Owner:** coffeequantz · **UX companion:** `CORVX_UX.md`
**Tagline:** Build without boundaries.
**One-liner:** CORVX is an open-source control plane for AI coding agents: it runs them durably, knows what each one is doing, routes decisions to you, and remembers what it learns about every project.

---

## 0. What changed in v0.3, and why

v0.2 spent most of its detail on infrastructure (terminals, GUI, plugins, a fact database) and described orchestration, the actual point of CORVX, only abstractly. A review after M0 and part of M1 reshaped it.

| Area | v0.2 | v0.3 | Why |
|---|---|---|---|
| Orchestration | CORVX runs missions, tasks and plans itself | An orchestrator is an ordinary agent session (e.g. Claude Code) that drives CORVX through **MCP tools** | Agent runtimes already plan and decompose well, and improve on their own. CORVX should give them hands, not compete with them. |
| Project understanding | Four-layer fact database with provenance and staleness | **Plain knowledge files** in the repo (`.corvx/`), changed through proposals you approve | Delivers the "ever clearer picture" at a fraction of the cost. Add structure only when evidence shows it changes outcomes. |
| Sessions | CORVX owns PTYs directly | A **session backend** interface; **tmux** is the v1 backend | Gives durable sessions (survive daemon restarts), a server-side screen and robust attach for free. Our own PTY code stays as a fallback. |
| Status model | `done` raised for every finished turn | New **`waiting`** state: the turn ended and the agent wants its next instruction. `done` means the task is complete. | Interactive agents finish a turn every prompt; treating each as "ready for review" floods the queue. |
| Supervision | Every attention item goes to the human | Each session has a **supervisor** (you, or an orchestrator). Items go to the supervisor first. | Lets an orchestrator handle its agents and escalate only what needs you. |
| GUI | Required in v1, built from M0 | **After** the orchestration milestone | A second large product before the first is proven; the protocol will change a lot first. |
| Runtimes in M1 | Claude Code, Codex, OpenRouter | **Claude Code** first, Codex next; OpenRouter's agent loop is deferred | The OpenRouter harness is a product in itself and does not test the core idea. |
| Plugins | Manifest, permissions, registry early | **Protocol + MCP + adapters** are the extension story; a plugin system comes later | Nobody can build plugins until the core shape is stable. |
| Detach key | `Ctrl+B` | **`Ctrl+\`** (configurable) | Claude Code uses `Ctrl+B` to send a running command to the background. |
| Security | Not specified for the local socket | Owner-only socket and data folder, cleaned agent environment, hook timeouts | Found in review: the socket allows starting any command as you; transcripts may hold secrets. |

M2 existed to **prove the core bet** before more infrastructure is built, and did (section 17). M3, the knowledge loop, is built; its done-when trial is next.

---

## 1. Problem

A developer who works mostly through AI agents runs many of them at once: several per project, several projects, some on a laptop and some on remote boxes. Today that means a pile of terminal tabs and no shared picture. You have to remember what each agent is doing, which one is stuck waiting for you, which finished work is unreviewed, and what each project as a whole looks like. Orchestrating agents, if any exist, start each session knowing nothing about the project beyond what fits in a prompt.

## 2. Product thesis

CORVX is a **control plane**, not an agent and not a terminal. It does three jobs well:

1. **Run agents durably.** Start agents in any project, on any of your machines. They keep running when you close a window, when the daemon restarts, or when the network drops.
2. **Know their state and route decisions to you.** Reliable status for every agent, and one ordered inbox of what needs you, with enough context to decide without hunting.
3. **Remember what it learns about each project.** A small, curated body of project knowledge that every agent starts from, and that improves with every piece of work.

Orchestration comes from combining these: an orchestrator is an agent that uses CORVX's tools to start, watch and direct other agents, ask you questions, and update the project's knowledge. CORVX supplies the hands, memory and oversight; the model supplies the judgement.

## 3. Goals and non-goals

### Goals

- Supervise many agents across many projects and machines from one place.
- An orchestrator agent can delegate to other agents, including on remote machines, and to other orchestrators.
- A triaged attention queue: you see what needs you, not everything that exists.
- Project knowledge that agents read and that improves through approved proposals.
- Agent-agnostic: any CLI agent works through an adapter; Claude Code first.
- Open: documented protocol, MCP tools and adapters others can build on.

### Non-goals

- A workflow or planning engine. Planning is the orchestrator agent's job.
- Our own agent loop in v1 (an API-model harness may come later).
- A hosted SaaS, billing or team administration.
- Replacing the IDE, the terminal, git or the agent runtimes.
- Telemetry by default. It is off, always, unless you opt in.

## 4. Principles

1. **One core, many clients.** All state and logic live in the daemon. TUI, CLI, scripts, agents (via MCP) and a later GUI are clients of the same API.
2. **Use the agent, don't rebuild it.** If a runtime already does something (planning, subagents, permission prompts), CORVX observes and connects it rather than reimplementing it.
3. **Location-transparent.** A session on a remote machine is addressed and handled the same way as a local one.
4. **Observed beats claimed.** CORVX distinguishes what it observed (exit codes, hook events, git state, test results) from what an agent asserted. Only observed facts drive automatic decisions.
5. **Human-controlled, earned autonomy.** Consequential actions need approval by default. Autonomy is granted per project and can be revoked.
6. **Prove before building.** Each milestone tests an assumption before the next layer of infrastructure is built on it.
7. **Boring persistence.** SQLite and plain files you can read, back up and delete.

## 5. Core concepts

| Concept | Meaning |
|---|---|
| **Node** | A machine running a `corvxd` daemon. |
| **Workspace** | A user-defined grouping of projects. Organisational only. |
| **Project** | A repository plus its knowledge files and policy. The unit of "which one needs me?". |
| **Session** | A running agent (or any command) in a terminal, owned by the session backend. Survives daemon restarts. |
| **Runtime** | The agent program a session runs, e.g. Claude Code. An adapter maps its signals to CORVX's status model. |
| **Supervisor** | Who a session answers to: you, or an orchestrator session. Attention items go to the supervisor first. |
| **Orchestrator** | A session whose agent uses CORVX's MCP tools to start and direct other sessions. Not special code: a normal session with tools, a role prompt and a budget. |
| **Mission** | A record of a goal: its definition of done, the sessions working on it, and a journal. Written by orchestrators and humans; not executed by CORVX. |
| **Attention item** | Something that needs a supervisor: a permission request, a question, a failure, a finished task, a knowledge proposal. |
| **Knowledge** | The curated files in `<repo>/.corvx/` that every agent starts from (section 10). |
| **Policy** | Rules for permissions, budgets and autonomy, per project. |

### 5.1 Hierarchy

```
Node
└── Workspace
    └── Project ──────────► Knowledge (.corvx/) + Policy
        └── Session (supervisor: you)                 e.g. an orchestrator
            └── Session (supervisor: that session)    e.g. a coding agent in a worktree
                └── …                                  delegation depth is limited by policy
```

## 6. Architecture

```
 You ── TUI · CLI · (later GUI) ──┐
                                  ▼
                     corvxd  (one per machine)
  ┌──────────────────────┬────────────────────────────┬─────────────────────────┐
  │ Sessions             │ Control plane              │ Knowledge               │
  │ backend: tmux (v1)   │ state · status · attention │ .corvx/ files per repo  │
  │ adapters: Claude     │ approvals · events · store │ proposals → inbox       │
  │ Code, Codex, any CLI │ MCP server for agents      │ generated project map   │
  └──────────────────────┴────────────────────────────┴─────────────────────────┘
            ▲ SSH                                ▲ MCP tools (stdio)
   corvxd on remote machines              orchestrator and agent sessions
```

### 6.1 Daemon (`corvxd`)

- Owns the session backend, the store, the event log and the MCP server.
- Runs detached from any terminal, so closing the terminal that started it does not stop it.
- Can restart or upgrade without stopping agents, because sessions live in the backend (section 7). `corvx daemon status` says whether a newer build is on disk, and `corvx daemon restart` (or the TUI palette) loads it; both refuse while sessions without tmux run, since those would end.
- Starts on demand from any client, or at login.

### 6.2 Protocol

- JSON-RPC 2.0, one message per line, over a Unix socket. Versioned; clients check the version on connect.
- **Requests** for commands, plus **subscriptions** for events (`events.subscribe`) and session output, so clients do not poll. *(Not yet built: today's clients poll. Required before the GUI and multi-node.)*
- Remote nodes are reached by running the client side over SSH (`ssh host corvx proxy`), so no port is ever opened.
- Public and documented. The CLI, TUI, MCP server and third-party tools all use it.

### 6.3 Local security

- The data folder is owner-only (`0700`), and the socket is owner-only (`0600`). Anyone who can reach the socket can start any command as you, so this is the main boundary.
- Agents start with a **cleaned environment**: variables that belong to the process that started the daemon (e.g. a parent Claude Code session's markers) are removed.
- Hook commands that agents run have short timeouts, so a slow or stopped daemon never stalls an agent.
- Transcripts may contain secrets; they are owner-only and have a retention limit (section 15).

## 7. Sessions

### 7.1 Session backend

Everything CORVX does with a terminal goes through one interface: start, stop, send input, read the current screen, stream output, resize, attach, list, and recover after a daemon restart.

| Backend | Status | Notes |
|---|---|---|
| **tmux** | v1 default | A dedicated tmux server per node (`tmux -L corvx`) with CORVX's own config: the prefix is `Ctrl+\`, the status bar is off, mouse handling is CORVX's choice. Gives durable sessions, a server-side screen (`capture-pane`), output streaming (`pipe-pane`) and proven attach/detach. Works the same over SSH. |
| **Native PTY** | Built (M1) | CORVX holds the PTY itself. Used for tests and as a fallback. Sessions end with the daemon, and attach replays raw output (which can confuse terminals). Not the default. |
| **Holder process** | Later | A small per-session process that owns the PTY, for platforms without tmux (Windows). Only if needed. |

### 7.2 Attach

`corvx session attach <id>` connects your terminal to the session (for tmux: a client of CORVX's tmux server). Detach with `Ctrl+\` then `d`; the session keeps running. Detaching restores your terminal's modes (mouse reporting, keyboard protocols). The TUI attaches the same way, and peeks using the server-side screen.

## 8. Agents and status

### 8.1 Runtimes

| Runtime | When | Status source |
|---|---|---|
| Claude Code | M1 (built) | Hooks, registered through a per-session settings file passed with `--settings` |
| Codex CLI | M5 | Its hook/notification mechanism, or terminal inference |
| Any CLI | Now | Process state only (`running` / exited) plus optional terminal inference |
| API models (OpenRouter etc.) | Later | Needs an agent loop CORVX would have to build; deferred |

### 8.2 Status model

| State | Meaning | Raises attention? |
|---|---|---|
| `working` | Doing something | No |
| `blocked` | Needs a decision now: a permission request, or a question the agent asked | Yes, severity 3 |
| `waiting` | Finished its turn and wants the next instruction | Yes, severity 1, to the supervisor |
| `done` | The task is complete: declared through MCP (`report_done`), or a one-shot run exited cleanly | Yes, severity 2 (review) |
| `failed` | Exited with an error | Yes, severity 3 |
| `stopped` | Ended on request, or quit while waiting on you | No |
| `unknown` | No signal yet | No (shown, never guessed) |

**Claude Code mapping:** prompt submitted or tool use → `working`; permission prompt → `blocked` (with the request text); idle notification → `waiting`; turn finished (`Stop`) → `waiting`; process exit → `done` for a one-shot run with exit code 0, `stopped` for an interactive session that was quit, `failed` otherwise.

Hooks first, terminal inference second, and never silent: if the state cannot be known it is `unknown`.

### 8.3 Approvals

Where a runtime allows a hook to answer a permission request, CORVX can approve or deny **from the inbox, without attaching**. For Claude Code this uses the permission hooks; the decision is returned to the waiting agent. Project policy can approve some requests automatically (e.g. read-only commands) and must route others to a human (e.g. deleting outside the worktree, force-push, deploys).

## 9. Orchestration through MCP

### 9.1 The idea

An orchestrator is an agent session started with CORVX's MCP server attached (`corvx mcp`, a stdio MCP server that talks to the daemon). Through it, the agent can run and supervise other agents. CORVX enforces policy, budgets and depth limits on every call, and records every call in the event log.

### 9.2 Tools (first version)

| Tool | What it does |
|---|---|
| `list_projects`, `read_knowledge` | What projects exist; read a project's knowledge files and map. |
| `start_agent` | Start a session in a project: runtime, initial prompt, optionally a **new git worktree** on a fresh branch. The caller becomes its supervisor. |
| `list_agents` | Sessions this orchestrator supervises, with status. |
| `read_output` | The session's current screen, or recent output. |
| `send_input` | Send an instruction to a session. |
| `wait_for` | Block until any supervised session becomes `blocked`, `waiting`, `done` or `failed` (with a timeout). The orchestrator's main loop. |
| `decide_approval` | Decide a supervised agent's permission request: allow, always, deny (with a message), or escalate to you. |
| `set_autopilot` | Approve a supervised agent's permission requests without asking; each is logged. |
| `stop_agent` | Stop a session. |
| `ask_human` | Put a question in your inbox and wait for the answer. |
| `report_done` | Mark this session's task complete, with a summary. |
| `propose_knowledge` | Propose an addition or edit to a project's knowledge files; it goes to your inbox. |
| `journal` | Append to the current mission's journal (`.corvx/journal/<date>-<session>.md`). An orchestrator's `report_done` summary is added too. |

### 9.3 Worktrees

`start_agent(worktree: true)` creates `<repo>/.corvx/worktrees/<session>` on branch `corvx/<mission>/<session>`. Worktrees are listed with their session, and removed only after their branch is merged or you discard them. Overlapping edits between concurrent agents are reported to the orchestrator and, if unresolved, to you.

### 9.4 Supervision and escalation

- Items from a supervised session go to its supervisor. An orchestrator sees them through `wait_for`; you see them in the inbox only if the orchestrator escalates (`ask_human`, or `decide_approval` with `escalate`) or the item is one policy reserves for humans. Permission requests go to the orchestrator first: it decides what belongs to the agent's task and escalates the rest. They stay visible to you, and you can decide any of them. An orchestrator started with `--approvals human` leaves them all to you.
- Model tiers: the orchestrator runs on a strong model and its agents on cheaper ones by default. It picks per task (haiku for mechanical work, opus for hard pieces).
- Autopilot: a session on autopilot has its permission requests approved without asking anyone; each approval is logged. An orchestrator can set it for its own agents, and you can set it for any session.
- CORVX rules: each rule asks the human, allows, or denies a matching request, whoever would otherwise decide it (the orchestrator, autopilot, or Claude Code's own settings). The daemon checks the rules before every tool call (Claude Code's `PreToolUse` hook): allow and deny are answered there, so the tool runs or is refused without a dialog; ask makes Claude Code raise a permission request even when its own settings allow the tool, and that request comes to the human at severity 3 with the rule as its note. The strictest matching rule wins (deny > ask > allow). Built in, all ask by default: `git-push`, `git-discard`, `delete-recursive`, `publish`, `sudo`, `credentials`, `corvx-control` (an agent changing CORVX's rules or approvals), `outside-workdir` (file writes outside the agent's worktree or project, temporary files excepted). What a rule does can be changed at any time, per project or everywhere (a project's setting wins): `corvx rules set <id> ask|allow|deny|off|default`, the TUI rules view (`l`), or `L` on a request a rule sent you (allow that rule in the project, and approve). Humans can add rules (a regular expression on commands or paths, with an action). Automatic decisions are logged (`attention.auto_approved`, `attention.auto_denied`, with the rule). Rules are kept in CORVX's database, not in the project, so an agent cannot change them by editing files, and the `corvx-control` rule covers it running the CLI. They match the request as text: they catch the usual forms, not an agent working around them. Not yet: a model judging requests; rules learned from your decisions.
- Autonomy levels, per project, changeable at any time (`corvx project autonomy`, TUI rules view `-`/`+`): `supervised` (every request to the human; orchestrators cannot decide or set autopilot), `guided` (default: the orchestrator decides, built-in rules ask), `autonomous` (requests no rule covers are approved without asking; built-in rules ask, except `git-discard`), `full` (built-in rules allow, except `corvx-control`). A level only sets defaults: rule settings win over it (project, then everywhere, then level, then the rule's own action).
- Trust from decisions: CORVX counts the human's approvals per project and kind of request (a simple shell command's program and subcommand, or the rule that sent it). After three, it opens a suggestion in the inbox; accepting adds an allow rule for that command (other plain arguments allowed; chaining, pipes and redirection not) or sets that rule to allow in the project. A declined or dismissed suggestion is not offered again. CORVX never changes a rule by itself.
- A project lead sits above missions: a long-lived session (label `lead · <project>`) that you talk to. It starts missions with `start_mission`, so their orchestrators are supervised by it: a mission's waiting, done and failed states and the orchestrator's own permission requests go to the lead, and the orchestrator's agents' requests still go to the orchestrator. Questions (`ask_human`) and rule-reserved requests reach you at any depth. Delegation is limited to three levels (lead, orchestrator, agent).
- You can always see every session and take over any of them, and send any running session a message without attaching (`t` in the TUI).
- Budgets (tokens, cost where reported, time, number of agents) and the delegation depth limit apply to everything an orchestrator starts.

## 10. Knowledge: how CORVX learns a project

### 10.1 Files

Each project gets a `.corvx/` folder, committed with the code by default:

| File | Contents | Written by |
|---|---|---|
| `brief.md` | What the project is, who it is for, current priorities | You (agents may propose edits) |
| `goals.md` | Where it is heading: guideposts (principles every mission follows) and goals, each `### Title · status` (active, next, done, dropped) | You (`g`, `corvx goals`, or by hand); agents may propose goals or marking one done |
| `decisions.md` | Architecture and product decisions, with dates and reasons | Proposals you approve |
| `conventions.md` | How things are done here: style, patterns, testing rules, pitfalls | Proposals you approve |
| `journal/` | One entry per mission: goal, what happened, what went wrong, what was learned | Orchestrators, at mission end; CORVX commits the entry when the orchestrator reports |
| `map.md` | Generated: stack, commands, structure, key entry points (from the project scan) | CORVX, refreshed automatically |

Agents read these through mechanisms their runtimes already support: for Claude Code, a reference from `CLAUDE.md`; for Codex, `AGENTS.md`; and for any agent, the `read_knowledge` tool.

### 10.2 The learning loop

1. Agents and orchestrators **propose** knowledge changes during and after work (`propose_knowledge`), with the evidence: which mission, which review finding, which correction you made.
2. Proposals reach your inbox. You accept, edit or reject them. Rejected proposals are kept with the reason, so they are not proposed again.
3. At the end of a mission, the orchestrator writes a journal entry and proposes what should be remembered.
4. Every new mission starts from the improved files.

Knowledge only changes through proposals you accept, so agents cannot quietly rewrite what other agents trust. You can also edit any of the files yourself (`corvx knowledge edit`, or the TUI), and CORVX commits what you change.

### 10.3 Later, only with evidence

Structured facts with provenance and staleness tracking (v0.2's four layers) remain a direction. They are added only when the journal shows which kinds of knowledge actually change outcomes, and where plain files fail.

## 11. Attention

The inbox answers "what needs me?".

- Kinds: `blocked` (permission or question), `failed`, `done`, `waiting`, `proposal`, `question` (from `ask_human`).
- Each item has: project, node, session, kind, severity, age and a one-line summary.
- Ordered by severity, then age; sortable by project or time. The top 7 show, the rest collapse into a count.
- Actions: approve or deny (permission requests), answer (questions), accept/edit/reject (proposals), acknowledge, snooze, dismiss with a reason, attach.
- Items for supervised sessions go to their orchestrator first (section 9.4).
- Desktop notifications for severity 3 only, configurable per project.

## 12. User interface

- **TUI first.** Layout, keys and flows are in `CORVX_UX.md`: project rail, inbox, sessions, attach and peek, command palette.
- **Detach / prefix key:** `Ctrl+\`, configurable.
- **GUI later** (M6), on the same protocol, once the protocol and orchestration model are stable. Native, not Electron.
- Status always has a glyph and a label, never colour alone.

## 13. Extensibility

In order of when they arrive:

1. **The protocol** (now): anything can drive CORVX through the socket.
2. **MCP tools** (M2): any MCP-capable agent can orchestrate through CORVX.
3. **Runtime adapters** (M1 onward): mapping a runtime's signals to the status model.
4. **Knowledge conventions** (M3): plain files anyone can read and write.
5. **A plugin system** (later): manifests, permissions and a registry, once there is demand and the core is stable.

## 14. Security and privacy

- Nothing leaves the machine unless you add a remote node or an integration.
- Telemetry is off unless you opt in.
- Owner-only socket and data folder; cleaned agent environment; hook timeouts (section 6.3).
- Agents are untrusted: their permissions come from policy; destructive actions need approval.
- Secrets live in the OS credential store, never in knowledge files, logs or transcripts. Transcript redaction is a goal; until it exists, transcripts are treated as sensitive.
- Remote nodes are reached over SSH only.
- An append-only event log records starts, stops, approvals, MCP calls and knowledge changes.

## 15. Data and storage

| Data | Location | Notes |
|---|---|---|
| Projects, sessions, attention, missions, events | `~/.local/share/corvx/corvx.db` | SQLite (WAL) |
| Transcripts | `~/.local/share/corvx/sessions/` | Owner-only; kept 30 days by default |
| tmux server | `corvx` socket name | One per node, CORVX's own config |
| Knowledge | `<repo>/.corvx/` | Plain text, committed by default |
| Worktrees | `<repo>/.corvx/worktrees/` | Ignored by git |
| Config | `~/.config/corvx/` | TOML |

## 16. Technology

| Area | Choice |
|---|---|
| Core | Rust |
| Session backend | tmux 3.x (v1); native PTY (fallback, tests) |
| Storage | SQLite |
| Protocol | JSON-RPC 2.0 over a Unix socket; SSH for remote |
| Agent tools | MCP, stdio server (`corvx mcp`) |
| TUI | ratatui |
| GUI (later) | To decide at M6 (GPUI, Tauri or other) |

Platforms: macOS and Linux first. Windows needs either WSL (tmux) or the holder-process backend; decided later.

## 17. Roadmap

Each milestone has a **done-when**. The order is chosen so that the core bet is tested before more infrastructure is built.

### M0 · Foundation — done
Daemon, protocol v0, SQLite store, project registry with a project scan, TUI shell, CLI.

### M1 · Sessions and attention — mostly done
- Done: sessions in a native PTY; Claude Code adapter with hooks; agent status; attention queue (CLI and TUI inbox); attach with key passthrough and terminal reset; end-to-end tests with a stand-in agent.
- Remaining:
  - Review fixes: `waiting` state and correct idle-notification handling; owner-only permissions; detached daemon; cleaned agent environment; hook timeouts; detach key `Ctrl+\`.
  - **tmux backend** as the default, so sessions survive daemon restarts.
  - TUI: sessions list with status, attach and peek from the TUI.
- **Done when:** three Claude Code sessions in two projects; restarting the daemon does not stop them; the inbox shows a permission request within 2 seconds; you can attach from the TUI and detach cleanly.

### M2 · Orchestration — the core bet · done
- Built and tested with a stand-in agent: `corvx mcp` (all tools in 9.2 except `propose_knowledge`, which belongs to M3); supervision with depth limit 3; supervised agents' waiting/done/failed go to the orchestrator, questions to you; worktrees on `corvx/<id>` branches; questions answered from the CLI (`corvx attention answer`) and TUI inbox (`r`); `corvx orchestrate` with a role prompt. Inline approvals: Claude Code's `PermissionRequest` hook waits (up to 24 h) for a decision from the inbox (`y` once, `Y` with Claude's own "don't ask again" rules minus mode changes, `n` with an optional reason), while the dialog stays answerable in the session; the first answer wins, and a decision nobody is waiting for any more turns into a plain blocked item. Resume: hooks record Claude's conversation id; a tmux session that vanishes (reboot) is `lost`; `corvx session resume` (TUI `R`) restarts it with `--resume`, together with the lost agents it supervised, and tells orchestration members what happened. Delegated approvals: an agent's permission requests go to its orchestrator (severity 1 in your inbox, "orchestrator deciding"), which decides them with `decide_approval` (allow, always, deny, escalate). Escalated requests, and the requests of agents whose orchestrator has ended, come to you. `corvx orchestrate --approvals human` keeps them all with you. Autopilot (`set_autopilot`, `corvx session autopilot`, TUI `a`/`A`) approves a session's requests without asking anyone, and logs each one. An orchestrator can set it only for its own agents. Model tiers: `corvx orchestrate --model` (default opus) and `--agent-model` (default sonnet); the role prompt tells the orchestrator when to pick haiku or opus for an agent. CORVX rules (section 9.4): each asks you, allows or denies matching requests, whoever would decide them; built in, pushes, destructive commands, publishing, credentials and writes outside the worktree ask you; changed at any time per project or everywhere from the CLI, the TUI rules view, or the inbox; verified against real Claude Code (ask, allow, deny). Autonomy levels per project (supervised, guided, autonomous, full) and suggestions to allow what you keep approving (section 9.4). Mission journal: `journal` tool, and an orchestrator's `report_done` closes its entry. Done-when met on 2026-11-12 with a small test project (a link shortener), at autonomy level guided: an Opus orchestrator updated the API contract, ran a backend and a frontend agent (Sonnet) in parallel worktrees plus a read-only brand reviewer, decided its agents' two permission requests and put them on autopilot, merged both branches into main (51 tests passing) and wrote the mission journal, in about 7 minutes and without needing the human. The run found one bug, since fixed: an orchestrator's done report was replaced in the inbox by "waiting" once its turn ended. Setting up and adding projects is still rough; making that easy in the TUI is open (section 18).
- `corvx mcp` server with the tools in section 9.2; supervision and escalation.
- `start_agent` with git worktrees.
- Inline approvals from the inbox for Claude Code.
- An orchestrator role prompt shipped as a template.
- **Done when:** on one of your real projects, an orchestrator session starts two coding agents in separate worktrees, monitors them through `wait_for`, involves you only through the inbox, and finishes with a journal entry. **And you would want to use it again.** If not, revisit before building further.

### M3 · Knowledge loop — built
- `.corvx/` files, generated `map.md`, `CLAUDE.md` / `AGENTS.md` wiring.
- `propose_knowledge` and proposal review in the inbox; mission journal.
- Built and tested with a stand-in agent: `corvx knowledge init` creates the missing files (brief, decisions, conventions, map), adds a marked block to `CLAUDE.md` that imports them (and to an existing `AGENTS.md`, as a list), and commits what it wrote. The map is generated from git's tracked files and the project scan (stack, test and build commands, top-level structure, entry points) and rewritten when knowledge changes. `read_knowledge` returns the files, the newest journal entries, proposals waiting and rejected ones with their reasons; it reads the project's own checkout, not a worktree, so it is current. `propose_knowledge` (decisions, conventions or brief; a title, text, evidence, and optionally the exact text it replaces) opens a `proposal` item in your inbox that the agent's later state changes do not close. You accept (`y`, or `corvx knowledge accept`), edit then accept (`e`, `--edit`, in `$EDITOR`) or reject with a reason (`n`, `corvx knowledge reject`; dismissing rejects too). Accepting appends a dated section with its evidence, or makes the replacement if the text is still there, and commits only that file (and the map), leaving other work in the checkout alone. A proposal with the title of a rejected one is refused with the reason, and so is one already waiting. The orchestrator prompt reads knowledge first, cites decisions and conventions in its plan and its agents' prompts, and ends a mission with zero to three proposals. The scan now recognizes plain Python projects and standard-library tests (`python3 -m unittest`). Remaining: the done-when run below.
- **Done when:** over five missions on a project, accepted proposals visibly change what later orchestrators do (they cite decisions and conventions in their plans).

### Usability — in progress, before M4
Multiple machines can wait; making CORVX pleasant on one machine comes first.
- Done: setting up in the TUI (add with a preview, knowledge, brief, the mission form; see open question 7). Projects can be renamed and moved between workspaces, and scanned again (`corvx project set`, `project rescan`, `e` in the TUI; a name and workspace when adding). Mission history (`corvx missions`, `m` in the TUI): each orchestrator, its agents, its state and its journal entry. Cleaning up after a mission (`corvx session cleanup`, `c`): the worktrees and branches of its agents whose work is merged, never forcing, keeping what is unmerged or has uncommitted changes. Stopping sessions from the TUI (`x`). The journal entry is committed when the orchestrator reports. `corvx daemon status|stop|restart`. Decisions and conventions can be edited directly (`corvx knowledge edit`, the palette).
- Done: the TUI reads left to right in four columns: projects; the selected project's lead and missions (current ones, sessions started by hand, and earlier missions folded under one row), each mission with its state, agents and the goal it serves; the agents of the selected mission as tiles with the last lines of their screens (with a project selected, every agent working on it); and the selection itself (a project's goals, knowledge, what needs you and next step; a session's facts and live screen; an ended mission's journal). `j`/`k` moves in a column, ←/→ between columns. Enter on a running session puts the keyboard in its pane (keys go to the session, which is resized to the pane; Ctrl+\ returns), `t` sends one message, `f` attaches full screen. The live pane shows the session in colour (`session.screen` with `ansi`), the mouse wheel scrolls its history (`history` lines from tmux's scrollback) or moves a column's selection, and a click selects. States have a glyph as well as a colour: a spinner while working, ● waiting, ▲ needs you, ✓ done, ✕ failed, ◌ interrupted. The inbox and rules span every project and take the room beside the projects; Enter in the inbox shows the item in the columns. (This replaced a board, then a tree; the sessions and missions views went with them.) Goals and guideposts (`goals.md`; `g`, `G`, `corvx goals`; the mission form asks which goal a mission serves). The project lead (`corvx lead`, `t` on a project): a long-lived session you talk to, which reads knowledge and goals, proposes goals, starts missions under itself with `start_mission`, passes corrections on, and reports back in its conversation.
- Next, from use: whatever the five M3 missions show is clumsy. Open: whether a mission's done report should also reach your inbox when a lead started it (today it goes to the lead, which tells you); voice input (GUI).

### M4 · Multiple machines
- `corvx proxy` over SSH; nodes in the TUI; `start_agent` on a remote project.
- **Done when:** an orchestrator on your laptop runs one agent locally and one on a remote machine, and both appear in one inbox.

### M5 · More runtimes
- Codex adapter; evaluate an API-model agent loop (OpenRouter).

### M6 · GUI
- Native GUI on the stable protocol, with event subscriptions.

### M7 · Extensibility and autonomy
- Plugin system if there is demand. Autonomy levels exist since M2 (section 9.4); still to come: evidence for raising a level, and automatic downgrade when agents misbehave.

## 18. Decisions and open questions

### 18.1 Decided

| # | Decision | Since |
|---|---|---|
| 1 | Community-ready: the protocol and tools are documented and public in design. The repository is public at github.com/coffeequantz/corvx. | v0.2, v0.3 |
| 2 | Core language Rust | v0.2 |
| 3 | Orchestration is agent-driven through MCP tools; CORVX is not a workflow engine | v0.3 |
| 4 | Knowledge as plain files with proposals; structure later, with evidence | v0.3 |
| 5 | tmux is the v1 session backend | v0.3 |
| 6 | Claude Code first; Codex next; API-model loop deferred | v0.3 (replaces "Claude Code, Codex, OpenRouter in M1") |
| 7 | GUI after the orchestration milestone | v0.3 (replaces "GUI from the start") |
| 8 | Detach / prefix key `Ctrl+\` | v0.3 (replaces `Ctrl+B`) |
| 9 | Name CORVX; domain acquired | v0.2 |
| 10 | `waiting` items for your own sessions go in the inbox at severity 1; the presentation can be refined later | v0.3 |
| 11 | License: MIT OR Apache-2.0, at the user's option (the Rust convention; Apache-2.0 adds a patent grant) | v0.3 |

### 18.2 Open

1. **Windows.** WSL with tmux, or a holder-process backend?
2. **Default orchestrator runtime and budget defaults.** Models are settled for now: opus for orchestrators, sonnet for agents (fable for the hardest orchestration, at about twice opus's price).
3. **Knowledge in git.** Committed by default for now: `knowledge init` and accepting a proposal commit just the knowledge files on the checkout's current branch. Opt-out per project if that gets in the way.
4. **Trademark check** before public launch.
5. **Non-Anthropic models for agents** (e.g. DeepSeek through OpenRouter): through a CLI agent that supports OpenRouter (runs today as a plain session, without hooks), or through CORVX's own agent loop (deferred)?
6. **Setting up and adding projects.** Done in the TUI: `a` adds a project with a preview of its scan, its name and workspace, and optional knowledge setup (Tab completes paths), `b` edits and commits the brief, `K` sets up knowledge, `e` renames a project or moves it to another workspace, `o` starts a mission from a form (goal, autonomy, models, approvals), `m` shows its missions, and a first run and each project show the next step. Still open: changing a project's id or path (sessions, rules and proposals refer to the id).

## 19. Reference points

Facts from public documentation, not independently verified.

- **Herdr** (`herdr.dev`): daemon plus thin clients; agent states Working/Blocked/Done/Idle; hooks where available. Lessons taken: daemon-plus-clients, location-transparent addressing. Avoided: telemetry on by default, separate backends for TUI and GUI.
- **tmux:** durable sessions, server-side screen, attach/detach. Now the v1 session backend.
- **Claude Code hooks** (`code.claude.com/docs/en/hooks`): the status and approval signals for the Claude Code adapter.
- **MCP:** the standard way to give agents tools; CORVX's orchestration interface.
- **Git worktrees:** isolation for parallel agents.

## Appendix A · Glossary

- **Adapter:** maps a runtime's signals to CORVX's status model.
- **Attention item:** something that needs a supervisor's decision or awareness.
- **Knowledge:** the curated files in `.corvx/` that agents start from.
- **MCP:** Model Context Protocol, how agents call CORVX's tools.
- **Mission:** a recorded goal with its sessions and journal.
- **Orchestrator:** a session whose agent directs other sessions through CORVX's tools.
- **Session backend:** what owns the agents' terminals (tmux in v1).
- **Supervisor:** who a session answers to: you or an orchestrator.
- **Worktree:** a separate working directory on its own branch, so parallel agents don't collide.
