<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# build+ci+licensing

## summary
Unit "build+ci+licensing" covers the GitHub Actions workflows and the cargo policy files under native/. Everything here is Windows-pinned in three ways: every workflow job runs on windows-latest (native-ci.yml:24,43; release.yml:60; dev-build.yml:29), cargo-deny evaluates only the x86_64-pc-windows-msvc graph (deny.toml:4-5), and cargo-about targets the same triple (about.toml:17). The workspace (native/Cargo.toml:4-18) lists 11 members, nine of them platform-specific crates (capture, codec, render, input, elevation, service, and parts of engine/transport), and the workspace dependency table declares windows 0.62 / windows-core 0.62 / windows-numerics 0.3 (Cargo.toml:31-34). The OS-agnostic dependency set is str0m 0.21, quinn-udp 0.6, reed-solomon-simd 3, tokio, tokio-tungstenite, serde, tracing and friends (Cargo.toml:35-54). Per-crate Cargo.toml files were not in the assigned read set, so the exact target-gating of windows deps inside each crate is unverified.

Licensing is enforced by cargo-deny in two places: native-ci.yml license-gate job (lines 42-51) and the release job (release.yml:72-77). The allow list (deny.toml:20-36) is permissive plus MPL-2.0 file-level copyleft. The ban list (deny.toml:48-56) names dxgcap, dxgcap2, captrs (AGPL), hwcodec (no license), x264 and x265 (GPL), and rivet-codec. Sources must come from crates.io only (deny.toml:58-61). cargo-about (about.toml) produces a third-party notices page but is not run in CI (native-ci.yml:4-5 says it is on-demand). about.hbs exists at native/about.hbs (confirmed by glob); the generated third-party-licenses.html is not committed.

Release and dev pipelines are manual (workflow_dispatch only, release.yml:16-17; dev-build.yml:15-17). The versioned Release reads version from native/Cargo.toml via grep (release.yml:36), skips if tag v<version> exists (release.yml:47-53), otherwise builds on Windows, runs cargo-deny, installs tauri-cli from source (release.yml:80), builds the service and the NSIS installer (release.yml:84,89), and publishes sharectrl-service.exe plus the -setup.exe installer via gh (release.yml:108-112). Unsigned; no signing or notarization step exists anywhere in these files. The test pipeline ("Build Test EXE", dev-build.yml) builds only -p app with the test-build profile and publishes a rolling pre-release named test-build, deleting the old dev and test-build releases each run (dev-build.yml:64-72).

## public_api
- native/Cargo.toml:4-18 [workspace] members: crates/protocol, crates/capture, crates/codec, crates/transport, crates/signaling, crates/input, crates/render, crates/elevation, crates/engine, app, service
- native/Cargo.toml:21 [workspace.package] version = "2.0.0" (single source of truth for release tag)
- native/Cargo.toml:31-54 [workspace.dependencies] shared version pins (windows 0.62, str0m 0.21, quinn-udp 0.6, reed-solomon-simd 3, tokio 1 with rt-multi-thread/macros/sync/time/net/io-util, tokio-tungstenite 0.24 rustls-tls-webpki-roots, crossbeam-channel 0.5, parking_lot 0.12, etc.)
- native/Cargo.toml:68-72 [profile.release] opt-level 3, lto thin, codegen-units 1, panic abort
- native/Cargo.toml:79-83 [profile.test-build] inherits release, opt-level 1, lto false, codegen-units 256 (used only by dev-build.yml)
- native/deny.toml:4-6 [graph] targets = [x86_64-pc-windows-msvc], all-features true
- native/deny.toml:8-14 [advisories] version 2, yanked deny, unmaintained workspace
- native/deny.toml:16-40 [licenses] allow list (15 entries incl. CDLA-Permissive-2.0), confidence-threshold 0.9, private ignore
- native/deny.toml:44-56 [bans] multiple-versions warn, wildcards allow, deny list of 7 crates
- native/deny.toml:58-61 [sources] unknown-registry/unknown-git deny, allow-registry crates.io index only
- native/about.toml:3-17 accepted list (12 entries, no MIT-0, no CDLA-Permissive-2.0), targets windows only
- native/README.md:31-33 documented commands: cargo build --workspace --release; cargo test --workspace (20 unit tests); cargo run -p engine --example latency_smoketest (README:46-47)

## platform_deps
- **Runner OS for every job: runs-on windows-latest** @ native-ci.yml:24, native-ci.yml:43, release.yml:60, dev-build.yml:29
  - purpose: CI and release build the Windows-only app; comments say the target is Windows only (native-ci.yml:4-5, release.yml:59, dev-build.yml:28)
  - portable: A port needs matrix runners (ubuntu-latest, macos-latest) for build+test; the release job must become per-OS. Nothing in these files is tied to Windows except the runs-on values, the target triple in deny.toml/about.toml, the .exe / -setup.exe paths and the NSIS bundle flag (release.yml:89, 100-101, 120-121; dev-build.yml:59, 80).
- **Target triple x86_64-pc-windows-msvc in cargo-deny graph** @ native/deny.toml:4-5
  - purpose: Restricts the license/ban/advisory graph to Windows dependencies
  - portable: Deps that exist only on macOS or Linux are never evaluated, so the license gate silently skips them. Each shipped target triple must be added to [graph].targets.
- **Target triple in cargo-about** @ native/about.toml:17
  - purpose: Generates the notices only for Windows deps
  - portable: Same as deny: add targets for every shipped OS, or the notices page under-reports.
- **Windows-only crates in the workspace dependency table** @ native/Cargo.toml:31-34 (windows 0.62, windows-core 0.62, windows-numerics 0.3, comment says MIT OR Apache-2.0)
  - purpose: DXGI, Media Foundation, WebView2, SendInput, SCM and token APIs used by capture/codec/render/input/elevation
  - portable: Per-crate Cargo.toml target gating was not in the read set, so this is unverified. The workspace dependency only declares versions; the crates that consume them determine whether they compile elsewhere.
- **NSIS installer bundle** @ release.yml:89 (cargo tauri build --bundles nsis); release.yml:100 (glob target/release/bundle/nsis/*-setup.exe); release.yml:120
  - purpose: Windows installer shipped as the release asset
  - portable: macOS needs a .app/.dmg bundle target, Linux needs deb/AppImage/rpm targets. Tauri CLI supports these, but the bundle config is in native/app and was not read.
- **Windows executable names** @ release.yml:101 sharectrl-service.exe; dev-build.yml:59 target/test-build/sharectrl.exe; release.yml:111 notes text
  - purpose: Artifact names and install instructions
  - portable: Non-Windows binaries have no .exe suffix. Release notes instruct the user to run sharectrl-service.exe --install-service (release.yml:111), which is the Windows SCM model and has no equivalent in this file set.

## portable_parts
- native/deny.toml allow list and ban list are OS-agnostic (deny.toml:20-36, 48-56); the same rules apply to any target once [graph] is widened
- native/about.toml accepted list is OS-agnostic (about.toml:3-16)
- native/Cargo.toml workspace.package metadata: version, edition 2021, rust-version 1.82, license Proprietary, publish false (Cargo.toml:21-26)
- native/Cargo.toml [workspace.lints] unsafe_op_in_unsafe_fn warn, rust_2018_idioms warn, clippy too_many_arguments and missing_safety_doc allowed (Cargo.toml:58-65)
- Non-Windows-specific workspace deps: str0m 0.21, quinn-udp 0.6 (Linux/macOS capable UDP GSO/GRO helper), reed-solomon-simd 3, tokio, tokio-tungstenite 0.24 (rustls), serde, serde_json, thiserror, anyhow, tracing, tracing-subscriber, sha2, hex, uuid, bytes, crossbeam-channel, parking_lot (Cargo.toml:35-54)
- The fmt/clippy/build/test sequence in native-ci.yml:33-40 is OS-agnostic except the runner
- The version-from-Cargo.toml and release-existence check in release.yml:32-53 is OS-agnostic (bash plus gh)
- The cargo-deny install via taiki-e/install-action (native-ci.yml:47-49; release.yml:73-75) ships prebuilt binaries for Linux and macOS as well

## abstraction_points
- Cargo feature or cfg seam: nothing in the read files defines one. Platform-specific crates must be selected per target (e.g., [target.'cfg(windows)'.dependencies]) in each crate's Cargo.toml, which was not read. This is where the Windows/mac/Linux split is expressed.
- Cargo target-table seam: deny.toml [graph].targets (deny.toml:4-5) and about.toml targets (about.toml:17) are the only place the set of shipped triples is declared. They should list every triple the product ships, e.g. x86_64-pc-windows-msvc, aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu.
- CI matrix seam: the runs-on value is hard-coded in four jobs (native-ci.yml:24,43; release.yml:60; dev-build.yml:29). A matrix over os would be the seam for three platforms.
- Release asset seam: release.yml:98-112 has a hard-coded file list (installers plus .exe). A per-OS asset map would be the seam for .dmg/.app/.deb/.AppImage outputs.
- Version source seam: release.yml:36 uses grep -m1 '^version' on native/Cargo.toml. This is fragile (it matches the first line starting with version, which currently is Cargo.toml:21) and should be swapped for cargo metadata or a dedicated version file before adding more release targets.

## cross_module_contracts
- native-ci.yml runs cargo fmt --check, cargo build --workspace --release, cargo test --workspace, cargo clippy --workspace -- -D warnings (native-ci.yml:34-40). The workspace build therefore compiles every member on the one CI OS. Any member that uses windows crates unconditionally would fail a non-Windows build. This is the single biggest compile-contract question for the port and it is not answered by these files.
- release.yml:77 runs cargo deny check before the build, and native-ci.yml:51 runs it on every push/PR touching native/**. The license gate is therefore a required contract on every dependency change, and only for the Windows graph (deny.toml:4-5).
- release.yml:84 builds -p service --release and release.yml:89 runs cargo tauri build --bundles nsis from native/app. The service binary and the app binary are two independent outputs of the same workspace, and the installer bundles the app only; the service exe is published as a separate asset (release.yml:101,108).
- dev-build.yml:44 builds -p app --profile test-build and publishes target/test-build/sharectrl.exe (dev-build.yml:59). The app crate is the only artifact of this flow; the service is not built.
- release.yml:47-53 treats a pre-existing tag v<version> as a skip, not a failure. Republishing an already-released version requires deleting the release first (it is not documented in the file).
- dev-build.yml:65-66 delete the releases named dev and test-build (with --cleanup-tag) before creating test-build (dev-build.yml:67-72), using --target $GITHUB_SHA. The release.yml Release flow does not pass --target (release.yml:108-112), so its tag points at the default-branch HEAD at run time.
- Crate-level cross-module contract (from native/README.md:24, 16-26, not from build files): engine is the only crate that wires capture, codec, transport, render and input together. The build files give no further detail on the seam, because it is in source.

## gotchas
- deny.toml:4-5 restricts [graph].targets to windows-msvc only. Running cargo deny check on macOS or Linux, or after a port, will not evaluate platform-only deps, so the licence gate becomes a false pass for them. Add the new triples here, and in about.toml:17.
- about.toml:3-16 and deny.toml:20-36 disagree. deny allows MIT-0 (deny.toml:22) and CDLA-Permissive-2.0 (deny.toml:35) and about does not (about.toml list). cargo-about generation can therefore fail or omit notices for crates that pass cargo-deny. The comment at deny.toml:34 says CDLA is for time-zone / root-cert data crates, which the port will likely pull in via TLS or time libraries.
- deny.toml:8-14 sets unmaintained = "workspace", so unmaintained crates are reported only for workspace deps (the comment says Tauri pulls unmaintained unic-* transitive crates). Any port that adds Linux or macOS UI or GTK/WebKit stacks may bring in unmaintained transitives that are not gated.
- deny.toml:58-61 denies unknown git sources. Any dependency patched in from a git repository (e.g. a fork of str0m or a platform crate) is rejected by the gate, so such deps must be published to crates.io or the policy must change.
- deny.toml:44-56 bans crates by name only. x264 and x265 are listed by crate name, so a wrapper with a different name that bundles x264 would pass the ban and only the licence check could catch it. Relevant to any software H.264 encoder on Linux.
- Cargo.toml:72 sets panic = "abort" in the release profile. Any port relying on catch_unwind or thread panic recovery in the media pipeline will not behave the same in release builds.
- Cargo.toml:82-83 test-build profile uses opt-level 1 and codegen-units 256 with lto false. That is the binary the dev-build flow publishes, so latency numbers from test builds are not representative of release (README:46-52 relies on the smoke-test; the profile comment at Cargo.toml:74-78 says it is not for distribution).
- native-ci.yml:24 and 43 run on windows-latest, but the GitHub-hosted Windows runner has no GPU. Hardware-dependent parts (Media Foundation encode, DXGI capture, D3D11 render) can compile and run unit tests but cannot be exercised for real. The README (README:86-99) says the same and calls for two physical machines.
- release.yml:13 and README:64 say the build needs no codec licence. That is a property of using OS-provided hardware codecs (Media Foundation). A port that ships a software encoder, or uses VideoToolbox or VA-API, changes this, and the notice must be re-checked against deny.toml's bans and about.toml's list.
- release.yml:72-77 and native-ci.yml:50-51 are the only enforcement of the licence rules (the README says CI-enforced, README:67-73). about.hbs (native/about.hbs) exists, but the generated third-party-licenses.html is not committed and no workflow generates it. The notices artifact is therefore not shipped with releases.
- release.yml:36 reads the version with grep -m1 '^version' and sed. It matches the first line starting with version in native/Cargo.toml. Any new top-level key starting with version, placed above line 21, would silently change the release version.
- release.yml:19-20 and dev-build.yml:23-24 set permissions contents: write for the whole workflow. dev-build.yml:65-66 also silently ignores errors (|| true) when deleting the previous releases.
- native/README.md:100 has a stray ``` fence with no opening fence after the status section (the code fences at lines 30, 34, 42-47 and 55-59 are balanced). Everything after line 100 renders as code in GitHub markdown; the doc is correct up to there.
- native-ci.yml:6-16 only triggers on changes under native/** or the workflow file itself. Changes to the Cloudflare signaling server under server/ (README:8) are not built or tested by this CI, and the protocol is shared with the Cloudflare side.
- README.md:3 and native-ci.yml:2-3 refer to Plan 04 (pans/04-NATIVE-REWRITE-PLAN.md and the section numbers §10, §11) that is outside native/. The section references (§8b, §10, §11.1, §11.4, §12) are only traceable through that plan, which was not read.

## tests
- native/README.md:33 states cargo test --workspace runs 20 unit tests across protocol, input, codec, transport and engine. The test list itself is in source (not read), so no per-test portability verdict is possible from these files.
- native-ci.yml:38 runs cargo test --workspace on windows-latest only. No Linux or macOS test job exists, so the existing unit tests have never been run on those OSes in CI (as far as these files show).
- native/README.md:46-52 names cargo run -p engine --example latency_smoketest as the go/no-go check. It is a manual on-hardware run, not in CI, and is Windows-capture-bound (README:50-52: capture to encode to decode on one machine).
- README.md:86-99 lists what needs real hardware (glass-to-glass latency, vendor MFT quirks, NAT traversal, BWE, secure-desktop injection). None of these is covered by CI, and each would need a per-OS equivalent test rig.
- Portability verdict for the test suite as a whole: NOT verified. The unit tests named in README:33 are described as protocol/input/codec/transport/engine tests. input and codec are the likeliest to contain Windows-only code (SendInput, Media Foundation), so those tests need per-crate inspection.
