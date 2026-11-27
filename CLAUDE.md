# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ShareCtrlScreen: screen sharing plus remote control between two machines over the internet. Each install generates a UUID v4 once, and that UUID is the machine's address. One binary does both roles (host and viewer).

- `native/` is **the product**: a Rust workspace with a Tauri 2 app (DXGI / Media Foundation / str0m on Windows). Windows ships; the macOS and Linux ports are in progress.
- `server/` is the signaling server (Cloudflare Worker + Durable Object). It only relays JSON (connect req/resp, SDP/ICE, ping/pong) and never sees pixels or input.
- `app/` is the **legacy** Electron app. It is still runnable but is not where the work happens.
- `pans/` holds the original design docs. **`pans/00-OVERVIEW-AND-PROTOCOL.md` is the wire contract**, and `pans/04-NATIVE-REWRITE-PLAN.md` is the native design (the code cites it as `§N`).
- `plans/` holds the plans. **`plans/00-DECISIONS.md` holds the owner's locked decisions, which override anything else in `plans/`.** Task IDs used in code and comments (F1, M4, L5, W5, D10, I5…) are defined in `plans/05-TASKS.md` (the macOS/Linux port). Multi-session tabs and account workspaces are planned in `plans/08-MULTI-SESSION-TABS.md`, with their structure and flows in `plans/08a-STRUCTURE-AND-FLOWS.md`. Detachable tab windows are planned in `plans/09-DETACHABLE-TABS.md` and `plans/09a-STRUCTURE-AND-FLOWS.md`. Remote unlock on Windows (the lock screen, UAC prompts, the sign-in screen) is planned in `plans/10-REMOTE-UNLOCK.md` and `plans/10a-STRUCTURE-AND-FLOWS.md`; only U1 and U2 are built (the viewer is told the host's screen cannot be shown; the service reacts to sign-in and other session events), none of the screens is reachable yet. Hardware test checklists live in `plans/checklists/`.

## Commands

Native workspace (run from `native/`):

```sh
cargo build --workspace --release
cargo test  --workspace                                  # unit tests on all 3 OSes
cargo test  -p <crate> --lib <module>::tests -- --nocapture   # single crate / module
cargo fmt --all --check                                  # CI gate
cargo clippy --workspace --all-targets -- -D warnings    # CI gate (warnings fail)
cargo deny check                                         # licence / banned-crate gate (CI)
cargo run -p smoketest --release [-- --mock|--software|--display <id>|--seconds <n>]  # capture→encode→decode latency go/no-go
cargo run -p gen-keymap                                  # regenerate platform-api/src/keymap.rs from data/dom_code_data.inc
```

Signaling server (run from `server/`): `npx wrangler dev --port 8787` (serves `ws://localhost:8787/ws`, no Cloudflare account needed) and `npm run test:accounts` (accounts API smoke test against wrangler dev). Pushes to `main` deploy the server automatically (`deploy-server.yml`).

Legacy Electron app (run from `app/`): `npm start`, or `npm start -- --profile b` for a second instance with its own UUID and config.

### The build lane (parallel agent work)

Under `/orchestrate`, agents work in worktrees under `.claude/worktrees/` and **do not run cargo directly**. A PreToolUse hook (`.claude/lane/guard.js`) refuses raw `cargo`. Instead, an agent writes a request file to `.claude/lane/queue/<TASK>-<nn>.request` (header: `task`, `side: windows|wsl`, `cwd`, `priority`, then `---`, then one `cargo`/`npm`/`npx` command per line with no shell metacharacters), ends its turn with `QUEUED <TASK>-<nn>`, and reads `.claude/lane/done/<name>.result`. The full protocol is in `.claude/lane/README.md`. Linux builds go through WSL (`side: wsl`, target dir `~/sct/<task>`).

### Env vars that turn test skips into failures

Hardware- and OS-dependent tests **skip** when a tool or device is missing. CI sets these variables so that a skip fails instead: `SCT_X11_TESTS_REQUIRED`, `SCT_WAYLAND_TESTS_REQUIRED`, `SCT_LINUX_INPUT_TESTS_REQUIRED`, `SCT_MAC_HW_TESTS_REQUIRED`, `SCT_CODEC_TESTS_REQUIRED` (a comma list of codec backends). Follow the same `SCT_*_REQUIRED` convention for new environment-dependent tests. The Linux X11 tests start private Xvfb servers. For the exact per-test invocations, see `.github/workflows/native-ci.yml`.

## Architecture (native)

The core is OS-independent and talks to the OS only through traits:

- **`crates/protocol`**: pure shared types (config schema, challenge-response, signaling/input/control message enums).
- **`crates/platform-api`**: **the OS boundary**. It defines traits for capture, codecs, the video surface + presenter, input injection, the shortcut hook, clipboard, secrets, autostart, paths and permissions. It has no OS dependencies. Feature `mock` provides in-memory backends (used by tests and `smoketest --mock`), and `unavailable.rs` covers capabilities a platform hasn't implemented yet (they fail safely). `keymap.rs` is **generated** by `tools/gen-keymap`, so never hand-edit it.
- **`crates/platform-{windows,macos,linux}`**: per-OS implementations. `platform-windows` wraps the Windows-only crates `capture` (DXGI), `codec` (MF encode + D3D11 decode), `render` (D3D11 flip-model), `input` (SendInput + `WH_KEYBOARD_LL`) and `elevation` (SYSTEM service primitives). macOS uses SCK/VideoToolbox/CGEvent, and Linux uses X11 (XShm/DAMAGE/XTest) plus Wayland portals/PipeWire/libei/uinput and VA-API. `surface-wgpu` is the shared wgpu presenter for the non-Windows video surfaces, and `codec-openh264` is the software fallback (Cisco binary downloaded at runtime, never bundled, for licence reasons).
- **`crates/engine`**: orchestration with no OS calls. It holds the handshake state machine (approve/password, view↔control) and the session (ICE, transport driver, media threads, clipboard/file transfer, tuning/bitrate).
- **`crates/transport`**: str0m WebRTC. Video travels on an **RTP media track with BWE/congestion control**. Data channels carry only ctl (reliable), cursor (unreliable) and bulk (reliable).
- **`crates/signaling`**: WebSocket client to the Worker (register, byte-exact ping/pong watchdog, backoff reconnect). The server's JSON protocol is reused verbatim from the Electron app; only the opaque `signal.data` payloads differ.
- **`app/`**: the Tauri binary. `src/main.rs` picks the platform by `cfg(target_os)` and wires the app together; the `#[tauri::command]`s are in `src/commands.rs`, engine events reach the UI through `src/bridge.rs`, native menus are in `src/menu.rs`. The UI is vanilla JS ES modules in `app/ui/` (entry `main.js`; `api.js` holds every command and event; see `plans/08a-STRUCTURE-AND-FLOWS.md` §2.4) plus `index.html` and `popout.html`. Video is drawn by a **native child window/surface** under the webview, not inside the webview.
- **`service/`**: the Windows SYSTEM service binary (`sharectrl-service.exe`). It follows the active session and launches the host as the user (with UIAccess). The host is then still the user's process, so UAC prompts and the lock/sign-in screen are **not** reachable; plans/10 adds the SYSTEM agent that reaches them.
- **`crates/codec-goldens`**: dev-only cross-codec matrix. Every encoder's output must decode bit-exactly on every decoder, checked against the committed streams in `native/testdata/golden`.

`native/vendor/libspa` is a patched libspa that lets PipeWire 0.3.48 (Ubuntu 22.04, the oldest supported Linux) build. On any pipewire-rs bump it must be re-vendored with the patches re-applied (see the comment in `native/Cargo.toml`).

## Conventions and constraints

- **Licensing is enforced.** Use permissively licensed dependencies only. `native/deny.toml` bans AGPL capture crates, GPL x264/x265 and the unlicensed `hwcodec`. The code is clean-room with **zero RustDesk code**. Don't add or change dependency versions without `cargo deny check`; shared versions live in `[workspace.dependencies]`.
- **Version and release.** The version is `[workspace.package] version` in `native/Cargo.toml`; every workspace crate inherits it, so `native/Cargo.lock` lists it once per crate and must be bumped with it (the release builds with `--locked`). To release, bump both, commit, and push the tag `v<version>`. That runs `release.yml`, which calls `build-assets.yml` per OS, which runs `native/scripts/release-assets.sh`. `native/scripts/release-dry-run.sh [windows|macos|linux]` reproduces this locally into `native/dist/`.
- **Profiles.** `release` (thin LTO, `panic = "abort"`) is for distribution. `test-build` is a fast-compiling profile for throwaway CI test exes only.
- **Workspace lints.** The workspace sets `unsafe_op_in_unsafe_fn` and `rust_2018_idioms` to warn, which fails CI because clippy runs with `-D warnings`.
- **Features need hardware checks.** Many behaviours (latency, vendor codec quirks, permissions, Wayland compositors) can only be validated on real hardware. A feature counts as working on an OS only after its checklist in `plans/checklists/` passes, so state in docs which paths are unverified.

## Orchestration

Planned work runs as parallel agent tasks through the **`orchestrate` skill** (`.claude/skills/orchestrate/SKILL.md`, invoked as `/orchestrate`): design review first, then one builder and one independent reviewer per task in its own worktree off `main`, every build through the lane, a gate on Windows and WSL, fast-forward merge, per-task cleanup, release. The rules below apply to every agent in that process and to any direct work in this repo.

## How we work

**Production grade, long term — first, always.** Build it the way it should exist in five years, not the way that works today. No patches "to get it working", no shortcuts, no stubs, no TODO/FIXME/`todo!`/`unimplemented!`, no dead or commented-out code. If the right design needs more time, take it.

- **Design before code.** A feature starts as a plan in `plans/` plus a structure document (ownership, file layout, types, contracts, state machines, every flow, invariants). It is reviewed adversarially and every finding is verified against the code before anything is built. Implementation follows the structure; changing a contract is a decision, not a side effect.
- **Research first.** Read the official docs for every OS/library API you use and cite them in a `//! ## References` block at the top of the module. Read the surrounding code and match its conventions.
- **Evidence over guesses.** Diagnose from logs and code (the device's `app.log` first), state the root cause with `file:line`, then fix that cause. Verify claims — including a reviewer's or another agent's — against the code before acting on them. Say plainly what is unverified.
- **Correct under failure.** No `unwrap`/`expect` on OS or I/O results; every failure maps to an error a user can act on. Think through every failure path, race and thread affinity (UI thread vs workers, lock order, no UI calls under a lock). Every invariant you rely on gets a comment saying **why**.
- **Never lose user data.** Config migrations are tested on a real old file, keep a backup, and on error keep running on the old data without persisting. The machine's UUID is never regenerated when a config with one exists.
- **Secrets stay secret.** Tokens and passwords live only in the OS secret store (DPAPI / Keychain / Secret Service); never in `config.json`, logs, events or the web UI.
- **Compatible by default.** New wire messages are optional and capability-gated in both directions; old peers and old servers keep working. Deploy the server before clients that need it.
- **Small, shippable steps.** Each phase ships on its own and leaves `main` releasable; risky refactors land with no behaviour change first.
- **Tests prove it.** Unit tests for pure logic, `platform_api::mock` and the loopback harness for the rest, `#[ignore = "needs <device>"]` plus a `plans/checklists/<ID>.md` for anything only real hardware can show. CI must be green on all three OSes; nothing is "done" that the gate did not pass.
- **Dependencies are a decision.** Permissive licences only, checked with `cargo deny check`; shared versions in `[workspace.dependencies]`.
- **Keep the docs true.** When work changes what a plan, a status table, `native/README.md` or this file says, update it in the same change.
- **Mark progress in the plan as you go.** Every finished part (a phase, a task, a step) is marked **complete in its plan in the same commit that finishes it**, with the commit hash and what remains unverified (for example "complete, device-pending: checklists/<ID>.md"). The plan is always the true answer to "where are we".
- **Decisions belong to the owner.** Present options with a recommendation; record the owner's answer in the plan's Decisions section; never quietly change a decided behaviour.
