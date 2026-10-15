use std::{rc::Rc, sync::Arc};

use floem::reactive::{RwSignal, Scope, SignalGet, SignalUpdate, SignalWith};
use lapce_rpc::terminal::{TermId, TerminalProfile};

use super::data::TerminalData;
use crate::{
    debug::RunDebugProcess, id::TerminalTabId, window_tab::CommonData,
    workspace::LapceWorkspace,
};

#[derive(Clone)]
pub struct TerminalTabData {
    pub scope: Scope,
    pub terminal_tab_id: TerminalTabId,
    pub active: RwSignal<usize>,
    pub terminals: RwSignal<im::Vector<(RwSignal<usize>, TerminalData)>>,
    /// Relative widths (flex weights) of the split panes. Panes without an
    /// entry use [`DEFAULT_PANE_WEIGHT`].
    pub pane_weights: RwSignal<im::HashMap<TermId, f64>>,
}

/// Weight of a pane that has never been split or resized.
pub const DEFAULT_PANE_WEIGHT: f64 = 1.0;

/// Narrowest width, in logical pixels, a pane can be dragged to.
pub const MIN_PANE_WIDTH: f64 = 80.0;

impl TerminalTabData {
    pub fn new(
        workspace: Arc<LapceWorkspace>,
        profile: Option<TerminalProfile>,
        common: Rc<CommonData>,
    ) -> Self {
        TerminalTabData::new_run_debug(workspace, None, profile, common)
    }

    /// Create the information for a terminal tab, which can contain multiple terminals.  
    pub fn new_run_debug(
        workspace: Arc<LapceWorkspace>,
        run_debug: Option<RunDebugProcess>,
        profile: Option<TerminalProfile>,
        common: Rc<CommonData>,
    ) -> Self {
        let cx = common.scope.create_child();
        let terminal_data =
            TerminalData::new_run_debug(cx, workspace, run_debug, profile, common);
        let terminals = im::vector![(cx.create_rw_signal(0), terminal_data)];
        let terminals = cx.create_rw_signal(terminals);
        let active = cx.create_rw_signal(0);
        let pane_weights = cx.create_rw_signal(im::HashMap::new());
        let terminal_tab_id = TerminalTabId::next();
        Self {
            scope: cx,
            terminal_tab_id,
            active,
            terminals,
            pane_weights,
        }
    }

    pub fn active_terminal(&self, tracked: bool) -> Option<TerminalData> {
        let active = if tracked {
            self.active.get()
        } else {
            self.active.get_untracked()
        };

        if tracked {
            self.terminals
                .with(|t| t.get(active).or_else(|| t.last()).cloned())
                .map(|(_, t)| t)
        } else {
            self.terminals
                .with_untracked(|t| t.get(active).or_else(|| t.last()).cloned())
                .map(|(_, t)| t)
        }
    }

    pub fn pane_weight(&self, term_id: &TermId, tracked: bool) -> f64 {
        let get = |w: &im::HashMap<TermId, f64>| {
            w.get(term_id).copied().unwrap_or(DEFAULT_PANE_WEIGHT)
        };
        if tracked {
            self.pane_weights.with(get)
        } else {
            self.pane_weights.with_untracked(get)
        }
    }

    /// Give a newly split pane half of the space of the pane it was split
    /// from. Call after `new` has been added to `terminals`.
    pub fn split_pane_weight(&self, source: TermId, new: TermId) {
        let half = self.pane_weight(&source, false) / 2.0;
        self.set_normalized_pane_weights(|id, weight| {
            if *id == source || *id == new {
                half
            } else {
                weight
            }
        });
    }

    /// Recompute weights after a pane was removed from `terminals`, so the
    /// remaining panes share its space and still fill the whole tab.
    pub fn remove_pane_weight(&self) {
        self.set_normalized_pane_weights(|_, weight| weight);
    }

    /// Store weights for exactly the current panes, normalised to an average
    /// of [`DEFAULT_PANE_WEIGHT`]. Flex grow factors that sum to less than 1
    /// would leave part of the tab empty.
    fn set_normalized_pane_weights(&self, weight: impl Fn(&TermId, f64) -> f64) {
        let ids: Vec<TermId> = self
            .terminals
            .with_untracked(|t| t.iter().map(|(_, t)| t.term_id).collect());
        let weights: Vec<f64> = ids
            .iter()
            .map(|id| weight(id, self.pane_weight(id, false)))
            .collect();
        let weights = normalize_pane_weights(&weights);
        self.pane_weights
            .set(ids.into_iter().zip(weights).collect());
    }
}

/// Scale `weights` so their average is [`DEFAULT_PANE_WEIGHT`]. Invalid input
/// (empty, non-finite or non-positive total) yields equal weights.
pub fn normalize_pane_weights(weights: &[f64]) -> Vec<f64> {
    let total: f64 = weights.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        return vec![DEFAULT_PANE_WEIGHT; weights.len()];
    }
    let scale = DEFAULT_PANE_WEIGHT * weights.len() as f64 / total;
    weights.iter().map(|w| w * scale).collect()
}

/// Compute pane widths after dragging the divider on the left edge of pane
/// `index` by `shift` pixels. Only panes `index - 1` and `index` change, and
/// the shift is clamped so neither gets narrower than `min_width`.
///
/// The result is normalised so the average weight is
/// [`DEFAULT_PANE_WEIGHT`], keeping it compatible with panes that have no
/// stored weight. Returns `None` when nothing can move.
pub fn resize_pane_weights(
    widths: &[f64],
    index: usize,
    shift: f64,
    min_width: f64,
) -> Option<Vec<f64>> {
    if index == 0 || index >= widths.len() {
        return None;
    }
    let (left, right) = (widths[index - 1], widths[index]);
    let min_shift = (min_width - left).min(0.0);
    let max_shift = (right - min_width).max(0.0);
    let shift = shift.clamp(min_shift, max_shift);
    if shift == 0.0 {
        return None;
    }

    let mut new = widths.to_vec();
    new[index - 1] = left + shift;
    new[index] = right - shift;

    let total: f64 = new.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    Some(normalize_pane_weights(&new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn resize_moves_only_neighbours() {
        let w = resize_pane_weights(&[100.0, 100.0, 100.0], 1, 50.0, 10.0).unwrap();
        assert!(approx(&w, &[1.5, 0.5, 1.0]));
    }

    #[test]
    fn resize_clamps_to_min_width() {
        let w = resize_pane_weights(&[100.0, 100.0], 1, 500.0, 20.0).unwrap();
        assert!(approx(&w, &[1.8, 0.2]));
        let w = resize_pane_weights(&[100.0, 100.0], 1, -500.0, 20.0).unwrap();
        assert!(approx(&w, &[0.2, 1.8]));
    }

    #[test]
    fn resize_rejects_invalid_or_noop() {
        assert!(resize_pane_weights(&[100.0, 100.0], 0, 10.0, 10.0).is_none());
        assert!(resize_pane_weights(&[100.0, 100.0], 2, 10.0, 10.0).is_none());
        assert!(resize_pane_weights(&[100.0, 100.0], 1, 0.0, 10.0).is_none());
        // Already at the minimum and pushed further.
        assert!(resize_pane_weights(&[10.0, 100.0], 1, -5.0, 10.0).is_none());
        assert!(resize_pane_weights(&[0.0, 0.0], 1, 5.0, 0.0).is_none());
    }

    #[test]
    fn normalize_keeps_ratios_and_fills_space() {
        // A lone pane left over from a split must grow back to full width.
        assert!(approx(&normalize_pane_weights(&[0.5]), &[1.0]));
        // Split of the first of two equal panes: 0.5, 0.5, 1.0.
        let w = normalize_pane_weights(&[0.5, 0.5, 1.0]);
        assert!(approx(&w, &[0.75, 0.75, 1.5]));
        assert!((w.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!(approx(&normalize_pane_weights(&[0.0, 0.0]), &[1.0, 1.0]));
        assert!(normalize_pane_weights(&[]).is_empty());
    }

    #[test]
    fn resize_handles_panes_below_minimum() {
        // The panel shrank so both panes are narrower than the minimum:
        // dragging must not grow the total or panic.
        assert!(resize_pane_weights(&[30.0, 30.0], 1, 10.0, 50.0).is_none());
    }
}
