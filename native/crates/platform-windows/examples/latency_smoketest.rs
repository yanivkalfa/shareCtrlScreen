//! §12 latency smoke-test — the single go/no-go for the whole native-rewrite
//! premise. A throwaway harness wiring `capture → encode → (loopback) → decode`
//! on ONE machine and measuring the per-stage time, so the media core can be
//! validated before wiring transport/UI/elevation. If capture+encode+decode
//! doesn't stay in the low-single-digit-milliseconds range here, the ~40 ms
//! glass-to-glass target is out of reach and the premise fails (§2 budget).
//!
//! Run on target hardware: `cargo run -p engine --example latency_smoketest`.
//! (No network, no window — this measures the GPU media path only.)

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};

    tracing_subscriber::fmt::init();

    let mut dup = match capture::Duplicator::new(0, 0) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("capture init failed: {e}");
            return;
        }
    };
    let mut encoder = match codec::Encoder::new(dup.device(), codec::EncoderConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("encoder init failed (no HW H.264 encoder?): {e}");
            return;
        }
    };
    let mut decoder = codec::Decoder::new(dup.device(), codec::Codec::H264, 1920, 1080).ok();

    let mut samples: Vec<(f64, f64, f64)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline && samples.len() < 300 {
        let t0 = Instant::now();
        let frame = match dup.acquire(Duration::from_millis(32)) {
            Ok(Some(f)) if !f.pointer_only => f,
            Ok(_) => continue,
            Err(capture::Error::AccessLost) => {
                let _ = dup.reinit();
                continue;
            }
            Err(e) => {
                eprintln!("capture error: {e}");
                break;
            }
        };
        let t_cap = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let units = encoder.encode(&frame.texture).unwrap_or_default();
        let t_enc = t1.elapsed().as_secs_f64() * 1000.0;

        let mut t_dec = 0.0;
        if let (Some(dec), Some(u)) = (decoder.as_mut(), units.first()) {
            let t2 = Instant::now();
            let _ = dec.decode(&u.data, u.keyframe, samples.len() as i64);
            t_dec = t2.elapsed().as_secs_f64() * 1000.0;
        }
        dup.release();

        if !units.is_empty() {
            samples.push((t_cap, t_enc, t_dec));
        }
    }

    if samples.is_empty() {
        println!("no frames captured (static screen?). Move a window and retry.");
        return;
    }
    let n = samples.len() as f64;
    let avg = |sel: fn(&(f64, f64, f64)) -> f64| samples.iter().map(sel).sum::<f64>() / n;
    let cap = avg(|s| s.0);
    let enc = avg(|s| s.1);
    let dec = avg(|s| s.2);
    println!("frames: {}", samples.len());
    println!("capture (acquire+meta):  {cap:.2} ms  (target ~1-3 ms, §2)");
    println!("encode  (HW MFT H.264):  {enc:.2} ms  (target ~1-4 ms, §2)");
    println!("decode  (HW MFT):        {dec:.2} ms  (target ~1-3 ms, §2)");
    println!("cap+enc+dec sum:         {:.2} ms", cap + enc + dec);
    println!("(add transport ~0-2 ms + RTT and render ~1 frame for glass-to-glass, §2)");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("latency_smoketest is Windows-only (DXGI/Media Foundation).");
}
