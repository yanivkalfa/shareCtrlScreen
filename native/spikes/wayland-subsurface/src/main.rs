//! L5-spike demo: a tao + wry window (Tauri 2's GTK3/WebKitGTK stack) with the
//! Wayland video subsurface on top, driven the way `engine::media::viewer`
//! drives a `VideoSurface`: create the presenter on the media thread, `show`,
//! then `fit` + `present` in a loop. The web page counts the input WebKit still
//! receives and reports it over IPC, so the log proves what the video took.
//!
//! Usage: `wayland-subsurface-spike [--seconds N] [--hide-at S --show-at S]`
//! (no `--seconds` = run until the window is closed).
//!
//! ## References
//! * tao 0.35 `EventLoop`/`WindowBuilder`, `WindowExtUnix::default_vbox`;
//!   wry 0.55 `WebViewBuilderExtUnix::build_gtk`: <https://docs.rs/tao/0.35>, <https://docs.rs/wry/0.55>
//! * The viewer loop being imitated: `native/crates/engine/src/media/viewer.rs`

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the Wayland subsurface spike runs on Linux only");
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match demo::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("spike: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
mod demo {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use platform_api::{Frame, VideoInput, VideoSurface};
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
    use tao::dpi::LogicalSize;
    use tao::event::{Event, StartCause, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::platform::unix::WindowExtUnix;
    use tao::window::WindowBuilder;
    use wayland_subsurface_spike::WaylandSubsurface;
    use wry::WebViewBuilderExtUnix;

    const PAGE: &str = r#"<!doctype html>
<html><body style="margin:0;background:#dde6f3;font:15px sans-serif;height:100vh">
<div style="height:40px;background:#1f6feb;color:#fff;display:flex;align-items:center;gap:12px;padding:0 12px">
  <button id="btn">Toolbar button</button><span id="st">web counters</span>
</div>
<div style="padding:24px;font-size:28px;color:#a00">WEBKIT CONTENT - if you can read this while the session is shown, the video is NOT above the web view.</div>
<script>
const s = {keys: 0, moves: 0, barMoves: 0, clicks: 0};
let last = "";
addEventListener("keydown", () => s.keys++);
addEventListener("pointermove", e => { s.moves++; if (e.clientY < 40) s.barMoves++; });
document.getElementById("btn").onclick = () => s.clicks++;
setInterval(() => {
  const j = JSON.stringify(s);
  document.getElementById("st").textContent = j;
  if (j !== last) { last = j; window.ipc.postMessage(j); }
}, 250);
</script></body></html>"#;

    #[derive(Debug)]
    enum UserEvent {
        Exit,
    }

    #[derive(Debug, Default, Clone, Copy)]
    struct Options {
        seconds: Option<f64>,
        hide_at: Option<f64>,
        show_at: Option<f64>,
    }

    fn options() -> Result<Options, String> {
        let mut o = Options::default();
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let mut value = || -> Result<f64, String> {
                args.next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| format!("{flag} needs a number of seconds"))
            };
            match flag.as_str() {
                "--seconds" => o.seconds = Some(value()?),
                "--hide-at" => o.hide_at = Some(value()?),
                "--show-at" => o.show_at = Some(value()?),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(o)
    }

    pub fn run() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        let opts = options()?;
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        let window = WindowBuilder::new()
            .with_title("ShareCtrlScreen - L5 Wayland subsurface spike")
            .with_inner_size(LogicalSize::new(1024.0, 640.0))
            .build(&event_loop)
            .map_err(|e| format!("window: {e}"))?;
        let vbox = window
            .default_vbox()
            .ok_or("tao created no default vbox for the web view")?;
        let _webview = wry::WebViewBuilder::new()
            .with_html(PAGE)
            .with_ipc_handler(|req| tracing::info!("web: {}", req.body()))
            .build_gtk(vbox)
            .map_err(|e| format!("web view: {e}"))?;

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::NewEvents(StartCause::Init) => {
                    if let Err(e) = start(&window, opts, proxy.clone()) {
                        tracing::error!("spike: {e}");
                        *control_flow = ControlFlow::Exit;
                    }
                }
                Event::UserEvent(UserEvent::Exit)
                | Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        })
    }

    fn start(
        window: &tao::window::Window,
        opts: Options,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<(), String> {
        let wh = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?
            .as_raw();
        let dh = window
            .display_handle()
            .map_err(|e| format!("display handle: {e}"))?
            .as_raw();
        let created = Instant::now();
        let surface = WaylandSubsurface::create(wh, dh).map_err(|e| e.to_string())?;
        tracing::info!(
            "SPIKE create_surface took {:.1} ms",
            created.elapsed().as_secs_f64() * 1e3
        );

        let (tx, rx) = std::sync::mpsc::channel();
        surface.set_input_sink(Some(tx));
        let counters = Arc::new(Counters::default());
        {
            let counters = counters.clone();
            std::thread::Builder::new()
                .name("spike-input-log".into())
                .spawn(move || log_input(rx, &counters))
                .map_err(|e| format!("input log thread: {e}"))?;
        }
        // The render thread owns the surface from here on, as the viewer media
        // thread does in the engine; the surface itself lives for the process.
        {
            std::thread::Builder::new()
                .name("spike-render".into())
                .spawn(move || {
                    if let Err(e) = render(surface.as_ref(), opts) {
                        tracing::error!("render: {e}");
                    }
                    tracing::info!(
                        "SPIKE input moves={} buttons={} keys={} wheels={}",
                        counters.moves.load(Ordering::Relaxed),
                        counters.buttons.load(Ordering::Relaxed),
                        counters.keys.load(Ordering::Relaxed),
                        counters.wheels.load(Ordering::Relaxed)
                    );
                    // The event loop is gone only if the window was closed.
                    let _ = proxy.send_event(UserEvent::Exit);
                })
                .map_err(|e| format!("render thread: {e}"))?;
        }
        Ok(())
    }

    #[derive(Default)]
    struct Counters {
        moves: AtomicU64,
        buttons: AtomicU64,
        keys: AtomicU64,
        wheels: AtomicU64,
    }

    fn log_input(rx: Receiver<VideoInput>, c: &Counters) {
        for ev in rx {
            match ev {
                VideoInput::Move { nx, ny } => {
                    let n = c.moves.fetch_add(1, Ordering::Relaxed);
                    if n % 25 == 0 {
                        tracing::info!("input: move #{n} ({nx:.3}, {ny:.3})");
                    }
                }
                other => {
                    let counter = match other {
                        VideoInput::Button { .. } => &c.buttons,
                        VideoInput::Key { .. } => &c.keys,
                        _ => &c.wheels,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    tracing::info!("input: {other:?}");
                }
            }
        }
    }

    /// Synthetic 1080p NV12: a grey ramp with a white bar sweeping across, so
    /// tearing, stalls and scaling are visible.
    struct Pattern {
        w: usize,
        h: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
        bar_x: usize,
    }

    const BAR_W: usize = 48;

    impl Pattern {
        fn new(w: usize, h: usize) -> Self {
            let mut y = vec![0u8; w * h];
            for row in y.chunks_exact_mut(w) {
                for (x, px) in row.iter_mut().enumerate() {
                    *px = Self::ramp(x, w);
                }
            }
            Self {
                w,
                h,
                y,
                uv: vec![128u8; w * h / 2],
                bar_x: 0,
            }
        }

        fn ramp(x: usize, w: usize) -> u8 {
            (16 + x * 219 / w) as u8
        }

        fn advance(&mut self) {
            let (w, old) = (self.w, self.bar_x);
            self.bar_x = (old + 12) % (w - BAR_W);
            for row in self.y.chunks_exact_mut(w) {
                for (x, px) in row.iter_mut().enumerate().skip(old).take(BAR_W) {
                    *px = Self::ramp(x, w);
                }
                row[self.bar_x..self.bar_x + BAR_W].fill(235);
            }
        }

        fn frame(&self) -> Frame<'_> {
            Frame::nv12(
                self.w as u32,
                self.h as u32,
                &self.y,
                self.w,
                &self.uv,
                self.w,
            )
        }
    }

    /// Present timing over one phase of the run.
    #[derive(Default)]
    struct Phase {
        present_ms: Vec<f64>,
        interval_ms: Vec<f64>,
        last: Option<Instant>,
    }

    impl Phase {
        fn record(&mut self, started: Instant, took: Duration) {
            self.present_ms.push(took.as_secs_f64() * 1e3);
            if let Some(last) = self.last {
                self.interval_ms
                    .push(started.duration_since(last).as_secs_f64() * 1e3);
            }
            self.last = Some(started);
        }

        fn report(&mut self, name: &str) {
            let stats = |v: &mut Vec<f64>| -> String {
                if v.is_empty() {
                    return "n/a".into();
                }
                v.sort_by(f64::total_cmp);
                let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
                format!(
                    "n={} p50={:.2} p95={:.2} p99={:.2} max={:.2}",
                    v.len(),
                    at(0.5),
                    at(0.95),
                    at(0.99),
                    at(1.0)
                )
            };
            tracing::info!(
                "SPIKE phase {name}: present ms [{}] interval ms [{}]",
                stats(&mut self.present_ms),
                stats(&mut self.interval_ms)
            );
            *self = Phase::default();
        }
    }

    fn render(surface: &WaylandSubsurface, opts: Options) -> Result<(), String> {
        let started = Instant::now();
        let mut presenter = surface.create_presenter().map_err(|e| e.to_string())?;
        tracing::info!(
            "SPIKE presenter ready in {:.1} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
        // As the viewer does: reveal only once the presenter exists.
        surface.show();
        let mut pattern = Pattern::new(1920, 1080);
        let frame_period = Duration::from_micros(16_667);
        let t0 = Instant::now();
        let mut phase = Phase::default();
        let mut phase_name = "shown";
        let mut hidden = false;
        let mut reported_size = surface.client_size();
        tracing::info!("SPIKE client_size {reported_size:?}");
        let mut next = Instant::now();
        loop {
            let t = t0.elapsed().as_secs_f64();
            if opts.seconds.is_some_and(|s| t >= s) {
                break;
            }
            if !hidden && opts.hide_at.is_some_and(|h| t >= h) && phase_name == "shown" {
                phase.report(phase_name);
                surface.hide();
                hidden = true;
                phase_name = "hidden";
            }
            if hidden && opts.show_at.is_some_and(|s| t >= s) {
                phase.report(phase_name);
                surface.show();
                hidden = false;
                phase_name = "re-shown";
            }
            surface.fit();
            let size = surface.client_size();
            if size != reported_size {
                tracing::info!("SPIKE client_size {size:?}");
                reported_size = size;
            }
            pattern.advance();
            let angle = t * 2.0;
            let cursor = Some((0.5 + 0.3 * angle.cos(), 0.5 + 0.3 * angle.sin()));
            let begin = Instant::now();
            presenter
                .present(&pattern.frame(), cursor)
                .map_err(|e| e.to_string())?;
            phase.record(begin, begin.elapsed());
            next += frame_period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        phase.report(phase_name);
        Ok(())
    }
}
