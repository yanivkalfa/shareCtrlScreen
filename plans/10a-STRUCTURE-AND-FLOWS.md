# 10a — Remote unlock: structure, contracts and flows

Companion to [10](10-REMOTE-UNLOCK.md), rewritten after its design review (10 §7). Plan 10 says
*what* and *why*; this document is the *how*: who owns what, where the code goes, the types and
messages, the state machines, every flow and the invariants. It is **step S**. Paths are relative
to `native/`. Tags in flows: [VU] viewer web page, [V] viewer engine, [H] host engine, [W] the
host's Windows capture wrapper and injector (`platform-windows`), [A] the agent, [S] the service.

Windows behaviour this document relies on was checked against Microsoft's documentation on
2026-11-27 (plan 10 §8, with the page for each claim). That reading changed §3.4–§3.7 here
(plan 10 §9); those parts belong to U3 and later and are reviewed again with U3's structure
after the U3.0 spike. §8 below (the sign-in screen) holds constraints only; step S6 writes its
structure.

## 1. Ownership

| Owner | Owns | Does not own |
|---|---|---|
| **Service** (`sharectrl-service.exe --run-service`, session 0, LocalSystem) | Which console session is active, whether a user is signed in, whether the session is locked (its session events); starting, watching and ending the host and the agent; the control pipe to the agent (§3.6); the client pipe's published name; the `SendSAS` call and the policy around it | Capture, input, the client pipe itself, any remote session |
| **Agent** (`sharectrl-service.exe --secure-agent`, the console session, LocalSystem) | The pipe (as server); **the decision what it will serve, from the machine's state** (§3.5); capture of and input into the `Winlogon` desktop; the frame sections; asking the service for Ctrl+Alt+Del | The network, remote sessions, permissions, the user's files and settings, the `Default` desktop |
| **Host's Windows platform** (`crates/platform-windows`) | The pipe client; one `ScreenCapture` over two sources and its D3D11 device; the forwarding injector and **the ledger of held keys**; the `ScreenState` it reports | Who is connected, permissions |
| **Host engine** (`crates/engine`) | Who is connected and with which permission (unchanged); sending `hstate`; honouring `sas` and `lock` only with control; not streaming a secure screen to a viewer that may not see it (D8) | Anything Windows-specific |
| **Viewer engine and UI** | Hiding the video and pop-outs while the host is `blocked`; the message in the tab; offering the two actions when the host advertises them | — |

The agent trusts nothing in the user's session (plan 10 §3.2). Every rule in §3.5 that limits
what the agent does is enforced by the agent from state it reads itself, never from what the
host tells it.

## 2. Code layout

```
crates/protocol/src/control.rs           HostState, SecureAttention, LockSession, ActionResult (§3.2)
crates/protocol/src/signaling.rs         Caps { hstate }, HostCaps { sas, lock } (§3.2)
crates/protocol/src/config/mod.rs        allow_lock_screen_control (§3.9)
crates/platform-api/src/capture.rs       ScreenState, SecureKind, BlockReason, ScreenCapture::screen_state
crates/platform-api/src/platform.rs      HostFeatures, secure_attention, lock_session,
                                         set_secure_desktop_control (§3.1)
crates/platform-api/src/mock.rs          a capture that stays lost; scripted ScreenState
crates/agent-ipc/            (new)       pure: the pipe's messages, framing, limits (§3.4). No OS code,
                                         tested on every OS
crates/secure-agent/         (new)       Windows-only: the agent's runtime (§3.5)
crates/elevation/src/agent.rs    (new)   the SYSTEM-in-session token, the job object, the launch
crates/elevation/src/pipe.rs     (new)   the named pipe: creation with its security descriptor, the
                                         published name, the client open and its check, overlapped I/O
crates/elevation/src/sas.rs      (new)   SendSAS and the policy handling (U4)
crates/elevation/src/session.rs          locked? (U3). Session-change events as plain values moved to
                                         service/src/events.rs in U2: the elevation crate is
                                         Windows-only, and the values must be tested on every OS
crates/elevation/src/desktop.rs          follow(): close the handle when SetThreadDesktop fails
crates/capture/src/lib.rs                Duplicator: device creation split from duplication (U3.1)
crates/input/Cargo.toml                  feature `keyhook` (default on); the agent builds without it
crates/platform-windows/src/secure_desktop.rs (new)
                                         AgentClient, SecureDesktopCapture, the forwarding injector
crates/platform-windows/src/adapters.rs  DdaCapture and SendInputInjector stay; create_capture and
                                         create_injector return the wrappers
crates/engine/src/media/host.rs          sends hstate; withholds secure frames per D8 (§3.3)
crates/engine/src/session/driver.rs      host: sas, lock (control only); viewer: hstate, actr, pop-outs
crates/engine/src/session/active.rs      SessionView: the host has a picture to show
crates/engine/src/session/files.rs       no paste chord unless the screen is normal
crates/engine/src/events.rs              SessionEvent::HostScreen, ActionResult
crates/engine/src/sessions.rs            Caps and HostCaps; send_secure_attention, lock_remote
service/src/main.rs                      --secure-agent; HandlerEx; the event-driven loop
service/src/events.rs        (new, U2)   pure: control and session-event codes, HandlerEx's replies,
                                         the lock-free hand-off from the handler to the loop
service/src/supervise.rs                 a sign-in resets the delay; a second Supervisor for the agent
app/src/commands.rs, bridge.rs, menu.rs  the two commands, the two events, the menu items
app/ui/api.js, session.js, tabs.js, settings.js, index.html
                                         the message in the tab, the two actions, the setting
plans/checklists/UNLOCK-U1.md … U6.md    device checklists
```

`secure-agent` depends on `capture`, `input` (without `keyhook`), `elevation` and `agent-ipc`.
`platform-windows` depends on `agent-ipc` and `elevation`. Neither depends on the other: the
service binary does not link the host's codecs, the host does not link the agent's runtime, and
no keyboard hook is compiled into a SYSTEM binary (`crates/input/src/lib.rs:12-13`).

## 3. Types and contracts

### 3.1 `platform-api`

```rust
/// What the host's screen is, as far as the capture backend can tell.
pub enum ScreenState {
    Normal,
    /// A secure desktop is showing and is being captured.
    Secure(SecureKind),
    /// A secure desktop is showing and cannot be captured.
    Blocked(BlockReason),
}
pub enum SecureKind { Lock, Prompt, SignIn }
pub enum BlockReason {
    NotSupported,       // this host has no way to show it (no service; before U3: always)
    Disabled,           // the user's setting is off
    NotAllowed,         // the machine's policy says no (UAC prompts without the switch, U5)
    AgentUnavailable,   // the agent is not answering
}

pub trait ScreenCapture {
    // … as today …
    /// Default: `Normal`. Read by the host loop after every `acquire` / `reinit`.
    fn screen_state(&self) -> ScreenState { ScreenState::Normal }
}

pub struct HostFeatures { pub secure_attention: bool, pub lock_session: bool }
pub type ActionDone = Box<dyn FnOnce(Result<(), HostActionError>) + Send>;

pub trait Platform {
    // … as today …
    /// Cheap and cached; never touches the pipe.
    fn host_features(&self) -> HostFeatures { HostFeatures::default() }      // all false
    /// Both return at once; `done` runs later on a platform thread.
    fn secure_attention(&self, done: ActionDone) { done(Err(Unsupported)) }
    fn lock_session(&self, done: ActionDone) { done(Err(Unsupported)) }
    /// The host's setting (§3.9). Default: ignored.
    fn set_secure_desktop_control(&self, _on: bool) {}
}
pub enum HostActionError { Unsupported, BlockedByPolicy, Failed(String) }
```

`CaptureError` does not change. `Lost` stays the one recoverable error
(`crates/platform-api/src/capture.rs:62-66`); `SecureDesktop` stays unused by Windows for the
reason given at `crates/platform-windows/src/adapters.rs:66-68`.

The mock gains `MockCaptureConfig::outage` (lost at frame *n*, `reinit` fails *k* times, with a
`ScreenState` to report meanwhile). Today's `lose_at` loses once and `reinit` always succeeds
(`crates/platform-api/src/mock.rs:197-202`, `:278-285`).

### 3.2 `protocol`

```rust
// control.rs
/// Host → Viewer. Sent when the ctl channel opens and on every change, only to a viewer
/// with `Caps::hstate`.
#[serde(rename = "hstate")]
HostState {
    screen: HostScreen,                       // "normal" | "secure" | "blocked"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,                     // secure: "lock" | "prompt" | "signin"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,                      // blocked: "unsupported" | "disabled" | "policy"
                                              //          | "agent" | "view-only"
}
/// Viewer → Host. Control only. Sent only to a host with `HostCaps::sas`.
#[serde(rename = "sas")]
SecureAttention,
/// Viewer → Host. Control only. Sent only to a host with `HostCaps::lock`.
#[serde(rename = "lock")]
LockSession,
/// Host → Viewer: how a `sas` or `lock` ended. Only to a viewer with `Caps::hstate`.
#[serde(rename = "actr")]
ActionResult {
    action: String,                           // "sas" | "lock"
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,                      // "policy" | "unsupported" | text
}

// signaling.rs
pub struct Caps { pub decode: Vec<String>, pub popout: bool, pub hstate: bool }
pub struct HostCaps { pub vpause: bool, pub sas: bool, pub lock: bool }
// every new flag: default false, omitted when false (the pattern of `popout`, `vpause`)
```

`HostScreen` parses an unknown value as `normal`, and unknown `kind` / `why` strings are shown
generically, so a later host can add values. Compatibility:

| Pair | Result |
|---|---|
| New host → old viewer | No `Caps::hstate`: the host sends neither `hstate` nor `actr`. The viewer behaves as today |
| Old host → new viewer | No `hstate` ever arrives: the viewer assumes `normal`. No `sas` / `lock` in `HostCaps`: the actions are not offered |
| New viewer → old host | Not sent (capability). If sent anyway, the host fails to parse it as `ControlMsg`, then as `InputMsg`, and drops it (`crates/engine/src/session/driver.rs:1042-1052`) |

No tag collides with `InputMsg` (`mm`, `md`, `mu`, `wh`, `kd`, `ku`,
`crates/protocol/src/input.rs:41-61`). No signaling-server change.

### 3.3 Engine

```rust
// events.rs
SessionEvent::HostScreen { screen: protocol::HostScreen, kind: Option<String>, why: Option<String> }
SessionEvent::ActionResult { action: String, ok: bool, why: Option<String> }

// sessions.rs
impl Engine {
    pub fn send_secure_attention(&self, id: SessionId);   // viewer; no-op without HostCaps::sas
    pub fn lock_remote(&self, id: SessionId);             // viewer; no-op without HostCaps::lock
}
```

- **Host media loop** (`media/host.rs`). The screen state belongs to the **session**
  (`SessionScreen` in `Shared::host_screen`, a leaf lock). After `open_capture` returns, after
  every `acquire` (read at the top of the next pass) and right after every `reinit`, each stream
  records `cap.screen_state()` there; nothing is sent while the lock is held. The session's state
  is the main stream's observation while it runs; while it is paused, the pop-out streams'
  (blocked if any is blocked, then secure, then normal); with the main stream paused and no
  pop-out, it stays as it was. A stream counts only what it observed since it last acquired: a
  resumed main stream counts again from its first acquire or reinit, so its pre-pause state is
  never sent. The **transport thread** alone takes each change (`pending_hstate`) and queues one
  `hstate` for it (queued until the channel opens), only for a viewer with `Caps::hstate` — one
  change is one message, in order, whichever streams took part. While `open_capture` waits on
  `Lost` (`host.rs:331-341`), the stream records `Blocked(NotSupported)` once, at the **second**
  refusal in a row (about 100 ms in), so a single transient refusal at open shows no card. A
  paused main stream does not acquire (`host.rs:660-663`).
- **D8 in the loop.** If the answer is "view-only viewers do not see secure screens": while the
  state is `Secure` and the session has no control, every stream's loop releases frames without
  encoding and the main stream reports `blocked` with `why: "view-only"`. The permission is read
  each pass, so a live change takes effect at the next frame and forces a keyframe.
- **Host driver** (`session/driver.rs`). `SecureAttention` and `LockSession` are handled next to
  `SelectDisplay`: only on the host, only while the control gate is open, otherwise ignored with
  a debug line. They call `platform.secure_attention(done)` / `platform.lock_session(done)` and
  return at once; `done` sends `actr` through `ctl_tx` from whichever thread runs it. A host-side
  `Toast` reaches only the host's own UI (`crates/engine/src/events.rs:113`), which is why a
  result message is needed. `sas` is accepted at most once a second.
- **Viewer driver.** `HostState` becomes `SessionEvent::HostScreen`. On `blocked` it also hides
  the pop-out surfaces (as link trouble does, `driver.rs:232`, `:416`). `Shared::host_block` is
  an epoch counter the present loops read. Each change makes every loop drop the frame it holds,
  so that a later show cannot repaint a picture from before the lock (`ViewOp::Show` repaints
  the held frame, `session/active.rs:37-48`), **and decode nothing until the next keyframe**:
  the control channel and video are not ordered against each other, so frames from before the
  block can arrive after its end, and the host restarts with a keyframe (K5). When a block has
  been over for 300 ms and no keyframe has come (it can arrive before `normal`, and is then
  never shown), a loop whose video is on screen asks for one, at most once a second. Before
  any present the loop reads the epoch again.
- **ActiveGate** (`session/active.rs`). A session's video is shown when it is on screen, ready,
  its link is not in trouble, no overlay is open in its window (`active.rs:5-8`) **and the host
  is not `blocked`**. `SessionView::set_host_blocked(bool)` follows `set_trouble`
  (`active.rs:187`).
- **File drop.** The paste chord after a dropped file (`session/files.rs:169-170`) is skipped
  unless the last state the host loop saw is `Normal`.
- **Capabilities.** The viewer's `Caps` adds `hstate: true` next to `popout`
  (`crates/engine/src/sessions.rs:429-431`). The host's `HostCaps { vpause: true }`
  (`sessions.rs:300`) adds `sas` and `lock` from `platform.host_features()`; `lock` only when the
  host's mode is `password` (D9).

### 3.4 The pipe (`crates/agent-ipc`, `elevation::pipe`)

**Name.** `\\.\pipe\ShareCtrlScreen.<128 random bits, hex>`, new for every agent start. The agent
creates the pipe, then tells the service the name over its control channel (§3.6); the service
writes it as a value named by the session id in a **volatile registry key** under `HKLM`. The
host reads the name there. The name is published only after the pipe exists, so nobody can
create it first.

- Volatility belongs to a key, not a value, and the flag is ignored for a key that already
  exists. The service creates the key itself with an explicit, protected security descriptor
  (SYSTEM and administrators full access, users read) instead of inheriting one.
- A volatile key under `HKLM` lasts until a full shutdown, and an ordinary shutdown is a fast
  start-up shutdown by default. So the service **deletes every published name when it starts**
  and removes a session's name when that session's agent stops. A stale name is never trusted
  anyway: the host checks what it opened (below).

**Creation.** Byte mode, one instance, overlapped, `FILE_FLAG_FIRST_PIPE_INSTANCE`, and
`PIPE_REJECT_REMOTE_CLIENTS` in the pipe mode. Security descriptor:

- Owner SYSTEM, set explicitly.
- SYSTEM: full access.
- The session's **logon SID** (not the user's SID: pipe names are machine-wide, and the same
  user can be signed in to another session): `FILE_READ_DATA | FILE_WRITE_DATA |
  FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE`. Individual rights, never
  `FILE_GENERIC_WRITE` or `GENERIC_WRITE`, because those include the right to create instances.
- A medium integrity label that blocks reading as well as writing from below
  (`S:(ML;;NWNR;;;ME)`): without the read part a low-integrity process could open the single
  instance and hold it.

The agent takes the logon SID from that session's own token, never from its command line. If
creation fails the agent logs why and exits non-zero; with a random name this means a real
fault, not a squatter.

**The host's open.** With exactly the rights of the access list above (asking for
`GENERIC_WRITE` would be refused) and `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, so the
server cannot act as the host's user; without the second flag the level would be impersonation.
`ERROR_PIPE_BUSY` (the one instance is taken) is a retry at the next `reinit`, not an error.

**The host's check of the server.** The pipe object's owner is SYSTEM (`GetSecurityInfo` on the
handle, which is why the host has `READ_CONTROL`). The host never needs to open the agent's
process. `GetNamedPipeServerSessionId` is documented only for a handle the server created; if
the U3.0 spike shows it works on the host's handle, "the server's session is the host's own" is
added as a second check, and recorded as resting on a device test. An elevated administrator
could create a pipe owned by SYSTEM; an administrator is outside what this check defends
against.

**The agent's check of the client.** The client's process (`GetNamedPipeClientProcessId`, a
handle kept for the life of the connection so the id cannot be reused) is `sharectrl.exe` from
the agent's own folder, in the agent's session, running as the session's user. This keeps other
accounts and accidents out. **It is not a defence against the user's own programs** (plan 10
§3.2), and no rule below relies on it.

**Framing.** `u32 LE header length`, `u32 LE tail length`, the header (JSON, UTF-8), the tail (raw
bytes, used only for cursor images). Limits: host → agent, header ≤ 4 KiB and no tail; agent →
host, header ≤ 64 KiB and tail ≤ 1 MiB. Anything over a limit, a header that does not parse, an
unknown `t`, or a message that is not valid in the current state closes the connection. A parse
error is logged as a kind and a length, never with any of the payload.

```rust
pub const VERSION: u32 = 1;
pub const MAX_ATTACHMENTS: usize = 4;          // the main stream and three pop-outs

// host → agent
enum ToAgent {
    Hello { v: u32 },
    /// Start capturing this output for attachment `a` (an id the host chooses, unique
    /// among its live attachments).
    Attach { a: u32, output: u32 },
    Detach { a: u32 },
    /// The host has finished reading frame `seq` of `a`; its section may be rewritten.
    Ack { a: u32, seq: u64 },
    TargetRect { rect: Option<Rect> },
    Input { msg: protocol::InputMsg },
    SecureAttention { id: u32 },
}

// agent → host
enum FromAgent {
    Hello { v: u32 },
    /// What the agent will serve right now, and why not. Sent after Hello and on every
    /// change. `serving: false` ends every attachment.
    Desktop { serving: bool, kind: Option<SecureKind>, why: Option<Refusal>, name: String },
    /// The answer to Attach: the section handle (already duplicated into the host), its
    /// size in bytes, and the picture's geometry. `picture: false` = attached, but the
    /// output delivers no picture yet (display asleep).
    Attached { a: u32, section: u64, len: u64, width: u32, height: u32, stride: u32, picture: bool },
    /// The answer to an Attach the agent will not serve.
    Refused { a: u32, why: Refusal },
    /// Frame `seq` of `a` is in its section. `dirty` empty = everything. The tail carries a
    /// new cursor shape as Desktop Duplication delivered it (the host converts it with
    /// `pointer_shape_to_bgra`, `crates/platform-windows/src/adapters.rs:97`).
    Frame { a: u32, seq: u64, dirty: Vec<Rect>, pointer_only: bool, cursor: Option<CursorNote> },
    /// The geometry of `a` changed; the attachment is over. The host attaches again.
    Resized { a: u32 },
    SecureAttentionResult { id: u32, ok: bool, why: Option<String> },   // why: "policy" | text
}
enum Refusal { NotSecure, NotAllowed, TooMany, NoSuchOutput, Failed }
```

- A version mismatch in `Hello` closes the connection; the host reports
  `Blocked(AgentUnavailable)`. Host and agent come from one installer and are replaced together,
  so a mismatch means a half-finished upgrade.
- The host validates `stride × height ≤ len`, `stride ≥ width × 4` and the size limits of a
  display before it maps a section. A violation closes the connection.
- **Late answers.** An `Attached` whose `a` the host is no longer waiting for (the wait timed
  out) is handled by the reader thread: it closes the section handle and sends `Detach { a }`.
  Nothing is leaked when a retry loop outruns the agent.
- **The section.** Unnamed, one per attachment, `stride × height` bytes, created with an
  explicit security descriptor (SYSTEM only) and duplicated into the host with read access
  only. Whether the holder of a handle can raise its own access is not documented, so the agent
  treats the section as write-only all the same: **it never reads a section and never takes a
  decision from its contents.**

### 3.5 The agent (`crates/secure-agent`)

**Start-up.** The agent first makes itself Per-Monitor-V2 DPI aware, before any other
window-system call: the injector's geometry is physical pixels (`crates/input/src/lib.rs:106`)
and the service binary has no manifest (`service/build.rs`). "Already set" (access denied) is
success; any other failure ends the agent. It then runs only if all of these hold, else it logs
and exits non-zero: it is LocalSystem; it holds SeTcbPrivilege; it connected to the control pipe
named on its command line and that pipe's server is the service (§3.6). So an `--secure-agent`
started by hand does nothing.

**What it serves — decided by the agent from what it reads itself:**

| Input desktop | Session | Machine switch (U5, D7) | The agent |
|---|---|---|---|
| `Default` | any | any | Nothing (`Refusal::NotSecure`) |
| `Winlogon` | locked | any | Serves: `kind: Lock` |
| `Winlogon` | not locked | on | Serves: `kind: Prompt` |
| `Winlogon` | not locked | off or absent | Nothing (`Refusal::NotAllowed`) |
| any other name (`ScreenSaver` included) | any | any | Nothing (`Refusal::NotSecure`) |

"Locked" comes from SYSTEM-side sources only: the lock and unlock events the service receives
for the session and passes on (§3.6), with the session's state as Windows reports it to a
SYSTEM process as the value at start. Microsoft documents the events as the way to know; the
query can fail and can answer "unknown". **Anything but a positive "locked" counts as not
locked**, so a doubt falls to the row that needs the administrator's switch. The machine switch
is a value under `HKLM` that only administrators can write. The agent reads none of this from
the host, the user's registry hive or the user's files. Until U5 the switch is not read and
counts as off. The order of the lock event and the desktop switch is not documented; the U3.0
spike measures it, and `agent-watch` re-evaluates on both.

**Threads**, each created for one job. None creates a window, installs a hook or enters a COM
single-threaded apartment, so `SetThreadDesktop` works on all of them.

| Thread | Job |
|---|---|
| `agent-pipe` | Overlapped accept and read; checks the client; routes messages. On disconnect: end every attachment, release held keys (below), accept again |
| `agent-write` | The one writer: a bounded queue of outgoing messages |
| `agent-capture-<a>` | One per attachment: follow the input desktop (`elevation::InputDesktopFollower`), duplicate the output, copy changed rectangles into the section, queue `Frame`, wait for `Ack` |
| `agent-input` | Follow the input desktop before every event, inject with `input::Injector` |
| `agent-watch` | Re-evaluates the table above when the duplication reports access lost, when the service signals a session change, and once a second; queues `Desktop` on change |
| `agent-control` | The control channel to the service (§3.6); exits the process when the service's end closes |

**Capture rules.**
- An attachment exists only while the table says "serves". When it stops, every capture thread
  drops its duplication and its section, and `Desktop { serving: false }` goes out.
- A duplication loss that is not a desktop change (a mode change) ends that attachment with
  `Resized`; the host attaches again and reads the new size.
- One section, one writer: a capture thread writes its section only after the previous `Ack`.
  Changes that arrive meanwhile are merged into the next frame's rectangles. If no `Ack` comes,
  the thread idles; it never blocks anything else.
- At most 30 frames a second per attachment.
- **One duplication per output.** Windows allows a process one duplication of an output. What a
  second attachment to an output already being served gets (a refusal, or frames from the one
  duplication) is open, and is settled in U3's structure review against how the host treats
  two captures of one output today. `DXGI_ERROR_UNSUPPORTED` and
  `DXGI_ERROR_SESSION_DISCONNECTED` from the duplication are retried, not fatal.
- **Display asleep.** If the output delivers no picture, the attachment still succeeds
  (`picture: false`) and the thread retries the duplication once a second and after every
  injected event. The first forwarded input wakes the display, as it would on the desktop.

**Input rules.**
- Key and button **downs** are injected only while the table says "serves".
- Key and button **ups** are always injected when the agent's thread can reach the input desktop,
  whether or not the agent saw the matching down. Refusing an up can only leave a key stuck.
- The agent remembers which downs it injected and has not seen released. It releases them when
  the pipe closes or the agent is told to stop, **while it is still on the secure desktop**. Once
  the input desktop is `Default` again, releasing is the host's job (§3.8), because the agent
  does not inject there.
- Pointer events are mapped with the last `TargetRect`, exactly as the host's injector maps them.

**Logging.** `sharectrl-agent.log` next to the binary, for the reason the service logs there
(`service/src/main.rs:124-127`); appended, with a size cap and one rotation, not truncated at
start. Never a key code, a typed character, a pointer position or any part of a message.

### 3.6 Service ↔ agent

A **named control pipe served by the service**, one per agent launch. (The first design passed
two anonymous pipe handles by inheritance. Windows does not inherit handles across sessions,
and anonymous pipes cannot be overlapped; plan 10 §8 row 18.)

- The service creates `\\.\pipe\ShareCtrlScreen.ctl.<128 random bits, hex>`: one instance,
  overlapped, first-instance, remote clients rejected, owner SYSTEM, access for SYSTEM only.
  It passes the name on the agent's command line. The name is not a secret: the access list
  is what keeps everyone but SYSTEM out.
- The service accepts exactly one client and only if its process id is the agent it has just
  created (`GetNamedPipeClientProcessId` on the service's own handle, compared with the id
  from the launch, while the service holds that process's handle). Anything else is
  disconnected and the launch counts as failed.
- The agent opens the pipe with identification-level impersonation only, and checks that the
  pipe's owner is SYSTEM before it sends anything.
- Either side closing the pipe ends the relationship: the agent exits (§5.9), the service
  treats the agent as gone.

Line protocol, UTF-8, one request at a time:

```
agent → service:   pipe <name>              the client pipe exists under this name
                   sas                      at most one a second
service → agent:   sas ok | sas policy | sas failed <text>
                   session <event>          lock, unlock, logon, logoff: the agent's source for
                                            "locked" (§3.5), sent as they arrive and once at
                                            connect with the current state
```

**`SendSAS` and the policy (D3, the recommended behaviour).** The call is made by the service:
Windows lets a service call it, and a SYSTEM process that is not a service only under conditions
the agent does not meet. To aim at the agent's session the service impersonates the agent's end
of the control pipe for the call, which is the documented way to choose the session; U4
verifies on a device that this reaches the console session. If the policy value that allows
services to send it is absent, the service sets it, calls `SendSAS(FALSE)`, and removes it
again. If the value is present and allows services, it just calls. If it is present and forbids,
it answers `sas policy` and changes nothing. A marker under the service's own key records "we
set it", so a crash between set and remove is repaired at the next service start. The value's
name and numbers are not in Microsoft's documentation; U4 confirms them, and how quickly
Windows picks up a change, on a device before this behaviour is relied on.

### 3.7 Service changes

- `RegisterServiceCtrlHandlerExW`, accepting stop, shutdown and session change. The handler only
  records the event and signals the loop; all work stays on the loop's thread. The handler
  takes no lock (Microsoft's rule for control handlers), and the controls are accepted from the
  moment the service reports running, not before.
- The loop waits on that signal with a 2 s timeout instead of sleeping
  (`service/src/main.rs:300`), so the poll stays as the fallback. An event and the poll can report
  the same change in either order; both are idempotent inputs to the supervisor.
- `Supervisor::user_arrived(now)`: a sign-in in the active session turns a pending `Retry` into
  `Wanted` and clears the failure count (plan 10 §2 fact 7). This is U2 and ships alone.
- From U3: a second `Supervisor` for the agent. It wants an agent exactly when the host is
  wanted: a user is signed in at the active console session. Exit code 0 means "told to stop";
  anything else is relaunched with the same doubling delay (`service/src/supervise.rs:28-33`).
- **Launch** (`elevation::agent`): create the control pipe (§3.6); duplicate the service's own
  token as a primary token, set its session id to the console session (SeTcbPrivilege is
  enabled the same way as for UIAccess, `crates/elevation/src/uiaccess.rs:172-209`); create the
  process suspended on `winsta0\default`, inheriting no handles, with an explicit process
  security descriptor that grants SYSTEM only (so the user's programs cannot end it); assign it
  to a **new** job object that kills its processes when the job handle closes; resume; accept
  the agent on the control pipe. A job belongs to the session of the first process put in it,
  so each launch gets its own job and the agent is its first process; a job is never reused for
  another session. If the assignment is refused, the launch continues without the job and logs
  it once: the agent still exits when its control pipe closes.
- **Stop.** On service stop, on a session change away from the agent's session, and on sign-out:
  close the control pipe, wait up to 5 s for the agent to exit, then terminate it and wait for
  the handle. Only then report stopped (K10). On **system shutdown** the service terminates the
  agent at once, without the wait.
- **`--stop-service` and `--uninstall-service`** (`crates/elevation/src/service.rs:416` waits only
  for the service's own process) also end any process still running `--secure-agent` from this
  binary, for the case of a service that died without stopping its agent.
- The published pipe name is removed when its agent stops.

### 3.8 Host's Windows platform (`secure_desktop.rs`)

```rust
pub(crate) struct AgentClient { /* overlapped pipe, reader thread, writer thread, slots */ }
impl AgentClient {
    /// Never called on `sct-transport`. Reads the published name, opens, checks the server, Hello.
    fn connect() -> Result<Arc<Self>, AgentError>;
    fn serving(&self) -> Option<SecureKind>;             // the last Desktop message (atomic)
    fn attach(&self, output: u32, timeout: Duration) -> Result<Attachment, AttachError>;
    fn enqueue_input(&self, msg: &InputMsg);             // never blocks
    fn secure_attention(&self, done: ActionDone);        // never blocks
}
pub(crate) struct Attachment { /* id, mapped section, geometry; Drop sends Detach */ }
impl Attachment {
    fn next_frame(&self, timeout: Duration) -> Result<Option<FrameNote>, AgentError>;
    fn ack(&self, seq: u64);
}

pub struct SecureDesktopCapture { device, output, source: Source, … }
enum Source {
    Local(Option<Duplication>),        // None: not duplicating yet (the W8 start-up race)
    Agent { attachment: Attachment, tex: Texture, held: Option<u64> },
    Blocked(BlockReason),
}
```

**The client.** One per host process, shared by every capture object and the injector. It is
created on a capture thread's `reinit` (never on `sct-transport`, which drives ICE, RTP and the
liveness pings) and at most once a second. It owns a reader thread (`sct-agent-rx`) and a writer
thread (`sct-agent-tx`) on one overlapped handle. The reader fills: the `serving` atomic; one
frame slot per attachment (a mutex and a condition variable); the answers to `attach`; the `done`
callbacks of `secure_attention`. Everyone else only enqueues to the writer's bounded queue. When
the queue is full, pointer moves are dropped; anything else that does not fit within 2 s breaks
the connection. Lock order: every slot's mutex and the queue's mutex are leaves; none is held
while another is taken, across a D3D call, or while an engine event is sent.

A broken pipe is not an error to the user: the client is dropped, attachments report `Lost`, the
state becomes `Blocked(AgentUnavailable)`, and a later `reinit` connects again.

**"Refused" has one meaning.** The wrapper turns to the agent only when `OpenInputDesktop` itself
was denied. A duplication that fails while the follow succeeds (the start-up race of
`adapters.rs:220-228`, a display asleep on the normal desktop) is today's transient: the source
stays `Local(None)`, `reinit` returns `Lost`, and the state reported stays `Normal`.

**The injector and the ledger of held keys.** `SendInputInjector` becomes the one place that
knows which keys and buttons are down, because it sees every event whatever its route:

- Route of a **down**: local if the local follow succeeds; else the agent if `serving()`; else
  dropped (counted, logged once per outage).
- Route of an **up**: wherever a down could go now. An up is sent even when the ledger has no
  matching down on that route; the local `input::Injector` must emit it too (to verify in U3.1
  that it does not swallow ups for keys it did not record).
- The ledger records a key at its down and clears it at its up, on whichever route.
- `release_all` (control revoked, session end; the existing contract,
  `crates/platform-api/src/input.rs:28-29`) sends an up for every key in the ledger by the
  current route, then clears it.
- When the route changes (`serving()` flips, or a local follow starts to succeed or fail) nothing
  is released: keys the controller still holds stay held, and their ups go by the new route.
- `set_target_rect` is remembered and forwarded as `TargetRect`.
- `focus_window_at` returns `false` while the agent is the route.

### 3.9 App and UI

| Piece | Contract |
|---|---|
| Event `host-screen` | `{ remoteId, screen, kind, why }` from `SessionEvent::HostScreen`, emitted by the TabManager (`app/src/tabs.rs`, where viewer session events go; `bridge.rs` passes them on) |
| Snapshot field `hostScreen` | On each tab of the `tabs-changed` snapshot (and `get_snapshot`): `{ remoteId, screen, kind, why }` as last received, or `null` (no session, or a host that never sent `hstate`). Reset on connect and on end. Detached windows draw from it |
| Event `action-result` | `{ remoteId, action, ok, why }` |
| Command `send_secure_attention(remoteId)` | → `Engine::send_secure_attention` (the pattern of `request_refresh`, `app/src/commands.rs:266-274`, `app/ui/api.js:66`) |
| Command `lock_remote(remoteId)` | → `Engine::lock_remote` |
| Session bar (`app/ui/index.html:134-146`) | "Ctrl+Alt+Del" and "Lock", each hidden unless the session has control and the host advertises it; in detached windows too |
| Native menu (`app/src/menu.rs`) | The same two items under Session on macOS and Linux. The menu is documented as "exactly three always-enabled items" (`menu.rs:15`); U4 changes that contract and its comment |
| The tab's message | While `blocked`: what the remote computer is showing and why it cannot be shown, in plain words per `why`, with what to do. Uses the tab's no-picture body (`app/ui/index.html:147-149`) like the link-trouble panel (`app/ui/tabs.js:250`) |
| Badge | While `secure`: "LOCK SCREEN", "WINDOWS PROMPT" or "SIGN-IN" by `kind`, next to the peer label |
| Setting | Settings → "Allow control of the lock screen", Windows hosts only, default on. Saved as `allowLockScreenControl`; applied with `platform.set_secure_desktop_control` at start and on change |

`Config` gains `allow_lock_screen_control: bool`, default `true`. The struct is
`#[serde(default)]` (`crates/protocol/src/config/mod.rs:119-121`), so an existing file loads with
the default and no migration or version bump is needed. Turning it off while attached drops every
attachment at once: the wrapper reports `Lost`, then `Blocked(Disabled)`.

The machine switch for UAC prompts (U5) is not in `config.json` and not in this table: it is set
by an administrator through an elevated action, designed with U5.

## 4. State machines

### 4.1 The capture wrapper's source

```
                         OpenInputDesktop denied, setting on,
                         agent serving, Attach answered
   Local(dup) ─────────────────────────────────────────────────────► Agent
      ▲  ▲                                                             │
      │  └──── follow ok, duplication ok ◄── Local(None) ◄─────────────┤ Desktop{serving:false},
      │                                         ▲                      │ Resized, pipe broken,
      │ follow ok,                              │ follow ok,           │ setting turned off
      │ duplication ok                          │ duplication fails    ▼
      └─────────────────────────────────── Blocked(reason) ◄───────────┘
                                             ▲
          OpenInputDesktop denied and ───────┘
          (no service | setting off | agent refuses | agent absent | Attach times out)
```

- Transitions happen only inside `reinit` (the constructor runs one). Each `reinit` tries, in
  order: the local follow and duplication; if the follow was denied, the agent.
- `acquire` returns `Lost` whenever the current source cannot deliver: the duplication reports
  access lost or denied; the attachment ended (`Desktop { serving: false }`, `Resized`, a broken
  pipe, the setting); always in `Local(None)` and `Blocked`.
- `screen_state()`: `Local(_)` → `Normal`; `Agent` → `Secure(kind)`; `Blocked(r)` → `Blocked(r)`.
- The reason in `Blocked`: no service installed or the host is not next to it → `NotSupported`;
  the setting → `Disabled`; `Refused { NotAllowed }` → `NotAllowed`; everything else →
  `AgentUnavailable`. `Refused { NotSecure }` (the prompt was already gone) → `Local(None)`.
- `dimensions()`: the size the duplication reports, or the attached size. Without either
  (`Local(None)`, `Blocked`) the output's mode as the duplication would report it, not the
  rotated desktop rectangle `displays.rs` uses.
- `device()`: the wrapper's device, the same in every state.
- The constructor fails with `NoOutput` when the output does not exist, as today, so
  `open_capture`'s fallback to the primary display keeps working (`media/host.rs:308-318`). It
  fails with a backend error when the device cannot be created. It does not fail because the
  desktop cannot be reached.

Before U3 there is no wrapper: `DdaCapture` itself reports `Blocked(NotSupported)` after a
denied `OpenInputDesktop` and `Normal` otherwise.

### 4.2 The agent's connection

```
Listening ──client connects, checks pass──► Connected ──Hello ok──► Ready ◄──────────┐
    ▲                                           │                     │ Attach, serving│ last Detach,
    │                                           │                     ▼               │ or not serving
    └──── disconnect / check fails / bad ───────┴─────────── Attached(1..=4) ─────────┘
          message (from any state)
```

`Input` and `SecureAttention` are valid in `Ready` and `Attached`. `Attach` while not serving is
answered with `Refused` and changes nothing.

### 4.3 The service's view of a session

```
NoUser ──the user's token appears (checked when an event or the poll wakes the loop)──► User: host Wanted, agent Wanted
User ──sign-out / session change──► NoUser: agent stopped, host left to end with its session
```

Each of host and agent then follows the existing `Supervisor` states (`Wanted`, `Running`,
`Stopped`, `Retry`, `service/src/supervise.rs:35-45`), independently.

## 5. Flows

### 5.1 The machine locks during a session

1. [W] The local duplication reports access lost; `acquire` returns `Lost`.
2. [H] The loop calls `reinit` (`media/host.rs:790-791`).
3. [W] `OpenInputDesktop` is denied. The setting is on; the client is connected (or connects
   now) and says `serving: Lock`. [W] sends `Attach { a, output }`.
4. [A] Its own table says "serves": it duplicates the output, creates the section, hands the host
   a handle, answers `Attached`.
5. [W] Validates and maps the section, creates its texture, source = `Agent`; `reinit` returns
   `Ok`.
6. [H] Re-reads the size, reads `screen_state()` = `Secure(Lock)`, sends `hstate secure lock`,
   forces a keyframe (`host.rs:813-819`).
7. [A] Sends `Frame` 1 with everything dirty. [W] copies it into the texture and returns it from
   `acquire`; [H] encodes; on `release` [W] sends `Ack`.
8. [V] Receives `hstate`; [VU] shows the badge. The picture is the lock screen.

If step 3 or 4 fails, the source is `Blocked(reason)`, `reinit` returns `Lost`, [H] sends
`hstate blocked` and retries every 250 ms as today; see 5.5.

### 5.2 The controller unlocks it

1. [VU] The controller presses a key. [V] sends the usual input messages.
2. [H] The control gate is open; the driver calls `dispatch` (`session/driver.rs:1044-1050`).
3. [W] The local follow is denied and the agent is serving: the event is queued for [A] and
   entered in the ledger.
4. [A] Its input thread follows the input desktop and injects. The lock screen shows the password
   box; the controller types and presses Enter; Windows signs in.
5. [A] `agent-watch` sees `Default`: every attachment ends, `Desktop { serving: false }` goes
   out. The Enter key may still be down.
6. [W] `acquire` returns `Lost`; [H] calls `reinit`; the local follow works; source =
   `Local(dup)`.
7. [H] Sends `hstate normal`, forces a keyframe. The session continues on the desktop.
8. [W] The controller's Enter-up arrives: the local route, sent although the local injector never
   saw the down; the ledger clears it. If the viewer disconnects before the up, the engine's
   `release_all` releases Enter from the ledger by the local route.

### 5.3 Connecting to a machine that is already locked

1. [H] Password mode: the proof is checked and the session is accepted without a person
   (`pans/00-OVERVIEW-AND-PROTOCOL.md:118-130`). In approve mode nobody answers and the request
   times out; nothing in this plan changes that.
2. [H] `open_capture` calls `create_capture`. [W] creates the device, then runs its first
   `reinit`: follow denied → agent → source = `Agent`. `create_capture` succeeds.
3. [H] Creates the encoder on the wrapper's device (`media/host.rs:422`) and continues as in 5.1
   from step 6. That this works while the Winlogon desktop has input is what U3.0 establishes;
   a failure here ends the media loop today, which is why U3.0 gates the phase.

If the agent cannot attach, `create_capture` still succeeds with source = `Blocked`: the loop
runs, sends `hstate blocked`, and retries. `open_capture`'s own wait (`host.rs:331-341`) then
serves only the other platforms.

### 5.4 The locked machine's display is asleep

This is the usual state of a machine someone connects to while it is locked: Windows turns the
display off 60 seconds after the console locks, by default.

1. [W] Attaches as in 5.1. [A] answers `Attached { picture: false }`.
2. [H] Sends `hstate secure lock`. No frame arrives; the viewer shows the badge and whatever the
   decoder last had, which the message under the badge explains ("the remote display is asleep;
   move the mouse to wake it").
3. [VU] The controller moves the mouse. [W] forwards it; [A] injects it; Windows wakes the
   display; [A]'s capture thread gets its duplication and sends `Frame` 1.

A view-only viewer cannot wake it; that is the same on the normal desktop today.

### 5.5 No service, the setting off, the agent refusing or unavailable

1. [W] `OpenInputDesktop` is denied and the agent route is not available. Source =
   `Blocked(reason)`.
2. [H] Sends `hstate blocked` with `why`.
3. [V] Hides the video and the pop-outs and drops the held frame; [VU] shows the message.
4. [H] Keeps retrying. When the machine is unlocked locally the local follow works again and
   [H] sends `hstate normal`.

### 5.6 Ctrl+Alt+Del

1. [VU] The action is visible because the host advertised `sas` and the session has control.
   [V] sends `sas`.
2. [H] Control gate open, rate limit passed → `platform.secure_attention(done)`; the driver's
   loop continues at once.
3. [W] Queues `SecureAttention { id }` for [A]; [A] writes `sas` to [S]; [S] applies the policy
   rule of §3.6 and calls `SendSAS`; the answer travels back to [W]'s reader thread, which runs
   `done`.
4. [H] `done` sends `actr` to [V]. On `policy`, [VU] says so.

### 5.7 Lock from the viewer

[V] sends `lock` → [H] control gate → `platform.lock_session(done)` (the host itself, as the
user) → `actr` → the machine locks → flow 5.1 follows by itself. Windows locks asynchronously
and reports only that the request was accepted, so `actr ok` means "requested"; the viewer
learns that the machine is locked from `hstate`. A host in `approve` mode does
not advertise `lock` (D9).

### 5.8 A UAC prompt (U5)

- **Machine switch on.** As 5.1 and 5.2 with `kind: Prompt`: the prompt takes input, the wrapper
  moves to `Agent`, the controller answers, the desktop returns, the wrapper moves to
  `Local(dup)`.
- **Machine switch off (the default recommended in D7), and every host before U5.** [A] answers
  `Refused { NotAllowed }`; the wrapper is `Blocked(NotAllowed)`; the viewer reads "the remote
  computer is showing a Windows security prompt; someone at the computer must answer it".
- **A prompt that comes and goes inside one retry.** The follow is denied, [A] answers
  `Refused { NotSecure }`, the source is `Local(None)`, and the next `reinit` 250 ms later
  succeeds locally. No `hstate` is sent.

### 5.9 Stopping the service, upgrading

1. [S] Receives stop. It closes the agent's control pipe and removes the published name.
2. [A] `agent-control` sees the close: release held keys, close the client's pipe, exit 0.
3. [S] Waits for the agent's process handle (5 s, then terminate and wait), then reports stopped.
4. [W] The pipe broke: `Lost`, then `Blocked(AgentUnavailable)` if a secure desktop is up.
5. The installer replaces both binaries and starts the service
   (`app/installer-hooks.nsh:98-106`); [S] launches a new agent; [W] connects at a later `reinit`.

### 5.10 A process dies

- **The agent, while the machine is locked.** [W] broken pipe → `Lost` →
  `Blocked(AgentUnavailable)` → `hstate blocked`. [S] sees the exit code and relaunches after the
  delay; the new agent publishes a new name; [W]'s next connect succeeds → `Agent` →
  `hstate secure` and a keyframe. Keys the dead agent held were released by Windows with its
  process; the ledger still lists them, and their ups go out by the new route.
- **The host, while attached.** [A] The client's pipe closes: end every attachment, release the
  downs it injected (it is still on the secure desktop), back to `Listening`. [S] handles the
  host as today (`service/src/main.rs:266-278`).
- **The service.** [A] `agent-control` sees its pipe close and exits as in 5.9 step 2; the job
  object ends it if it does not.

### 5.11 Several captures in one session: monitor switch and pop-outs

A host has one session at a time (`crates/engine/src/sessions.rs:266`). Within it:

- **Pop-outs.** Each pop-out stream owns a capture object on its own thread
  (`media/host.rs:396`), so each has its own source and, on the lock screen, its own attachment
  for its own output (at most `MAX_ATTACHMENTS`). Every stream records what its capture sees;
  the pop-outs' observations decide the session's state only while the main stream is paused
  (§3.3, K13). Pointer
  input aimed at a pop-out reaches [A] with that stream's `TargetRect` (`InputTarget`,
  `session/driver.rs:978-1003`).
- **Monitor switch.** The new capture object is created while the old one is alive
  (`media/host.rs:496-497`): a second attachment for the other output, then the old one is
  dropped and detaches.
- **While `blocked`.** No stream delivers; the viewer hides the main video and the pop-outs
  together (§3.3), so nobody clicks on a stale picture.

### 5.12 Control is revoked on the lock screen

[H] The engine calls `release_all`. [W] sends an up for every key in the ledger by the current
route (the agent). Input after that is dropped by the control gate before it reaches [W]. If D8
says view-only viewers do not see secure screens, the next frame is withheld and [H] sends
`hstate blocked view-only`.

### 5.13 Sign-out and fast user switching

[S] The session event or the poll reports the change. The old session's agent is stopped (5.9
steps 1–3). The new session gets a host and an agent when a user is signed in there
(`service/src/supervise.rs:85-97`). A viewer connected to the old session's host loses it as
today.

### 5.14 A background tab

The viewer pauses the main stream of a tab that is not on screen; the host stops acquiring
(`media/host.rs:660-663`) and the main stream's observation stops counting. Pop-out streams are
not paused: while one is open, its capture speaks for the session, so a lock meanwhile reaches
the viewer as `blocked` and hides the pop-out (K21). With no pop-out open nothing of the host is
on screen at the viewer, and the host notices at resume: `acquire` returns `Lost`, and flow 5.1
runs from step 2; the main stream's pre-pause state is never sent in between. An attachment held
across a pause idles on its unacknowledged frame.

## 6. Thread and failure rules

- No `unwrap` or `expect` on a pipe, token, process, section, registry or D3D result. Each
  failure maps to a `BlockReason`, a `Refusal`, an error with a message a person can act on, or
  a logged line.
- The pipe is overlapped on both ends, with one reader and one writer thread each. No thread
  performs a blocking read or write on the handle.
- `sct-transport` never connects, never waits for an answer and never blocks on the queue: it
  enqueues input and starts actions whose results arrive on another thread.
- The agent's threads never create windows or hooks and never initialise a single-threaded COM
  apartment.
- No UI call and no engine event is sent while a lock is held.
- Every wait has a timeout: `attach` 2 s; a `sas` round trip 3 s (then `done(Failed)`); the
  service's wait for the agent 5 s; a frame wait is the engine's own `acquire` timeout.

## 7. Invariants

| # | Invariant |
|---|---|
| K1 | The agent captures and injects key and button downs only on the `Winlogon` desktop, and only in the cases its own table (§3.5) allows. It reads that table's inputs itself, never from the host. |
| K2 | No rule that limits the agent depends on the client being the genuine host. The client check keeps other accounts out and nothing more. |
| K3 | The agent opens no network connection and no file except its log, and reads nothing from the user's profile or registry hive. |
| K4 | No key code, typed character, pointer position or message payload is written to any log by the host, the agent or the service. |
| K5 | Every change of capture source and every change of size reaches the engine as `Lost` and is followed by a re-read of the size and a keyframe. |
| K6 | The agent writes a section only between an `Ack` and its next `Frame` and never reads one. The host reads it only between a `Frame` and its `Ack`, after validating its geometry. |
| K7 | A message over its size limit, unparsable, unknown or out of state closes the connection. The agent's parser has no `unsafe`. |
| K8 | Input reaches the agent only through the engine's control gate. |
| K9 | The host's ledger knows every key and button that is down, whatever its route. Ups always pass, on both sides. `release_all` empties the ledger by the current route. The agent's own release happens only while it is on the secure desktop (pipe closed, told to stop). |
| K10 | The service reports stopped only after the agent's process has ended; `--stop-service` leaves no agent running from the binary. |
| K11 | With the user's setting off the host never attaches. The setting is a preference; the agent's table is the protection. |
| K12 | A host without the service behaves as today, plus `hstate`. |
| K13 | `hstate` is sent when the ctl channel opens and whenever the session's screen state changes, only to a viewer that advertised `hstate`. The session's screen state is what the main stream's capture reports while the main stream runs; while it is paused, it is what the pop-out streams' captures report (blocked if any of them is blocked). With the main stream paused and no pop-out open, nothing of the host is on screen at the viewer and the state is observed at resume. |
| K14 | The pipe exists before its name is published. It is created with its access list (the session's logon SID, individual rights, no right to create instances), its integrity label (no read or write from below), an explicit SYSTEM owner, first-instance and reject-remote, or not at all. The service deletes every published name when it starts. |
| K15 | The agent ends when the service ends: it exits when its control pipe closes, and its own job object ends it where Windows allows the assignment. The control pipe is the service's, open to SYSTEM only, and accepts only the process the service has just created. |
| K16 | ShareCtrlScreen never stores, caches or replays a Windows password or PIN. |
| K17 | The normal desktop's capture and input path is the code that runs today; the agent is tried only after `OpenInputDesktop` was denied. |
| K18 | `sas` and `lock` are honoured only for a session with control; `sas` at most once a second; `lock` is advertised only in `password` mode. |
| K19 | An attachment the host is not waiting for is closed and detached by the reader; no section handle outlives its attachment. |
| K20 | Nothing on `sct-transport` waits for the pipe. |
| K21 | While the host is `blocked`, the viewer shows neither the main video nor a pop-out, and no frame from before the block is shown afterwards: not the one it held, and not one that arrives late (nothing older than the first keyframe after a block is decoded). Residual, plan 10 §10 F3: a **keyframe** from before the block that arrives only after its end cannot be told from the host's restart keyframe and is shown until that one comes; a present already under way when the block lands may finish. |

## 8. The sign-in screen: constraints for step S6

No structure is given here yet. S6 writes it, under these constraints from the design review:

1. **The profile is set with an administrator's consent.** Which address, server and password
   answer at the sign-in screen is machine-wide state. It is written by an elevated action, never
   by a request from a user-level process (K2 applies: such a request could come from any
   program).
2. **A session that began at the sign-in screen never continues into another account's
   desktop.** At the latest when someone signs in with an account other than the profile's
   owner, that session ends. Whether it may continue for the owner's own sign-in is for S6.
3. **The profile's password hash is a secret.** It is enough to answer the challenge
   (`pans/00-OVERVIEW-AND-PROTOCOL.md:132-138`). It belongs in the OS secret store at machine
   scope, not in a file or registry key protected only by an access list.
4. **Least privilege for the network.** Plan 10 §3.1 keeps the network out of SYSTEM processes.
   S6 compares a pre-login process under a low-privilege identity that uses the agent (with a
   client rule for it and a `SignIn` row in §3.5's table) against the engine as LocalSystem, and
   justifies its choice.
5. **Consistent state.** Whatever captures the sign-in screen reports `Secure(SignIn)`, by the
   same rules as §4.1.

Open for S6: D4 and D6; the release and takeover of the address without a moment where neither
process is registered or both are; what the user's host shows meanwhile; sign-out during a live
user session; what an account sync means for a process with no account; uninstall and the
profile.
