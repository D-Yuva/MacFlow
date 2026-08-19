use std::cmp::Ordering;

pub type ProcessId = i32;
pub type WindowId = u32;

/// Stable identity for a real WindowServer window.
///
/// A Core Graphics window number is only unique while that window is alive and
/// can eventually be reused. Pairing it with the owning process prevents an
/// old MRU entry from ranking an unrelated window after a process exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowIdentity {
    pub pid: ProcessId,
    pub window_id: WindowId,
}

impl WindowIdentity {
    #[must_use]
    pub const fn new(pid: ProcessId, window_id: WindowId) -> Self {
        Self { pid, window_id }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub fn is_usable(self) -> bool {
        self.width >= 80.0 && self.height >= 60.0
    }

    #[must_use]
    pub fn is_fullscreen_like(self, screen: Self) -> bool {
        self.width >= screen.width * 0.85 && self.height >= screen.height * 0.85
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WindowKey {
    pub pid: ProcessId,
    pub window_id: Option<WindowId>,
    /// Distinguishes AX-only windows when Core Graphics cannot provide an ID.
    pub fallback_ordinal: u16,
}

impl WindowKey {
    /// Returns the stable part of this key. AX-only fallback ordinals are
    /// intentionally excluded because they are snapshot-local positions, not
    /// identities that can safely survive between switcher presentations.
    #[must_use]
    pub fn identity(&self) -> Option<WindowIdentity> {
        self.window_id
            .map(|window_id| WindowIdentity::new(self.pid, window_id))
    }
}

#[derive(Clone, Debug)]
pub struct WindowMetadata {
    pub key: WindowKey,
    pub title: Box<str>,
    pub app_name: Box<str>,
    pub bundle_identifier: Option<Box<str>>,
    pub bounds: Bounds,
    pub minimized: bool,
    pub synthetic_app: bool,
}

impl WindowMetadata {
    #[must_use]
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.app_name
        } else {
            &self.title
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowOrder {
    pub window_id: WindowId,
    pub z_index: usize,
}

/// Sorts windows without allocating a rank map. Window lists and the MRU list are
/// deliberately capped and small, so linear scans use less memory than hash maps.
pub fn sort_by_mru_and_z_order<T>(
    windows: &mut [T],
    window_id: impl Fn(&T) -> Option<WindowId>,
    recent: &[WindowId],
    z_order: &[WindowOrder],
) {
    windows.sort_by(|left, right| {
        let left_id = window_id(left);
        let right_id = window_id(right);
        let left_recent =
            left_id.and_then(|id| recent.iter().position(|candidate| *candidate == id));
        let right_recent =
            right_id.and_then(|id| recent.iter().position(|candidate| *candidate == id));

        match (left_recent, right_recent) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => {
                let rank = |id: Option<WindowId>| {
                    id.and_then(|id| {
                        z_order
                            .iter()
                            .find(|entry| entry.window_id == id)
                            .map(|entry| entry.z_index)
                    })
                    .unwrap_or(usize::MAX)
                };
                rank(left_id).cmp(&rank(right_id))
            }
        }
    });
}

/// Stable-identity variant of [`sort_by_mru_and_z_order`].
///
/// The live z-order can continue to use WindowServer's globally unique current
/// IDs, while persistent MRU ranking must compare both PID and window ID so a
/// recycled ID never inherits another process's history.
pub fn sort_by_stable_mru_and_z_order<T>(
    windows: &mut [T],
    identity: impl Fn(&T) -> Option<WindowIdentity>,
    recent: &[WindowIdentity],
    z_order: &[WindowOrder],
) {
    windows.sort_by(|left, right| {
        let left_identity = identity(left);
        let right_identity = identity(right);
        let left_recent =
            left_identity.and_then(|value| recent.iter().position(|candidate| *candidate == value));
        let right_recent = right_identity
            .and_then(|value| recent.iter().position(|candidate| *candidate == value));

        match (left_recent, right_recent) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => {
                let rank = |value: Option<WindowIdentity>| {
                    value
                        .and_then(|value| {
                            z_order
                                .iter()
                                .find(|entry| entry.window_id == value.window_id)
                                .map(|entry| entry.z_index)
                        })
                        .unwrap_or(usize::MAX)
                };
                rank(left_identity).cmp(&rank(right_identity))
            }
        }
    });
}

/// Moves the currently focused window to the start of a presentation without
/// changing the persistent MRU history. Keeping this separate from MRU sorting
/// prevents merely opening (or cancelling) the switcher from promoting a
/// window that the user never selected.
pub fn pin_window_to_front<T>(
    windows: &mut [T],
    window_id: impl Fn(&T) -> Option<WindowId>,
    focused_window_id: Option<WindowId>,
) -> bool {
    let Some(index) = focused_window_id.and_then(|focused| {
        windows
            .iter()
            .position(|window| window_id(window) == Some(focused))
    }) else {
        return false;
    };
    windows[..=index].rotate_right(1);
    true
}

/// Selects the most-recently ordered window that is not the currently focused
/// CG window. This skips duplicate representations of the origin window and
/// makes a single shortcut press an actual alternate-window switch.
pub fn preferred_alternate_index<T>(
    windows: &[T],
    window_id: impl Fn(&T) -> Option<WindowId>,
    origin_window_id: Option<WindowId>,
) -> usize {
    if windows.len() <= 1 {
        return 0;
    }
    origin_window_id
        .and_then(|origin| {
            windows
                .iter()
                .position(|window| window_id(window).is_some_and(|id| id != origin))
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mru_precedes_live_z_order() {
        let mut ids = [Some(1), Some(2), Some(3), None];
        sort_by_mru_and_z_order(
            &mut ids,
            |id| *id,
            &[3],
            &[
                WindowOrder {
                    window_id: 2,
                    z_index: 0,
                },
                WindowOrder {
                    window_id: 1,
                    z_index: 1,
                },
            ],
        );
        assert_eq!(ids, [Some(3), Some(2), Some(1), None]);
    }

    #[test]
    fn stable_mru_does_not_leak_across_pid_reuse() {
        let old = WindowIdentity::new(10, 42);
        let current = WindowIdentity::new(20, 42);
        let other = WindowIdentity::new(20, 7);
        let mut identities = [Some(current), Some(other)];
        sort_by_stable_mru_and_z_order(
            &mut identities,
            |identity| *identity,
            &[old, other],
            &[
                WindowOrder {
                    window_id: 42,
                    z_index: 0,
                },
                WindowOrder {
                    window_id: 7,
                    z_index: 1,
                },
            ],
        );
        assert_eq!(identities, [Some(other), Some(current)]);
    }

    #[test]
    fn window_key_exposes_only_a_real_stable_identity() {
        let real = WindowKey {
            pid: 55,
            window_id: Some(99),
            fallback_ordinal: 4,
        };
        assert_eq!(real.identity(), Some(WindowIdentity::new(55, 99)));

        let ax_only = WindowKey {
            pid: 55,
            window_id: None,
            fallback_ordinal: 4,
        };
        assert_eq!(ax_only.identity(), None);
    }

    #[test]
    fn bounds_thresholds_match_switcher_filtering() {
        assert!(Bounds::new(0.0, 0.0, 80.0, 60.0).is_usable());
        assert!(!Bounds::new(0.0, 0.0, 79.9, 60.0).is_usable());
        let screen = Bounds::new(0.0, 0.0, 1000.0, 800.0);
        assert!(Bounds::new(0.0, 0.0, 850.0, 680.0).is_fullscreen_like(screen));
    }

    #[test]
    fn alternate_selection_skips_duplicate_origin_windows() {
        let ids = [Some(10), Some(10), Some(20), Some(30)];
        assert_eq!(preferred_alternate_index(&ids, |id| *id, Some(10)), 2);
        assert_eq!(preferred_alternate_index(&ids, |id| *id, None), 1);
        assert_eq!(preferred_alternate_index(&ids[..1], |id| *id, Some(10)), 0);
    }

    #[test]
    fn presentation_pins_current_without_promoting_unclicked_windows() {
        let mut ids = [Some(4), Some(2), Some(3), Some(1)];
        sort_by_mru_and_z_order(
            &mut ids,
            |id| *id,
            &[2, 1],
            &[
                WindowOrder {
                    window_id: 4,
                    z_index: 0,
                },
                WindowOrder {
                    window_id: 3,
                    z_index: 1,
                },
            ],
        );
        assert_eq!(ids, [Some(2), Some(1), Some(4), Some(3)]);

        assert!(pin_window_to_front(&mut ids, |id| *id, Some(4)));
        assert_eq!(ids, [Some(4), Some(2), Some(1), Some(3)]);
        assert_eq!(preferred_alternate_index(&ids, |id| *id, Some(4)), 1);
    }

    #[test]
    fn pinning_is_stable_when_focused_window_is_missing() {
        let mut ids = [Some(2), Some(1), None];
        assert!(!pin_window_to_front(&mut ids, |id| *id, Some(99)));
        assert_eq!(ids, [Some(2), Some(1), None]);
        assert!(!pin_window_to_front(&mut ids, |id| *id, None));
        assert_eq!(ids, [Some(2), Some(1), None]);
    }
}
