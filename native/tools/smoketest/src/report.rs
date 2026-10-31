//! The result table and the pass/fail verdict against the plan 04 section 2
//! latency budget.

use std::fmt::Write as _;

use crate::measure::Samples;
use crate::stats::Summary;

/// One row of the section 2 budget.
struct Budget {
    stage: &'static str,
    /// The section 2 target, as printed.
    target: &'static str,
    /// Upper end of the target; p95 above it is a miss.
    ceiling_ms: f64,
    /// Whether a miss fails the run. Capture does not: `acquire` returns when
    /// the OS has a new frame, so its time includes waiting for the screen to
    /// change, which says nothing about the backend. It is still printed.
    gates: bool,
}

const CAPTURE: Budget = Budget {
    stage: "capture",
    target: "~1-3 ms",
    ceiling_ms: 3.0,
    gates: false,
};
const ENCODE: Budget = Budget {
    stage: "encode",
    target: "~1-4 ms",
    ceiling_ms: 4.0,
    gates: true,
};
const DECODE: Budget = Budget {
    stage: "decode",
    target: "~1-3 ms",
    ceiling_ms: 3.0,
    gates: true,
};

pub struct Report {
    pub text: String,
    pub within_budget: bool,
}

pub fn build(samples: &Samples) -> Result<Report, String> {
    let rows = [
        (CAPTURE, &samples.capture),
        (ENCODE, &samples.encode),
        (DECODE, &samples.decode),
    ];
    let mut text = String::new();
    let mut within_budget = true;

    // A fmt::Write into a String cannot fail, but the results are still
    // propagated rather than discarded.
    let fail = |e: std::fmt::Error| e.to_string();
    writeln!(
        text,
        "{:<8} {:>6} {:>8} {:>8} {:>8} {:>8}  {:<9} verdict",
        "stage", "n", "avg", "p50", "p95", "max", "budget"
    )
    .map_err(fail)?;
    for (budget, data) in rows {
        let summary = Summary::of(data).ok_or_else(|| {
            "no frames went through the pipeline; keep the screen changing (move a window, \
             play a video) and retry"
                .to_string()
        })?;
        let miss = summary.p95 > budget.ceiling_ms;
        if miss && budget.gates {
            within_budget = false;
        }
        let verdict = match (miss, budget.gates) {
            (false, _) => "ok",
            (true, true) => "OVER BUDGET",
            (true, false) => "over (not gated: includes waiting for a new frame)",
        };
        writeln!(
            text,
            "{:<8} {:>6} {:>7.2}ms {:>7.2}ms {:>7.2}ms {:>7.2}ms  {:<9} {}",
            budget.stage,
            summary.count,
            summary.avg,
            summary.p50,
            summary.p95,
            summary.max,
            budget.target,
            verdict
        )
        .map_err(fail)?;
    }
    writeln!(
        text,
        "gate: p95 of encode and decode within the section 2 target; transport and render \
         (~1 frame) are not measured here"
    )
    .map_err(fail)?;
    if samples.reinits > 0 {
        writeln!(text, "capture was rebuilt {} time(s)", samples.reinits).map_err(fail)?;
    }
    Ok(Report {
        text,
        within_budget,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(cap: f64, enc: f64, dec: f64) -> Samples {
        Samples {
            capture: vec![cap; 20],
            encode: vec![enc; 20],
            decode: vec![dec; 20],
            reinits: 0,
        }
    }

    #[test]
    fn within_budget_passes() {
        let r = build(&samples(2.0, 3.0, 2.0)).expect("report");
        assert!(r.within_budget);
        assert!(!r.text.contains("OVER BUDGET"));
    }

    #[test]
    fn slow_encode_or_decode_fails() {
        assert!(
            !build(&samples(1.0, 4.5, 1.0))
                .expect("report")
                .within_budget
        );
        assert!(
            !build(&samples(1.0, 1.0, 3.5))
                .expect("report")
                .within_budget
        );
    }

    #[test]
    fn slow_capture_is_reported_but_does_not_fail() {
        let r = build(&samples(16.0, 1.0, 1.0)).expect("report");
        assert!(r.within_budget);
        assert!(r.text.contains("not gated"));
    }

    #[test]
    fn gate_uses_p95_not_the_average() {
        // 19 fast frames and one slow one: p95 of 20 is the 19th value.
        let mut s = samples(1.0, 1.0, 1.0);
        s.encode[0] = 50.0;
        assert!(build(&s).expect("report").within_budget);
        s.encode[1] = 50.0;
        assert!(!build(&s).expect("report").within_budget);
    }

    #[test]
    fn no_frames_is_an_error_not_a_pass() {
        assert!(build(&Samples::default()).is_err());
    }
}
