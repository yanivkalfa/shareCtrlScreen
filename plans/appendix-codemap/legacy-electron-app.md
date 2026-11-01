<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# legacy-electron-app

## summary
Scope: the six legacy Electron files (main/input.js, main/keyhook.js, main/main.js, renderer/viewer.js, renderer/host.js, package.json), all read in full. Did the old build support macOS or Linux? No. package.json:4 describes it as "Screen share + remote control between two Windows machines", and the only electron-builder target is win (package.json:39-44: nsis, portable). The Electron shell is cross-platform, so main.js and the renderer load on any OS, but the two native modules are Windows-only. input.js:6 calls koffi.load('user32.dll') at module top level, which cannot succeed off Windows. main.js requires input lazily (main.js:293-297), so the failure surfaces at the first injected event (getInput() at main.js:358, 368). keyhook.js:21-22 loads user32.dll and kernel32.dll at top level, but main.js requires it lazily (main.js:305-308) and reconcileHook() wraps the path in try/catch and latches keyhookBroken (main.js:319-339), so the feature fails soft. Nothing in the six files is macOS- or Linux-aware: no process.platform checks, no alternate injector, no alternate hook.

Architecture: a two-peer WebRTC session. Signaling runs over a server socket (App.Signaling, not in the read set). The host captures its screen via getDisplayMedia, which the main process answers from desktopCapturer (main.js:25-55), and sends it as a video track. Input travels back over two DataChannels: 'ctl' (ordered, reliable; host.js:246) and 'mm' (unordered, maxRetransmits 0; host.js:247). The host's main process injects that input through input.js (SendInput), and the keyboard hook (keyhook.js, WH_KEYBOARD_LL) captures OS-reserved keys while the viewer window is focused. Input IPC is enforced in main.js (fromMainFrame, validation at main.js:344-369), and permission is enforced in host.js (onInput, host.js:386-401). The viewer shows video with letterbox math (viewer.js:428-459) and is the only role that sends pointer or keyboard input.

Reusable for a native mac/linux port: the wire protocol is already platform-neutral. The viewer sends DOM KeyboardEvent.code strings (viewer.js:541-543, 553-554, 566-570) and normalized [0,1] coordinates (viewer.js:428-459). The Windows-specific piece is the host-side table mapping DOM codes to Set-1 scancodes (input.js:84-120). Replace that table with per-OS keycode tables (macOS virtual keycodes, or Linux evdev/XKB) and keep the DOM-code key identity. The message schema, the viewer capture logic (viewer.js:397-572), the host permission gate, the stuck-input release discipline, and the challenge-response auth (host.js, viewer.js, main.js:198-213) are OS-agnostic. Windows-only policies to replace: the Alt+Tab/Win-key suppression set (keyhook.js:74-80), the virtual-desktop coordinate math (input.js:183-219), and the Windows-indicator stop behaviour (host.js:226-227).

Display geometry is also Windows-coupled: screen:size and screen:list use scaleFactor (main.js:215-236), and input:setDisplay uses screen.dipToScreenRect to get physical pixels for the Win32 virtual desktop (main.js:256-267). A macOS or Linux injector needs its own display-geometry contract; nothing here is validated on a second platform.

Not in the read set and needed to finish the port audit: main/config.js (required at main.js:5), app/preload.js (the window.native bridge), renderer/index.html and CSS (the object-fit: contain assumption at viewer.js:425), App.Caps (codec and hardware detection, host.js:263 and 342-352), App.Signaling, and App.UI helpers (countdown, validUuid, reloadConfig). Also not checked: whether README.md claims mac/linux support.

## public_api
- main/input.js:294-304 module.exports = { mouseMove, mouseButton, wheel, key, releaseAll, setTargetRect, lookup (diagnostics), CB_SIZE }
- main/input.js:137-152 mouseInput(fields): builds an INPUT{type:0, u.mi} object with zeroed defaults
- main/input.js:154-156 send(input): SendInput(1,[input],CB_SIZE); returns the count of events inserted; does not check GetLastError
- main/input.js:163-175 setTargetRect(rect): validates {x,y,w,h} (numeric x/y, w>0, h>0) and stores a copy; otherwise sets null (primary path)
- main/input.js:183-219 mouseMove(nx,ny): primary path (targetRect null) maps with MOUSEEVENTF_ABSOLUTE across the primary display; non-primary maps into the shared display's physical rect, then normalizes against the virtual desktop with VIRTUALDESK
- main/input.js:227-238 mouseButton(b,isDown,nx,ny): button ids 0=left, 1=middle, 2=right (BUTTON_FLAGS at 221-225); tracks downButtons; moves to (nx,ny) first, then sends the button event
- main/input.js:241-246 wheel(dx,dy): dy then dx, each as its own SendInput; values are WHEEL_DELTA units (multiples of 120), passed as mouseData int32
- main/input.js:249-271 key(code,isDown): looks up the DOM code (lookup at 123-127), tracks downKeys, sends a SCANCODE event with EXTENDEDKEY for extended keys and KEYUP on release; wVk is always 0
- main/input.js:277-292 releaseAll(): key-up for every downKeys entry (via key()), then button-ups for every downButtons entry WITHOUT a move; clears both sets
- main/keyhook.js:118-139 install(forward): idempotent global WH_KEYBOARD_LL install; forward(code,isDown) called for each suppressed key; returns true/false; error path disables the feature
- main/keyhook.js:141-150 uninstall(): UnhookWindowsHookEx, hHook=null, forwardFn=null; procPtr kept registered for cheap reinstall
- main/keyhook.js:152-154 isInstalled()
- main/main.js:10-17 --profile <name> IIFE: appends '-<name>' to userData before whenReady
- main/main.js:25-55 installDisplayMediaHandler(): session.defaultSession.setDisplayMediaRequestHandler, useSystemPicker:false; answers with the configured display (cfg.shareDisplayId, falling back to primary, then sources[0]) and audio:'loopback' when cfg.shareAudio
- main/main.js:58-85 createWindow(): BrowserWindow 1050x720, min 800x560, contextIsolation true, sandbox true, nodeIntegration false, preload ../preload.js; blocks will-navigate and window open; wires blur/focus to reconcileHook; close clears wantHook and uninstalls the hook
- main/main.js:110-289 registerIpc(): ipcMain.handle channels: config:get, config:set (validated patch), recents:add, recents:clear, password:verify, password:verifyProof, screen:size, screen:list, input:setDisplay, input:inject, input:releaseAll, keyhook:set
- main/main.js:310-314 forwardShortcut(code,isDown): webContents.send('passthrough-key', {code, down}) (main to renderer push)
- main/main.js:320-339 reconcileHook(): installs the hook only if wantHook AND the window is focused; uninstalls otherwise; latches keyhookBroken on any error
- main/main.js:344-369 injectValidated(msg): whitelist mm/md/mu/wh/kd/ku; clamps coordinates to [0,1]; button must be 0/1/2; wheel deltas clamped to +/-1200; code string max 32 chars; then calls input.* functions
- main/main.js:372-380 app.whenReady boot (installDisplayMediaHandler, createWindow, registerIpc); window-all-closed calls app.quit()
- renderer/viewer.js:5-617 App.Viewer IIFE exports: wire, startRequest, onConnectResponse, onPasswordRequired, wantsSignal, onSignal, onRemoteEnd, teardown, isPeer, updateShortcutCapture, getPermission, getPeer, getPc, getCtl, getMm (616)
- renderer/host.js:5-574 App.Host IIFE exports: wire, onConnectRequest, wantsSignal, onSignal, onRemoteEnd, endSession, isPeer, setPermission, getPermission, getPeer, getPc (560-573)
- renderer/viewer.js:26-33 sha256hex / computeProof(plaintext,nonce) = SHA256( SHA256(plaintext) + ':' + nonce ), lowercase hex via crypto.subtle
- renderer/host.js:28-32 randomHex(): 16 bytes from crypto.getRandomValues as lowercase hex (the password nonce)

## platform_deps
- **koffi.load('user32.dll') and user32.func SendInput(uint32, INPUT*, int)** @ main/input.js:6, 46
  - purpose: Synthesize mouse and keyboard events in the host session
  - portable: macOS: CGEventPost / CGEvent* (Quartz), needs Accessibility trust, uses virtual keycodes and points-based coordinates. Linux: XTest (X11), or uinput/libei/portal (Wayland), needs elevated or permissioned access. Contract: synchronous, one event per call, returns the count inserted, keeps move-before-click ordering, and must not run without permission.
- **GetSystemMetrics SM_X/Y/CX/CYVIRTUALSCREEN (76-79) with MOUSEEVENTF_VIRTUALDESK/ABSOLUTE normalized 0..65535 coordinates** @ main/input.js:50-53, 194-197, 209-217
  - purpose: Map a point on one display into the whole virtual desktop, so a non-primary monitor is controllable
  - portable: macOS: CGDisplayBounds / CGGetActiveDisplayList for union bounds, in points (not physical pixels), with a flipped Y axis versus Win32's top-left origin. Linux: X11 XRandR bounds, or Wayland per-output logical positions. Keep the degenerate-metrics fallback (input.js:198-207).
- **koffi struct layout check: sizeof(INPUT) must equal 40 on x64** @ main/input.js:10-44, 57-60
  - purpose: Guard against a struct layout mismatch that would silently corrupt every injected event
  - portable: Not needed for CoreGraphics or XTest, which take plain functions. The invariant to keep: a broken injection path must fail loudly at load, not inject garbage.
- **KEYEVENTF_SCANCODE / KEYEVENTF_EXTENDEDKEY keyboard injection with wVk=0** @ main/input.js:78-80, 256-270
  - purpose: Inject keys by Set-1 scancode so the event is layout-independent (same physical key)
  - portable: macOS: CGEventCreateKeyboardEvent takes a virtual keycode. Linux: XKB or evdev keycodes. The DOM-code to native-key table (input.js:84-120) must be rebuilt per OS.
- **SetWindowsHookExW(WH_KEYBOARD_LL=13), UnhookWindowsHookEx, CallNextHookEx, GetModuleHandleW(NULL), user32 and kernel32 loads** @ main/keyhook.js:21-22, 39-47, 49, 125-127, 144, 113
  - purpose: Global low-level keyboard hook that swallows OS-reserved combos (Win keys, Alt+Tab, Alt+Esc) while the viewer is focused
  - portable: macOS: CGEventTap (session tap) needs Input Monitoring or Accessibility permission and can swallow events by returning NULL, but cannot prevent every system shortcut (verify for Cmd+Tab). Linux: X11 XGrabKeyboard / XGrabKey for focused grabs, evdev read on /dev/input (root or group), or a Wayland shortcut-inhibit protocol. Contract: the callback must always pass through on error, must never block the global keyboard, and must be removed on blur.
- **GetAsyncKeyState(VK_MENU) polling to decide whether Alt is held** @ main/keyhook.js:46, 70-72, 76-80
  - purpose: Decide whether Tab and Escape are suppressed (only with Alt held)
  - portable: macOS: CGEventSourceFlagsState / modifier flags. Linux: XQueryKeymap, or modifier state tracked inside the hook. Note the code queries async state per event, not state the hook tracks itself (see gotchas).
- **Virtual-key constants VK_TAB 0x09, VK_ESCAPE 0x1b, VK_MENU 0x12, VK_LWIN 0x5b, VK_RWIN 0x5c; WM_KEY* and LLKHF_INJECTED** @ main/keyhook.js:49-68
  - purpose: Identify keys and detect the hook's own injected events (ignored to prevent a feedback loop)
  - portable: macOS: CGKeyCode values and a user-data tag on self-injected events. Linux: keycodes plus a source-marker scheme.
- **Desktop capture: desktopCapturer.getSources({types:['screen']}), display_id matching, session.setDisplayMediaRequestHandler with useSystemPicker:false, audio:'loopback'** @ main/main.js:25-55
  - purpose: Pick which screen to share and provide system-audio loopback
  - portable: The Electron API is cross-platform, but the code comment (main.js:44-45) asserts Windows system loopback. Whether audio:'loopback' works on macOS or Linux, and what useSystemPicker does there, is not established by this code (verify against Electron 43 docs and test). macOS needs Screen Recording permission; Linux on Wayland needs the PipeWire/portal path.
- **screen.getPrimaryDisplay().scaleFactor, screen.getAllDisplays(), screen.dipToScreenRect(null, bounds)** @ main/main.js:36, 217-220, 227-236, 243-266
  - purpose: Report display sizes in physical pixels, and convert a display's DIP bounds into a physical screen rect for the Win32 virtual desktop
  - portable: The Electron DIP/physical conversion is cross-platform. A macOS injector wants points (DIP); a Linux injector wants logical or physical coordinates depending on the compositor. The conversion at main.js:256-267 is the one place to retarget.
- **Windows-only UI behaviour: user can stop capture from Windows' own sharing indicator** @ renderer/host.js:226-227
  - purpose: Ends the session when the captured track ends
  - portable: macOS and Linux have their own stop indicators (macOS Screen Recording bar; PipeWire portal stop). Keep the track 'ended' listener (host.js:227); only the mechanism changes.
- **Windows packaging: build.win (nsis, portable), koffi asarUnpack, GitHub publish** @ package.json:19-44
  - purpose: Packaging for Windows only
  - portable: A port needs mac (dmg/zip; signing and notarization are needed for an input injector and screen capture to run cleanly) and linux (AppImage/deb/rpm) targets. koffi has mac and linux builds; the koffi.load('user32.dll') calls are the Windows-only part.

## portable_parts
- renderer/viewer.js:26-33 computeProof / sha256hex: pure JS crypto (challenge-response), OS-agnostic
- renderer/host.js:28-32 randomHex; renderer/host.js:39-115 onConnectRequest (busy check, reloadConfig, approve vs password path, challenge issuance and verify); renderer/host.js:118-152 showIncoming / resolveIncoming / 30 s auto-deny
- renderer/viewer.js:122-148 onConnectResponse and REASON_TEXT (viewer.js:115-120); renderer/viewer.js:94-112 onPasswordRequired / submitPassword
- renderer/viewer.js:176-286 handleOffer, tuneOpus (viewer.js:269-286), ICE queue (viewer.js:288-298), connection-state handling with 8 s disconnect grace (viewer.js:229-251)
- renderer/host.js:181-300 startSession (getDisplayMedia constraints at 199, audio fallback at 200-209, DataChannels at 246-247, addTrack at 250-259, offer at 285-295); host.js:302-321 tuneOpus (duplicate); host.js:323-336 tuneSender
- renderer/host.js:355-401 wireChannels / sendPerm / onInput (permission gate at 397, whitelist at 398)
- renderer/viewer.js:397-572 input capture: normalize() letterboxing (428-459); one-mouse-move-per-frame coalescing via requestAnimationFrame (461-472) with backpressure threshold 65536 (viewer.js:469); mousedown/up with clampToEdge (496-516); wheel-to-Windows-units (521-532); keydown/keyup incl. auto-repeat (534-555); passthrough from main (563-572). Only the WHEEL_DELTA 120 convention and the F11 exclusion are Windows-specific.
- main/main.js:110-289 IPC handlers; config validation at 116-186 is OS-agnostic; recents at 188-196; password verify and challenge-response with 2 s damper at 198-213
- main/main.js:10-17 --profile userData suffix; main/main.js:20 autoplay-policy switch; main/main.js:58-85 BrowserWindow configuration (blocked navigation, denied window.open, sandbox and contextIsolation)
- main/input.js:84-120 SC and SC_EXT tables: a reusable DOM-code key identity list, but the target is Windows-specific; main/input.js:122 documents deliberately unsupported keys (Pause, media keys, Fn)
- main/input.js:133-134 downKeys/downButtons tracking and main/input.js:277-292 releaseAll, with callers (host.js:413, 496; viewer.js:411, 477-482, 558, 362-363): the stuck-input discipline is portable as a design

## abstraction_points
- Input injector interface (the seam a ported main process would implement): mouseMove(nx, ny) -> count; mouseButton(b in 0|1|2, isDown, nx, ny) -> count (must move before the click); wheel(dx, dy) -> count in WHEEL_DELTA(120) units; key(code /* DOM KeyboardEvent.code */, isDown) -> count (unknown codes silently ignored, input.js:251); releaseAll() -> count; setTargetRect(Option<Rect{x,y,w,h}>) in native pixels or points; lookup(code) -> Option<{native_key, ext}>. Currently mapped 1:1 to input.js:294-304.
- Keyboard-hook interface: install(forward: fn(code, is_down)) -> bool (idempotent; errors disable the feature); uninstall(); is_installed() -> bool (keyhook.js:118-154). The suppressed-key policy (keyhook.js:74-80) and its VK->code table (keyhook.js:63-68) should become a per-OS policy object.
- Display provider: list displays and pick the one to share (main.js:36-42, 225-236, 250-266), and convert a display's bounds into the injector's coordinate space (main.js:259-267). The Windows-only dipToScreenRect step is the abstraction point.
- Platform key table: DOM code -> native keycode per OS (replaces SC/SC_EXT, input.js:84-120). Wire identity stays the DOM code string.
- Hook policy: which keys to swallow (keyhook.js:74-80: Win keys always; Tab/Esc only with Alt). The macOS and Linux equivalents are OS-specific and may not be fully grabbable.
- Stop-share indicator: the track 'ended' path (host.js:226-227) is the seam; the mechanism is OS-specific.
- Desktop-capture handler (main.js:25-55): the source-choice logic (display id match, primary fallback, audio:'loopback') is the seam; loopback audio is platform-specific.
- Permissions and preflight: no abstraction exists. macOS needs Accessibility and Input Monitoring (tap and injection) and Screen Recording (capture) checked and prompted. Linux needs Wayland portals and uinput or X11 access. The code has no preflight; a failed SendInput is silent (input.js:154-156).

## cross_module_contracts
- Wire protocol over the 'ctl' DataChannel (ordered, reliable; host.js:246; viewer.js:189-194): viewer->host {t:'md', b, x, y}, {t:'mu', b, x, y}, {t:'kd', code}, {t:'ku', code}, {t:'wh', dx, dy}; host->viewer {t:'perm', value:'view'|'control'} (host.js:378) and {t:'bye'} (host.js:532; viewer.js:371). All coordinates are normalized to [0,1] over the shared picture (viewer.js:428-459).
- Wire protocol over the 'mm' DataChannel (unordered, maxRetransmits:0; host.js:247; viewer.js:195-196, 469-471): viewer->host {t:'mm', x, y} only, one per animation frame, dropped when bufferedAmount >= 65536 (viewer.js:469). Host receives on mmChan.onmessage (host.js:373), through the same onInput gate as ctl (host.js:386-401).
- Signaling messages (via App.Signaling, not read): connect-request {to, password (null | proof), caps:{decode:[...]}} (viewer.js:46-49); connect-response {accepted, reason ('denied'|'busy'|'bad-password'|'timeout'), permission} (host.js:52, 97-102, 140-145, 160-165; viewer.js:127-140); password-required {nonce} (host.js:81, 89); signal {data:{kind:'offer'|'answer'|'ice', sdp|candidate}} (viewer.js:215-226, 261-265; host.js:268-279, 291-295); end-session {to} (host.js:213, 240, 535; viewer.js:374, 576).
- Renderer-to-main IPC via window.native (preload.js not read). Names used by the renderer: recentsAdd (viewer.js:308), keyhookSet (viewer.js:353, 363), onPassthroughKey (viewer.js:563), inputSetDisplay (host.js:231, 498), inputInject (host.js:400), inputReleaseAll (host.js:413, 496), passwordVerifyProof (host.js:95). Each maps to an ipcMain.handle in main.js, and all are gated by fromMainFrame (main.js:90-92).
- Main-to-renderer push 'passthrough-key' {code, down} (main.js:312), consumed at viewer.js:563-572. This is the only path by which OS-captured keys reach the viewer; it is asynchronous relative to the hook callback.
- Authentication contract: the viewer sends proof = SHA256(SHA256(password) + ':' + nonce) (viewer.js:26-33); the host verifies in the main process via password:verifyProof (main.js:208-213; host.js:95). The renderer never holds the hash (main.js:101 exposes only hasPassword). A wrong proof costs a 2 s delay in main (main.js:211), and the challenge is kept for retry (host.js:93-94).
- Timers per side. Viewer: requestTimer 30 s (viewer.js:9, 69-75); disconnect grace 8 s (viewer.js:21, 240-243); a requestAnimationFrame loop flushMove runs only while captureEnabled (viewer.js:416, 462-464). Host: incomingTimer 30 s (host.js:9, 124); negotiationTimer 30 s from accept to ctl open (host.js:188-192); disconnect grace 8 s (host.js:429-432). Main: 2 s damper on wrong passwords (main.js:202, 211). The hook callback runs on the thread that calls SetWindowsHookExW (keyhook.js:127, threadId 0 and the Electron main thread).
- Global state crossing the boundary: main.js:22 win; main.js:293 input (lazy module); main.js:303 keyhook; main.js:304 wantHook (renderer intent, set by keyhook:set at main.js:284-288); main.js:319 keyhookBroken one-way latch; input.js:161 targetRect (set by input:setDisplay, main.js:240-268, and by session start/teardown host.js:231, 498); keyhook.js:82-84 hHook / procPtr / forwardFn.

## gotchas
- Invariant: the host must never leave keys or mouse buttons stuck down. Release points: viewer blur (viewer.js:558), viewer teardown (viewer.js:362 -> setCaptureEnabled(false) at 410-412), control->view drop (host.js:412-414), host cleanup (host.js:496), and input.js releaseAll (277-292). Button-ups are sent without a move so the cursor does not jump (input.js:276).
- Click safety: mouseButton moves to (nx,ny) before pressing (input.js:234-237), because the mm channel is unreliable and a preceding 'mm' may have been dropped. A port must keep move-then-click order and must not assume the move arrived.
- Button-ups must never be dropped: viewer.js:506-516 uses clampToEdge so a release over the bars is still sent; viewer.js:479-481 sends mu for every held button using lastPos.
- Permission is enforced twice: main.js:344-369 validates shape and range, and host.js:397 drops input unless permission is 'control' (the mm channel goes through the same gate, host.js:373, 386-401).
- control->view must release everything (host.js:412-414): the viewer's ku/mu messages are discarded by the gate, so keys held at the switch would stay down on the host.
- Auto-repeat is forwarded deliberately: viewer.js:541-543 sends kd on every keydown including repeats, because the remote OS will not generate repeats for injected keys.
- The viewer sends keydown for every key, including Ctrl, Alt, Meta and Tab, with preventDefault and stopPropagation (viewer.js:534-555), except F11 (viewer.js:536, 548). Unknown codes are still sent and are silently ignored on the host (input.js:251); the viewer does not filter by the host's table.
- Keyhook suppression is decided per event, not per modifier state: keyhook.js:76-80 checks altDown() via GetAsyncKeyState at each event. A Tab keydown while Alt is held is suppressed and forwarded; if Alt is released before Tab's keyup, the keyup is NOT suppressed and reaches the local OS. The remote sees 'kd Tab' without a matching 'ku'. viewer.js:558 blur or teardown releases it, so this is a lifecycle hazard rather than a permanent stuck key. A port should track the suppressed set inside the hook.
- Alt itself (VK_MENU) is never suppressed (keyhook.js:76-80 covers only Win, Tab, Esc). Windows still sees Alt keydown locally while Tab is captured. The remote gets Alt only through the viewer's DOM keydown (viewer.js:534-544), which runs only while the page has focus.
- The hook body is wrapped in try and passes through on error (keyhook.js:86-114); CallNextHookEx is used on nCode<0 and on the error path (keyhook.js:88, 113). Keys are swallowed only for the suppressed set. The ~300 ms low-level-hook timeout is cited as a backstop (keyhook.js:15-16); the code does nothing explicit to meet it, and forwarding is an IPC send (main.js:310-314).
- The hook must never block or break the global keyboard: keyhook.js:12-14 and 100-106 wrap forwarding in try/catch. Ctrl+Alt+Del is never suppressed (keyhook.js:17), since the kernel handles it below any hook.
- The hook is removed on blur, close and teardown (main.js:77-82, 319-339; viewer.js:363 keyhookSet(false) on every teardown). procPtr is kept registered for cheap reinstall (keyhook.js:125, 149-150); the koffi callback must not be garbage-collected, so it is held at module scope (keyhook.js:83).
- The hook is installed only while the window is focused (main.js:322); losing focus removes it (main.js:77-78). Clicking away is the escape hatch (keyhook.js:9-11).
- keyhookBroken is a one-way latch (main.js:319, 323, 333-334): any exception in reconcileHook disables the feature until restart.
- koffi struct sizes are asserted at load: sizeof(INPUT) must be 40 on x64 or the module throws (input.js:57-60). The code says not to hand-pack structs (input.js:9). MOUSEINPUT.mouseData is int32 rather than DWORD so negative wheel deltas pass through with identical layout (input.js:13-15).
- Virtual-desktop math divides by (vw-1) and (vh-1) (input.js:214-215), matching the 0..65535 range; the degenerate-metrics fallback is the primary path (input.js:198-207). Coordinates are physical pixels 'for a per-monitor-DPI-aware process' (input.js:49). Nothing in the six files sets DPI awareness; it presumably comes from Electron's Windows manifest (not verified here).
- A non-primary share whose display is missing or unplugged falls back to targetRect null (main.js:250-254), i.e. the primary path, so control lands on the wrong screen silently.
- main.js:240-268: input:setDisplay null-checks input at 246, 252 and 264, but calls getInput() unconditionally at 267, so the injector module loads on the non-primary path. Harmless today, but inconsistent with the lazy-load intent (main.js:277-278).
- Wheel conversion: the viewer sends one +/-120 event per DOM wheel event regardless of magnitude (viewer.js:526-529), with the Y sign flipped. High-resolution wheels and trackpads lose magnitude. The host clamps to +/-1200 (main.js:363).
- Mouse coordinates are normalized to the picture (viewer.js:428-459). viewer.js:430 returns null when videoWidth is 0, so no input is sent before the first frame. The letterbox math assumes object-fit: contain on the video element (CSS not read).
- Pointer moves and downs over the letterbox bars are ignored (viewer.js:453-455), but mouseup is clamped (viewer.js:510). Wheel is always prevented and sent when captureEnabled, even over bars (viewer.js:521-532).
- Capture failure path: main.js:28-51 always answers the request (callback({}) on throw at 47-49 or an empty source list at 33). Renderer side, failure shows 'Could not capture the screen' and ends the session (host.js:210-218).
- Audio is best-effort: a loopback failure retries video-only (host.js:200-209); adding audio tracks is guarded (host.js:255-259). Opus is forced to stereo, 128 kbps, FEC on, DTX off (viewer.js:275-276 on the answer; host.js:309-310 on the offer).
- Video encoder hints: host.js:223 sets contentHint 'detail'; host.js:325-331 sets degradationPreference 'maintain-resolution', maxBitrate 8 Mbps, scalabilityMode L1T1. host.js:328-330 says H.264 needs more bitrate than VP9/AV1, so codec choice via App.Caps (host.js:342-352) assumes a hardware H.264 encoder.
- The viewer sets jitterBufferTarget = 0 on the video receiver only; the comment says zeroing the audio buffer causes crackle (viewer.js:204-210).
- app 'window-all-closed' calls app.quit() on every platform (main.js:378-380). There is no 'activate' handler, so macOS dock reopen is not handled; the window is created once in whenReady (main.js:372-376).
- --profile suffixes userData before whenReady (main.js:10-17). No requestSingleInstanceLock; the comment says two instances on one machine is the supported test setup (main.js:7-9).
- Ctrl+Alt+Del and the OS Win key cannot be injected by this path, and Pause, media keys and Fn are deliberately unsupported (input.js:122).
- A failed SendInput returns zero and is ignored (input.js:154-156); main.js:279 discards releaseAll's count; no GetLastError is read. A port needs its own permission and error reporting.

## tests
- No test files are in the read set. input.js:300-303 exports lookup and CB_SIZE 'for diagnostics/tests'. package.json:6-9 has only start and dist scripts; no test script exists.
- Pure, portable candidates for a cross-platform test suite: lookup (input.js:123-127); tuneOpus (viewer.js:269-286 and host.js:303-321, identical logic in two copies); normalize letterbox math (viewer.js:428-459, needs a DOM stub); computeProof (viewer.js:30-33); injectValidated validation (main.js:344-369, once getInput is stubbed); host onInput permission gate (host.js:386-401).
- A cross-platform test plan could also check that every DOM code the viewer can send (viewer.js:541-543) resolves in each per-OS table, or is documented as unsupported (input.js:122).
