# CORVX — UX & Interaction Design

**Companion to:** `CORVX_SPEC.md` (v0.3) · **Status:** Draft v0.2 · **Date:** 2026-11-09
**Prototype:** `prototype/index.html` (open in a browser; it simulates the TUI layout and flows)

> **Changes with spec v0.3.** Read this document with these in mind until each section is revised:
> - **Prefix / detach key is `Ctrl+\`**, not `Ctrl+B` (Claude Code uses `Ctrl+B` to background a running command). Replaced throughout.
> - **New agent state `waiting`**: the turn ended and the agent wants its next instruction. Shown as `WAITING`, severity 1. `done` now means the task is complete.
> - **Orchestrator view** shows an ordinary agent session that drives CORVX through MCP tools, not a CORVX planning engine. Plan review (4.3) becomes the orchestrator asking you through the inbox.
> - **Model view becomes a Knowledge view**: the `.corvx/` files and pending proposals, not four layers of facts.
> - **Approvals** can be answered from the inbox without attaching, where the runtime allows it.
> - The prototype predates v0.3 and still shows missions and the four-layer model.

This document describes how CORVX feels to use: what the user sees, what they do, and what the system does back. It is written for the TUI first, because the TUI sets the interaction model. The GUI uses the same screens, modes and keys, with mouse and windows added on top.

---

## 1. The experience in one paragraph

You open CORVX and the first thing you see is **what needs you**. Not a list of everything that exists, but a short, ordered inbox: an agent is blocked on an approval, a plan is ready, a test run failed, a proposal came in about a project convention. You clear the inbox in a few keystrokes, each action sending the work forward. When you want to direct the work, you start a **mission** by describing an outcome. An orchestrator turns it into a plan, you approve or change the plan, and then you mostly stay out of the way. You step in only where the system asks you to. Over time the orchestrator's picture of each project gets richer, and you can see exactly what it knows and why it makes the decisions it makes.

## 2. Design principles for the UX

1. **Attention first.** The default screen answers "what needs me?" before anything else.
2. **Explain the state.** Every status says what it means, how long it has been that way, and what the user can do about it. No bare colours or unlabelled icons.
3. **One key, one meaning.** A key does the same thing wherever it appears. Actions shown on screen are always the keys that work.
4. **Confirm the consequential, not the trivial.** Navigation is instant. Approvals and destructive actions show exactly what will happen before they happen.
5. **Never fake progress.** Pending actions show as pending until the daemon confirms. The UI never shows "approved" before the daemon says so.
6. **Show the evidence.** Orchestrator decisions link to the facts they used. Proposals link to the missions and reviews that produced them.
7. **Stay in the terminal's rhythm.** Keyboard first, dense text, no modal dialogs for routine work, and nothing that blocks typing.

---

## 3. Screen anatomy

```
┌ top bar: CORVX · scope · global counts · nodes ─────────────────────────────────────────┐
│ rail (projects & nodes) │ main (current view: tabs)                                    │
│                         │                                                              │
│                         │                                                              │
│                         │                                                              │
│ peek strip (optional, one agent's last lines)                                          │
├ status line: MODE · focus · view · hints for the current selection ────────────────────┤
│ command / input line (appears in INSERT and PALETTE modes)                              │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.1 Top bar
- Product name, current scope (e.g. `all projects`, `Client work`, `api-gateway`).
- Global counts by severity: `! 2 blocked · ✗ 1 failed · ✓ 1 to review · ◆ 2 proposals`.
- Node status: `local ●  vps1 ●`. A disconnected node shows `vps1 ◌ reconnecting 12s`.

### 3.2 Rail (left)
- Workspaces, each with its projects. Each project shows its own badges and branch state.
- The top-level orchestrator sits above the projects as `◆ top-level`.
- Rail is navigable with `j`/`k` and opens a project with `Enter`.

### 3.3 Main area
Tabbed views. The tab bar shows the open views. The active view is always one of the views in section 4.

### 3.4 Status line
Always shows: the mode (`NORMAL`, `INSERT`, `PALETTE`), the focused zone (`rail`, `main`), and the keys that apply to the current selection. Hints change with the selection, so the user never has to guess.

### 3.5 Peek strip
Optional, toggled with `p`. Shows the last few lines of one agent's output without taking focus. Useful for watching an agent while working in another view.

---

## 4. Views

| View | Purpose | Reached by |
|---|---|---|
| **Inbox** | Everything that needs the human, ordered by severity. The default home. | Start-up, `Ctrl+\ i` |
| **Project** | One project: its missions, agents and model health. | `Enter` on a project in the rail, `Ctrl+\ p` |
| **Mission** | One mission: tasks, dependencies, progress, budget, orchestrator log. | `Enter` on a mission in a project or inbox |
| **Plan review** | A mission plan waiting for approval: tasks, predicted files, budget, acceptance criteria. | `Enter` on a "plan ready" inbox item |
| **Agent** | One agent's live session. Read by default; type with `i`. | `Enter` on an agent, `Ctrl+\ a` |
| **Orchestrator** | Conversation and actions of an orchestrator, at project or top-level scope. | `o`, `Ctrl+\ o` |
| **Model** | Project Model: layers, health, stale facts, pending proposals. | `Ctrl+\ d` (for "data/knowledge"), or from Project view |
| **New mission** | Form to describe a new mission. | `m` |
| **Help** | Keys for the current mode and view. | `?` |

### 4.1 Inbox (the home screen)

```
 INBOX · 4 need you                                                   sorted by severity
 ▸ ! BLOCKED   api-gateway / claude-2     approval: rm -rf dist/           2m
   ✗ FAILED    ml-pipeline@vps1 / a3      pytest exit 1 · 2 failing         9m
   ◆ PLAN      billing-ui / orch-billing  plan ready · 6 tasks · $1.80      14m
   ◆ PROPOSAL  api-gateway                 convention: RFC 7807 errors (3×)  1h
   ✓ DONE      api-gateway / add-login    mission complete · review         —

 ▸ a approve · d deny · s snooze · Enter open · Ctrl+K palette
```

Ordering rules:
1. `blocked` and `failed` (needs action now).
2. `plan ready` and `done` (needs review).
3. `proposal` (needs a decision, not urgent).
4. Digest (informational, shown only when the inbox is otherwise empty).

The inbox shows the top 7 items. The rest collapse into a count ("+5 more · A show all").

Sorting (`S` cycles, the current order is shown under the heading):
- **severity** (default): blocked and failed first, then plans and reviews, then proposals; older items first within a level.
- **project**: grouped by project, then by severity.
- **newest first** / **oldest first**: by when the item arrived.

Items disappear when resolved. An empty inbox shows the digest ("while you were away: 2 missions done, 3 agents ran, 1 proposal accepted") and a prompt for the next action.

### 4.2 Project view

```
 api-gateway · main · ✎ dirty · workspace: Client work
 MISSIONS     ● add-login-endpoint   running  2/3 tasks   ! 1 blocked
 AGENTS       ● claude-1  working  12m · schema
              ! claude-2  blocked   4m · handler  (approval)
 MODEL        L1 structure ✓ fresh   L2 state ✓ 3m ago
              L3 knowledge 3 ADR · ⚠ 2 stale   L4 experience 9 missions
```

### 4.3 Mission and plan review

A plan is a mission in `plan-ready` state. The plan review screen is the one place where the human shapes work before it starts:

```
 PLAN · billing-ui · "Add invoice PDF export"            orch-billing · budget $1.80 cap $5.00
 [1] model + PDF renderer         claude-1   deps —        predicts: src/invoice/*.ts
 [2] API endpoint                 codex      deps 1        predicts: src/api/*.ts
 [3] UI button + download         claude-2   deps 2        predicts: src/ui/*.tsx
 [4] tests                        codex      deps 1,2
 Acceptance: export returns valid PDF · existing tests green · no new deps
 ▸ a approve plan · r reject (with note) · Enter on a task to edit it
```

Approving a plan starts the tasks with dependencies respected. Rejecting asks for a note, which goes to the orchestrator as an instruction.

### 4.4 Mission view (running)

Shows the task list with live status and progress, a budget bar, and the orchestrator's last decisions. A blocked task shows its reason and the inbox item that resolves it.

### 4.5 Agent view

- Read mode by default: the live output of the session, scrolled with `j`/`k` and `PgUp`/`PgDn`.
- `i` enters INSERT mode and sends input to the agent. `Esc` returns to NORMAL. The agent keeps running in both modes.
- The header shows the state, runtime, node, task and time in state.
- Pending approvals appear at the bottom of the view with the same actions as the inbox.

### 4.6 Orchestrator view

- A conversation with the orchestrator, plus its actions as structured cards, e.g. `Paused task 3 · reason: waiting on schema review` with a link to the facts it used.
- You can instruct it (`i` → text → `Enter`), e.g. "prioritise the auth tests". Its reply appears as a message, and any resulting action appears as a card.
- Orchestrators never silently change a plan. A change of plan is always a card you can see and undo.

### 4.7 Model view

```
 PROJECT MODEL · api-gateway                                         health: 1 stale · 0 disputed
 L1 structure    42 facts · fresh          re-scanned 2h ago
 L2 state        118 facts · fresh         last commit 3m ago
 L3 knowledge    12 facts · 2 stale ⚠      approved by you 4 times
 L4 experience   9 missions · 27 outcomes

 PROPOSALS (1)
 ◆ Convention: API errors use RFC 7807 problem+json
   Evidence: review in mission add-login (2×), review in billing-ui (1×)
   ▸ a accept (writes .corvx/conventions.md) · e edit · d reject (note)

 STALE FACTS (2)
 ⚠ ADR-004 refers to src/auth/session.ts, changed 1d ago   ▸ v re-verify
```

Accepting a proposal writes it into `.corvx/` as a plain file, so it is visible in the repo and in version control. Rejecting keeps the reason so the same proposal isn't repeated.

---

## 5. Modes and focus

| Mode | Entered by | Keys do | Left by |
|---|---|---|---|
| **NORMAL** | default; `Esc` | navigate and act | — |
| **INSERT** | `i`, or automatically in forms | type into the input line or agent session | `Esc` |
| **PALETTE** | `Ctrl+K` | filter and run commands | `Esc`, or `Enter` runs |
| **PREFIX** | `Ctrl+\` | next key jumps to a view or pane | any key |

Focus zones: `rail`, `main`, and `split` when a split pane is open. `Tab` cycles between them. The focused zone has a visible outline and a label in the status line. The detail area is part of `main`, not a separate zone.

Split panes: `Ctrl+\ "` (or `Ctrl+\ %`) opens a second pane on the right. It shows an agent, by default the project orchestrator when the main view is already an agent. Typing (`i`) goes to whichever pane has focus.

### 5.1 Prefix key (`Ctrl+\`)

Terminal-style jumps. After `Ctrl+\`, the next key is one of:

| Key | Jump to |
|---|---|
| `i` | Inbox |
| `p` | Project (of the current selection) |
| `m` | Mission (of the current selection) |
| `o` | Orchestrator |
| `a` | Agent (of the current selection) |
| `d` | Model |
| `[` / `]` | Previous / next tab |
| `Ctrl+\` | Send a literal `Ctrl+\` to the attached agent |

---

## 6. Keyboard reference

### 6.1 Global (all modes except INSERT, where noted)

| Key | Action |
|---|---|
| `?` | Help for the current view |
| `Ctrl+K` | Command palette (works in INSERT too) |
| `Ctrl+\` | Prefix |
| `Tab` | Cycle focus: rail ↔ main |
| `Esc` | Back one level; leave INSERT; close overlay |
| `q` | Close the current tab; on the last tab, detach the TUI (the daemon keeps running) |
| `Ctrl+C` | Interrupt the attached agent when in an agent view; otherwise quit the TUI with confirmation |

### 6.2 Navigation

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | Move selection |
| `g` / `G` | First / last item |
| `Enter` | Open the selection |
| `h` / `l` | Previous / next tab |
| `/` | Filter the current list |
| `S` | Cycle the inbox sort (severity, project, newest first, oldest first) |
| `A` | Show all inbox items, or back to the top 7 |
| `T` | Show or hide the orchestrator's reasoning trace |
| Click | Select a row or focus a pane. Never opens or acts; `Enter` does that |

### 6.3 Acting on items

| Key | Action | Works on |
|---|---|---|
| `a` | Approve | approvals, plans, proposals, done items |
| `d` | Deny or reject (asks for a note on proposals and plans) | approvals, plans, proposals |
| `r` | Retry or resume | failed tasks and agents |
| `s` | Snooze for 15 minutes | any inbox item |
| `o` | Hand the item to the orchestrator | blocked, failed, plan items |
| `p` | Peek at the agent's output | any item with an agent |
| `v` | Re-verify a stale fact | model stale facts |

### 6.4 Creating things

| Key | Action |
|---|---|
| `m` | New mission in the current project (or any project, if on the top level) |
| `n` | New agent in the current project (choose runtime, task, node) |
| `o` | Orchestrator for the current project |
| `Ctrl+K` → "add project" | Add a project from a path |

---

## 7. Core flows

### 7.1 First run

1. `corvx` starts the daemon if it isn't running, then opens the TUI.
2. The inbox is empty and says: "No projects yet. Press `Ctrl+K` and type 'add project', or run `corvx project add ./path`."
3. Adding a project shows the L1 scan appearing live: languages, test command, branch, workspace choice. Confirming it lands on the project view.

### 7.2 Daily loop: clearing the inbox

1. Open CORVX. The inbox shows what needs you, with the most urgent item selected.
2. `Enter` opens the item in its view, with the context needed to decide: the command, the diff, the output, the plan, or the evidence.
3. Act with `a`, `d`, `r`, `s` or `o`. The item leaves the inbox once the daemon confirms the action.
4. The next item is selected. When the inbox is empty, the digest appears.

Target: a typical inbox of 3–5 items clears in under two minutes, with no window switching.

### 7.3 Approval of a consequential action

When an agent asks to run a destructive command, the approval card shows:

```
 APPROVAL · api-gateway / claude-2 · risk: destructive
 Command:  rm -rf dist/
 Path:     /Users/…/api-gateway/.worktrees/claude-2/dist  (inside worktree ✓)
 Reason:   "clear previous build output before running the test build"
 Blast radius: 214 files, 18 MB

 ▸ a approve once · A approve for this agent this mission · d deny (note) · o ask orchestrator
```

- Approvals for destructive actions are only ever "once" or "this mission". There is no permanent approval from the UI; permanent rules are edited in policy.
- A deny with a note goes back to the agent as context, so it can choose a different approach.

### 7.4 Starting a mission

1. `m` opens the New mission form, with INSERT active on the goal field.
2. Type the outcome, e.g. "Add invoice PDF export with tests". `Tab` moves between fields: project (defaults to the current one), definition of done (the orchestrator will suggest one), constraints, autonomy for this mission (defaults to the project level), budget (defaults to policy).
3. `Enter` creates the mission. Its status shows `planning` for a few seconds, with a visible "orchestrator is planning" line.
4. When the plan is ready, an inbox item appears. The user reviews the plan (7.5) rather than being shown a raw output dump.

### 7.5 Reviewing and approving a plan

1. Open the plan from the inbox item. Read the tasks, dependencies, predicted files and budget.
2. To change a task, `Enter` on it and edit the assignee, the runtime or the acceptance criteria. Edits are recorded as a plan revision.
3. `a` approves the plan. Tasks start in dependency order. Parallel tasks start together only if their predicted files don't overlap.
4. `r` rejects, with a note. The orchestrator replans and presents a new plan.

### 7.6 Watching and steering agents

- The mission view shows which tasks are running, blocked or done, with progress from the agent's own signals (or an inferred estimate, marked as such).
- `p` peeks at any agent without leaving the current view. `Enter` attaches fully.
- `i` in an attached agent types into its session. `Esc` detaches. Detaching never stops the agent.
- To steer through the orchestrator instead, `o` and instruct it. This is the preferred path for changes that affect more than one task.

### 7.7 Conflict and integration

When two tasks predict edits to the same file, the plan review shows a warning before approval. During the run, an overlap that actually happens becomes an inbox item: "claude-1 and claude-2 both changed `src/api/routes.ts`". The item offers: let the orchestrator merge, open a diff, or pick one side. There's no silent merge.

### 7.8 Reviewing a finished mission

A done mission appears as an inbox item. Opening it shows the acceptance criteria with pass/fail, the changed files, the test results, the orchestrator's summary, and any proposals it generated. `a` marks it reviewed. The branch stays unmerged until the human chooses to merge (or delegates merge to the orchestrator, if policy allows).

### 7.9 Accepting or rejecting a proposal

1. A proposal appears in the inbox with its evidence (missions and reviews that led to it).
2. `Enter` opens the Model view on that proposal. The full statement is shown, along with the layer it would go into.
3. `a` accepts: it's written to `.corvx/` and is marked as human-verified. `e` edits the wording first. `d` rejects with a note, which prevents the same proposal coming back.

### 7.10 Remote node drops

1. The top bar shows `vps1 ◌ reconnecting`. The inbox gets one medium-severity item: "vps1 unreachable · 1 agent running there (last seen 12s ago)".
2. Agents on vps1 keep their last-known state, marked with `?` and the age, rather than a guessed state.
3. When the link returns, the state resyncs from the event log. Missed events appear in the activity stream, and the marks clear.
4. Nothing is lost while disconnected, because the remote node runs its own daemon.

### 7.11 Talking to the top-level orchestrator

For work across projects ("which of my projects is closest to shipping?", "pause everything on vps1 after the current tasks"), the user opens the top-level orchestrator (`o` from the top of the rail). It answers from project summaries and attention items, and any action across projects is shown as a card that the user confirms.

---

## 8. Status language

Every state has a glyph, a label and a colour. The label is always shown; colour is never the only signal.

| State | Glyph | Label | Colour | Needs you? |
|---|---|---|---|---|
| working | `●` | working | blue | no |
| blocked | `!` | BLOCKED | amber | **yes** |
| done | `✓` | DONE | green | **yes** (review) |
| idle | `◌` | idle | grey | no |
| failed | `✗` | FAILED | red | **yes** |
| stopped | `■` | stopped | grey | no |
| unknown / stale | `?` | stale 12s | dim | no (shown dim) |
| plan / proposal | `◆` | PLAN / PROPOSAL | violet | **yes** |
| inferred (not hook-confirmed) | suffix `~` | e.g. `working~` | as state | no |

Rules:
- Times are always shown ("4m", "2h") for states that are waiting on someone.
- A state from terminal inference shows `~` so the user knows it is an estimate.

---

## 9. Feedback, timing and errors

- **Pending.** Any action sent to the daemon shows `…` until confirmed. Confirmation replaces it with the outcome.
- **Failure.** Failures show the reason and what to do next, in the status line and in the item. Example: `approve failed: daemon unreachable — retrying (r)`.
- **No modal dialogs** for routine actions. Dialogs appear only for destructive or irreversible actions, and they name the action explicitly (e.g. "Delete worktree claude-2 with 3 uncommitted files? type the agent name to confirm").
- **Toasts** are one line, auto-dismiss after 4 seconds, and never steal focus.
- **Notifications** (desktop or sound) fire only for blocked, failed and plan-ready items, and only when the TUI or GUI is not focused. Configurable per project.
- **Speed.** Local navigation feels instant (target < 16 ms). Approvals wait for confirmation and show `…` rather than pretending.

---

## 10. Empty, loading and degraded states

| Situation | What the user sees |
|---|---|
| No projects | Guidance on adding a project; no fake data |
| Nothing needs you | The digest, and "nothing needs you · 2 agents working" |
| Planning in progress | "orchestrator is planning · started 40s ago · ⏹ cancel" |
| Agent output empty | "no output yet · started 2m ago" |
| Node unreachable | Last-known state, dimmed, with age; actions that need the node are disabled with the reason |
| Daemon not running | The TUI offers to start it, and says what it will do |
| Model has little knowledge | Model view shows coverage gaps: "no conventions recorded yet · 0 ADRs" with a suggested first step |

---

## 11. Accessibility

- Every state has a text label; colour is never the only cue.
- High-contrast theme included. Respects the terminal's own colour settings.
- Reduced motion: progress uses text (`3/6`) as well as bars; no blinking.
- Key bindings are remappable and documented in the config file.
- Screen readers: the TUI is limited by its medium, so the GUI exposes the same content through platform accessibility APIs. Known gap, tracked for the GUI milestone.

---

## 12. Configuration surface (UX-relevant)

- Keybindings: `~/.config/corvx/keys.toml`, including the prefix key.
- Theme: dark, light, auto, high-contrast, or a custom theme file.
- Density: compact or comfortable.
- Default view on start, and the inbox ordering rules (advanced).
- Notification rules per project.
- Layouts saved per workspace.

---

## 13. Open UX questions

**Resolved (review of v0.1 of this document):**

| # | Question | Decision |
|---|---|---|
| 1 | Inbox ordering | Severity first by default. Sortable by project, newest first, or oldest first (`S` cycles). |
| 2 | Plan editing depth | Both: direct edits in the plan review, and changes through the orchestrator. Direct edits are recorded as plan revisions. |
| 3 | Attention threshold | Show the top 7 items. The rest collapse into a count, with `A` to show all. |
| 4 | Orchestrator reasoning | Decisions and evidence visible by default. The full reasoning trace is one key away (`T`). |
| 5 | Split panes | **In v1.** A right-hand pane for a second view or agent (`Ctrl+\ "` or `Ctrl+\ %`). |
| 6 | Mouse | Click selects and focuses. It never opens or acts; `Enter` does that. |

The original questions are kept below for reference.



1. **Inbox ordering.** Is severity-then-age the right default, or should the user be able to sort by project? Proposed: severity first, with a project filter on `/`.

yes, severity fist, sortable by project, but also sortable by time / which one came in first

2. **Plan editing depth.** Can the user edit a plan's tasks directly, or should edits always go through the orchestrator? Proposed: both. Direct edits are recorded as revisions.

both, as you propose.

3. **Attention threshold.** How many items before the inbox feels overwhelming? Proposed: the inbox shows the top 7, with the rest collapsed into a count, and an explicit "show all" action.

your proposal is good.

4. **Orchestrator autonomy visibility.** How much of the orchestrator's reasoning should be visible by default? Proposed: decisions and evidence visible; the full reasoning trace one key away.

yes. good.

5. **Split panes in the TUI.** Needed in v1, or is the single-main-area-plus-peek model enough? Proposed: enough for v1; splits arrive with M5 parity in the GUI.

split panes from v1 

6. **Mouse in the TUI.** Click to focus and select only, to keep the keyboard model clean? Proposed: yes.

yes

---

## 14. What the prototype covers

`prototype/index.html` implements a simulated version of this document:

- Inbox with the severity ordering, approvals, plan review, proposals, and the done state.
- Start a mission (`m`), watch the orchestrator plan, approve the plan, and watch tasks run in dependency order.
- Agent view with live-looking output, peek (`p`), and typing into an agent (`i`).
- Model view with proposals and stale facts.
- The node-drop flow (via the palette) with stale marks and the inbox item.
- Modes (NORMAL, INSERT, PALETTE, PREFIX), the status line hints, the help overlay, and the prefix-key view jumps. (The prototype still uses `Ctrl+\`; the product uses `Ctrl+\`.)

It is not connected to any real agent or daemon. Everything is scripted, so it shows the interaction and the timing, not the real output.
