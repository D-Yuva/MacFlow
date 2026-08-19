use std::collections::VecDeque;

use super::model::{WindowId, WindowIdentity, WindowKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Hidden,
    AwaitingWindows,
    Visible,
    Hiding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Debug)]
pub struct SwitcherSession {
    generation: u64,
    phase: Phase,
    selected_index: usize,
    window_count: usize,
}

impl Default for SwitcherSession {
    fn default() -> Self {
        Self {
            generation: 0,
            phase: Phase::Hidden,
            selected_index: 0,
            window_count: 0,
        }
    }
}

impl SwitcherSession {
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected_index
    }

    #[must_use]
    pub const fn window_count(&self) -> usize {
        self.window_count
    }

    /// Starts a new show cycle. Every cycle invalidates prior asynchronous show
    /// and hide completions, preventing stale callbacks from changing the UI.
    pub fn begin_show(&mut self) -> Generation {
        self.generation = self.generation.wrapping_add(1);
        self.phase = Phase::AwaitingWindows;
        Generation(self.generation)
    }

    pub fn present(&mut self, generation: Generation, window_count: usize) -> bool {
        self.present_at(generation, window_count, usize::from(window_count > 1))
    }

    pub fn present_at(
        &mut self,
        generation: Generation,
        window_count: usize,
        selected_index: usize,
    ) -> bool {
        if generation.0 != self.generation || self.phase != Phase::AwaitingWindows {
            return false;
        }
        if window_count == 0 {
            self.phase = Phase::Hidden;
            self.window_count = 0;
            self.selected_index = 0;
            return false;
        }
        self.window_count = window_count;
        self.selected_index = selected_index.min(window_count - 1);
        self.phase = Phase::Visible;
        true
    }

    pub fn select_next(&mut self) {
        if self.phase == Phase::Visible && self.window_count != 0 {
            self.selected_index = (self.selected_index + 1) % self.window_count;
        }
    }

    pub fn select_previous(&mut self) {
        if self.phase == Phase::Visible && self.window_count != 0 {
            self.selected_index = (self.selected_index + self.window_count - 1) % self.window_count;
        }
    }

    /// Selects a card directly (for pointer input) without ever allowing an
    /// out-of-range index into the renderer or focus path.
    pub fn select_index(&mut self, index: usize) -> bool {
        if self.phase != Phase::Visible || index >= self.window_count {
            return false;
        }
        self.selected_index = index;
        true
    }

    /// Replaces the visible presentation's item count while preserving a
    /// caller-resolved stable selection. Used when a fresh discovery snapshot
    /// arrives after the cached grid has already appeared.
    pub fn refresh_visible(&mut self, window_count: usize, selected_index: usize) -> bool {
        if self.phase != Phase::Visible || window_count == 0 {
            return false;
        }
        self.window_count = window_count;
        self.selected_index = selected_index.min(window_count - 1);
        true
    }

    /// Returns a token that must still match when the platform animation ends.
    pub fn begin_hide(&mut self) -> Option<Generation> {
        match self.phase {
            Phase::Hidden => None,
            Phase::AwaitingWindows => {
                self.generation = self.generation.wrapping_add(1);
                self.phase = Phase::Hidden;
                None
            }
            Phase::Visible | Phase::Hiding => {
                self.generation = self.generation.wrapping_add(1);
                self.phase = Phase::Hiding;
                Some(Generation(self.generation))
            }
        }
    }

    pub fn finish_hide(&mut self, generation: Generation) -> bool {
        if generation.0 != self.generation || self.phase != Phase::Hiding {
            return false;
        }
        self.phase = Phase::Hidden;
        true
    }
}

#[derive(Debug)]
pub struct RecentWindows {
    ids: VecDeque<WindowId>,
    capacity: usize,
}

/// Bounded persistent ordering for exact windows.
///
/// Unlike the legacy window-ID-only history, every entry includes its owning
/// PID. This is the history to use for quick switching and future MRU sorting;
/// it can also be pruned directly from a fresh WindowServer identity snapshot.
#[derive(Debug)]
pub struct RecentWindowHistory {
    identities: VecDeque<WindowIdentity>,
    capacity: usize,
}

impl RecentWindowHistory {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            identities: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn record(&mut self, identity: WindowIdentity) {
        if self.capacity == 0 {
            return;
        }
        if let Some(index) = self
            .identities
            .iter()
            .position(|candidate| *candidate == identity)
        {
            self.identities.remove(index);
        }
        self.identities.push_front(identity);
        self.identities.truncate(self.capacity);
    }

    /// Records a key only when it carries a real WindowServer identity.
    pub fn record_key(&mut self, key: &WindowKey) -> bool {
        let Some(identity) = key.identity() else {
            return false;
        };
        self.record(identity);
        true
    }

    /// Removes one known-closed identity without requiring a second snapshot.
    /// This is the safe response to `ExactQuickSwitchResult::WindowClosed`.
    pub fn remove(&mut self, identity: WindowIdentity) -> bool {
        let Some(index) = self
            .identities
            .iter()
            .position(|candidate| *candidate == identity)
        else {
            return false;
        };
        self.identities.remove(index);
        true
    }

    /// Records a completed switch as a deterministic destination/origin pair.
    pub fn record_switch(
        &mut self,
        origin: Option<WindowIdentity>,
        destination: Option<WindowIdentity>,
    ) {
        let Some(destination) = destination else {
            return;
        };
        if let Some(origin) = origin {
            self.record(origin);
        }
        self.record(destination);
    }

    pub fn record_switch_keys(
        &mut self,
        origin: Option<&WindowKey>,
        destination: Option<&WindowKey>,
    ) {
        self.record_switch(
            origin.and_then(WindowKey::identity),
            destination.and_then(WindowKey::identity),
        );
    }

    /// Removes identities absent from a fresh WindowServer snapshot. Returns
    /// the number removed so the controller can decide whether to redraw or
    /// persist an updated history without another comparison pass.
    pub fn prune_live(&mut self, live: &[WindowIdentity]) -> usize {
        let before = self.identities.len();
        self.identities
            .retain(|identity| live.iter().any(|candidate| candidate == identity));
        before - self.identities.len()
    }

    /// Most recent live destination that is not the current window. This is the
    /// exact target for a two-window quick-toggle gesture.
    #[must_use]
    pub fn alternate(&self, current: Option<WindowIdentity>) -> Option<WindowIdentity> {
        self.identities
            .iter()
            .copied()
            .find(|candidate| Some(*candidate) != current)
    }

    #[must_use]
    pub fn as_slices(&self) -> (&[WindowIdentity], &[WindowIdentity]) {
        self.identities.as_slices()
    }

    #[must_use]
    pub fn rank(&self, identity: WindowIdentity) -> Option<usize> {
        self.identities
            .iter()
            .position(|candidate| *candidate == identity)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.identities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }
}

impl RecentWindows {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            ids: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn record(&mut self, id: WindowId) {
        if self.capacity == 0 {
            return;
        }
        if let Some(index) = self.ids.iter().position(|candidate| *candidate == id) {
            self.ids.remove(index);
        }
        self.ids.push_front(id);
        self.ids.truncate(self.capacity);
    }

    /// Records a completed switch as a pair. The destination becomes most
    /// recent and the origin remains immediately behind it, which gives the
    /// next invocation deterministic two-window toggling. A destination
    /// without a stable CG window ID is intentionally not persisted.
    pub fn record_switch(&mut self, origin: Option<WindowId>, destination: Option<WindowId>) {
        let Some(destination) = destination else {
            return;
        };
        if let Some(origin) = origin {
            self.record(origin);
        }
        self.record(destination);
    }

    #[must_use]
    pub fn as_slices(&self) -> (&[WindowId], &[WindowId]) {
        self.ids.as_slices()
    }

    #[must_use]
    pub fn rank(&self, id: WindowId) -> Option<usize> {
        self.ids.iter().position(|candidate| *candidate == id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cold_show_is_rejected_after_modifier_release() {
        let mut session = SwitcherSession::default();
        let show = session.begin_show();
        assert_eq!(session.phase(), Phase::AwaitingWindows);
        assert_eq!(session.begin_hide(), None);
        assert_eq!(session.phase(), Phase::Hidden);
        assert!(!session.present(show, 10));
    }

    #[test]
    fn stale_hide_cannot_close_a_reopened_overlay() {
        let mut session = SwitcherSession::default();
        let first_show = session.begin_show();
        assert!(session.present(first_show, 4));
        let hide = session.begin_hide().unwrap();
        let second_show = session.begin_show();
        assert!(session.present(second_show, 3));
        assert!(!session.finish_hide(hide));
        assert_eq!(session.phase(), Phase::Visible);
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut session = SwitcherSession::default();
        let show = session.begin_show();
        assert!(session.present(show, 3));
        assert_eq!(session.selected_index(), 1);
        session.select_next();
        session.select_next();
        assert_eq!(session.selected_index(), 0);
        session.select_previous();
        assert_eq!(session.selected_index(), 2);
        assert!(session.select_index(1));
        assert_eq!(session.selected_index(), 1);
        assert!(!session.select_index(3));
        assert_eq!(session.selected_index(), 1);
    }

    #[test]
    fn presentation_accepts_an_explicit_alternate_index() {
        let mut session = SwitcherSession::default();
        let show = session.begin_show();
        assert!(session.present_at(show, 4, 2));
        assert_eq!(session.selected_index(), 2);
    }

    #[test]
    fn visible_refresh_preserves_a_stable_selection_and_updates_bounds() {
        let mut session = SwitcherSession::default();
        let show = session.begin_show();
        assert!(session.present_at(show, 3, 1));
        assert!(session.refresh_visible(5, 3));
        assert_eq!(session.window_count(), 5);
        assert_eq!(session.selected_index(), 3);
        assert!(session.refresh_visible(2, 9));
        assert_eq!(session.selected_index(), 1);
    }

    #[test]
    fn mru_is_unique_and_bounded_under_stress() {
        let mut recent = RecentWindows::new(40);
        for id in 0..100_000 {
            recent.record(id % 97);
            assert!(recent.len() <= 40);
        }
        assert_eq!(recent.len(), 40);
        assert_eq!(recent.rank(99_999 % 97), Some(0));
    }

    #[test]
    fn completed_switches_keep_the_toggle_pair_ahead_of_unclicked_windows() {
        let mut recent = RecentWindows::new(40);
        recent.record(90);
        recent.record_switch(Some(1), Some(2));
        assert_eq!(recent.rank(2), Some(0));
        assert_eq!(recent.rank(1), Some(1));
        assert_eq!(recent.rank(90), Some(2));

        recent.record_switch(Some(2), Some(1));
        assert_eq!(recent.rank(1), Some(0));
        assert_eq!(recent.rank(2), Some(1));
        assert_eq!(recent.rank(90), Some(2));
    }

    #[test]
    fn ax_only_destination_does_not_corrupt_stable_mru_history() {
        let mut recent = RecentWindows::new(4);
        recent.record(7);
        recent.record_switch(Some(7), None);
        assert_eq!(recent.rank(7), Some(0));
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn stable_history_distinguishes_reused_window_ids_by_process() {
        let mut recent = RecentWindowHistory::new(4);
        let first = WindowIdentity::new(10, 7);
        let reused = WindowIdentity::new(20, 7);
        recent.record(first);
        recent.record(reused);
        assert_eq!(recent.rank(reused), Some(0));
        assert_eq!(recent.rank(first), Some(1));
    }

    #[test]
    fn stable_history_prunes_closed_windows_and_keeps_order() {
        let mut recent = RecentWindowHistory::new(8);
        let a = WindowIdentity::new(1, 10);
        let b = WindowIdentity::new(1, 11);
        let c = WindowIdentity::new(2, 12);
        recent.record(a);
        recent.record(b);
        recent.record(c);
        assert_eq!(recent.prune_live(&[a, c]), 1);
        assert_eq!(recent.as_slices().0, &[c, a]);
        assert!(recent.as_slices().1.is_empty());
    }

    #[test]
    fn stable_history_removes_one_closed_identity_without_a_snapshot() {
        let mut recent = RecentWindowHistory::new(4);
        let a = WindowIdentity::new(1, 10);
        let b = WindowIdentity::new(1, 11);
        recent.record(a);
        recent.record(b);
        assert!(recent.remove(a));
        assert!(!recent.remove(a));
        assert_eq!(recent.as_slices().0, &[b]);
    }

    #[test]
    fn stable_history_records_switch_pairs_and_finds_alternate() {
        let mut recent = RecentWindowHistory::new(4);
        let origin = WindowIdentity::new(1, 10);
        let destination = WindowIdentity::new(2, 20);
        recent.record_switch(Some(origin), Some(destination));
        assert_eq!(recent.alternate(Some(destination)), Some(origin));
        assert_eq!(recent.alternate(Some(origin)), Some(destination));

        recent.record_switch(Some(destination), Some(origin));
        assert_eq!(recent.rank(origin), Some(0));
        assert_eq!(recent.rank(destination), Some(1));
    }

    #[test]
    fn stable_history_ignores_ax_only_keys() {
        let mut recent = RecentWindowHistory::new(4);
        let ax_only = WindowKey {
            pid: 4,
            window_id: None,
            fallback_ordinal: 2,
        };
        assert!(!recent.record_key(&ax_only));
        recent.record_switch_keys(Some(&ax_only), Some(&ax_only));
        assert!(recent.is_empty());
    }

    #[test]
    fn stable_history_is_unique_and_bounded_under_stress() {
        let mut recent = RecentWindowHistory::new(40);
        for index in 0..250_000_u32 {
            recent.record(WindowIdentity::new((index % 17) as i32, index % 97));
            assert!(recent.len() <= 40);
        }
        let (first, second) = recent.as_slices();
        for (index, identity) in first.iter().chain(second).enumerate() {
            assert!(
                first
                    .iter()
                    .chain(second)
                    .skip(index + 1)
                    .all(|candidate| candidate != identity)
            );
        }
    }
}
