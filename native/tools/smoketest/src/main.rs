//! Per-OS latency smoke test (plan 04 section 12): the go/no-go for the ~40 ms
//! glass-to-glass premise. Wires `capture -> encode -> decode` through the
//! `platform-api` traits of whatever OS this is built for, on one machine with
//! no network and no window, and checks the GPU media path against the
//! section 2 budget.
//!
//! Run on target hardware with something moving on screen (wiggle the mouse
//! over a window, play a video):
//!
//! ```sh
//! cargo run -p smoketest --release
//! cargo run -p smoketest --release -- --mock      # runs anywhere, for CI
//! ```
//!
//! Exit code: 0 within budget, 1 budget missed, 2 usage error or the pipeline
//! could not run.

mod args;
mod measure;
mod report;
mod stats;

use std::process::ExitCode;
use std::sync::Arc;

use codec_openh264::{OpenH264, WithSoftwareH264};
use platform_api::mock::MockPlatform;
use platform_api::unavailable::NoCodecs;
use platform_api::{CodecFactory, Platform};

use args::{Args, Command};

/// Frame rate of the mock screen. Far above any real display so the mock
/// capture never sleeps for long and the run is quick and deterministic.
const MOCK_FPS: u32 = 1000;

/// The OS backend for this build and its Cisco OpenH264 handle; the same
/// selection as the app's `make_platform()`, so the tool measures exactly what
/// the app would run (and shares the app's downloaded codec).
fn os_platform() -> (Arc<dyn Platform>, Arc<OpenH264>) {
    #[cfg(windows)]
    let platform = platform_windows::WindowsPlatform::new();
    #[cfg(target_os = "macos")]
    let platform = platform_macos::MacPlatform::new();
    #[cfg(target_os = "linux")]
    let platform = platform_linux::LinuxPlatform::new();
    let openh264 = platform.openh264();
    (Arc::new(platform), openh264)
}

fn mock_platform(size: Option<(u32, u32)>) -> Arc<dyn Platform> {
    // The mock keeps its paths under this root but the tool never touches them.
    let mut platform = MockPlatform::new(&std::env::temp_dir());
    platform.capture.fps = MOCK_FPS;
    if let Some((width, height)) = size {
        platform.capture.width = width;
        platform.capture.height = height;
    }
    Arc::new(platform)
}

/// Cisco OpenH264 only: the adapter over a "hardware" factory that always
/// refuses, so every H.264 request takes the software path (downloading the
/// binary on first use, exactly as the app would).
fn software_codecs(openh264: Arc<OpenH264>) -> WithSoftwareH264<NoCodecs> {
    let codecs = WithSoftwareH264::new(
        NoCodecs {
            reason: "--software skips the hardware codec",
        },
        openh264,
    );
    // Desktop Duplication frames live on the GPU; read them back like the app.
    #[cfg(windows)]
    let codecs = codecs.with_readback(Box::new(platform_windows::D3d11Readback));
    codecs
}

fn run(args: &Args) -> Result<bool, String> {
    // The OS backend's handle even for `--mock`: one download location, the
    // per-user codecs directory, never a temp path.
    let (os, openh264) = os_platform();
    let platform = if args.mock {
        mock_platform(args.size)
    } else {
        os
    };
    let software = software_codecs(openh264);
    let codecs: &dyn CodecFactory = if args.software {
        &software
    } else {
        platform.codecs()
    };

    let info = platform.info();
    println!(
        "platform: {} ({} codecs)",
        info.name,
        if args.software { "software" } else { "default" }
    );
    for d in platform.displays() {
        println!(
            "display {}: {} {}x{}{}",
            d.id,
            d.name,
            d.width,
            d.height,
            if d.primary { " (primary)" } else { "" }
        );
    }

    // A mock screen takes its size from the platform config; only a real
    // backend needs the size asked of it.
    let request = if args.mock { None } else { args.size };
    let samples = measure::measure(
        &*platform,
        codecs,
        args.display.as_deref(),
        request,
        args.duration,
    )?;
    let report = report::build(&samples)?;
    print!("{}", report.text);
    Ok(report.within_budget)
}

fn main() -> ExitCode {
    // Capture and codec backends report why they fell back through `tracing`.
    tracing_subscriber::fmt::init();

    let args = match args::parse(std::env::args().skip(1)) {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", args::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("FAIL: latency budget missed");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn mock_args() -> Args {
        Args {
            mock: true,
            software: false,
            display: None,
            duration: Duration::from_secs(1),
            size: None,
        }
    }

    // These assert that the whole pipeline runs and produces a verdict, not what
    // the verdict is: the budget is wall-clock and a loaded test machine can
    // preempt a microsecond mock encode past 4 ms. The pass/fail logic itself is
    // covered deterministically in `report`; a CI step running `--mock` can gate on the exit code.
    #[test]
    fn mock_run_completes() {
        assert!(run(&mock_args()).is_ok());
    }

    #[test]
    fn mock_run_honours_size() {
        let args = Args {
            size: Some((320, 180)),
            ..mock_args()
        };
        assert!(run(&args).is_ok());
    }

    #[test]
    fn unknown_display_is_an_error() {
        let args = Args {
            display: Some("no-such-display".into()),
            ..mock_args()
        };
        assert!(run(&args).is_err());
    }

    /// `--software` reaches OpenH264 and never the hardware codec. Checked with
    /// the codec turned off, so the test needs no network: the error must come
    /// from the software side.
    #[test]
    fn software_selects_openh264_only() {
        let openh264 = Arc::new(OpenH264::new(
            std::env::temp_dir().join("sct-smoketest-openh264-unused"),
        ));
        openh264.set_enabled(false);
        let codecs = software_codecs(openh264);
        assert_eq!(codecs.host_encodable(), vec![platform_api::Codec::H264]);
        let err = codecs
            .new_encoder(None, platform_api::EncoderConfig::default())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("--software skips the hardware codec"), "{err}");
        assert!(err.contains("Cisco OpenH264"), "{err}");
    }
}
