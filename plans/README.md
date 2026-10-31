# ShareCtrlScreen on macOS and Linux — research and execution plan

Written 2026-10-31 against commit `5a0d195`. The older design docs live in `../pans/`
(plan 04 is the Windows-native rewrite this builds on). Everything here is meant to be
executed **in one go**, in parallel lanes — see [05-TASKS.md](05-TASKS.md).

| File | What it answers |
|---|---|
| [00-DECISIONS.md](00-DECISIONS.md) | **The owner's locked decisions — these override anything below that disagrees** |
| [01-MACOS-FINDINGS.md](01-MACOS-FINDINGS.md) | No Apple account: what works, what the user clicks, signing so permissions survive updates, SCK/VideoToolbox/CGEvent details |
| [02-LINUX-FINDINGS.md](02-LINUX-FINDINGS.md) | X11 vs Wayland, portals/PipeWire/libei, VA-API/NVENC, packaging |
| [03-TESTING-AND-MACHINES.md](03-TESTING-AND-MACHINES.md) | Do you need a Mac/Linux box, what runs in CI, what was verified on your hardware today |
| [04-TARGET-ARCHITECTURE.md](04-TARGET-ARCHITECTURE.md) | The `platform-api` traits, per-OS data paths, the video-surface design, codec/keymap/permission policy |
| [05-TASKS.md](05-TASKS.md) | 50 tasks in 7 lanes with dependencies, sizes, machines and acceptance tests |
| [06-CRATES-AND-LICENSES.md](06-CRATES-AND-LICENSES.md) | Every crate with version and licence as verified on crates.io |
| [07-CODEBASE-SEAMS.md](07-CODEBASE-SEAMS.md) | File-by-file: what is Windows-bound, what is portable, known bugs to fix in the split |
| [appendix-codemap/](appendix-codemap/) | The raw per-subsystem maps from the 20-reader code sweep (line-level) |
| [../.claude/skills/port-orchestrate/SKILL.md](../.claude/skills/port-orchestrate/SKILL.md) | **How the parallel phase is run** (`/port-orchestrate`): task selection, agent briefs, model routing, verification gate, merging, hardware hand-off |
| [checklists/](checklists/) | Tester checklists for hardware-verified tasks |

## TL;DR

1. **Feasible without any Apple registration.** Build on free GitHub macOS runners, sign
   with a **self-signed certificate** (not plain ad-hoc, or users lose Screen Recording /
   Accessibility on every update), ship a DMG; users click *Privacy & Security → Open Anyway*
   once. Costs: no notarization (that one dialog), and Sequoia's monthly "still allow screen
   recording?" prompt for unattended hosts.
2. **You need a real Mac only for the permission-gated parts** (capture, input injection,
   event tap, the first-run UX) — 2–3 days of a rented Mac mini over VNC (≈ €15–30 on
   Scaleway, 24 h minimum) covers it; everything else (VideoToolbox included) is testable on
   GitHub runners. A macOS VM on your PC is both an EULA problem and GPU-less, so useless here.
3. **Linux needs no new hardware.** Your WSL2 already builds the workspace (verified: only
   `secret.rs` DPAPI and the `service` bin fail — both expected). Desktop/portal testing
   goes in VMs on machine A; hardware codecs on machine B (live USB) or machine A's NVIDIA via
   WSL2 (NVENC libs are present there). CI covers X11, headless Wayland (sway/mutter +
   portals), uinput, software codecs.
4. **Yes, the pipeline becomes testable end-to-end with no screen and no GPU**: the trait
   split (`platform-api`) plus mock capture/surface/injector and a software H.264 codec give a
   loopback test (two engines, real str0m/UDP, real FEC) that runs on all three CI OSes.
5. **The port is ~70 % re-plumbing, ~30 % new OS code.** `pipeline.rs` is mostly portable
   already; Windows calls sit at a few dozen call sites. New code: macOS backend (SCK, VT,
   NSView+wgpu, CGEvent/tap, pasteboard, Keychain, LaunchAgent) and Linux backend (portal+
   PipeWire, XShm/XDamage, VA-API/NVENC, wgpu surface, XTest/libei/uinput, clipboard, Secret
   Service, XDG autostart), each behind the same ~10 traits.
6. **Two spikes before committing designs:** the Wayland `wl_subsurface` video surface under
   WebKitGTK (3-day time box, with ordered fallbacks) and `cros-codecs` vs raw `cros-libva`
   for VA-API encode. X11 and macOS surfaces are straightforward ports of the Windows
   child-window design.
7. **Codec policy:** keep H.264/HEVC/AV1 negotiation; add Cisco OpenH264 as a runtime-
   downloaded software fallback on all OSes (licence requires the download-not-bundle
   model + a notice); this also fixes machine B.
8. **Out of scope on macOS/Linux (stated in the UI):** login/lock screen capture (needs
   root daemons + TCC/greeter access that third-party apps cannot get), focus-without-click
   on Wayland, DRM content.
9. **Critical path:** F1 (trait crate, 2–3 days) → F2 (pipeline split) → the three surface
   tasks (W1 | M4 | L5) → hardware integration. With ~10 parallel workers the whole thing is
   roughly 5–6 weeks including a week of on-hardware integration.
