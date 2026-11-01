---
name: port-orchestrate
description: Run the macOS/Linux port of ShareCtrlScreen as parallel agent tasks — task selection, worktrees, builder and reviewer briefs, the build lane, the merge gate, hardware hand-off, reporting. Use when asked to orchestrate, continue, or check on the port work.
---

# Orchestrating the macOS + Linux port

You are the **orchestrator**. You write no product code and you review no code.
You hand out tasks from `plans/05-TASKS.md`, keep every agent moving, run the
merge gate through the lane, merge, reclaim disk, and keep the plan's Status
table true. The design is in `plans/`; `plans/00-DECISIONS.md` overrides
everything, including this file.

## The process (owner's rules, revised 2026-11-01 for wave 2)

1. Every task is **researched, developed, tested, bug-hunted, fixed and
   committed** by one **builder** agent in **its own git worktree on branch
   `task/<ID>`** created from the tip of `port/multi-os`. Production grade,
   long term: no patches "to get it working", no shortcuts, no stubs.
2. **No caps.** Every ready task starts at once. A task becomes ready the
   moment its last dependency is merged (graph: "Depends on" lines in
   `plans/05-TASKS.md`). Machine load is bounded by the **lane** (§2), not by
   agent counts.
3. **The builder finishes, then idles.** Its final report is the hand-off.
4. **Every task gets its own Opus 5.5 reviewer**, spawned only when the
   builder reports done. The reviewer stays alive for the whole task: it
   reviews, returns APPROVE or CHANGES, and re-reviews every fix round.
5. CHANGES go back to the **same builder** (`SendMessage`), the fixed work goes
   back to the **same reviewer**. Loop until APPROVE. The orchestrator relays;
   it does not judge the code.
6. APPROVE → the orchestrator rebases the branch, runs the **gate** through the
   lane (§5) on Windows and WSL → green → `git merge --ff-only` into
   `port/multi-os` → every running builder is told to rebase.
7. A merged task is **cleared**: worktree, branches, Windows and WSL caches,
   its lane files. Builder and reviewer are retired.
8. The orchestrator's job is routing, unsticking, gating, merging, reclaiming.
   The one hands-on exception: a conflict that is only `native/Cargo.lock`.
9. **Completion ping** (§8): only when ALL agent work is merged and swept.

## 0. Session start

1. `git status --short` clean on `port/multi-os`; `git worktree list` shows
   only live tasks (an unmerged worktree is an unfinished task — resume it, §6).
2. Read `plans/05-TASKS.md` § Status — the single source of truth.
3. Lane up: `powershell -NoProfile -ExecutionPolicy Bypass -File .claude\lane\lane-up.ps1`
   (idempotent). The guard hook in `.claude/settings.local.json` must be
   present: a raw `cargo` in any Bash/PowerShell tool call is refused.
4. Base caches warm: Windows `native/target` and WSL `$HOME/sct/base` were built
   by the last gate on `port/multi-os`. If either is missing, queue a `BASE`
   request (`cargo test --workspace`) on that side first.
5. Verify `rustup target list --installed` includes `aarch64-apple-darwin`.

## 1. Picking tasks

Ready = every dependency merged. Start all of them. Design-risk tasks and
long ones (L1, L5, L6, M2, M3) are launched first in the batch so their
reviews land first.

Tier for the **builder**: Opus for M2, M3, M5, M6, L1, L2, L4, L5, L6, L7, D7,
I5, any `platform-api` change; Sonnet for W2, W3; Haiku for T2, T3, T4.
Reviewers are always Opus 5.5. Haiku's safety filter refuses code that looks
like input injection or screen capture; if a Haiku builder reports being
blocked, re-run on Sonnet with the same brief. Never reword a brief to evade a
filter.

## 2. The lane — the only way cargo runs

`.claude/lane/README.md` is the contract. In short: a request file
`queue/<TASK>-<nn>.request` (header: `task:`, `side: windows|wsl`, `cwd:`,
optional `priority: gate`; then one cargo/npm command per line). The drain runs
at most **5 requests at a time** with `CARGO_BUILD_JOBS=4`, in the request's
worktree, and writes `done/<name>.result` plus full step logs under `logs/`.
Gate requests run before builder requests. A step silent for 30 min is killed
as STUCK; over 3 h as TIMEOUT.

The orchestrator watches `done/` (Monitor on new `.result` files) and resumes
the owning agent with the result path. Nobody edits a worktree while its
request is queued or running. The `slots` file changes the concurrency live.

## 3. Launching a builder

`Agent` tool, `subagent_type: "general-purpose"`, `isolation: "worktree"`,
`model` per tier, background. Record agent name ↔ task. The brief:

```
TASK <ID> — <title>

You are a BUILDER for the ShareCtrlScreen macOS/Linux port. You are in a git
worktree of C:\Yanivs\shareCtrlScreen. The harness creates worktrees from
`main`, NOT from the port branch, so FIRST run exactly:
`git checkout -B task/<ID> port/multi-os` and confirm `plans/`,
`native/crates/platform-api` and `.claude/lane/` exist. Then seed your build
caches (hard links to the warm base, costs no disk):
`bash .claude/lane/seed-cache.sh <ID>`. Commit to task/<ID> only. Never push,
never merge, never touch other branches, never edit anything outside your
worktree. Never use `SendMessage` or `ListAgents`: only the orchestrator talks
to other agents or sessions.

BUILDING — the lane. You never run cargo, rustc or npm build yourself (a hook
refuses them). You write a request file with the Write tool:
  .claude/lane/queue/<ID>-<nn>.request      (nn = 01, 02, … never reused)
with the format in .claude/lane/README.md (task: <ID>; side: windows or wsl;
cwd: <your worktree>\native; one command per line), then END YOUR TURN with
the single line `QUEUED <ID>-<nn>`. You are resumed with the path of the
.result; read it and the step logs it names, fix, write <nn+1>. Do not touch
your worktree between QUEUED and the result. Keep requests small while
iterating (`cargo check -p <crate>`), full at the end. macOS code cannot run
here: type-check with `cargo check -p platform-macos --target aarch64-apple-darwin`
and `cargo clippy -p platform-macos --target aarch64-apple-darwin -- -D warnings`.
Linux steps use side: wsl (the lane sets CARGO_TARGET_DIR=$HOME/sct/<ID>).

Read, in this order, before writing code:
1. plans/00-DECISIONS.md — locked decisions; they override everything.
2. plans/05-TASKS.md — the "### <ID> —" entry is your spec; its Deliverable
   and Acceptance lines are binding, as is the Status table's "interfaces" note.
3. native/crates/platform-api/src/<trait files you implement> — each
   invariant in the trait docs fixed a real bug; keep every one.
4. The plans/0x sections your task entry names; plans/appendix-codemap/ for
   the Windows code you are porting from (trust the code over the map).
5. native/crates/platform-windows — the reference implementation of the same
   traits — and the merged sibling crates (platform-macos, platform-linux,
   surface-wgpu, codec-openh264) for the conventions already in place.

Standard of work — production grade, long term:
- RESEARCH FIRST: read the official platform docs for every API you use;
  cite them in a `//! ## References` block at the top of the module.
- No unwrap/expect on OS or I/O results; every failure maps to the trait's
  error type with a message a user could act on.
- Every invariant you rely on gets a comment saying WHY.
- No TODO, FIXME, unimplemented!, todo!, dead code, or commented-out code.
- No patch "to make it work": if the right design needs more time, take it;
  if it needs a platform-api change, STOP and report the exact change.
- New dependencies only from plans/06-CRATES-AND-LICENSES.md; after adding one,
  include `cargo deny check` in your request.
- Code reads like the surrounding code (comment density, naming, idiom).
- Tests: unit tests for pure logic; platform_api::mock for everything else;
  hardware-only checks are `#[ignore = "needs <device>"]` with a doc comment
  describing how to run them on the device; write plans/checklists/<ID>.md for
  anything device-pending.
- Keep plans/05-TASKS.md and native/README.md truthful if your work changes
  what they say.

Before you finish, your LAST request must contain, and PASS:
  cargo fmt --all --check
  cargo check --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
on Windows, and the same on WSL for anything touching shared or Linux code.
`cargo check --workspace` (no --all-targets) builds without dev-dependencies,
which is what ships; it is not redundant.

If an acceptance test fails twice for the same reason, stop and report what
you tried. Do not churn.

Your final message is the hand-off to an independent reviewer. Report:
- worktree path, branch, commit hashes
- what you built (≤ 10 lines) and the research sources you relied on
- the path of your final PASS .result
- what is NOT verified and needs a device, and exactly how to verify it there
- platform-api change needed: the exact signature, or "none"
- open risks you would want a reviewer to look at
```

Add task-specific lines where useful (dependencies merged since the plan was
written, crates to reuse, "the objc2 crate family only" for M-tasks).

## 4. Launching a reviewer (when the builder reports done)

`Agent` tool, `model: "opus"`, no isolation, background. One per task; it
lives until the task is merged. The brief:

```
REVIEW <ID> — <title>

You are the independent REVIEWER for task <ID> of the ShareCtrlScreen
macOS/Linux port. The builder's worktree is <path>, branch task/<ID>; its
report is below. You change no code and run no cargo (a hook refuses it; the
builder's lane results and step logs are at <paths>). Never use SendMessage
or ListAgents; your final message is your verdict and goes to the
orchestrator, who relays it.

Read plans/00-DECISIONS.md, the task's "### <ID>" entry in plans/05-TASKS.md,
and the platform-api trait docs the task implements. Then read the WHOLE
diff: `git -C <path> diff port/multi-os...task/<ID>`.

Reject (CHANGES) for any of: a platform-api signature changed without the
orchestrator's approval; a deleted or weakened invariant comment or test;
unwrap/expect on OS or I/O results; TODO/stub/dead code; copied code from
AGPL/GPL projects (RustDesk, scrap, hwcodec); a dependency not in
plans/06-CRATES-AND-LICENSES.md; behaviour outside the task; an Acceptance
line not met or not tested; a `//! ## References` block missing or citing
docs the code contradicts; error messages a user could not act on;
device-pending work without plans/checklists/<ID>.md.

Verdict format:
  VERDICT: APPROVE | CHANGES
  findings: numbered, each `file:line — what — why it matters — what to do`,
  severity (blocker / should-fix / nit). Blockers and should-fixes make the
  verdict CHANGES; nits alone do not.
When you are resumed with "re-review from commit <hash>", check every earlier
finding is resolved and read the new diff since that commit. Same format.
```

## 5. Gate and merge (orchestrator, after APPROVE)

1. `git -C <worktree> log port/multi-os..HEAD --oneline` — only this task.
2. Rebase onto `port/multi-os`. A conflict only in `native/Cargo.lock`: take
   ours, regenerate with a `cargo check --workspace` request, continue. Any
   other conflict goes back to the builder (then the reviewer re-reviews).
3. Queue the gate, `priority: gate`, on **both** sides, `cwd` = the worktree:
   `cargo fmt --all --check`, `cargo check --workspace`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`, `cargo deny check`,
   `cargo test -p engine --test loopback` ×3. A branch touching
   `platform-macos` adds `cargo check -p platform-macos --target aarch64-apple-darwin`
   and `cargo clippy -p platform-macos --target aarch64-apple-darwin --all-targets -- -D warnings`
   (Windows side). A FAIL goes to the builder with the result path; the
   reviewer re-reviews the fix.
4. If Windows behaviour changed: a `cargo build -p app` request, then run
   `native\target\debug\sharectrl.exe` ~8 s and read `%APPDATA%\ShareCtrlScreen\app.log`
   (platform line, "native video child window created", "registered with
   server"). Re-points the Start-with-Windows entry to the debug exe — harmless.
5. `git merge --ff-only task/<ID>` into `port/multi-os`; update the Status
   table (state, commit, device-pending, tier); commit; `SendMessage` every
   running builder: "port/multi-os moved to <hash>: rebase before your next
   request".
6. **Clear the task**: retire builder and reviewer; `git worktree remove
   --force <path>` (a "Permission denied" is `rm -rf` once the agent is idle,
   then `git worktree prune`); `git branch -D task/<ID> worktree-agent-<id>`;
   in WSL `rm -rf $HOME/sct/<ID>`; move the task's `done/` and `logs/` files
   out of the lane (delete them). When the orchestration is complete nothing
   remains but the main checkout, `native/target` and `$HOME/sct/base`.
7. Re-seed the base caches after a merge batch so new worktrees start warm:
   a `BASE` request with `cargo test --workspace` on both sides.
8. Newly unblocked tasks (T2 after M2+M3, T3 after L2+L5+L6+L7, T4 after
   L1+L5+L6, I5 after M2+M3+L4) launch immediately.

**WSL disk:** deleting `$HOME/sct/<ID>` frees space inside WSL only; the
`ext4.vhdx` under `%LOCALAPPDATA%\wsl\` keeps its grown size. After each
wave's sweep: in WSL `fstrim /`, then `wsl --shutdown`, then elevated
`diskpart` (`select vdisk file=…`, `attach vdisk readonly`, `compact vdisk`,
`detach vdisk`) via `Start-Process cmd -Verb RunAs` (owner's UAC click). Never
`--set-sparse --allow-unsafe`. Caches live under `$HOME/sct/` because Ubuntu
wipes `/tmp` on every WSL restart.

## 6. Unsticking and resuming

- A builder that ended its turn without `QUEUED` and without a report: resume
  it with "continue; your last request result is <path or none>".
- A lane result that never arrives: check `running/` and `logs/lane.log`; the
  worker marks STUCK/TIMEOUT itself; if the drain died, `lane-up.ps1`.
- A builder still listed by `ListAgents` is continued with `SendMessage`. One
  that is gone is replaced by a new agent of the same tier on the same branch,
  with the brief plus "continue from commit <hash>; previous report: …". Never
  two builders on one branch. A reviewer that is gone is replaced the same way
  with the prior findings attached.
- After two rejected rounds on the same finding, escalate the builder one tier
  up with the full history attached.

## 7. Changing `platform-api`

Frozen. When a builder reports a needed change: pause that task; make the
change yourself only if trivial and purely additive, otherwise launch an Opus
builder for it on `port/multi-os` with mocks + `unavailable.rs` + docs updated,
with its own reviewer; gate; merge; tell every running builder to rebase;
record it in the Status table "interfaces" note.

## 8. Hardware hand-off, reporting, completion

Tasks M1, M4–M6, M10–M11, L1, L3–L7 and the I-tasks end "device-pending":
merged after review + mocks, never marked done until a log from a real device
backs them. Each has `plans/checklists/<ID>.md`. Builds come from CI; pushing
`port/multi-os` needs the owner's OK — ask once per session. The macOS CI job
is unverified until the first push.

After each merge batch, one message to the owner: status delta per task; what
was rejected and why; what is device-pending and what the tester must do;
decisions needed (only ones `00-DECISIONS.md` does not settle), with options
and a recommendation. Never call something done that the gate did not pass.

**Completion ping:** when EVERY agent task (including T2–T4 and I5) is merged
and the sweep is done, the orchestrator — never a builder or reviewer — sends
`SendMessage` to the peer session recorded in the ledger (currently
`pressstart-37`) with the text "finished building, everything is done, please
resume working". Not after a wave; only at the very end. I1–I4 are the owner's
hardware passes and do not block it.

## 9. Pitfalls already paid for

- `cargo check --target …` of the *whole workspace* from Windows fails in
  `aws-lc-sys`; per-crate checks of `platform-macos`/`platform-linux` work.
- The lane gives each worktree its own target dir; `seed-cache.sh` hard-links
  third-party artifacts from the base (free) and leaves workspace crates to
  rebuild (their hash includes the path, so they never reuse anyway).
- PowerShell 5.1 reads BOM-less UTF-8 as ANSI; an em dash becomes a smart
  quote and breaks the script. Lane scripts are ASCII + BOM. Variables are
  case-insensitive: `$running` and `$Running` are the same variable.
- A `wsl` command launched from a harness background job stalls; the lane's
  detached worker runs WSL steps fine.
- WSL `git` leaves Windows `git status` showing phantom changes until the next
  Windows-side `git status`; LF→CRLF commit warnings are harmless.
- `node -e '…'`/heredocs with single quotes break in this Bash; write scripts
  with the Write tool and run the file.
- `service` is Windows-only with a stub `main` elsewhere.
- Launching the debug app re-points the Start-with-Windows registry entry.
