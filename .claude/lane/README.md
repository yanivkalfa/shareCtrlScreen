# The lane — one queue for every cargo run on this machine

Every heavy command (cargo check / clippy / test / build, npm build) on this box
goes through the lane. Agents never run cargo themselves; they write a request
file and the drain runs it, at most **5 requests at a time**, each with
`CARGO_BUILD_JOBS=4` (24 threads on the machine, ~20 for builds). A hook on the
agents' shell refuses raw `cargo`.

```
.claude/lane/
  README.md          this file
  drain.ps1          scheduler: claims requests, keeps ≤ N workers alive (N = slots file, default 5)
  run-request.ps1    worker: runs one request's steps in its worktree, writes the result
  lane-up.ps1        watcher: starts the drain if it is not running (idempotent)
  guard.js           PreToolUse hook: refuses `cargo` in Bash/PowerShell tool calls
  seed-cache.sh      hard-links the warm base caches into a task worktree (Windows + WSL)
  slots              optional: a single number overriding the 5-slot default (read every tick)
  queue/             <task>-<nn>.request  — waiting
  running/           claimed requests (+ <name>.pid of the worker)
  done/              <name>.request + <name>.result — finished
  logs/              lane.log, drain.pid, <name>.step<i>.log (full output of every step)
```

## Request file

`queue/<TASK>-<nn>.request` — `<TASK>` is the task id (M2, L5, BASE for the
integration branch), `<nn>` a running number that is never reused. UTF-8, LF or
CRLF, any text.

```
# why: first full check after implementing the encoder session
task: M2
side: windows            # windows | wsl
cwd: C:\Yanivs\shareCtrlScreen\.claude\worktrees\agent-xyz\native
priority: normal         # normal | gate   (gate runs before every normal request)
---
cargo fmt --all --check
cargo check -p platform-macos --target aarch64-apple-darwin
cargo clippy -p platform-macos --target aarch64-apple-darwin -- -D warnings
```

Rules the worker enforces:

- `side: windows` runs each line through `cmd` in `cwd`.
  `side: wsl` runs it through `wsl -e bash` in the WSL view of `cwd`
  (`/mnt/c/...`) with `source ~/.cargo/env` and
  `CARGO_TARGET_DIR=$HOME/sct/<task>` (`BASE` → `$HOME/sct/base`; override with
  a `wsl_target:` header line).
- A line must start with `cargo`, `npm` or `npx`; shell metacharacters
  (`; & | < > `` ` `` $( `) are refused — one command per line, nothing else.
- Steps run in order; the first failing step stops the request.
- A step whose log has not grown for 30 minutes is killed as STUCK; a step over
  3 hours is killed as TIMEOUT. Both are FAIL.
- `CARGO_BUILD_JOBS=4` is set for every step.

Result: `done/<name>.result` — `STATUS: PASS|FAIL`, one line per step with
exit code and duration, the path of each step's full log, and the last lines of
the failed step. The request file moves to `done/` beside it.

## The agent loop

1. Edit code in your worktree.
2. Write `queue/<TASK>-<nn>.request` (next number).
3. End your turn with the single line `QUEUED <TASK>-<nn>`.
4. You are resumed with the path of the `.result`. Read it (and the step logs it
   names), fix, write `<nn+1>`.

Nobody edits a worktree while its request is queued or running.

## Operator commands

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .claude\lane\lane-up.ps1   # start / ensure the drain
Get-ChildItem .claude\lane\queue, .claude\lane\running                           # what is waiting / running
Get-Content .claude\lane\logs\lane.log -Tail 30
Set-Content .claude\lane\slots 3                                                # change the concurrency
```

Seeding a worktree's caches (run once, from inside the worktree, before the
first request):

```bash
bash .claude/lane/seed-cache.sh <TASK>
```
