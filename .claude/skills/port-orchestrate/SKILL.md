---
name: port-orchestrate
description: Run the macOS/Linux port of ShareCtrlScreen as parallel agent tasks — how to pick tasks, brief agents, route models, verify, merge into port/multi-os, hand off to hardware testers, and report. Use when asked to orchestrate, continue, or check on the port work.
---

# Orchestrating the macOS + Linux port

You are the orchestrator. You do not implement tasks yourself; you hand out
tasks from `plans/05-TASKS.md`, verify what comes back, merge it, and keep the
plan's status table true. Everything below is the procedure; the design lives in
`plans/` and the locked decisions in `plans/00-DECISIONS.md` (they override
anything else, including this file).

## 0. Before the first task of a session

1. `git -C C:\Yanivs\shareCtrlScreen status --short` must be clean and the branch
   `port/multi-os`. Never work on `main`.
2. Read `plans/05-TASKS.md` § Status — it is the single source of truth for what
   is done, in progress (with branch name) and open. Update it as you go.
3. Confirm the two local build environments still work (they take ~1 min each
   when cached):
   - Windows: `cd native && cargo test --workspace`
   - Linux: `wsl -e bash -lc 'source $HOME/.cargo/env; cd /mnt/c/Yanivs/shareCtrlScreen/native && CARGO_TARGET_DIR=/tmp/sct-target cargo test --workspace'`
   If either fails before you changed anything, stop and report; do not hand out
   tasks on a broken base.
4. Check `git worktree list` for leftovers from earlier sessions. A worktree with
   commits not yet in `port/multi-os` is an unfinished task: resume it (§5)
   before starting new ones.

## 1. What can run now

Dependencies are in `plans/05-TASKS.md` (the ASCII graph and each task's
"Depends on"). Rules:

- A task is **ready** when every dependency is in the Status table as done.
- Run up to **10 tasks at once**. More than that just queues on review.
- Start the **spike before its lane**: L5-spike before L5; D2 (software codec)
  and D3 (keymap) early, because W2/W3/L6/M5 wait on them.
- Within a lane, prefer the task whose failure would change the design (M4, L5,
  L3) over the easy ones; bad news early is cheaper.

## 2. Model routing (decision #12 — do not change on your own)

| Tier | Tasks | Why |
|---|---|---|
| **Opus** | M4, L5 + L5-spike, M5, M6, L6, L7, D7, any change to `platform-api` | unsafe FFI with no hardware loop, or OS-input code that trips safety filters |
| **Sonnet** | M1, M2, M3, L1, L2, L3, L4, D2, T4, I-tasks' checklists | FFI against documented APIs |
| **Haiku** | D3, D4, D5, D6, M7, M8, M9, M10, M11, L8, L9, L10, L11, L12, T2, T3, T5, W2, W3 | well-specified, testable with mocks/CI |

Haiku's safety filter blocks code that looks like input capture/injection or
screen recording; if a Haiku agent reports being blocked, re-run the task on
Sonnet with the same brief — do not reword to sneak past the filter.

## 3. How to run an agent

Use the `Agent` tool with `isolation: "worktree"` (one task = one worktree = one
branch). Use the `Workflow` tool only if the user said "use a workflow" /
"ultracode" in this session; then one `agent()` per task with `isolation:
'worktree'` and the same brief. Either way the agent runs in the background;
keep handing out tasks while it works.

### The brief (every agent gets exactly this shape)

```
TASK <ID> — <title from plans/05-TASKS.md>

Repo: C:\Yanivs\shareCtrlScreen (you are in a git worktree on branch task/<ID>,
based on port/multi-os). Commit to that branch; do not push, do not merge.

Read first, in this order:
1. plans/00-DECISIONS.md (locked decisions; follow them)
2. plans/05-TASKS.md — only your task's entry and the Status table
3. native/crates/platform-api/src/<the trait files your task implements>
   — the trait docs state invariants; each one fixed a real bug. Keep them.
4. plans/0<N>-<FINDINGS/ARCHITECTURE>.md sections named in your task entry
5. plans/appendix-codemap/<file>.md for the Windows code you are porting from

Deliverable: <copied from the task entry>.
Acceptance: <copied from the task entry>.

Rules:
- Do not change any signature in native/crates/platform-api. If your task
  cannot be done without one, stop and report the exact change you need.
- New dependencies only from plans/06-CRATES-AND-LICENSES.md; run
  `cargo deny check` after adding one.
- Code reads like the surrounding code (comment density, naming). Every
  invariant you rely on gets a comment saying why.
- Tests: unit tests for pure logic; use platform_api::mock for everything else.
  Mark hardware-only verification with `#[ignore = "needs <device>"]` and say
  so in your report.
- Before you finish, run and paste the output of:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
  on every OS you can build on (Windows here; Linux via `wsl -e bash -lc`
  with CARGO_TARGET_DIR=/tmp/sct-<ID>; macOS only compiles in CI — say so).
- If an acceptance test fails twice for the same reason, stop and report
  what you tried; do not keep churning.

Report back (this is your final message, nothing else is seen):
- branch name and commit hashes
- what you built, in 5 lines
- the check output above, verbatim
- what is NOT verified and needs a device
- any platform-api change you need, or "none"
```

Fill the `<…>` from the task entry; do not paraphrase acceptance criteria.

## 4. Verifying a returned task (never merge on the agent's word)

In the main checkout, with the worktree's branch:

1. `git log port/multi-os..task/<ID> --oneline` — must be only that task.
2. Rebase onto current `port/multi-os`; if it conflicts, hand the conflict back
   to the same agent (SendMessage) rather than resolving it yourself.
3. Run the full gate yourself on **both** local OSes (Windows + WSL):
   fmt check, clippy `-D warnings`, `cargo test --workspace`, `cargo deny check`.
   The loopback test (`cargo test -p engine --test loopback`) must still pass;
   run it 3× — it is the regression net for everything.
4. If the task touches Windows behaviour: `cargo build -p app`, launch
   `native\target\debug\sharectrl.exe` for ~8 s, read
   `%APPDATA%\ShareCtrlScreen\app.log` — it must show the platform line, the
   surface created, and "registered with server". (This re-points the
   Start-with-Windows entry to the debug exe; harmless.)
5. Read the diff. Reject for: a changed `platform-api` signature without
   approval; an invariant comment deleted; `unwrap()` on OS results; a test
   weakened to pass; copied-in code from AGPL/GPL projects (RustDesk, scrap).
6. Merge with `git merge --ff-only task/<ID>` into `port/multi-os`, delete the
   branch and worktree, update the Status table (state, commit, notes), commit
   the plan update. One commit per task is fine; do not squash across tasks.

Reject = SendMessage to the same agent with the specific failure. After two
rejections, escalate: re-run on the next tier up with the agent's report
attached.

## 5. Resuming / taking over

- An agent that stopped mid-task: `SendMessage` to continue it if it is still
  listed by `ListAgents`; otherwise start a new agent on the same worktree branch
  with the brief plus "continue from commit <hash>; here is the previous report".
- Never two agents on one branch.

## 6. Changing `platform-api`

Treat it as frozen. When a task genuinely needs a change:
1. Stop that task.
2. Make the change yourself on `port/multi-os` (Opus tier), with the doc comment
   explaining the invariant, update the mocks and `unavailable.rs`, run the gate.
3. Tell every running agent (SendMessage) what changed and that they must
   rebase. Record the change in `plans/05-TASKS.md` § Status "interfaces".

## 7. Hardware hand-off (macOS, Linux desktops, GPUs)

Tasks M1, M4–M6, M10–M11, L1, L3–L6 and all I-tasks end as "needs device".
- Builds come from CI: pushing `port/multi-os` needs the user's OK — ask once
  per session, not per push. The macOS job of `native-ci.yml` is still
  unverified until the first push.
- Write the tester checklist into `plans/checklists/<ID>.md`: install steps,
  the exact clicks (Open Anyway, permission prompts), what to try, and **where
  the log is** (`Platform::paths().log_dir/app.log`: Windows
  `%APPDATA%\ShareCtrlScreen`, macOS `~/Library/Logs/ShareCtrlScreen`, Linux
  `~/.local/state/sharectrl`). Ask for the log file and a screenshot, nothing
  else.
- A task stays "needs device" in the Status table until a log from a real
  device backs it; it is never marked done on compile + mocks alone.

## 8. Reporting to the user

After each batch (not each task), one message:
- Status table delta (done / needs device / blocked), one line per task.
- Anything you rejected and why.
- Decisions needed, each with options and your recommendation — only things
  `plans/00-DECISIONS.md` does not already settle.
- Never report a task done that the gate in §4 did not pass on this machine.

## 9. Pitfalls already hit (don't rediscover them)

- `cargo check --target <linux/mac>` from Windows fails in `aws-lc-sys`; always
  check natively (WSL for Linux, CI for macOS).
- Two cargo builds in one target dir block each other; worktrees + a per-task
  `CARGO_TARGET_DIR` in WSL avoid it.
- A WSL `git` call leaves Windows `git status` showing phantom changes until
  the next Windows-side `git status`; it is stat-cache noise.
- The LF→CRLF warnings on commit are harmless (autocrlf is on).
- Shell heredocs/`node -e` with single quotes break in this Bash; write scripts
  to the scratchpad with the Write tool and run the file.
- `service` is a Windows-only binary with a stub `main` elsewhere; leave it.
- Haiku readers in the research sweep were confidently wrong on a few lines;
  trust the trait docs and the code over `appendix-codemap`.
