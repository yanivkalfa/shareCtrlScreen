# 10 — Remote unlock: the lock screen, UAC prompts and the sign-in screen on Windows

Written 2026-11-27 against `main` @ `0091c93` (v2.3.3). Read-only research of the Windows
service, the `elevation` crate, the capture and input adapters, the host media loop and the
control protocol; every claim cites `file:line` (paths relative to `native/` unless they start
with `plans/`, `pans/`, `docs/`, `README.md` or `CLAUDE.md`). Structure, contracts and flows are
in [10a](10a-STRUCTURE-AND-FLOWS.md). Nothing here is built yet.

**Revision 2**, after the design review of the same day (§7). The review changed the trust
model, the phases and several contracts; §2–§6 below already include its results.

**Revision 3**, after Microsoft's documentation was read for every Windows claim the design
makes (2026-11-27, §8). The reading confirmed most of them, contradicted one contract (how the
service hands the agent its control channel) and added constraints; §9 lists the changes, which
are already in [10a](10a-STRUCTURE-AND-FLOWS.md). What the documentation does not say is listed
in §8 as device-only and is answered by the U3.0 spike or a phase checklist.

## Progress

| Step | What | Status |
|---|---|---|
| S | This plan + structure ([10a](10a-STRUCTURE-AND-FLOWS.md)), adversarial review folded in, Microsoft references read | **in progress**: plan and 10a written, review folded in (§7), Microsoft references read and folded in (§8, §9). Open: the owner's answers to D3, D7 and D8 (§5) |
| U1 | The viewer is told what the host's screen is doing | **complete** (`b6053f0`; review fixes `61cc9b1`, `832fe60`; CI fixes `5b1021a`, `9ae8e4a`, `26b4207`, `fce6796`; reviewed independently in two rounds, the keyframe rule in a third; CI green on Windows, macOS and Linux at `fce6796`), **device-pending**: [checklists/UNLOCK-U1.md](checklists/UNLOCK-U1.md). Nothing of it has run on a Windows device; known gap: §10 F3. As built: §4 U1 |
| U2 | Service: session events; a sign-in no longer waits out the launch delay | **complete** (`b7d4787`, review fixes `0c31f97`; reviewed independently; CI green on Windows, macOS and Linux at `7884dd6`), **device-pending**: [checklists/UNLOCK-U2.md](checklists/UNLOCK-U2.md). The Windows half has run in CI's tests only, never as a service on a device. As built: §4 U2 |
| U3 | The agent: the lock screen of a signed-in user | not started (begins with a device spike) |
| U4 | Ctrl+Alt+Del and Lock | not started |
| U5 | UAC prompts | not started |
| S6 | Structure and review for the sign-in screen | not started |
| U6 | The sign-in screen with nobody signed in | not started |
| Final | Whole-feature review, merge, release | not started |

## 1. What the owner asked for (2026-11-27)

- "When I connect to a Windows computer that's locked, it just shows a black screen and I can't
  unlock it." The remote computer must be unlockable through ShareCtrlScreen.
- Scope, chosen from three options: the **lock screen** of a signed-in user, **UAC prompts**, and
  the **sign-in screen** when nobody is signed in (after a restart or a sign-out), in phases.
- Controlling the lock screen is **on by default** for an installed host, with a setting on the
  host to turn it off.
- The approved outline also had a "Lock remote computer" action next to Ctrl+Alt+Del.

This matches decision #8 (`plans/00-DECISIONS.md:15`): lock screen in scope on every OS, the
sign-in screen on Windows only. This plan covers Windows. The macOS and Linux lock screens stay
with their own tasks (`plans/checklists/M1.md:74-77`).

## 2. Where we are today (facts the design must respect)

1. **The host is the user's process.** The service asks for the signed-in user's token and starts
   `sharectrl.exe` with it (`service/src/main.rs:305-315`) on `winsta0\default`
   (`crates/elevation/src/session.rs:82`). The only addition is the UIAccess flag; the host stays
   the same user at the same integrity level (`crates/elevation/src/uiaccess.rs:10-13`,
   `:234-256`). Nothing runs as SYSTEM inside the user's session.
2. **The lock screen, the sign-in screen and UAC prompts are on the Winlogon desktop.** A user
   process cannot open it, and UIAccess does not help (`uiaccess.rs:56-59`, `pans/04` §8b
   lines 448-451). The desktop follower that would attach to it exists
   (`crates/elevation/src/desktop.rs:38-68`) and is called by the capture thread
   (`crates/platform-windows/src/adapters.rs:260-274`) and the injector (`adapters.rs:769`), but
   it runs in the user's process, so `OpenInputDesktop` is refused.
3. **Capture retries forever and sends nothing.** Both `AccessLost` and `AccessDenied` become
   `CaptureError::Lost` (`adapters.rs:69`). Mid-session the host loop calls `reinit`, sleeps
   250 ms and tries again without limit (`crates/engine/src/media/host.rs:790-803`). Connecting
   while already locked never leaves `open_capture` (`host.rs:296-345`), so no encoder is created
   and no frame is ever sent.
4. **The viewer is not told.** No host-state message exists: `ControlMsg` has `perm`, `bye`, `kf`,
   `vpause`, `vresume`, `cur`, `vsize`, `ping`, `pong`, `clip`, `curshape`, `displays`, `seldisp`,
   `popout`, `popclose`, `itgt` (`crates/protocol/src/control.rs:10-126`). The viewer reveals its
   surface once its decoder exists (`crates/engine/src/media/viewer.rs:66-69`) and the renderer
   clears to black (`crates/render/src/lib.rs:345-348`). That is the black screen. A session that
   was connected before the lock shows its last frame instead.
5. **Input is dropped without a trace.** `let _ = self.desktop.follow();` (`adapters.rs:769`)
   discards the refusal, and the `SendInput` results are not checked
   (`crates/input/src/lib.rs:100`, `:218`). Events go to the desktop the thread was already on.
6. **Ctrl+Alt+Del cannot be sent.** No `SendSAS` call and no wire message exist. The platform hint
   says it "can never be captured" (`crates/platform-windows/src/lib.rs:138-140`), which is true
   for capturing on the viewer and says nothing about sending it to the host.
7. **Nobody signed in means no host.** `WTSQueryUserToken` fails, the launch is retried with a
   doubling delay up to 300 s (`service/src/main.rs:287-290`, `service/src/supervise.rs:28-33`).
   The delay resets only when the session id changes (`supervise.rs:86-90`), so a sign-in that
   keeps the same session id can leave the machine without a host for up to five minutes. Read
   from the code, not observed.
8. **The service reacts to nothing but stop.** It registers `RegisterServiceCtrlHandlerW`, accepts
   only `SERVICE_ACCEPT_STOP` (`service/src/main.rs:177`, `:187-191`, `:197`) and polls the active
   console session every 2 s (`main.rs:279`, `:300`). Lock, unlock, sign-in and sign-out are not
   events for it.
9. **There is no channel between the service and the host.** The service passes flags
   (`main.rs:245-251`); the host reads the service's state (`app/src/updater.rs:177-183`).
   `pans/04` §8b line 485 calls for "Service ↔ engine IPC over a named pipe"; it was never built.
10. **The documents disagree with the code and with each other.**
    - `README.md:35` says the service "reaches the lock screen and UAC prompts";
      `docs/FIRST-RUN.md:24-26` adds the sign-in screen; `README.md:132` says the secure desktop
      is out of reach; `README.md:140` says "SYSTEM + desktop-follow" for the lock screen.
    - `README.md:141` says the login window works. `crates/platform-windows/src/lib.rs:141` sets
      `login_screen_supported: true`, which hides the note "someone must sign in on this machine
      before you can connect" (`app/ui/settings.js:33`, `app/ui/index.html:411-414`).
    - The same claim is in `CLAUDE.md:53`, `native/README.md:97`,
      `app/installer-hooks.nsh:57-58` and the header of `crates/elevation/src/lib.rs:1-2`.
    - `plans/checklists/W8.md:63` expects "Clicking a UAC prompt on H from B still works". The
      code gives no path for that.
    - `plans/05-TASKS.md:240-242` says the capability "is tracked separately". No task tracks it.
11. **What already works and must keep working.**
    - The capture frame is a D3D11 texture on the device the encoder was built from
      (`adapters.rs:333-342`, `media/host.rs:422`).
    - The host loop recovers from `Lost` by `reinit`, a re-read of the size and a forced keyframe
      (`host.rs:790-819`). The size is re-read only there (`host.rs:813-817`).
    - Input is injected only while the session has control
      (`crates/engine/src/session/driver.rs:1044-1052`).
    - A host→viewer message an old viewer does not know is ignored (`control.rs:83`, `:89`).
12. **One host session, several captures.** A host serves one viewer at a time
    (`crates/engine/src/sessions.rs:266`, `crates/engine/src/session/registry.rs:170`). That one
    session can hold several capture objects at once: a monitor switch creates the new capture
    while the old one is alive (`media/host.rs:496-497`), and every pop-out stream opens its own
    on its own thread (`host.rs:396`).
13. **A paused stream does not capture.** A background tab's main stream skips `acquire`
    altogether (`host.rs:660-663`).
14. **Approval needs a person.** In `approve` mode the host's prompt auto-denies after 30 s
    (`pans/00-OVERVIEW-AND-PROTOCOL.md:193-195`). A locked machine has nobody to click Allow, so
    reaching one needs `password` mode (`pans/00:26-32`) or a session that was already connected.

## 3. Target design

### 3.1 A SYSTEM agent in the user's session, used only for the secure desktop

The service starts a second process next to the host: the **secure-desktop agent**, running as
LocalSystem inside the console session. It is the "SYSTEM-context capture/inject helper" of
`pans/04` §8b (lines 480-484). It captures the secure desktop, injects input into it, and asks
the service for Ctrl+Alt+Del. It never talks to the network and has no notion of a remote
session; the host stays the one place that knows who is connected and with which permission.

On the normal desktop the host captures and injects exactly as today. The host asks for the
agent only after `OpenInputDesktop` was refused, so the verified path (W8) is not touched.

### 3.2 Who the agent trusts: nobody in the user's session

The host is a medium-integrity process of the signed-in user (fact 1). Any other program running
as that user can read its memory, copy its handles or load code into it. So the design assumes:

> **Whatever the host can ask of the agent, any program running as the signed-in user can ask.**

The checks on who connects to the pipe keep honest mistakes and other accounts out. They are not
a defence against the user's own programs, and nothing in this plan claims they are. What the
agent will do is therefore decided **on the SYSTEM side**, by the state of the machine:

| The secure desktop is up because… | What a same-user program could gain | The agent |
|---|---|---|
| The session is **locked** | The lock screen's picture; typing guesses; restart from the power button. Unlocking still needs the password | Serves it (U3). The user's own setting can turn this off (a preference, not a protection) |
| A **UAC prompt** or the Ctrl+Alt+Del screen is up in an **unlocked** session | On an administrator's account: approving its own elevation with one click, which removes what UAC does | Refuses unless an **administrator** enabled it for the machine (U5, decision D7) |
| **Nobody is signed in** | — (no user programs run) | Not used; see §3.8 |

One cost remains by construction: the Windows password a controller types travels through the
user-level host on its way to the agent, where the user's own programs could read it. Windows
normally keeps that password away from user programs. The alternative that avoids it (the whole
engine as SYSTEM) puts the network stack in a SYSTEM process instead. This is stated in the
README's limits when U3 ships.

### 3.3 The pipe

One named pipe per console session, served by the agent, with a **random name** the service
publishes where only SYSTEM can write and users can read. Only SYSTEM and that sign-in session
(its logon SID, so not the same user signed in elsewhere) may open it; remote clients are
refused. The host accepts a pipe only if the pipe object is owned by SYSTEM (§8 rows 19, 20).

Messages are small JSON documents with a length prefix and a version. The agent parses input from
a less privileged process: every message has a size limit, and a malformed, unknown or
out-of-state message closes the connection. Both ends use overlapped I/O with one writer thread,
and nothing on the host's transport thread ever waits for the pipe.

### 3.4 Frames

The agent copies changed regions of the captured desktop into a shared-memory section and tells
the host which rectangles changed. The host copies them into a texture on its own D3D11 device
and hands that texture to the encoder, so the encoder path is today's. The agent does not write
the next frame until the host has acknowledged the last one, and never reads the section back.
Shared memory was chosen over shared GPU textures by the review (D5): the lock screen changes
little, and shared memory has none of the cross-process GPU failure modes.

### 3.5 One capture object for the engine

In `platform-windows` a wrapper implements `ScreenCapture` over two sources: the existing
duplication for the normal desktop and an **attachment** to the agent for the secure desktop.
Every change of source, and every change of size, reaches the engine as `CaptureError::Lost`, so
the handover is the reinit-then-keyframe path the engine already has (fact 11). Each capture
object has its own attachment, so a monitor switch and pop-out windows work on the lock screen
like the main picture (fact 12).

This needs the duplication to be creatable later than its device (`capture::Duplicator::new`
creates both, `crates/capture/src/lib.rs:140`); that split is a refactor with no behaviour change
and lands first. Whether the hardware encoder can be created and keeps encoding while the
Winlogon desktop has input is unknown; **U3 starts with a device spike** that answers it before
the rest of U3 is built.

### 3.6 Telling the viewer

A new host→viewer message `hstate` says what the host's screen is: `normal`, `secure` (a secure
desktop, streamed through the agent) or `blocked` (a secure desktop that cannot be shown, with
the reason). While `blocked`, the viewer hides the video and its pop-outs, drops the frame it was
holding, and shows a message in the tab. This ships first and is useful alone: it turns today's
silent black screen into an explanation. `hstate` is sent only to a viewer that asked for it in
its capabilities.

### 3.7 Ctrl+Alt+Del and Lock

Two viewer→host messages, `sas` and `lock`, each offered only to a host that advertises it and
honoured only for a session with control. `sas` goes host → agent → service, and the service
calls `SendSAS`; the result returns to the viewer asynchronously. `lock` locks the host's
session; a host advertises it only in `password` mode, because locking an `approve` host would
leave nobody able to approve the next connection.

On current Windows the lock screen does not need Ctrl+Alt+Del: a key press or a click dismisses
it and the password box appears. `sas` is for machines whose policy requires it.

### 3.8 The sign-in screen (U6)

With nobody signed in there is no user, no user host and no user profile, so something must run
without a user and answer for the machine. That is a larger design than the agent and has its own
structure and review step (S6). The review fixed three constraints for it now:

- The profile that says which address and password answer at the sign-in screen is machine-wide.
  **Writing it needs an administrator's consent**, never a request from a user-level process.
- **A session that began at the sign-in screen never continues into another account's desktop.**
- S6 must compare a low-privilege pre-login process that uses the agent against putting the
  engine and its network stack in a SYSTEM process, and justify the choice.

### 3.9 Policies

- **Permission.** Typing on the lock screen needs control, exactly like typing on the desktop.
  Whether a view-only viewer sees secure screens at all is decision D8.
- **Setting.** `allowLockScreenControl`, default on (D2). Off: the host never uses the agent.
- **Credentials.** ShareCtrlScreen never stores, caches or types Windows passwords by itself. Key
  events are never logged, on any side, at any log level.
- **Honest limits.** A UAC credential prompt still needs credentials. Protected content stays
  black. A portable host has no agent. A remote desktop (RDP) session is not the console and is
  not covered.

## 4. Phases (each ships on its own)

Sizes: S ≤ 1 day, M 2–4 days, L 1–2 weeks. Every phase passes the lane gates on Windows and WSL,
the macOS compile check and the loopback suite, and writes its device checklist
(`plans/checklists/UNLOCK-<phase>.md`). Tests that need the installed service follow the
`SCT_*_REQUIRED` convention with `SCT_AGENT_TESTS_REQUIRED`.

```
S ─┬─► U1 ─┬─► U3 ─┬─► U4 ─┬─► S6 ─► U6 ─► Final
   └─► U2 ─┘       └─► U5 ─┘
```

### S — Structure and review (S)

This plan and [10a](10a-STRUCTURE-AND-FLOWS.md); the adversarial review folded in (§7); the
Microsoft references of §8 read and cited; D3, D7 and D8 answered by the owner.

### U1 — The viewer is told (M)

- Deliverable: `ScreenState` in `platform-api`; `hstate` and the viewer capability in `protocol`;
  the host loop sends it on change and while it waits in `open_capture`; `DdaCapture` reports
  `Blocked(NotSupported)` after a refused attach; the viewer hides the video and pop-outs, drops
  its held frame and shows the message. Dropped input is logged once per outage, never per event.
  `login_screen_supported` becomes `false` until U6. Every document of fact 10 says what is true.
- Tests: serde round trips (an old viewer never receives `hstate`); a mock capture that stays
  lost; a loopback test that the viewer receives `blocked` and then `normal`.
- Acceptance: a locked host shows "The remote computer is locked" on the viewer within 2 s, and
  the picture returns by itself after a local unlock.
- **As built** (in review, branch `task/U1`; device-pending:
  [checklists/UNLOCK-U1.md](checklists/UNLOCK-U1.md)):
  - `platform-api`: `ScreenState` / `SecureKind` / `BlockReason` and
    `ScreenCapture::screen_state` (default `Normal`) as 10a §3.1, plus `ScreenAccess`: the one
    rule for `Blocked` — the last attempt to get a picture decides (one that got a picture is
    `Normal`; a failed one is `Blocked` only when refused). The mock has
    `MockCaptureConfig::outage` (`MockOutage { at, failed_reinits, refused }`, `u32::MAX` = stays
    lost; it reports by `ScreenAccess`, so the refusal is given as a `BlockReason` rather than a
    whole `ScreenState`) and, beyond 10a, `MockPlatform::capture_refusals` (`create_capture`
    refused with `Lost` *n* times) and `MockPlatform::outage_on` (the outage on one display).
  - `protocol`: `ControlMsg::HostState { screen, kind, why }`, `HostScreen` (an unknown value
    parses as `normal`), the `kind` / `why` words in `control::hstate`, `Caps::hstate` (omitted
    when false).
  - Host (`engine/src/media/host.rs`): the screen state is the session's (`SessionScreen` in
    `Shared::host_screen`, K13 as revised by the review): every stream records what its capture
    observed since it last acquired; the main stream decides while it runs, the pop-outs while it
    is paused. The transport thread alone takes each change and queues one `hstate`, only for a
    viewer that advertised it (`ViewerFeatures`, replacing the pop-out-only peer set in
    `lib.rs`). `open_capture`'s wait records `Blocked(NotSupported)` once, at the second refusal
    in a row. A dropped file's paste chord is skipped unless the session's state is `Normal`
    (`session/files.rs`).
  - Windows (`platform-windows/src/adapters.rs`): `DdaCapture` reports `Blocked(NotSupported)`
    while its last reinit failed after `OpenInputDesktop` was denied (`E_ACCESSDENIED`; any other
    failure stays `Normal`; a duplication that succeeds is `Normal`, by `ScreenAccess`). The
    capture's attach failures are logged per capture thread (across `open_capture`'s new
    captures) and the injector's per injector, when they start and end, never per retry or
    event. `login_screen_supported` is `false`.
  - Viewer: the driver turns `hstate` into `SessionEvent::HostScreen`, the ActiveGate hides the
    main video (`SessionView::set_host_blocked`), pop-outs are hidden and shown with it (and a
    pop-out opened while blocked waits), and `Shared::host_block` (an epoch shared with the
    pop-outs) makes every present loop drop the frame it holds, decode nothing until the next
    keyframe, and re-check before any present (K21). After the end of a block, a loop whose
    video is on screen asks for a keyframe if none has come within 300 ms. The keyframe rule
    was added after CI on macOS showed frames from before the lock arriving after `normal`
    (`9ae8e4a`); `video_from_before_the_lock_that_arrives_after_it_is_not_shown` reproduces it
    with a test hook that hands the last frames from before the lock to the media loop again
    after the lock is over.
  - App/UI: the TabManager keeps the state per tab (`hostScreen` in the snapshot, so detached
    windows have it) and emits `host-screen` (from `app/src/tabs.rs`, where viewer events go,
    rather than `bridge.rs`); the tab shows a card with words per `why` (generic for unknown
    values) under the session bar, and the `secure` badge by `kind`.
  - Docs of fact 10 corrected (README, docs/FIRST-RUN.md, CLAUDE.md, native/README.md, the
    installer hooks, `elevation`'s header, W8.md, 05-TASKS.md).
  - Before U3 a Windows host cannot tell its lock screen from a UAC prompt, so the
    `unsupported` card reads "The remote computer is locked" and its text names the prompt too.
  - Residual (§10 F3): a keyframe from before the lock that is delivered only after `normal`
    is shown until the host's restart keyframe replaces it (delta frames are not: see the
    Viewer bullet). And when the restart keyframe arrives before `normal` on a screen that then
    stays still, the viewer's request for another is served only once the host's screen or
    pointer changes; until then the video area is black.
  - Unverified until the checklist runs: that `OpenInputDesktop` fails with access denied on
    the lock screen and UAC prompts and not in a disconnected session; the 2 s on a device;
    whether a re-shown native surface flashes its retained buffer (per viewer OS).

### U2 — Service: session events (S–M)

- Deliverable: `RegisterServiceCtrlHandlerExW` with session-change controls; the loop waits on
  them with the 2 s poll as fallback; a sign-in resets the launch delay (fact 7).
- Tests: the supervisor with session events, on every OS.
- Acceptance: on a device, signing in after the service has been failing to launch for several
  minutes brings the host up within 5 s. Independent of U1; it fixes something that affects users
  today.
- **As built (in review).**
  - `service/src/events.rs` (new, pure, tested on every OS): the control codes and HandlerEx's
    replies (`NO_ERROR` for stop, shutdown, session change and INTERROGATE,
    `ERROR_CALL_NOT_IMPLEMENTED` otherwise); the `WM_WTSSESSION_CHANGE` event values as
    `SessionChange`/`SessionEvent`; `Inbox`, the hand-off from the handler to the loop: a
    single-producer ring of atomics, a stop flag and `Thread::unpark`, so the handler never
    blocks, locks, allocates or logs. The loop's thread is registered as the waiter before the
    handler is, so no wake-up is lost.
  - `service/src/main.rs`: `RegisterServiceCtrlHandlerExW` accepting stop, shutdown and session
    change; the handler reports `STOP_PENDING` on stop or shutdown and wakes the loop, records
    session events (the notification is read only when its `cbSize` covers it); the loop waits on
    the inbox with a 2 s timeout instead of sleeping, so a stop is immediate and the poll stays as
    the fallback. The status handle is an `AtomicPtr` instead of a `static mut`. While no host
    runs, the loop asks for the console user's token (the first step of a launch) to learn
    whether someone is signed in; any failure means nobody yet (the error is undocumented).
  - `service/src/supervise.rs`: the supervisor knows whether a user is signed in
    (`Unknown`/`Absent`/`Present`). `follow`, `session`, `wants_user_check`,
    `user_arrived(now) -> Option<Arrived>`, `user_left`, `session_event`. While nobody is signed
    in no launch is attempted, so no delay builds up (one log line, not one per retry). A
    sign-in (token present after it was seen missing) drops a pending `Retry` (and a clean
    `Stopped`: the user signed out and back in) to `Wanted` and clears the failure count; a
    launch failing right after starts again from 2 s. A logoff event for the console session is
    `user_left`; a logon event only wakes the loop, and the token check that follows reports the
    sign-in, so event and poll are the same input in either order. Other sessions' events are
    ignored; lock, unlock and the rest are logged and drive nothing. Clean-exit rule, doubling
    backoff, cap, `STABLE_AFTER` and session-change relaunch unchanged; a transient 0xFFFFFFFF
    from `WTSGetActiveConsoleSessionId` is still a session change, as before.
  - Microsoft docs read and cited in the modules' References: HandlerEx
    (<https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nc-winsvc-lphandler_function_ex>:
    "a service will only be notified of a user logon if it is fully loaded before the logon
    attempt is made", "avoid operations that might block, such as taking a lock", INTERROGATE
    returns `NO_ERROR`), RegisterServiceCtrlHandlerExW, SERVICE_STATUS, SetServiceStatus, the
    control-handler pages, WM_WTSSESSION_CHANGE, WTSSESSION_NOTIFICATION, WTSQueryUserToken,
    WTSGetActiveConsoleSessionId, system error codes. §8's row on
    `SERVICE_ACCEPT_SESSIONCHANGE` is confirmed from the docs; that the events arrive on a real
    machine is device-pending.
  - Device-pending: every step of [checklists/UNLOCK-U2.md](checklists/UNLOCK-U2.md), the 5 s
    acceptance first. The Windows half was type-checked and linted for
    `x86_64-pc-windows-msvc` only; it runs first in Windows CI.

### U3 — The agent: the lock screen of a signed-in user (L)

Built on a feature branch and merged when the whole phase passes, so no release ever ships an
agent that does nothing.

- U3.0 **Spike on a device, before anything else.** Each question is something Microsoft's
  documentation does not answer (§8). If any answer is no, U3's design returns to review before
  more is built.
  1. **Does the lock screen accept a password typed by a SYSTEM process in the session, on a
     fully updated Windows 11?** A Microsoft Q&A answer of 2026-11-24 (not documentation; §8
     row 17) says the January 2026 security updates make Windows authentication dialogs accept
     input only from "a physical keyboard, trusted UIAccess accessibility apps, or apps running
     elevated", and calls elevation "a temporary mitigation". This is the first question because
     a "no" ends the design of U3 as written.
  2. With the Winlogon desktop up, can the host create the hardware encoder on its device, and
     does an encoder created earlier keep encoding?
  3. Does Desktop Duplication work from a SYSTEM process in the session whose thread is attached
     to the input desktop? What does it do while the display is off (a locked console turns its
     display off after 60 s by default, so this is the normal case for flow 10a §5.3): fail, or
     deliver nothing?
  4. Does `SendInput` from that thread reach the Winlogon desktop, and where does it go when the
     thread is still attached to another desktop?
  5. Does the lock event the service receives arrive before or after the input desktop becomes
     `Winlogon`, and what does the session's lock state read at that moment (10a §3.5)?
  6. Does `GetNamedPipeServerSessionId` work on a client handle (10a §3.4)?
  7. Can the agent be the first process of a fresh job object created by the service (§8 row 6)?
  8. What does a secure screen saver look like to the agent (input desktop `ScreenSaver`)?
- U3.1 (no behaviour change) `capture::Duplicator` splits device creation from duplication; the
  `input` crate's keyboard hook moves behind a feature the agent does not enable; the handle leak
  in `InputDesktopFollower::follow` (`crates/elevation/src/desktop.rs:58`) is fixed.
- U3.2 The pipe (`agent-ipc`, `elevation::pipe`), the agent's launch and supervision.
- U3.3 The agent's capture and input **while the session is locked**; the wrapper and the
  forwarding injector; the held-key ledger; `hstate secure`; the setting.
- Tests: the wrapper's state machine against a fake agent; framing and limits (oversized,
  truncated, unknown, wrong version, out of state); attachment ids and late answers; the held-key
  ledger across source changes; the engine loopback with a mock that changes source.
- Acceptance: lock the host, see the lock screen, dismiss it, type the password or PIN, arrive on
  the desktop, in one session without reconnecting. The same when connecting to an already locked
  host in password mode, with its display asleep. Win+L from the viewer leaves no key stuck.
  Clicks land right on a scaled second monitor. A UAC prompt still shows `blocked`. Latency on
  the normal desktop is unchanged against the W8 checklist.

### U4 — Ctrl+Alt+Del and Lock (S)

- Deliverable: `sas`, `lock` and the result message; the two `HostCaps` flags; the viewer
  actions; `SendSAS` in the service; the policy handling of D3.
- Acceptance: on a machine that requires Ctrl+Alt+Del to sign in, the action brings up the
  password box; where an administrator forbids software Ctrl+Alt+Del the viewer is told so; Lock
  is not offered by a host in `approve` mode.

### U5 — UAC prompts (M)

- Deliverable: the machine-wide switch an administrator sets (D7), read by the agent; the agent
  serves the secure desktop of an unlocked session when it is on; `hstate` says which kind of
  screen it is; the README's limits and the stated risk.
- Acceptance: with the switch on, a consent prompt and a credential prompt are visible and usable
  from the viewer, and a prompt that appears and is dismissed locally in under a second leaves the
  session running. With it off, the viewer sees `blocked` with the reason.

### S6 — Structure and review for the sign-in screen (S)

10a §8 completed under the three constraints of §3.8, reviewed adversarially; D4 and D6 answered.

### U6 — The sign-in screen (L)

- Deliverable: per S6.
- Acceptance: restart the host, connect from the viewer without anyone touching it, sign in,
  work. Sign out and connect again. With the option off, the machine is unreachable until someone
  signs in and the settings say so.

### Final (S)

Whole-feature review, the README status table, version bump, release.

## 5. Decisions

| # | Question | Decision |
|---|---|---|
| D1 | Scope | Lock screen + UAC prompts + sign-in screen, in phases (owner, 2026-11-27) |
| D2 | Default for the lock screen | On by default, a host setting turns it off (owner, 2026-11-27). The setting is the user's preference; it does not protect against the user's own programs (§3.2) |
| D3 | Windows may refuse software Ctrl+Alt+Del unless a machine policy allows it. May the service change that policy? | **Open.** Recommended: allow it only for the moment of sending and put the previous value back; if an administrator set the policy explicitly, leave it and tell the viewer |
| D4 | Which identity answers at the sign-in screen? | **Open, in S6.** Fixed by the review: an administrator must consent to setting it |
| D5 | Shared memory or shared GPU textures for the agent's frames? | Shared memory (design review, 2026-11-27) |
| D6 | Is reaching the sign-in screen on by default? | **Open, in S6.** Recommended: off |
| D7 | UAC prompts: once the agent serves them, any program running as the signed-in user can approve its own elevation on an administrator's account (§3.2). What is the default? | **Open.** Recommended: off until an administrator turns it on for the machine. The alternative, on by default, matches AnyDesk and gives up UAC's protection on every machine that installs ShareCtrlScreen |
| D8 | Does a **view-only** viewer see the lock screen and UAC prompts? They can show account names, notifications and a password someone reveals while typing locally | **Open.** Recommended: no; a view-only viewer gets `blocked` with the reason |
| D9 | "Lock remote computer" | Kept from the approved outline, offered only by a host in `password` mode (design review, 2026-11-27) |

## 6. Risks

| Risk | Mitigation |
|---|---|
| A program running as the signed-in user uses the agent | The agent's reach is decided on the SYSTEM side (§3.2): the lock screen only, unless an administrator enables UAC prompts. What remains (the lock screen's picture, password guesses that can lock the account out, the typed password passing through the host) is stated in the README |
| Another local account squats the pipe's name to imitate the agent or to block it | A random name published by the service after the pipe exists; the host accepts only a pipe owned by SYSTEM; the host connects without letting the server impersonate it |
| The hardware encoder cannot be created, or stops, while the Winlogon desktop has input | U3.0 answers this on a device before the rest is built |
| Windows refuses a password typed by software on the lock screen (§8 row 17) | U3.0 item 1 answers it first. A "no" stops U3 and opens the credential-provider question (§9 item 11) |
| The verified normal-desktop path regresses | The duplication code does not change behaviour (U3.1); the agent is tried only after `OpenInputDesktop` is refused; the W8 checklist is re-run in U3 |
| Keys stuck across the change between desktops (the Enter that submits the password, Win+L) | One ledger of held keys in the host, which sees every event; key-ups always pass (10a §3.8) |
| The agent or the pipe dies while the machine is locked | The wrapper falls back to today's retry loop and reports `blocked`; the service restarts the agent with the existing doubling delay |
| The installer cannot replace `sharectrl-service.exe` because an agent still runs from it | The service ends the agent and waits for it before it reports stopped; `--stop-service` also ends agents left behind by a dead service |
| A SYSTEM process parsing messages from a user process | Size limits, strict parsing, no `unsafe` in the parser, a test for every malformed shape; a parse error closes the connection and logs no payload |
| A locked machine's display is asleep and delivers no picture | The attachment does not depend on a first frame; the first forwarded input wakes the display (10a §5.4) |
| An unsigned SYSTEM process that injects into Winlogon looks like malware to antivirus | Unknown until device runs; noted in the checklists |
| A pre-login session is live when someone signs in (U6) | Constrained in §3.8, designed in S6 |
| A manual install in a folder the user can write lets the user replace the service binary | Exists today (`README.md:307`), not made worse by this plan; noted for the owner |

## 7. Design review (2026-11-27) → changes

One independent adversarial review of this plan and 10a at `469a212`. Its claims about the code
were checked against the code before being accepted; its claims about Windows behaviour could not
be checked (§8) and were accepted where the safer design costs little. Where this section and
§2–§6 or 10a disagree, this section wins.

### Blockers, fixed in the design

1. **The agent is a confused deputy for the user's own programs.** The first draft listed the
   pipe's access list and the peer checks as the mitigation. They are not one: the host is medium
   integrity (`crates/elevation/src/uiaccess.rs:10-13`), so a same-user program can copy its pipe
   handle or run code inside it. → §3.2: the threat model is written down; the agent decides by
   machine state on the SYSTEM side; the lock screen and UAC prompts are separated; UAC control is
   an administrator's machine-wide switch and an owner decision (D7).
2. **The host could not authenticate the agent, and the pipe's name could be squatted.** A
   predictable name in a machine-wide namespace lets any local account create it first; the
   server check as drafted needed the host to open a SYSTEM process. → §3.3, 10a §3.4: a random
   name published by the service; the host checks the pipe object's owner and session.
3. **The recommended sign-in handover was a cross-account takeover.** A session begun with one
   account's ShareCtrlScreen password would have continued into whichever account signed in
   locally, and a user-level process would have written the machine-wide profile. → §3.8: the
   recommendation is struck; two constraints are fixed now; the rest is S6.
4. **"Several sessions on one host" cannot happen, and several captures in one session can**
   (facts 12). The draft's protocol had one frame slot and no way to tell captures apart. →
   10a §3.4 and §5.11: attachments with ids, one per capture object; pop-outs and the monitor
   switch use them like the main picture.

### Major, folded in

5. **Pipe I/O.** Blocking reads and writes on one handle from two threads serialise each other,
   and the draft made the transport thread wait up to 3 s. → overlapped I/O, one writer thread
   per end, results delivered asynchronously, nothing on `sct-transport` waits (10a §3.8, §6).
6. **Held keys.** The draft had the agent release keys after the desktop had left its reach, and
   left unsaid who releases a key pressed on one side and released on the other. → one ledger in
   the host; key-ups always pass (10a §3.8, K9).
7. **The "create the encoder later" fallback was not a design.** → removed; U3.0 is a gate.
8. **Display asleep.** → 10a §5.4; first step of U3's checklist.
9. **State machine gaps.** A size change now reaches the engine as `Lost` (the size is re-read
   only there, `media/host.rs:813-817`); "refused" means `OpenInputDesktop` was denied, nothing
   else; the constructor still returns `NoOutput` so the primary-display fallback keeps working
   (`host.rs:308-318`); U1 has its own reason, `NotSupported` (10a §4.1).
10. **A late answer leaked a section into the host.** → attachments have ids; an answer nobody
    waits for is closed and detached; sizes are validated before mapping (10a §3.4).
11. **Job objects across sessions** may not be allowed. → unverified list; the agent also exits
    when its control pipe closes; `--stop-service` ends agents left behind (10a §3.7).
12. **The agent's DPI awareness.** The service binary has no manifest (`service/build.rs`), the
    app is Per-Monitor-V2 (`app/sharectrl.manifest:16`) and the injector's geometry depends on it
    (`crates/input/src/lib.rs:106`). → the agent sets it at start (10a §3.5).
13. **Owner decisions presented as settled.** View-only viewers and secure screens → D8. UAC
    default → D7. Lock → D9. D5 was the reviewer's to decide: shared memory.
14. **Phases.** UAC control would have shipped in U3 and been verified in U5; U2 would have
    shipped an agent that does nothing. → U3 serves the locked session only and is built on a
    feature branch; U5 adds prompts; U2 is the service fix alone and can ship first.
15. **A network-facing SYSTEM process in U6** contradicted §3.1 without discussion. → an S6
    constraint (§3.8).

### Minor, folded in

- Citations corrected: the forced keyframe is `media/host.rs:819`; `Toast` is
  `crates/engine/src/events.rs:113`; the viewer's link-trouble panel is `app/ui/tabs.js:250`
  (`session.js:153` is the host's modal); the view type is `SessionView`
  (`crates/engine/src/session/active.rs:117`); `README.md:35` does not mention the sign-in screen.
- The native Session menu is documented as "exactly three always-enabled items"
  (`app/src/menu.rs:15`); adding two conditional items changes that contract and is part of U4.
- The agent acts on an allow-list of desktops (`Winlogon`), not on "anything but `Default`".
- A file drop presses the paste chord after the transfer
  (`crates/engine/src/session/files.rs:169-170`); it is skipped unless the screen is `normal`.
- A paused stream learns of a lock when it resumes (fact 13); the invariant says so.
- `hstate` carries the kind of secure screen, so the viewer does not call a UAC prompt a lock
  screen.
- `hstate` and the result message are sent only to a viewer that advertises them, as CLAUDE.md
  requires, instead of relying on old viewers ignoring them.
- The pre-login password hash is enough to answer the challenge
  (`pans/00-OVERVIEW-AND-PROTOCOL.md:132-138`), so it is a secret and belongs in the OS secret
  store at machine scope, not in an access-controlled file or key (S6).
- Small contracts added in 10a: how the agent gets its control handles; an agent started by hand
  refuses to run; the setting toggled while attached; logs carry no payload and are not truncated
  at each start; the agent is terminated at system shutdown without the 5 s wait; no COM
  single-threaded apartment on the agent's threads.

### Not changed

- Each change of source forces a keyframe outside the one-per-second limit
  (`media/host.rs:819` against `:727`), so a UAC prompt costs two. Kept; U5's checklist watches
  the bitrate.
- Starting the host at raised integrity was suggested as a defence. Not adopted: the review
  itself notes it does not close the ways code gets loaded into the host, and §3.2 no longer
  depends on the host being trustworthy.

## 8. What Microsoft's documentation says (read 2026-11-27)

Two research passes read `learn.microsoft.com` for every Windows claim of this plan and 10a; the
deciding sentences were checked again against the saved pages before being accepted. `W` is
`https://learn.microsoft.com/en-us/windows/win32`. **Confirmed** means a fetched page says it.
**Device** means the documentation is silent and a device must show it (the U3.0 spike or the
named checklist). Builders cite these pages in their modules' `//! ## References` blocks.

| # | Claim | Result | Source | Consequence |
|---|---|---|---|---|
| 1 | The sign-in screen, the Ctrl+Alt+Del screen and UAC prompts are on the `Winlogon` desktop of `WinSta0`; user programs cannot open it | **Confirmed.** "The Winlogon desktop's security descriptor allows access to a very restricted set of accounts, including the LocalSystem account"; the system switches to it "when the user presses the CTRL+ALT+DEL key sequence, or when the User Account Control (UAC) dialog box is open". The lock screen is only called "a secure desktop" | `W/winstation/desktops`, `W/secauthn/winlogon-states` | The allow-list of `Winlogon` (10a §3.5) stands. A secure screen saver uses a third desktop, `ScreenSaver`: U3.0 item 8 |
| 2 | Desktop Duplication of the secure desktop needs LocalSystem | **Confirmed** for the account ("only an application that runs at LOCAL_SYSTEM can access the secure desktop"); a desktop switch or mode change gives `DXGI_ERROR_ACCESS_LOST`; not available in session 0. **Device** for the thread's desktop. New: "A process can have only one desktop duplication interface on a single desktop output"; `DXGI_ERROR_UNSUPPORTED` and `DXGI_ERROR_SESSION_DISCONNECTED` are retry cases | `W/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput`, `…/nf-dxgi1_2-idxgioutputduplication-acquirenextframe`, `W/direct3d11/direct3d-11-1-features` | The agent must be in the user's session (it is). One duplication per output in the agent: 10a §3.5 |
| 3 | Desktop Duplication with the display asleep | **Device.** Documented nearby: "when the console is locked, Windows waits for 60 seconds of inactivity before powering off the display" | `https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/monitor-powers-off-when-pc-locked` | Connecting to an already locked host normally meets a display that is off: 10a §5.4 is the common path. U3.0 item 3 |
| 4 | The hardware encoder works while the Winlogon desktop has input | **Device.** Nothing documented either way | — | U3.0 item 2 |
| 5 | `TokenSessionId` on a duplicate of a LocalSystem token needs SeTcbPrivilege and gives a SYSTEM process in that session | **Confirmed** ("must have the Act As Part Of the Operating System privilege"; "The process is run in the session specified in the token") | `W/api/winnt/ne-winnt-token_information_class`, `W/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw` | 10a §3.7 launch stands |
| 6 | A process in a user's session can be put in a job object created in session 0 | **Confirmed with a condition:** "A job is associated with the session of the first process to be assigned to the job"; all its processes must be in that session | `W/api/jobapi2/nf-jobapi2-createjobobjectw`, `…/nf-jobapi2-assignprocesstojobobject` | A fresh job per agent launch, the agent its first process, never reused across sessions (10a §3.7). U3.0 item 7 confirms |
| 7 | A SYSTEM process can read whether a session is locked | **Partly.** `WTSINFOEX_LEVEL1.SessionFlags` has `LOCK`, `UNLOCK` and `UNKNOWN` (0xFFFFFFFF); the query fails when Remote Desktop Services is not running; on Windows 7 the two values are reversed. `LockWorkStation`'s page says the documented way to know is the session notification. Timing against the desktop switch: **Device** | `W/api/wtsapi32/ns-wtsapi32-wtsinfoex_level1_w`, `…/nf-wtsapi32-wtsquerysessioninformationw`, `W/api/winuser/nf-winuser-lockworkstation` | The service's lock and unlock events are the primary source; the query gives the value at start; unknown never counts as locked (10a §3.5). U3.0 item 5 |
| 8 | `SendSAS(FALSE)` works from a LocalSystem service and is governed by a policy whose default forbids services | **Confirmed** for both ("an application must either be running as a service or have the uiAccess attribute"; "If you disable or don't configure this setting, only Ease of Access applications running on the secure desktop can simulate the SAS"). The agent is not a service, so it cannot call it. The policy's key is `Software\Microsoft\Windows\CurrentVersion\Policies\System`; its value name and numbers are **not on Learn**. Which session a service in session 0 reaches is not documented; impersonating a token of the session is ("simulates a SAS on the session associated with the impersonated token") | `W/api/sas/nf-sas-sendsas`, `https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-winlogon` | The call stays in the service (10a §3.6). D3's handling of the value is verified on a device in U4 |
| 9 | `OpenInputDesktop` in a disconnected session | **Confirmed, and not a refusal:** it "returns a handle to the desktop that becomes active when the user restores the connection" | `W/api/winuser/nf-winuser-openinputdesktop` | A disconnected session is never reported as `blocked` because of this call |
| 10 | `SetThreadDesktop` fails on a thread that owns windows or hooks | **Confirmed** | `W/api/winuser/nf-winuser-setthreaddesktop` | 10a §3.5 threads stand |
| 11 | A current lock screen is dismissed without Ctrl+Alt+Del unless policy says otherwise | **Partly.** Dismissal by "touch, the keyboard, or by dragging it with the mouse" is documented; two Microsoft pages disagree on the policy's default | `https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-controlpaneldisplay`, `…/policy-csp-localpoliciessecurityoptions` | A plain click may not be enough (U3 checklist). Managed machines can require the SAS: U4 is needed |
| 12 | `RegisterServiceCtrlHandlerExW` with `SERVICE_ACCEPT_SESSIONCHANGE` delivers lock, unlock, logon, logoff and console events | **Confirmed**, with values (`SERVICE_CONTROL_SESSIONCHANGE` 0xE; `WTS_SESSION_LOGON` 0x5 … `WTS_SESSION_UNLOCK` 0x8) and rules: "a service will only be notified of a user logon if it is fully loaded before the logon attempt is made"; the handler "should avoid operations that might block, such as taking a lock"; controls are accepted only once running | `W/api/winsvc/nc-winsvc-lphandler_function_ex`, `W/termserv/wm-wtssession-change`, `W/api/winsvc/nf-winsvc-setservicestatus` | U2 as designed; the 2 s poll must stay |
| 13 | `WTSQueryUserToken` when nobody is signed in | Needs LocalSystem and SeTcbPrivilege (**confirmed**); the error is **not documented**. `WTSGetActiveConsoleSessionId` returns 0xFFFFFFFF also while the console session is being attached or detached | `W/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken`, `W/api/winbase/nf-winbase-wtsgetactiveconsolesessionid` | The supervisor treats any failure as "no user yet" and never branches on one error code |
| 14 | Synchronous pipe I/O on one handle is serialised across threads | **Confirmed** ("If this flag is not specified, then I/O operations are serialized") | `W/api/fileapi/nf-fileapi-createfilew`, `W/ipc/synchronous-and-overlapped-input-and-output` | Overlapped I/O, one `OVERLAPPED` and event per pending operation |
| 15 | An unlabelled pipe is treated as medium integrity; an explicit label keeps low-integrity processes out | **Confirmed**, with a refinement: the default policy blocks writes only | `W/secauthz/mandatory-integrity-control`, `W/api/winnt/ns-winnt-system_mandatory_label_ace` | The label also blocks reads: `S:(ML;;NWNR;;;ME)` (10a §3.4) |
| 16 | A medium-integrity process cannot open a SYSTEM process or its token | **Partly.** Integrity leaves it at most `SYNCHRONIZE`, `PROCESS_QUERY_LIMITED_INFORMATION` and `PROCESS_TERMINATE`; whether the default access list grants those is not documented | `https://learn.microsoft.com/en-us/previous-versions/dotnet/articles/bb625962(v=msdn.10)` | The agent's process gets an explicit SYSTEM-only security descriptor (10a §3.7) |
| 17 | Injected input reaches the lock screen's password box | **Device**, and at risk. `SendInput`'s page does not say which desktop receives the input, and a blocked call cannot be detected ("neither GetLastError nor the return value will indicate the failure was caused by UIPI blocking"). A Microsoft Q&A answer by vendor staff (2026-11-24, not documentation; the KB it cites, KB5080542, is on a site this work could not reach) says that since the January 2026 updates authentication dialogs "only accept input from trusted local sources, such as a physical keyboard, trusted UIAccess accessibility apps, or apps running elevated", with elevation "only as a temporary mitigation", and points to credential providers as the supported way | `W/api/winuser/nf-winuser-sendinput`, `https://learn.microsoft.com/en-us/answers/questions/6079966/supported-windows-11-integration-for-remote-assist` | U3.0 item 1. If typing is refused, U3 needs another way to unlock (§9) |
| 18 | Handles reach the agent by inheritance from the service | **Contradicted.** "You cannot inherit handles across sessions. Additionally, if this parameter is TRUE, you must create the process in the same session as the caller"; anonymous pipes do not support overlapped I/O | `W/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw`, `W/ipc/anonymous-pipe-operations` | 10a §3.6 rewritten: a named control pipe served by the service |
| 19 | The host can tell the pipe's server is the agent | **Partly.** The owner can be read from the handle with `READ_CONTROL`. `GetNamedPipeServerSessionId` is documented only for a handle "created by the CreateNamedPipe function", which the host's is not | `W/api/aclapi/nf-aclapi-getsecurityinfo`, `W/api/winbase/nf-winbase-getnamedpipeserversessionid` | The owner check is the contract; the session check is added only if U3.0 item 6 shows it works |
| 20 | The pipe's access list keeps other sessions and squatters out | **Confirmed**, with two corrections: pipe names are machine-wide and Microsoft says to "use the logon SID on the DACL"; `FILE_GENERIC_WRITE` includes the right to create instances ("FILE_APPEND_DATA and FILE_CREATE_PIPE_INSTANCE have the same definition") | `W/ipc/named-pipe-security-and-access-rights`, `W/api/namedpipeapi/nf-namedpipeapi-createnamedpipew` | Logon SID and individual rights on both ends (10a §3.4) |
| 21 | An unnamed section's handle can be re-duplicated with more access by its holder | **Not documented** either way ("In some cases, the new handle can have more access rights than the original handle") | `W/api/handleapi/nf-handleapi-duplicatehandle` | Unchanged: the agent never reads a section. It is duplicated into the host read-only, with an explicit security descriptor |
| 22 | A volatile registry value under `HKLM` disappears at restart | **Partly.** Volatility belongs to a key, is "ignored for keys that already exist", and lasts until "a full shutdown"; "a fast startup shutdown is the default" | `W/api/winreg/nf-winreg-regcreatekeyexw` | A volatile key with its own security descriptor; the service clears stale names when it starts (10a §3.4) |
| 23 | The agent can make itself Per-Monitor-V2 aware without a manifest | **Confirmed**: the call must come "before you call any APIs that depend on the DPI awareness"; `ERROR_ACCESS_DENIED` means it was already set; Windows 10 1703 or later | `W/api/winuser/nf-winuser-setprocessdpiawarenesscontext` | 10a §3.5 start-up |
| 24 | The lock action reports whether the machine locked | **No.** `LockWorkStation` "executes asynchronously"; success means "the operation has been initiated" | `W/api/winuser/nf-winuser-lockworkstation` | `actr` for `lock` means "requested"; the viewer learns of the lock from `hstate` (10a §5.7) |

Nothing in this plan counts as working until its device checklist passes (decision #15).

## 9. References review (2026-11-27) → changes

What §8 changed in the design. Each item is in 10a already. The parts of 10a that changed belong
to U3 and later; they are reviewed again with the rest of U3's structure after the U3.0 spike,
before U3.1 starts. U1 and U2 are not affected except where noted.

1. **The service ↔ agent channel is a named pipe served by the service** (10a §3.6). Inheriting
   two anonymous pipe handles across sessions is ruled out by the documentation (§8 row 18).
2. **`SendSAS` stays in the service** (§8 row 8); the branch "the agent may call it" is closed.
   The service aims at the agent's session by impersonating the agent's end of the control pipe
   while it calls, which is the documented way to choose the session.
3. **Lock state comes from the service's session events**, with the query as the start value and
   "unknown" never treated as locked (§8 row 7, 10a §3.5).
4. **The client pipe** (10a §3.4): the session's logon SID instead of the user's SID; individual
   access rights on both ends; a label that blocks reads as well as writes; the owner set
   explicitly; the published name in a volatile key that the service clears at start; the host's
   check rests on the owner.
5. **The agent's process and its sections get explicit security descriptors** (§8 rows 16, 21).
6. **One job object per agent launch** (§8 row 6); review item 11 of §7 is relaxed accordingly.
7. **One duplication per output in the agent** (§8 row 2). What a second attachment to the same
   output gets is open in 10a §3.5 and is settled in U3's structure review.
8. **A sleeping display is the normal case** when connecting to a locked host (§8 row 3).
9. **`lock` reports "requested"** (§8 row 24).
10. **U2:** the handler takes no lock, controls are accepted only once the service reports
    running, and the poll stays because a sign-in before the service has loaded is never
    notified (§8 rows 12, 13).
11. **U3.0 has a new first question** (§8 row 17). If Windows refuses a password typed by the
    agent, the lock screen needs a different mechanism. Microsoft's answer names a credential
    provider; that is a new design with its own plan, structure and review, and a decision for
    the owner. Nothing of U3 beyond the spike is built before this is known. U1 and U2 are
    useful either way.

For the owner's open decisions (§5), from the reading:

- **D3.** The policy exists and its default forbids services (§8 row 8). The value's name and
  numbers are not in Microsoft's documentation, so "set it for the moment of sending and put it
  back" rests on a device test in U4, not on a citation.
- **D7.** Windows has its own administrator switch for a related case: "UIAccess programs
  (including Windows Remote Assistance) can automatically disable the secure desktop for
  elevation prompts" for standard users (`EnableUIADesktopToggle`, off by default). The host is a
  UIAccess program (§2 fact 1). It does not replace D7: it does not cover administrators'
  prompts, and it removes the secure desktop instead of serving it.

## 10. Follow-ups found on the way (not part of a phase yet)

| # | What | Found | Status |
|---|---|---|---|
| F1 | **A brief "no console session" is treated as a session change.** `WTSGetActiveConsoleSessionId` returns 0xFFFFFFFF while the console session is being attached or detached (§8 row 13). `Supervisor::follow` (`service/src/supervise.rs`) resets on it, so the same session id coming back counts as new: a second host is launched while the first still runs, it steps aside with exit code 0, the policy records a clean exit, and the first host is no longer watched. If that host later crashes it is not relaunched until the session really changes. The behaviour predates U2 (the old `poll` did the same) and was kept unchanged there | U2 review, 2026-11-27 | **Open: the owner decides** whether to fix it now as its own small task (recommended: ignore a "no session" shorter than a few polls, and keep watching the running host across it) or leave it |
| F2 | **`full_session_over_real_webrtc` could fail on the untouched tree** ("surface hidden after the session", `crates/engine/tests/loopback.rs`): the test asserted the surface was hidden the moment both roles were idle, but the hide is queued after that and applied asynchronously. About one suite run in ten on `5e8ac67` in a loaded container | U1 build, 2026-11-27 | **Fixed** in the test (`d4ac638`): it waits for the hide. The engine's order is as designed (U1 review) |
| F3 | **The viewer cannot tell which video belongs to after a block.** `hstate` and video travel separately, so the viewer does not know which frame is the host's first after a block. U1 uses "the next keyframe", which leaves two rare cases (10a K21): a keyframe from before the block that arrives late is shown, and a restart keyframe that arrives before `normal` is lost, after which a still screen stays black until the host has a new frame to encode (the host answers a keyframe request only with a captured frame, `crates/engine/src/media/host.rs`) | U1 CI and review, 2026-11-27 | **Open: the owner decides.** Recommended, as its own small task: `hstate normal` carries an optional marker of the first frame after the block (capability-gated, old peers unaffected), and the host answers a keyframe request on a still screen by encoding its last frame again |
