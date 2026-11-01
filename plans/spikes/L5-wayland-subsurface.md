# L5-spike — Wayland subsurface under WebKitGTK: result (2026-11-01)

**Verdict: the subsurface works.** A wgpu-rendered `wl_subsurface` of the GTK3 toplevel sits
above WebKitGTK inside a tao/wry window (Tauri 2's stack). It takes pointer input on its own
surface, keeps keys away from WebKit, does the hover-bar reveal, renders pixel-exact at a
fractional scale, and implements the `platform-api` `VideoSurface`/`Presenter` traits
**unchanged**. Verified on **sway 1.9 (wlroots, headless)** with real seat input, and on
**Weston 9.0 (WSLg)** for presentation. **GNOME (mutter) and KDE (KWin) are not verified** —
no such session was available. The decision-#3 fallback is **not** triggered. L5 proceeds
with the subsurface, gated on the device pass below.

Code: [`native/spikes/wayland-subsurface/`](../../native/spikes/wayland-subsurface/)
(standalone crate, Linux-only, not a workspace member). Run it under a Wayland session with
`cargo run --release -- [--seconds N] [--hide-at S --show-at S]`. Build deps beyond Tauri's:
none (libwayland, Mesa Vulkan/EGL at runtime). The sway scripts also need `sway grim wtype
python3`.

## What was built

```
GTK toplevel wl_surface  (GDK's; WebKitGTK paints into it)
└── area   ours, desync, 1×1 transparent shm buffer, EMPTY input region,
    │       positioned at the content widget's origin (GTK state)
    └── video  ours, desync, wgpu swapchain (Vulkan/GL), wp_viewport,
               wp_fractional_scale_v1, opaque, full input region
```

* `WaylandSubsurface::create(RawWindowHandle, RawDisplayHandle)` — the exact
  `Platform::create_surface` signature. It finds the GTK toplevel by matching the handle's
  `wl_surface` against `gtk_window_list_toplevels()` through public GDK API, so it needs no
  GTK object from the caller.
* `area` + `video` is GStreamer gtkwaylandsink's layout. The video's viewport (video state)
  and position (area state) change together in one commit we own: `set_sync → commit video →
  commit area → set_desync`. Only the area's position is GTK state, and it changes only on
  relayout, which GTK commits anyway (`queue_draw`).
* Input: our own `wl_seat` + `wl_pointer` on our own event queue over GDK's connection
  (`Backend::from_foreign_display`), dispatched on a dedicated thread. Keys come from the
  toplevel's `key-press-event`/`key-release-event`, which run before GtkWindow hands keys to
  WebKit. They return `Propagation::Stop` while the video is shown.
* Presenter: wgpu 30, CPU NV12 upload, one WGSL pass (BT.709 limited → RGB, letterbox,
  cursor), Mailbox > Immediate > FIFO, `desired_maximum_frame_latency = 1`.
* Test tooling: `src/bin/drive.rs` (wlr virtual pointer, stdin-scripted),
  `scripts/sway-headless.sh` (full scenario + screenshots), `scripts/sway-latency.sh`.

## Proof points

| Requirement | sway 1.9 headless (wlroots) | Weston 9.0 (WSLg) |
|---|---|---|
| Subsurface of `gdk_wayland_window_get_wl_surface`, above WebKitGTK | ✅ screenshot: the video covers the page, which shows again when hidden | ✅ screenshot: the video covers the page |
| Rendered by wgpu from raw Wayland handles | ✅ Vulkan (lavapipe), Mailbox | ✅ Vulkan (lavapipe) Mailbox; GL (llvmpipe) FIFO |
| Pointer events on its own surface | ✅ move / left / right / wheel ±120 with correct signs; release after a drag outside arrives (implicit grab); the page saw **0** of them | ✅ real host-mouse motion arrived (no scripted input on WSLg) |
| Keyboard intercepted on the GTK window | ✅ keys reach the sink, page `keydown` count **0** | not scriptable |
| Hover-bar clip | ✅ reveal at ≤ 2 phys px; page got the bar hover + **toolbar click**; band (40–56 px) keeps it; deeper hides it; picture does not move | not scriptable |
| Fractional scale | ✅ 1.5: GDK says 2, `preferred_scale` = 180/120 → buffer 1920×1130 for 1280×753 logical, 1:1 on screen; 1.25 → 1920×1141 for 1536×913 | n/a (no fractional-scale-v1, compositor v4) |
| Resize / relayout | ✅ floating 900×560 and back: `client_size` follows | ✅ |
| Hide = not visible, not hit-testable | ✅ pointer falls through to the page (0 video events while hidden) | ❌ **video stays on screen** (see below) |
| Contract unchanged | ✅ `VideoSurface`/`Presenter` implemented as-is | ✅ |

## Measurements (release build, software GPU)

This machine's WSL2 has no hardware Vulkan or EGL (dzn absent, `eglInitialize: DRI2: failed
to get driver name`), so every number is **llvmpipe/lavapipe on the CPU**. Treat them as an
upper bound on cost and as proof of correct pacing, not as GPU performance.

| Run | present() ms p50 / p95 / max | frame interval ms p50 / p95 |
|---|---|---|
| WSLg Weston, Vulkan, 1024×593 | 7.2 / 20 / 93 | **16.68** / 21 |
| WSLg Weston, GL (FIFO only), 1024×593 | 21.5 / 55 / 762 | 22.1 / 55 |
| sway, Vulkan, 1920×1153 @1.0 | 10.4–13.3 / 23–28 / 51–108 | **16.7–16.8** / 23–28 |
| sway, Vulkan, 1920×1130 @1.5 | 13.2 / 34 / 190 | 16.8 / 34 |
| sway, Vulkan, 1920×1141 @1.25 (1536×913 logical) | 8.0 / 20 / 97 | **16.68** / 20 |
| **hidden** (any compositor/backend) | **0.00 / 0.00 / 0.27** | 16.67 / 17–37 |

* `create_surface`: 1.3–12.8 ms. Presenter (wgpu instance + adapter + device + pipeline):
  370–820 ms on lavapipe (first run up to 1.9 s).
* **Input latency** (virtual-pointer button sent → `VideoInput::Button` logged, 120 events,
  sway, video presenting 1080p on the CPU): **p50 0.5–2.1 ms, p95 5–13 ms, max 8–28 ms**
  over three runs (the spread follows the load from other builds on the shared machine).
  Holding the surface lock across the whole render gave p50 3 ms / p95 20 ms. The shipped
  design takes the lock only around the Wayland requests (see below). The remaining tail
  matches llvmpipe saturating the CPU; re-measure on a real GPU.

## What did not work, deviations, and how they were handled

1. **WSLg (Weston 9.0, RDP backend): unmapping the parent does not hide the video.** `hide`
   attaches a NULL buffer to `area`. By the protocol ("A sub-surface is hidden if the parent
   becomes hidden … recursively") that unmaps `video`. sway does this; WSLg keeps showing the
   last video frame, even while GTK repaints underneath. WSLg is not a target, so the spike
   keeps the protocol-correct design. **L5 must check this on GNOME and KDE.** If a target
   compositor behaves like WSLg, the fix inside this design is to also attach NULL to `video`
   under the surface lock in `hide`. The presenter would then need to re-present its last
   frame on `show` (keep the textures; a static remote screen sends no new frame).
2. **GDK 3 touch path (crash hazard, designed out).** GDK casts the user data of any surface
   named in `wl_touch.down` to a `GdkWindow` and `g_object_ref`s it **without** the
   `GDK_IS_WINDOW` check that pointer/keyboard/DnD/tablet have (`gdk_wayland_seat_add_touch`).
   A wayland-rs-managed proxy stores a Rust struct there, so a touchscreen tap on the video
   would corrupt memory. The video `wl_surface` is therefore created through libwayland
   directly (`wl_proxy_marshal_array_constructor_versioned`). Its user data is a never-shown,
   widget-less `GdkWindow` (the "sink"): GDK's checks pass, its events go to a window GTK
   drops, and GDK keeps the arrow cursor over the video. **Unverified with a real
   touchscreen.**
3. **`place_above(parent)` would be wrong.** 04 §4 says "place_above". A new subsurface is
   already top-most, and `place_above(parent)` would put it *below* any other subsurface of
   the toplevel. The spike does not call it.
4. **Virtual keyboards are not evdev.** `wtype` uploads its own keymap, so `hardware_keycode`
   was 9/10/11 for "abc" and mapped to Escape/1/2. Interception is proven; identity is not.
   Physical keyboards use the evdev keymap. D3's table should be fed the XKB keycode **and**
   tested with a real keyboard. Remote-input tools (and IMEs) that inject through virtual
   keyboards will produce odd codes.
5. **Key mapping coverage is temporary.** Until D3 lands, the spike maps only the main block
   (evdev = Set-1 for codes 1–83, 86–88) through `input::scancode`. Arrows, navigation and the
   right-hand modifiers are dropped.
6. **GL backend offers FIFO only** (wgpu GLES/EGL on Wayland). Presents are paced by frame
   callbacks. The never-present-while-hidden rule makes this safe (0 ms while hidden), but
   latency is worse; Vulkan should stay first (decision #2).
7. **Two page `pointermove`s at virtual-pointer creation** (before any motion over the video).
   They are crossing artefacts and harmless; none occur afterwards.
8. **File drops are out of the spike's scope.** `set_drop_sink` logs and refuses. L5 needs a
   `wl_data_device` on our seat (GDK refuses drags over the sink window, so there is no
   conflict).

## Design rules found (L5 must keep them)

* **One surface lock** serializes every request that changes the video surface's state or
  commits it. Mesa sends attach + damage + commit inside `present`, so the presenter takes the
  lock only for `send_viewport + present`. Our reveal/relayout commits can then never publish
  half a frame, and pointer motion is not blocked by rendering.
* **The viewport source is computed from the last *committed* buffer size, in wl_fixed
  units**, y rounded down and height = remainder. An `out_of_buffer` error is fatal, and
  **GDK `_exit(1)`s on any display error**, so every protocol error takes down the whole app.
* **Never present while hidden.** An unmapped surface gets no frame callbacks; FIFO would
  block forever.
* `fit()` is a no-op on Wayland: geometry follows GTK `size-allocate` on the UI thread. The
  destination follows the new size immediately, so the last frame stretches until the
  presenter resizes (the Windows behaviour). The presenter resizes on its next frame.
* Our queue shares GDK's fd through libwayland's `prepare_read` protocol (GDK's event source
  uses it). Our input wake-ups are coupled to the GTK main loop reading the socket. They were
  fast in every run, but a GTK main thread blocked for long would delay video input too.
* Coordinates: toplevel `GdkWindow` coordinates = `wl_surface` coordinates. They include the
  CSD shadow (WSLg floating: content origin 26,70; sway tiled: 20,67). Compositors place the
  window by its xdg geometry, which excludes that shadow.

## Not verified — needs a device

| Item | How to verify |
|---|---|
| **GNOME (mutter) Wayland** — stacking, input, reveal, **hide**, fractional 125/150 % (`gsettings set org.gnome.mutter experimental-features "['scale-monitor-framebuffer']"`) | On Ubuntu 22.04/24.04 GNOME Wayland, in the spike dir: `cargo run --release -- --seconds 40 --hide-at 23 --show-at 30`. Move over the video (log `input: move`), click/scroll, type (page `keys` must stay 0), push to the top edge (blue bar appears; clicking "Toolbar button" increments page `clicks`), go deeper than 56 px (bar hides). At 23 s the video hides: **the page must be visible**. Repeat at 150 %: log `fractional scale 1.500`, `client_size` = logical × 1.5. |
| **KDE Plasma 6 (KWin) Wayland** | Same steps; also try 125 %. |
| **Real sway/Hyprland session on a GPU** | Same steps, plus `scripts/sway-headless.sh` can run nested. |
| **Hardware Vulkan / GL presenter** (Intel/AMD/NVIDIA) | Log line `presenter: Vulkan adapter "<GPU>"`; the shown-phase interval should be ≈16.7 ms with present() < 2 ms; repeat with `WGPU_BACKEND=gl`. |
| **NVIDIA proprietary** | Same; watch for EGL/Vulkan WSI explicit-sync differences. |
| **Touchscreen** tap on the video | No crash, no GTK criticals; the tap is not delivered to the page. |
| Physical keyboard identity | Type "a", arrows, AltGr: the log shows `KeyA`; arrows are dropped until D3. |
| Pixel exactness of the subsurface **position** at fractional scale | The protocol leaves subsurface position rounding undefined. At 125 % check the video's left/top edge for a 1-px seam or blur. |

The spike prints everything needed: `SPIKE …` lines (timings, client_size), `input: …`
(events delivered to the sink), `web: {…}` (what WebKit received).

## Recommendation for L5

**Build the subsurface (option 1).** Port the spike's `wl.rs`/`input.rs`/`surface.rs`/
`gtk_side.rs`/`presenter.rs` into `platform-linux`; the shader moves to
`platform-api/shaders/nv12.wgsl`. Gate "done" on the GNOME + KDE device pass above. Keep
decision #3 (separate undecorated window) as a **runtime** fallback, selected when a required
global is missing (`SpikeError::MissingGlobal`, e.g. no `wp_viewporter`) or a compositor fails
the hide check. Option 3 (WebCodecs in WebKitGTK) is not needed: nothing here pointed to it,
and it still crashes the web process on decoder faults.

**platform-api change needed: none.**

## Open questions for review

* **Unreveal depth units.** The trait says "deeper than 56 px". Windows compares *physical*
  px, so at 200 % (bar = 80 px) Windows re-hides the bar the moment the pointer re-enters the
  video. The spike uses 56 *CSS* px (a 16 px band at every scale). Decide which one is
  intended and align the Windows code or the trait doc.
* Keys are intercepted whenever the video is shown (not only after a click into it, as with
  Windows focus). AnyDesk-like, but confirm.
* The CPU-only numbers above must be redone on a real GPU before tuning anything.

## Sources

wayland.xml (wl_subsurface, wl_pointer, wl_surface.set_input_region) ·
[viewporter](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/stable/viewporter/viewporter.xml) ·
[fractional-scale-v1](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/fractional-scale/fractional-scale-v1.xml) ·
[GTK 3.24 gdkdevice-wayland.c](https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c),
[gdkeventsource.c](https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkeventsource.c),
[gtkmain.c](https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gtk/gtkmain.c) ·
[GstWlWindow](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwlwindow.c),
[gstgtkwaylandsink.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/ext/gtk/gstgtkwaylandsink.c) ·
wayland-backend 0.3.17 `sys` (foreign display / `ObjectId::from_ptr`, foreign objects in
events) · wgpu 30 (`SurfaceTargetUnsafe::RawHandle`, `InstanceDescriptor::display`,
`CurrentSurfaceTexture`, `Queue::present`) · tao 0.35.3 `platform_impl/linux` (Wayland
handle = `gdk_wayland_window_get_wl_surface`, header bar, `default_vbox`) ·
[wlr-virtual-pointer-unstable-v1](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1).
