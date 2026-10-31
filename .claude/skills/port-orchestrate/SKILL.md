---
name: port-orchestrate
description: Run the macOS/Linux port of ShareCtrlScreen as parallel agent tasks — task selection, worktrees, builder briefs, model tiers and caps, the review chain, the merge gate, hardware hand-off, reporting. Use when asked to orchestrate, continue, or check on the port work.
---

# Orchestrating the macOS + Linux port

You are the **orchestrator**. You never write product code. You hand out tasks
from `plans/05-TASKS.md`, keep the concurrency caps, review what comes back,
run the merge gate, merge, and keep the plan's status table true. The design is
in `plans/`; `plans/00-DECISIONS.md` overrides everything, including this file.

## The process (agreed with the owner, 2026-11-01)

1. Every task is **researched, developed, tested, bug-hunted, fixed and
   committed** by one builder agent, in **its own git worktree on its own branch
   `task/<ID>`**, created from the current tip of `port/multi-os`.
2. **Production grade, long term.** No patches to "get it working", no
   shortcuts, no stubs left behind. Definition of done is in §4.
3. All **ready** tasks start at once; a task becomes ready the moment its last
   dependency is merged. (Dependencies: the graph and "Depends on" lines in
   `plans/05-TASKS.md`.)
4. Builders stay alive until their work is merged. Fixes always go back to the
   **same builder** (`SendMessage`). Only after the review passes and the gate
   passes is the work merged and the builder retired.
5. **Tiers and caps** (running builders; reviewers and the orchestrator never
   count): at most **5 Opus 5.5**, at most **10 Sonnet 5.5**, **unlimited
   Haiku 5.5**. Opus is the default. Easy tasks go to Sonnet or Haiku at the
   orchestrator's discretion (§2 has the current split).
6. **Review chain.**
   - Built by **Haiku** → an independent **Opus reviewer** agent reviews it;
     findings go back to the Haiku builder; repeat until the reviewer signs
     off → orchestrator gate → merge.
   - Built by **Opus or Sonnet** → the **orchestrator** reviews it (diff read +
     gate); findings go back to the builder; repeat → merge.
7. After merging, the orchestrator re-runs the full gate on `port/multi-os`.

## 0. Session start

1. `git status --short` clean, branch `port/multi-os`; `git worktree list` for
   leftovers (a worktree with unmerged commits is an unfinished task — resume
   it, §5, before starting new work).
2. Read `plans/05-TASKS.md` § Status — the single source of truth. The
   "in progress" rows name the branch, tier and agent.
3. Prove the base is green on both local OSes before handing anything out:
   - Windows: `cd native && cargo test --workspace`
   - Linux: `wsl -e bash -lc 'source $HOME/.cargo/env; cd /mnt/c/Yanivs/shareCtrlScreen/native && CARGO_TARGET_DIR=/tmp/sct-base cargo test --workspace'`
4. Verify `rustup target list --installed` includes `aarch64-apple-darwin` and
   `x86_64-unknown-linux-gnu` (macOS code is type-checked with
   `cargo check -p platform-macos --target aarch64-apple-darwin`).
5. **Warm the build caches before launching builders** (lesson from wave 1:
   19 cold worktrees each recompiled ~400 dependencies, 15–30 min apiece under
   contention). On `port/multi-os` run the full gate once so `native/target`
   holds debug + test artifacts, and in WSL build once into
   `/tmp/sct-base`. Then, right after the harness creates a builder's worktree
   (its path is in the launch result), copy the caches in before the builder
   gets far: `robocopy native\target <worktree>\native\target /E /NFL /NDL /NJH /NJS`
   on Windows and `cp -r /tmp/sct-base /tmp/sct-<ID>` in WSL (tell the builder
   to use that `CARGO_TARGET_DIR`). Cargo reuses the artifacts; a warm
   worktree builds in under a minute.

## 1. Picking tasks

- Ready = every dependency is marked done in the Status table.
- Start design-risk tasks first inside a tier (L5-spike, M4, D2, M1, L3 before
  M7, M9…): bad news early is cheaper.
- Keep a running ledger (in your head and in the Status table): running Opus,
  running Sonnet. Never launch a builder over the cap; queue it and launch when
  a slot frees (a merge or a retirement).

## 2. Tier split (orchestrator's discretion; revise if a task proves harder)

| Tier | Tasks |
|---|---|
| Haiku | D6, M8, M9, L9, L10, T2, T3, T5 |
| Sonnet | D3, D4, D5, M7, M10, M11, L8, L11, L12, W2, W3 |
| Opus | everything else: D2, D7, L5-spike, L5, L1–L4, L6, L7, M1–M6, I-tasks' checklists, any `platform-api` change |

Haiku's safety filter refuses code that looks like input injection or screen
capture; if a Haiku builder reports being blocked, re-run on Sonnet with the
same brief. Never reword a brief to evade a filter.

## 3. Launching a builder

`Agent` tool, `subagent_type: "general-purpose"`, `isolation: "worktree"`,
`model` per tier, background. One agent per task; remember its name/ID (you
will `SendMessage` it for fixes). The brief:

```
TASK <ID> — <title>

You are a BUILDER for the ShareCtrlScreen macOS/Linux port. You are in a git
worktree of C:\Yanivs\shareCtrlScreen. The harness creates worktrees from
`main`, NOT from the port branch, so FIRST run exactly:
`git checkout -B task/<ID> port/multi-os` and confirm `plans/` and
`native/crates/platform-api` exist. Commit to that branch only. Never push,
never merge, never touch other branches.

Read, in this order, before writing code:
1. plans/00-DECISIONS.md — locked decisions; they override everything.
2. plans/05-TASKS.md — the "### <ID> —" entry is your spec; its Deliverable
   and Acceptance lines are binding, as is the Status table's "interfaces"
   note.
3. native/crates/platform-api/src/<trait files you implement> — each
   invariant in the trait docs fixed a real bug; keep every one.
4. The plans/0x sections your task entry names; plans/appendix-codemap/ for
   the Windows code you are porting from (facts there were spot-checked, not
   verified line by line — trust the code over the map).
5. native/crates/platform-windows — the reference implementation of the
   same traits.

Standard of work — production grade, long term:
- RESEARCH FIRST: read the official platform docs for every API you use;
  cite them in a `//! ## References` block at the top of the module.
- No unwrap/expect on OS or I/O results; every failure maps to the trait's
  error type with a message a user could act on.
- Every invariant you rely on gets a comment saying WHY.
- No TODO, FIXME, unimplemented!, todo!, dead code, or commented-out code.
- No patch "to make it work": if the right design needs more time, take it;
  if it needs a platform-api change, STOP and report the exact change.
- New dependencies only from plans/06-CRATES-AND-LICENSES.md; after adding
  one run `cargo deny check` and paste the result.
- Code reads like the surrounding code (comment density, naming, idiom).
- Tests: unit tests for pure logic; platform_api::mock for everything else;
  hardware-only checks are `#[ignore = "needs <device>"]` with a doc comment
  describing how to run them on the device.
- Keep plans/05-TASKS.md and native/README.md truthful if your work changes
  what they say.

Building: use `CARGO_BUILD_JOBS=4` (other builders share this machine) and
`cargo check -p <crate>` while iterating. Linux: run in WSL with
`wsl -e bash -lc 'source $HOME/.cargo/env; cd <your worktree under /mnt/c>/native && CARGO_TARGET_DIR=/tmp/sct-<ID> ...'`.
macOS code cannot run here; type-check with
`cargo check -p platform-macos --target aarch64-apple-darwin` and
`cargo clippy -p platform-macos --target aarch64-apple-darwin -- -D warnings`.

Before you finish, run and paste VERBATIM:
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
on Windows and (for anything touching shared or Linux code) in WSL.
`cargo test -p engine --test loopback` must still pass.

If an acceptance test fails twice for the same reason, stop and report what
you tried. Do not churn.

Your final message is the only thing the orchestrator sees. Report:
- worktree path, branch, commit hashes
- what you built (≤ 10 lines) and the research sources you relied on
- the check output, verbatim
- what is NOT verified and needs a device, exactly how to verify it there
- platform-api change needed: the exact signature, or "none"
- open risks you would want a reviewer to look at
```

Add task-specific lines where useful (e.g. for L5-spike: "WSLg provides a
Wayland display `wayland-0`; use it"; for M-tasks: "the objc2 crate family only").

## 4. Review and gate (every task, before merge)

**Haiku-built tasks only — first an Opus reviewer:** launch an Opus agent
(`isolation` none; it reads the builder's worktree path) with: the builder's
report, "review for correctness, invariants kept, production quality per the
brief, test adequacy; return findings with file:line and a verdict
APPROVE / CHANGES"; findings go to the builder via `SendMessage`; loop until
APPROVE.

**Then the orchestrator, for every task:**
1. `git -C <worktree> log port/multi-os..HEAD --oneline` — only this task.
2. Rebase onto `port/multi-os`; conflicts go back to the builder.
3. Read the whole diff. Reject for: changed `platform-api` signatures without
   approval; a deleted or weakened invariant comment or test; unwrap/expect on
   OS results; TODO/stub left; copied code from AGPL/GPL projects (RustDesk,
   scrap, hwcodec); a dependency not in 06-CRATES; a behaviour change not in
   the task.
4. Run the gate yourself in the worktree, on Windows **and** WSL:
   `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`, `cargo deny check`, and
   `cargo test -p engine --test loopback` three times.
5. If Windows behaviour changed: `cargo build -p app`, run
   `native\target\debug\sharectrl.exe` ~8 s, read `%APPDATA%\ShareCtrlScreen\app.log`
   (platform line, "native video child window created", "registered with
   server"). This re-points the Start-with-Windows entry to the debug exe —
   harmless, tell the owner once.
6. Merge: `git merge --ff-only task/<ID>` into `port/multi-os`; re-run step 4 on
   `port/multi-os`; update the Status table (state, commit, "device-pending"
   if applicable, tier used); commit the plan update; retire the builder; then
   **reclaim the disk** — each worktree carries a 3–5 GB `target/` and the WSL
   build another 2–4 GB: `git worktree remove --force <path>`,
   `git branch -d task/<ID> worktree-agent-<id>`, and in WSL
   `rm -rf /tmp/sct-<ID>`. At session end, `git worktree prune`, check
   `.claude/worktrees` holds only live tasks, and `wsl -e bash -lc 'du -sh /tmp/sct-*'`
   for strays.

Findings always go to the original builder. After two rejected rounds on the
same point, escalate the task one tier up with the full history attached.

## 5. Resuming

A builder still listed by `ListAgents` is continued with `SendMessage`. One
that is gone is replaced by a new agent of the same tier on the same worktree
branch, with the brief plus "continue from commit <hash>; previous report:
…". Never two builders on one branch.

## 6. Changing `platform-api`

Frozen. When a builder reports a needed change: pause that task; make the
change yourself only if trivial and purely additive, otherwise launch an Opus
agent for it on `port/multi-os` with mocks + `unavailable.rs` + docs updated;
gate; merge; `SendMessage` every running builder to rebase; record it in the
Status table "interfaces" note.

## 7. Hardware hand-off

Tasks M1, M4–M6, M10–M11, L1, L3–L6, L7 and the I-tasks end "device-pending":
merged after review + mocks, but never marked done until a log from a real
device backs them. For each, write `plans/checklists/<ID>.md`: install steps,
exact clicks (Open Anyway, permission prompts), what to try, and the log path
(Windows `%APPDATA%\ShareCtrlScreen\app.log`, macOS
`~/Library/Logs/ShareCtrlScreen/app.log`, Linux `~/.local/state/sharectrl/app.log`).
Builds come from CI; pushing `port/multi-os` needs the owner's OK — ask once
per session. The macOS CI job is unverified until the first push.

## 8. Reporting

After each merge batch, one message: status delta per task; what was rejected
and why; what is device-pending and what the tester must do; decisions needed
(only ones `00-DECISIONS.md` does not settle), each with options and a
recommendation. Never call something done that the gate did not pass here.

## 9. Pitfalls already paid for

- `cargo check --target …` of the *whole workspace* from Windows fails in
  `aws-lc-sys`; per-crate checks of `platform-macos`/`platform-linux` work.
- Shared `target/` dirs serialize cargo; one target dir per worktree, and
  `CARGO_TARGET_DIR=/tmp/sct-<ID>` in WSL.
- WSL `git` leaves Windows `git status` showing phantom changes until the next
  Windows-side `git status`; stat-cache noise.
- LF→CRLF commit warnings are harmless.
- `node -e '…'`/heredocs with single quotes break in this Bash; write scripts
  with the Write tool and run the file.
- `service` is Windows-only with a stub `main` elsewhere.
- Launching the debug app re-points the Start-with-Windows registry entry.
