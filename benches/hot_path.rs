//! Micro-benchmarks for the daemon hot path (no criterion dependency).
//!
//! ```sh
//! cargo bench --bench hot_path
//! ```

use std::collections::VecDeque;
use std::time::Instant;

fn chunked_line_count(output: &VecDeque<String>) -> usize {
    let newlines: usize = output
        .iter()
        .map(|chunk| chunk.bytes().filter(|b| *b == b'\n').count())
        .sum();
    match output.iter().rev().find(|chunk| !chunk.is_empty()) {
        None => 0,
        Some(last) => newlines + usize::from(!last.ends_with('\n')),
    }
}

fn main() {
    // Synthetic retained scrollback ~200 KB of agent output.
    let mut output = VecDeque::new();
    let line = "error: expected `;`, found `}` at src/main.rs:42:5\n";
    let mut bytes = 0usize;
    while bytes < 200_000 {
        output.push_back(line.to_string());
        bytes += line.len();
    }

    let iters = 5_000usize;
    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..iters {
        total = total.wrapping_add(chunked_line_count(&output));
    }
    let elapsed = start.elapsed();
    let per = elapsed / iters as u32;
    println!(
        "chunked_line_count: {iters} iters over ~{} chunks → {:?} total, ~{:?} each (checksum {total})",
        output.len(),
        elapsed,
        per
    );

    // vt100 process throughput: feed a 4 KB chunk repeatedly.
    let mut parser = vt100::Parser::new(40, 120, 2000);
    let chunk = b"\x1b[31mhello\x1b[0m world\n".repeat(200);
    let start = Instant::now();
    let n = 2_000usize;
    for _ in 0..n {
        parser.process(&chunk);
    }
    let elapsed = start.elapsed();
    println!(
        "vt100 process: {n} × {} bytes → {:?} total, ~{:?} each",
        chunk.len(),
        elapsed,
        elapsed / n as u32
    );

    // View-size min across viewers (cheap but guards the multi-phone path).
    let now = Instant::now();
    let mut overrides = Vec::new();
    for i in 0..8 {
        overrides.push(vmux_tui_view_stub(i, now));
    }
    let start = Instant::now();
    let mut cols = 0u32;
    for _ in 0..100_000 {
        cols = cols.wrapping_add(min_cols(&overrides, Instant::now()) as u32);
    }
    println!(
        "min_live_view: 100000 iters × 8 viewers → {:?} (checksum {cols})",
        start.elapsed()
    );
}

// Local stub so the bench does not depend on private daemon modules.
fn vmux_tui_view_stub(i: usize, now: Instant) -> (u16, u16, Instant) {
    (
        40 + (i as u16 % 20),
        20 + (i as u16 % 10),
        now + std::time::Duration::from_secs(30),
    )
}

fn min_cols(overrides: &[(u16, u16, Instant)], now: Instant) -> u16 {
    overrides
        .iter()
        .filter(|(_, _, exp)| *exp > now)
        .map(|(c, _, _)| *c)
        .min()
        .unwrap_or(80)
}
