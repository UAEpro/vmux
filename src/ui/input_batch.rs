//! Pure helpers for coalescing attach keystrokes into fewer Input RPCs.

use std::time::{Duration, Instant};

/// Default coalescing window before a keystroke batch is flushed.
pub const INPUT_BATCH_MS: u64 = 8;

/// Max buffered chars before a forced flush (latency / memory bound).
pub const INPUT_BATCH_MAX_CHARS: usize = 256;

/// Whether a key payload should bypass batching (escapes / controls).
pub fn is_control_payload(data: &str) -> bool {
    data.starts_with('\u{1b}') || data.chars().any(|c| c.is_control())
}

/// True when a non-empty batch has been idle long enough to flush.
pub fn batch_ready(started: Option<Instant>, empty: bool) -> bool {
    if empty {
        return false;
    }
    started
        .map(|t| t.elapsed() >= Duration::from_millis(INPUT_BATCH_MS))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_sequences_bypass_batch() {
        assert!(is_control_payload("\u{1b}[A"));
        assert!(is_control_payload("\n"));
        assert!(!is_control_payload("hello"));
        assert!(!is_control_payload("a"));
    }

    #[test]
    fn empty_batch_never_ready() {
        assert!(!batch_ready(None, true));
        assert!(!batch_ready(Some(Instant::now()), true));
    }

    #[test]
    fn batch_ready_after_window() {
        let started = Instant::now() - Duration::from_millis(INPUT_BATCH_MS + 5);
        assert!(batch_ready(Some(started), false));
        assert!(!batch_ready(Some(Instant::now()), false));
    }
}
