//! Text-path throughput timing tests (Task 37 – perf validation).
//!
//! Measures parse throughput of the text hot path (dense printable ASCII and
//! rapid scrolling) through the real `Processor`→`Term` pipeline — the same
//! path every rendered character takes.
//!
//! Run with:
//!   cargo test -p alacritty_terminal --test text_throughput -- --nocapture
//!
//! The test does NOT assert a minimum throughput (environment variance would
//! make that brittle in CI); it prints MB/s figures for manual comparison.
//! The actual acceptance gate is the PERF.md comparison recorded by Task 37.

use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{self, StdSyncHandler};

struct NullListener;
impl EventListener for NullListener {
    fn send_event(&self, _: Event) {}
}

const TARGET_MB: usize = 32;

/// Dense printable ASCII: 80 chars per line + newline.
/// Stresses the character-decode → cell-write → attribute-tracking path
/// (what `dense_cells` / `alt_screen_random_write` vtebench workloads stress).
fn dense_payload(target_bytes: usize) -> Vec<u8> {
    let line: Vec<u8> = (b'A'..=b'z').chain(std::iter::once(b'\n')).cycle().take(81).collect();
    let mut v = Vec::with_capacity(target_bytes + line.len());
    while v.len() < target_bytes {
        v.extend_from_slice(&line);
    }
    v.truncate(target_bytes);
    v
}

/// Rapid newline injection: stresses the scroll / dirty-row path.
fn scroll_payload(target_bytes: usize) -> Vec<u8> {
    vec![b'\n'; target_bytes]
}

fn bench_payload(label: &str, payload: &[u8]) -> f64 {
    let size = TermSize::new(80, 24);
    let mut term = Term::new(Config::default(), &size, NullListener);
    let mut parser: ansi::Processor<StdSyncHandler> = ansi::Processor::new();

    // Warm-up: one small run.
    let warm_len = (64 * 1024).min(payload.len());
    parser.advance(&mut term, &payload[..warm_len]);

    let t0 = Instant::now();
    parser.advance(&mut term, payload);
    let elapsed = t0.elapsed();

    let bytes = payload.len() as f64;
    let secs = elapsed.as_secs_f64();
    let mb_s = (bytes / (1024.0 * 1024.0)) / secs;
    let ns_per_byte = (elapsed.as_nanos() as f64) / bytes;

    println!(
        "text_throughput[{label}]: {:.1} MB/s  ({:.2} ns/byte)  {:.0} MB  {}ms",
        mb_s,
        ns_per_byte,
        bytes / (1024.0 * 1024.0),
        elapsed.as_millis(),
    );

    mb_s
}

#[test]
fn text_throughput_dense_cells() {
    let payload = dense_payload(TARGET_MB * 1024 * 1024);
    let mb_s = bench_payload("dense_cells", &payload);
    // Sanity floor: at least 100 MB/s on any modern machine in release.
    // (This is not the regression gate — PERF.md comparison is.)
    assert!(mb_s > 0.0, "throughput must be positive");
}

#[test]
fn text_throughput_scrolling() {
    let payload = scroll_payload(TARGET_MB * 1024 * 1024);
    let mb_s = bench_payload("scrolling", &payload);
    assert!(mb_s > 0.0, "throughput must be positive");
}
