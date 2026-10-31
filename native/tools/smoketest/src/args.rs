//! Command line. Hand-rolled: six flags do not justify a parser dependency.

use std::time::Duration;

pub const USAGE: &str = "\
Usage: smoketest [OPTIONS]

Runs capture -> encode -> decode on this machine and prints per-stage timings
against the plan 04 section 2 budget. Exits 1 when the budget is missed.

Options:
  --mock             use the in-memory MockPlatform (runs anywhere, for CI)
  --software         use the software codec factory instead of the OS one
  --display <id>     display id from the printed list (default: primary)
  --seconds <n>      how long to sample (default: 5)
  --size <W>x<H>     ask the capture backend for this output size
  -h, --help         show this help";

const DEFAULT_SECONDS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub mock: bool,
    pub software: bool,
    pub display: Option<String>,
    pub duration: Duration,
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Args),
    Help,
}

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Command, String> {
    let mut args = Args {
        mock: false,
        software: false,
        display: None,
        duration: Duration::from_secs(DEFAULT_SECONDS),
        size: None,
    };
    let mut it = argv.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--mock" => args.mock = true,
            "--software" => args.software = true,
            "--display" => args.display = Some(value(&mut it, "--display")?),
            "--seconds" => {
                let v = value(&mut it, "--seconds")?;
                let secs =
                    v.parse::<u64>().ok().filter(|s| *s > 0).ok_or_else(|| {
                        format!("--seconds expects a positive integer, got '{v}'")
                    })?;
                args.duration = Duration::from_secs(secs);
            }
            "--size" => args.size = Some(parse_size(&value(&mut it, "--size")?)?),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(Command::Run(args))
}

fn value<I: Iterator<Item = String>>(it: &mut I, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_size(v: &str) -> Result<(u32, u32), String> {
    let bad =
        || format!("--size expects WxH with even, non-zero sides (e.g. 1920x1080), got '{v}'");
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.parse().map_err(|_| bad())?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    // Odd sides cannot be encoded as 4:2:0.
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(bad());
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let Ok(Command::Run(a)) = run(&[]) else {
            panic!("defaults must parse");
        };
        assert!(!a.mock && !a.software);
        assert_eq!(a.display, None);
        assert_eq!(a.duration, Duration::from_secs(5));
        assert_eq!(a.size, None);
    }

    #[test]
    fn every_flag() {
        let Ok(Command::Run(a)) = run(&[
            "--mock",
            "--software",
            "--display",
            "dxgi:1",
            "--seconds",
            "12",
            "--size",
            "1280x720",
        ]) else {
            panic!("must parse");
        };
        assert!(a.mock && a.software);
        assert_eq!(a.display.as_deref(), Some("dxgi:1"));
        assert_eq!(a.duration, Duration::from_secs(12));
        assert_eq!(a.size, Some((1280, 720)));
    }

    #[test]
    fn help_wins() {
        assert_eq!(run(&["--mock", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            &["--bogus"][..],
            &["--seconds"],
            &["--seconds", "0"],
            &["--seconds", "x"],
            &["--size", "1920"],
            &["--size", "1921x1080"],
            &["--size", "0x0"],
            &["--display"],
        ] {
            assert!(run(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
