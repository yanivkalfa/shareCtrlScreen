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

use platform_api::mock::MockPlatform;
use platform_api::{CodecFactory, Platform};

use args::{Args, Command};

/// Frame rate of the mock screen. Far above any real display so the mock
/// capture never sleeps for long and the run is quick and deterministic.
const MOCK_FPS: u32 = 1000;

/// The OS backend for this build; the same selection as the app's
/// `make_platform()`, so the tool measures exactly what the app would run.
fn os_platform() -> Arc<dyn Platform> {
    #[cfg(windows)]
    return Arc::new(platform_windows::WindowsPlatform::new());
    #[cfg(target_os = "macos")]
    return Arc::new(platform_macos::MacPlatform::new());
    #[cfg(target_os = "linux")]
    return Arc::new(platform_linux::LinuxPlatform::new());
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

/// The software codec factory, once one exists in this build.
///
/// Task D2 (OpenH264 loaded from the Cisco binary at runtime) replaces this
/// with its factory; until then `--software` has nothing to select.
fn software_codecs(_platform: &dyn Platform) -> Option<&dyn CodecFactory> {
    None
}

fn run(args: &Args) -> Result<bool, String> {
    let platform = if args.mock {
        mock_platform(args.size)
    } else {
        os_platform()
    };
    let codecs: &dyn CodecFactory = if args.software {
        software_codecs(&*platform).ok_or(
            "no software codec in this build; --software needs the OpenH264 backend (task D2)",
        )?
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

    #[test]
    fn mock_run_is_within_budget() {
        assert_eq!(run(&mock_args()), Ok(true));
    }

    #[test]
    fn mock_run_honours_size() {
        let args = Args {
            size: Some((320, 180)),
            ..mock_args()
        };
        assert_eq!(run(&args), Ok(true));
    }

    #[test]
    fn unknown_display_is_an_error() {
        let args = Args {
            display: Some("no-such-display".into()),
            ..mock_args()
        };
        assert!(run(&args).is_err());
    }

    #[test]
    fn software_without_a_backend_says_so() {
        let args = Args {
            software: true,
            ..mock_args()
        };
        let err = run(&args).expect_err("no software codec yet");
        assert!(err.contains("no software codec in this build"), "{err}");
    }
}
