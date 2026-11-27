# UNLOCK-U2 — the service reacts to session events: tester checklist

What this proves: the SYSTEM service (`sharectrl-service.exe`) is told about stop, shutdown and
session changes by Windows; a sign-in at the console brings the host up within 5 s however long
the service had been waiting before; lock and unlock relaunch nothing; stop, restart and
shutdown still work. Plan: `plans/10-REMOTE-UNLOCK.md` U2, structure:
`plans/10a-STRUCTURE-AND-FLOWS.md` §3.7, §4.3.

What **is** covered on every OS by `cargo test -p service`:
* the relaunch policy with sign-ins, sign-outs and session events — `service/src/supervise.rs`
  (a sign-in after a long backoff launches at once and resets the delay; another session's events
  do nothing; event-then-poll and poll-then-event agree; repeated events are idempotent; a sign-in
  while the host runs, or after a clean exit while the same user stays, changes nothing; a
  sign-out and sign-in in the same session brings a cleanly-exited host back);
* the control and event codes, the handler's return values, and the hand-off from the handler to
  the loop (ordering, overflow, wake-ups from another thread, stop) — `service/src/events.rs`;
* Windows CI only: the plain codes equal the `windows` crate's constants
  (`the_plain_codes_match_the_windows_headers`).

**Not covered without hardware:** that Windows really delivers the notifications, the 5 s
acceptance, the SCM's handling of the stop and shutdown reports, and fast user switching. All
of it is unverified until this passes.

The log is `sharectrl-service.log` next to `sharectrl-service.exe` (normally
`C:\Program Files\ShareCtrlScreen\`), truncated at each service start. Read it from an elevated
terminal. Times below are from its timestamps.

## Before you start

* A Windows 10 or 11 machine with the build under test installed by the installer (the service
  is installed and running). Two local accounts, A and B, both with passwords; fast user
  switching enabled (the default).
* **Turn off automatic sign-in after a restart** for account A: Settings → Accounts → Sign-in
  options → "Use my sign-in info to automatically finish setting up after an update or restart"
  → Off. With it on, Windows signs A in (and locks) during the restart, the user's token is there
  at once, and step 1 shows neither the "no user token" line nor a sign-in.
* A second machine as the viewer, to see the host come up (optional: Task Manager on the host
  shows `sharectrl.exe` as well).

## Steps

1. **Sign-in after a long wait (the acceptance).** Restart Windows and leave it at the sign-in
   screen for at least **6 minutes** (before U2 the service's relaunch delay had reached its
   5-minute cap by then). Then sign in as A.
   Expect in the log, once at the start of the wait:
   `no user token for console session <N> (...): nobody is signed in there yet (or this process is not running as LocalSystem). The host starts as soon as someone signs in`
   — and **no** further lines while you wait (no `retrying in` lines). At the sign-in:
   `session event: logon in console session <N>`, then
   `a user signed in to console session <N>; launching the host` (or `... launching the host now instead of in <s>s`),
   then `launched engine pid=<pid> in session <N> with UIAccess`. The `launched engine` line is
   **at most 5 s** after the `logon` line; the host's tray icon appears.
2. **Service restarted at the sign-in screen.** Sign out. In an elevated terminal (over a remote
   shell, or before signing out schedule it with `schtasks`): `sharectrl-service --stop-service`
   then `sharectrl-service --start-service`. Sign in as A. Expect the same lines as step 1 and
   the host within 5 s. (If the service was started after the sign-in had begun, Windows may send
   no `logon` event: the `a user signed in` line then comes from the 2 s poll alone, still within
   5 s.)
3. **Lock and unlock relaunch nothing.** Signed in as A with the host running, press Win+L, wait
   10 s, unlock. Expect `session event: lock in console session <N>` and
   `session event: unlock in console session <N>`, and **no** `launched engine` line; the host's
   pid (Task Manager) is unchanged.
4. **Fast user switching.** Signed in as A, Switch user, sign in as B. Expect a
   `launched engine ... in session <M>` line for B's session within 5 s; B's host appears. Switch
   back to A: A's session gets a host (a new one if A's ended; a running one just steps aside,
   exit code 0). Events for the session that is not at the console are not logged at the
   default level.
5. **Sign out and back in, same account.** Signed in as A, Quit the host from its tray menu
   (`the host exited cleanly ...` in the log; the host stays down). Sign out, sign in as A again.
   Expect the host back within 5 s (`a user signed in ...` or a new session id and a
   `launched engine` line).
6. **A crash still backs off.** Signed in, end `sharectrl.exe` in Task Manager three times in a
   row, quickly. Expect `the host exited abnormally ... relaunching in 2s`, then `4s`, then `8s`,
   and the host back each time after that delay: a signed-in user does not reset the backoff.
7. **Stop is immediate.** Signed in with the host running: `sharectrl-service --stop-service`.
   Expect it to print `service 'ShareCtrlScreenHost' stopped` within about 1 s (before U2 it
   could take up to 2 s), the log's last line `the service is stopping; the host keeps running in
   its session`, and the host still running. `sc query ShareCtrlScreenHost` shows `STOPPED`.
   `--start-service` brings it back; the running host is kept (the new service's launch steps
   aside, exit code 0).
8. **Shutdown.** Signed in with the host running, restart Windows. After the restart, the
   System event log (Event Viewer → Windows Logs → System) has **no** error 7011 / 7034 / 7043
   for the service ("did not respond", "terminated unexpectedly", "did not shut down properly"),
   and after signing in the host comes up as in step 1.
9. **Remote Desktop.** From another PC, RDP into account B while A is at the console (Windows
   Pro). Expect no `launched engine` line for B's RDP session (only the console session gets a
   host), and A's host unaffected.

Report for each step: pass / fail, the Windows version (`winver`), and the log lines around any
failure.
