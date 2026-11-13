# CORVX

Build without boundaries.

CORVX is a local-first control plane for AI coding agents: it runs them durably, knows what each one is doing, routes decisions to you, and remembers what it learns about every project. Design: [`CORVX_SPEC.md`](CORVX_SPEC.md) (v0.3). Interaction design: [`CORVX_UX.md`](CORVX_UX.md). Clickable prototype (pre-v0.3): `prototype/index.html`.

> Status: M2 (orchestration through MCP) is done; M3 (the knowledge loop) is built and waiting for its five-mission trial; a usability pass comes before M4 (multiple machines). Not ready for general use. No license has been chosen yet, and the repository is not public.

## Layout

| Crate | What it is |
|---|---|
| `crates/corvx-proto` | Wire protocol: JSON-RPC 2.0 messages and shared types. |
| `crates/corvx-core` | Store (SQLite), project registry and scan, attention queue, event log. |
| `crates/corvx-client` | Client for the daemon. Starts `corvxd` on demand, detached from your terminal. |
| `crates/corvxd` | The daemon: sessions (tmux or native PTY backend), agent status, hooks. |
| `crates/corvx` | `corvx` command: CLI and terminal UI. |

## Requirements

- Rust (stable)
- tmux 3.x, for sessions that survive daemon restarts (`brew install tmux`). Without it, CORVX uses its native PTY backend, and sessions end when the daemon stops.
- For the end-to-end tests: Python 3 and `pip install pyte`.

## Build and try it

```sh
cargo build --workspace
cargo test --workspace
python3 tests/e2e/run_e2e.py        # end-to-end, with a stand-in agent; no model calls

./target/debug/corvx project add ./my-app
./target/debug/corvx knowledge init my-app        # .corvx/ knowledge files, read by every agent
./target/debug/corvx session start my-app --runtime claude -- claude
./target/debug/corvx session attach <id>      # Ctrl+\ then d to detach
./target/debug/corvx attention                 # what needs you
./target/debug/corvx attention approve <item>  # approve a permission request (--always: don't ask again)
./target/debug/corvx attention deny <item> use make test   # deny, telling the agent what to do instead
./target/debug/corvx session resume <id>       # continue an agent interrupted by a restart
./target/debug/corvx session autopilot <id> on # approve its permission requests without asking
./target/debug/corvx orchestrate my-app "add CSV export"   # orchestrator on opus, agents on sonnet
./target/debug/corvx project autonomy my-app autonomous   # supervised|guided|autonomous|full
./target/debug/corvx knowledge my-app          # what CORVX knows about it, and proposals waiting
./target/debug/corvx knowledge accept <id> --edit   # accept a proposal (or: reject <id> why)
./target/debug/corvx knowledge edit my-app decisions   # edit a knowledge file yourself; committed
./target/debug/corvx goals add my-app "CSV export"     # where the project is heading (goals set ... done)
./target/debug/corvx lead my-app "what should we do next?"   # talk to the project's lead (starts it)
./target/debug/corvx missions my-app           # its missions: orchestrators, agents, journal entries
./target/debug/corvx session cleanup <id>      # remove a finished mission's merged worktrees and branches
./target/debug/corvx project set my-app --name "My app" --workspace clients   # the id stays
./target/debug/corvx project rescan my-app     # scan again: stack, test and build commands
./target/debug/corvx daemon restart            # load a new build; sessions in tmux keep running
./target/debug/corvx rules                     # what CORVX asks you, allows or denies
./target/debug/corvx rules set delete-recursive allow --project my-app   # ask|allow|deny|off|default
./target/debug/corvx rules add --command '\bterraform\s+apply\b' --reason "changes infrastructure"
./target/debug/corvx rules add --command '\bmake\s+deploy-staging\b' --reason "staging deploy" --action allow
./target/debug/corvx                           # terminal UI: the board · t talk · g goal · o mission · i inbox
```

The terminal UI opens on a board. Projects are on the left; the selected project's head shows its stack, autonomy, knowledge, goals and what needs you; below it, from left to right, its work (the lead and the missions), the agents under the selected one with what each is doing, and a live view of the selected session's screen. →/Tab moves into a column, Enter attaches, and `t` sends a message to the selected session without attaching, e.g. "you're using too many agents, keep it to two". With the projects column selected, `t` talks to the project's lead, and starts it if none runs.

The lead is a long-lived agent on a strong model that you talk to about a project, in your own words. It reads the project's knowledge and goals, records new goals as proposals for you to accept, starts missions (`start_mission`: an orchestrator under it, which runs its own agents), passes your corrections on to them, and tells you when they finish. Goals and guideposts live in `.corvx/goals.md`: guideposts are principles every mission follows, and each goal has a status (active, next, done, dropped). `g` adds a goal, `G` edits the file, and the mission form asks which goal a mission serves.

The terminal UI covers the whole loop without the CLI. `a` adds a project: type its path (Tab completes), look at what CORVX found (stack, test and build commands, git) and choose whether to set up its knowledge files. `b` opens its brief in `$EDITOR` and commits it when you save. `o` starts a mission: the goal, the autonomy level, the orchestrator's and agents' models, and who approves, with a line on what knowledge the orchestrator starts from. `e` renames a project or moves it to another workspace, and scans it again. `m` lists its missions, newest first, with the selected one's journal entry: Enter watches a running orchestrator, `c` removes the worktrees and branches of its agents whose work is merged (keeping anything unmerged or with uncommitted changes), `x` stops it, `R` resumes an interrupted one. The sessions view has `x` and `c` too. The palette (`:`) also edits the decisions and conventions, rescans a project, and restarts the daemon when a newer build is on disk (the top bar says so). Each project shows the next step to take, and a first run shows how to begin.

Permission requests from Claude Code agents reach the inbox as approvals. You can decide them there, or attach and answer the dialog in the session; whichever answer comes first applies. When the machine restarts, agent sessions are marked lost. Resuming one starts Claude Code again with its conversation, and resuming an orchestrator also resumes the lost agents under it.

An orchestrator runs on a strong model (`--model`, default opus) and starts its agents on a cheaper one (`--agent-model`, default sonnet), picking another per task when it needs to. Its agents' permission requests go to it first. It allows, denies or escalates each one, and escalated ones reach your inbox. You see the rest there too, marked "orchestrator deciding", and can decide any of them yourself. `--approvals human` sends them all to you instead. An agent on autopilot is not asked at all, though each approval is still logged. The orchestrator can turn autopilot on for its agents (`set_autopilot`), and you can turn it on for any session (`a` in the sessions view, `A` on a request in the inbox). When an orchestrator ends, its agents' open requests come to you. An orchestrator writes a journal entry for its mission (`.corvx/journal/<date>-<session>.md` in the project: the goal, what happened, what went wrong, what was learned), and its final report is added to it. CORVX then commits that journal file, and only that file.

How much agents do without asking is a dial you set per project, and can change at any time (`corvx project autonomy`, or `-`/`+` in the TUI rules view):

| Level | Permission requests no rule covers | Built-in rules |
|---|---|---|
| `supervised` | come to you; orchestrators cannot decide them or turn on autopilot | ask you |
| `guided` (default) | an orchestrator decides its agents' requests, and escalates what it should not decide | ask you |
| `autonomous` | approved without asking | ask you, except discarding git work on the agent's own branch |
| `full` | approved without asking | allow, except an agent changing CORVX's own rules |

Rules you set yourself always win over the level, so "autonomous, but always ask me before `git push`" or "full, but never `sudo`" both work. And when you keep approving the same thing (three times for the same simple command, or for requests under the same rule), CORVX offers in the inbox to allow it without asking from now on; it never changes a rule by itself. An allowed command may take other arguments, but not be chained, piped or redirected.

CORVX rules decide some requests themselves, whoever would otherwise decide them (the orchestrator, autopilot, or Claude Code's own settings). Each rule asks you, allows the request at once, or denies it at once, and you can change that at any time, for one project or all of them: `corvx rules set`, the rules view in the TUI (`l`; `h` ask, `y` allow, `n` deny, `o` off, `d` default, `g` switches between the selected project and every project), or `L` on a request a rule sent you, which allows that rule in its project from now on and approves the request. When several rules match, the strictest wins (deny, then ask, then allow), so a broad allow does not override a narrower ask. Built-in rules, all set to ask: `git push`, discarding git work (`reset --hard`, `clean -f`, `branch -D`), deleting directories, publishing (`npm publish`, `gh pr create`, ...), `sudo`, anything touching credentials or keys, an agent changing CORVX's own rules or approvals, and writing files outside the agent's working directory (temporary files excepted). You can add rules of your own (a regular expression on commands or paths). Every automatic approval or denial is logged. Rules match what an agent asks to run as text, so they catch the usual ways of doing these things, not an agent determined to get around them.

Each project keeps what has been learned about it in plain files in its `.corvx/` folder: `brief.md` (yours: what it is, what matters now), `goals.md` (yours: goals and guideposts; agents may propose goals or marking one done), `decisions.md` and `conventions.md` (changed only through proposals you accept), a generated `map.md` (stack, commands, structure, entry points) and the mission `journal/`. `corvx knowledge init` creates them and adds a block to the project's `CLAUDE.md` that imports them, so every agent starts from them; orchestrators also read them with `read_knowledge`, and are told to cite the decisions and conventions that shape their plan. At the end of a mission an orchestrator proposes what should be remembered (`propose_knowledge`, one point each, with its evidence). Proposals reach your inbox: `y` accepts (the file is written and just that file is committed, so new worktrees see it), `e` edits it in `$EDITOR` first, and `n` rejects it with a reason. A rejected proposal is not accepted again, and agents see why it was rejected.

`CORVX_E2E_REAL_CLAUDE=1 python3 tests/e2e/run_e2e.py` also runs one scenario against real Claude Code. It uses your Claude account.

Data lives in `~/.local/share/corvx` (owner-only). Override with `CORVX_HOME` or `--data-dir`.

## Protocol

One JSON object per line over the daemon's socket. Methods are listed in `crates/corvx-proto/src/lib.rs`. Start with `daemon.info`, which returns the protocol version.
