//! Phone-fit / remote view-size overrides.
//!
//! Multiple live viewers each hold a leased size; the effective PTY size is
//! `min(layout, min(live overrides))` per axis (tmux "smallest client wins").

use std::time::{Duration, Instant};

use crate::protocol::PaneSize;

/// Bounds for `SetPaneViewSize.lease_ms`.
pub const VIEW_LEASE_MS_MIN: u64 = 500;
pub const VIEW_LEASE_MS_MAX: u64 = 60_000;

/// One leased override from a single viewer (phone, script, …).
#[derive(Debug, Clone)]
pub struct ViewOverride {
    /// Stable id for this viewer (`surface-…`, attach client id, or empty for
    /// legacy single-slot callers that replace the whole list).
    pub viewer_id: String,
    pub size: PaneSize,
    /// The override silently expires at this instant unless re-leased.
    pub expires_at: Instant,
}

/// Clamp a lease duration into the safe window.
pub fn clamp_lease_ms(lease_ms: u64) -> Duration {
    Duration::from_millis(lease_ms.clamp(VIEW_LEASE_MS_MIN, VIEW_LEASE_MS_MAX))
}

/// Effective view size across all non-expired overrides: min rows and min cols.
pub fn min_live_view(overrides: &[ViewOverride], now: Instant) -> Option<PaneSize> {
    let mut rows = u16::MAX;
    let mut cols = u16::MAX;
    let mut any = false;
    for v in overrides {
        if v.expires_at > now {
            any = true;
            rows = rows.min(v.size.rows);
            cols = cols.min(v.size.cols);
        }
    }
    if !any {
        return None;
    }
    Some(PaneSize { rows, cols })
}

/// `min(layout, view)` per axis when a view is present.
pub fn effective_pane_size(layout: PaneSize, view: Option<PaneSize>) -> PaneSize {
    match view {
        Some(view) => PaneSize {
            rows: layout.rows.min(view.rows).max(2),
            cols: layout.cols.min(view.cols).max(2),
        },
        None => layout,
    }
}

/// Effective size for layout + multi-viewer leases.
pub fn effective_with_overrides(
    layout: PaneSize,
    overrides: &[ViewOverride],
    now: Instant,
) -> PaneSize {
    effective_pane_size(layout, min_live_view(overrides, now))
}

/// Insert or refresh a viewer lease. Empty `viewer_id` replaces the entire set
/// (legacy single-viewer behaviour).
pub fn upsert_override(
    overrides: &mut Vec<ViewOverride>,
    viewer_id: String,
    size: PaneSize,
    lease: Duration,
    now: Instant,
) {
    let expires_at = now + lease;
    if viewer_id.is_empty() {
        overrides.clear();
        overrides.push(ViewOverride {
            viewer_id,
            size,
            expires_at,
        });
        return;
    }
    if let Some(existing) = overrides.iter_mut().find(|v| v.viewer_id == viewer_id) {
        existing.size = size;
        existing.expires_at = expires_at;
    } else {
        overrides.push(ViewOverride {
            viewer_id,
            size,
            expires_at,
        });
    }
}

/// Drop one viewer (by id) or all when `viewer_id` is empty / None semantics.
pub fn clear_override(overrides: &mut Vec<ViewOverride>, viewer_id: Option<&str>) -> bool {
    match viewer_id {
        None | Some("") => {
            let had = !overrides.is_empty();
            overrides.clear();
            had
        }
        Some(id) => {
            let before = overrides.len();
            overrides.retain(|v| v.viewer_id != id);
            overrides.len() != before
        }
    }
}

/// Remove expired leases. Returns true if any were dropped.
pub fn expire_overrides(overrides: &mut Vec<ViewOverride>, now: Instant) -> bool {
    let before = overrides.len();
    overrides.retain(|v| v.expires_at > now);
    overrides.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_across_two_viewers() {
        let now = Instant::now();
        let overrides = vec![
            ViewOverride {
                viewer_id: "a".into(),
                size: PaneSize { cols: 40, rows: 20 },
                expires_at: now + Duration::from_secs(10),
            },
            ViewOverride {
                viewer_id: "b".into(),
                size: PaneSize { cols: 80, rows: 10 },
                expires_at: now + Duration::from_secs(10),
            },
        ];
        let m = min_live_view(&overrides, now).unwrap();
        assert_eq!(m.cols, 40);
        assert_eq!(m.rows, 10);
    }

    #[test]
    fn expired_viewer_ignored() {
        let now = Instant::now();
        let overrides = vec![
            ViewOverride {
                viewer_id: "dead".into(),
                size: PaneSize { cols: 10, rows: 10 },
                expires_at: now - Duration::from_secs(1),
            },
            ViewOverride {
                viewer_id: "live".into(),
                size: PaneSize { cols: 50, rows: 25 },
                expires_at: now + Duration::from_secs(5),
            },
        ];
        let m = min_live_view(&overrides, now).unwrap();
        assert_eq!(m, PaneSize { cols: 50, rows: 25 });
    }

    #[test]
    fn empty_viewer_id_replaces_all() {
        let now = Instant::now();
        let mut o = vec![ViewOverride {
            viewer_id: "a".into(),
            size: PaneSize { cols: 40, rows: 20 },
            expires_at: now + Duration::from_secs(10),
        }];
        upsert_override(
            &mut o,
            String::new(),
            PaneSize { cols: 30, rows: 15 },
            Duration::from_secs(5),
            now,
        );
        assert_eq!(o.len(), 1);
        assert!(o[0].viewer_id.is_empty());
        assert_eq!(o[0].size.cols, 30);
    }

    #[test]
    fn upsert_refreshes_same_viewer() {
        let now = Instant::now();
        let mut o = Vec::new();
        upsert_override(
            &mut o,
            "phone-1".into(),
            PaneSize { cols: 40, rows: 20 },
            Duration::from_secs(5),
            now,
        );
        upsert_override(
            &mut o,
            "phone-1".into(),
            PaneSize { cols: 36, rows: 18 },
            Duration::from_secs(5),
            now,
        );
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].size.cols, 36);
    }
}
