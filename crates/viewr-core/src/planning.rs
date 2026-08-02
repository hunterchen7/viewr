//! Pure navigation-wave planning, separated from cache and worker state so
//! scheduling behavior can be tested and benchmarked deterministically.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::types::Tier;

const BROWSE_WINDOW: u32 = 24;
const FULL_NEIGHBOR_WINDOW: u32 = 1;
const INTERACTIVE_TARGET_CAPACITY: usize =
    2 + BROWSE_WINDOW as usize + (BROWSE_WINDOW / 3) as usize + FULL_NEIGHBOR_WINDOW as usize * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Purpose of a planned render target.
pub enum PlanKind {
    /// A display candidate. The engine chooses develop versus rehydrate from
    /// current cache state.
    Display,
    /// A Full-resolution candidate admitted by the adaptive RAM working set.
    /// It is useful for instant future zooming but is not yet user-visible.
    Prefetch,
    /// A far browse render that should warm ring 2 and disk without entering
    /// the decoded RGBA ring.
    ///
    /// This variant is available to deterministic planning callers. Production
    /// [`crate::jobs::Engine`] planning uses its separate persistent warm lane
    /// instead of rebuilding these targets on every navigation.
    Warm,
}

#[derive(Debug, Clone)]
/// Byte estimates used to grow the Full-resolution navigation working set.
///
/// Current and immediate visible neighbors are mandatory even when their
/// estimated total exceeds `budget_bytes`. Farther candidates form one
/// direction-weighted prefix and are admitted only while the complete prefix
/// fits. Unknown images use a non-zero conservative fallback estimate.
pub struct FullPrefetchBudget {
    budget_bytes: u64,
    fallback_bytes: u64,
    per_index_bytes: Arc<HashMap<usize, u64>>,
}

impl FullPrefetchBudget {
    /// Creates a Full-resolution prefetch budget and per-folder size estimates.
    pub fn new(budget_bytes: u64, fallback_bytes: u64, known_bytes: HashMap<usize, u64>) -> Self {
        Self {
            budget_bytes,
            fallback_bytes: fallback_bytes.max(1),
            per_index_bytes: Arc::new(known_bytes),
        }
    }

    /// Creates a budget from shared cache observations without cloning the
    /// per-folder maps on every navigation.
    pub fn from_observations(
        budget_bytes: u64,
        fallback_bytes: u64,
        per_index_bytes: Arc<HashMap<usize, u64>>,
    ) -> Self {
        Self {
            budget_bytes,
            fallback_bytes: fallback_bytes.max(1),
            per_index_bytes,
        }
    }

    fn bytes_for(&self, index: usize) -> u64 {
        self.per_index_bytes
            .get(&index)
            .copied()
            .unwrap_or(self.fallback_bytes)
            .max(1)
    }
}

#[derive(Debug, Clone)]
/// Byte estimates used to cap the decoded Browse navigation wave.
///
/// The current image and immediate visible neighbors remain mandatory. Farther
/// Browse targets form a priority prefix that cannot exceed `budget_bytes`.
pub struct BrowsePrefetchBudget {
    budget_bytes: u64,
    fallback_bytes: u64,
    per_index_bytes: Arc<HashMap<usize, u64>>,
}

impl BrowsePrefetchBudget {
    /// Creates a Browse budget and per-folder size estimates.
    pub fn new(budget_bytes: u64, fallback_bytes: u64, known_bytes: HashMap<usize, u64>) -> Self {
        Self::from_observations(budget_bytes, fallback_bytes, Arc::new(known_bytes))
    }

    /// Creates a budget from shared cache observations without cloning them on
    /// every navigation.
    pub fn from_observations(
        budget_bytes: u64,
        fallback_bytes: u64,
        per_index_bytes: Arc<HashMap<usize, u64>>,
    ) -> Self {
        Self {
            budget_bytes,
            fallback_bytes: fallback_bytes.max(1),
            per_index_bytes,
        }
    }

    fn bytes_for(&self, index: usize) -> u64 {
        self.per_index_bytes
            .get(&index)
            .copied()
            .unwrap_or(self.fallback_bytes)
            .max(1)
    }
}

#[derive(Debug, Clone)]
/// Browse and Full byte budgets for one adaptive navigation plan.
pub struct NavigationPrefetchBudgets {
    full: FullPrefetchBudget,
    browse: BrowsePrefetchBudget,
}

impl NavigationPrefetchBudgets {
    /// Combines independently measured Full and Browse cache budgets.
    pub fn new(full: FullPrefetchBudget, browse: BrowsePrefetchBudget) -> Self {
        Self { full, browse }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullSelection {
    index: usize,
    effective_distance: u32,
    required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One desired render in a navigation wave.
pub struct PlanTarget {
    /// Folder index to render.
    pub index: usize,
    /// Render quality/cache tier.
    pub tier: Tier,
    /// Coarse priority class; lower values run first.
    pub class: u8,
    /// Direction-weighted distance used within a priority class.
    ///
    /// Entries behind the navigation direction count three times farther than
    /// entries ahead.
    pub effective_distance: u32,
    /// Whether the render is required for display, speculative Full prefetch,
    /// or persistent background warming.
    pub kind: PlanKind,
}

/// Build the desired navigation wave without inspecting cache state.
///
/// `sequence` is display order after filtering. An empty slice means identity
/// order. `include_warm` adds browse-only work outside the interactive window
/// for a configured disk cache. That option deliberately builds an O(`len`)
/// reference set; [`crate::jobs::Engine`] passes `false` and owns one persistent
/// folder-warm lane instead.
///
/// `current` is clamped to `len - 1`, and any negative `direction` means
/// backward while zero and positive values mean forward. If a nonempty
/// `sequence` omits `current`, its first position is used as the navigation
/// origin; out-of-range sequence entries are ignored. Callers should normally
/// supply unique valid indices.
///
/// Full targets always include the current item and its immediate visible
/// neighbors so zoom can use native-resolution pixels without starting new
/// work. `zoomed` raises the current Full target to the highest priority.
/// Without warm targets, allocation is bounded by the interactive windows,
/// although locating the current item in a filtered sequence is linear in the
/// sequence length.
pub fn build_plan_targets(
    len: usize,
    current: usize,
    direction: i8,
    zoomed: bool,
    sequence: &[usize],
    include_warm: bool,
) -> Vec<PlanTarget> {
    if len == 0 {
        return Vec::new();
    }
    let current = current.min(len - 1);
    let mut targets = Vec::with_capacity(if include_warm {
        len.saturating_add(4)
    } else {
        INTERACTIVE_TARGET_CAPACITY
    });
    {
        let mut display = |index, tier, class, effective_distance| {
            targets.push(PlanTarget {
                index,
                tier,
                class,
                effective_distance,
                kind: PlanKind::Display,
            });
        };

        display(current, Tier::Browse, 0, 0);
        display(current, Tier::Full, u8::from(!zoomed), 0);

        let current_position = if sequence.is_empty() {
            current
        } else {
            sequence
                .iter()
                .position(|&index| index == current)
                .unwrap_or_default()
        };
        let mut add_neighbor = |position: usize, index: usize| {
            if index == current || index >= len {
                return;
            }
            let ahead = (position > current_position) == (direction >= 0);
            let distance = position.abs_diff(current_position).min(u32::MAX as usize) as u32;
            let effective_distance = if ahead {
                distance
            } else {
                distance.saturating_mul(3)
            };
            if distance <= FULL_NEIGHBOR_WINDOW {
                display(index, Tier::Browse, 2, effective_distance);
                display(index, Tier::Full, 3, effective_distance);
            } else if effective_distance <= BROWSE_WINDOW {
                display(index, Tier::Browse, 4, effective_distance);
            }
        };
        let (positions_before, positions_after) = if direction >= 0 {
            ((BROWSE_WINDOW / 3) as usize, BROWSE_WINDOW as usize)
        } else {
            (BROWSE_WINDOW as usize, (BROWSE_WINDOW / 3) as usize)
        };
        if sequence.is_empty() {
            let first = current_position.saturating_sub(positions_before);
            let last = current_position
                .saturating_add(positions_after)
                .min(len - 1);
            for index in first..=last {
                add_neighbor(index, index);
            }
        } else {
            let first = current_position.saturating_sub(positions_before);
            let last = current_position
                .saturating_add(positions_after)
                .min(sequence.len() - 1);
            for (offset, &index) in sequence[first..=last].iter().enumerate() {
                add_neighbor(first + offset, index);
            }
        }
    }

    if include_warm {
        for index in 0..len {
            if index == current {
                continue;
            }
            let ahead = (index > current) == (direction >= 0);
            let distance = index.abs_diff(current).min(u32::MAX as usize) as u32;
            let effective_distance = if ahead {
                distance
            } else {
                distance.saturating_mul(3)
            };
            if effective_distance > BROWSE_WINDOW {
                targets.push(PlanTarget {
                    index,
                    tier: Tier::Browse,
                    class: 6,
                    effective_distance,
                    kind: PlanKind::Warm,
                });
            }
        }
    }

    targets
}

/// Build the production navigation wave with an adaptive Full-resolution
/// working set.
///
/// The Browse wave remains bounded by `BROWSE_WINDOW`. Full candidates begin
/// with the current image and both immediate visible neighbors, then grow in a
/// forward-biased 3:1 wave until the next candidate would cross the byte
/// budget. Optional Full work is lower priority than the complete interactive
/// Browse wave and is identified as [`PlanKind::Prefetch`].
///
/// A filtered `sequence` ignores out-of-range entries and later duplicates
/// while preserving the first occurrence.
pub fn build_plan_targets_with_full_prefetch(
    len: usize,
    current: usize,
    direction: i8,
    zoomed: bool,
    sequence: &[usize],
    budgets: &NavigationPrefetchBudgets,
    include_warm: bool,
) -> Vec<PlanTarget> {
    if len == 0 {
        return Vec::new();
    }
    let current = current.min(len - 1);
    let normalized_sequence =
        (!sequence.is_empty()).then(|| normalize_filtered_sequence(len, sequence));
    let sequence = normalized_sequence.as_deref().unwrap_or(sequence);
    let current_position = current_position(current, sequence);
    build_plan_targets_with_normalized_prefetch(
        len,
        current,
        direction,
        zoomed,
        (sequence, current_position),
        budgets,
        include_warm,
    )
}

/// Production variant for a sequence normalized once by the owning engine.
pub(crate) fn build_plan_targets_with_normalized_prefetch(
    len: usize,
    current: usize,
    direction: i8,
    zoomed: bool,
    order: (&[usize], usize),
    budgets: &NavigationPrefetchBudgets,
    include_warm: bool,
) -> Vec<PlanTarget> {
    if len == 0 {
        return Vec::new();
    }
    let current = current.min(len - 1);
    let (sequence, current_position) = order;
    let ordered_len = if sequence.is_empty() {
        len
    } else {
        sequence.len()
    };
    let current_position = current_position.min(ordered_len.saturating_sub(1));
    let full = adaptive_full_selections(
        len,
        current,
        current_position,
        direction,
        sequence,
        &budgets.full,
    );
    let mut targets = Vec::with_capacity(if include_warm {
        len.saturating_add(full.len()).saturating_add(2)
    } else {
        INTERACTIVE_TARGET_CAPACITY.saturating_add(full.len())
    });
    targets.push(PlanTarget {
        index: current,
        tier: Tier::Browse,
        class: 0,
        effective_distance: 0,
        kind: PlanKind::Display,
    });
    targets.push(PlanTarget {
        index: current,
        tier: Tier::Full,
        class: u8::from(!zoomed),
        effective_distance: 0,
        kind: PlanKind::Display,
    });

    let index_at = |position: usize| {
        if sequence.is_empty() {
            position
        } else {
            sequence[position]
        }
    };
    for position in current_position.saturating_sub(1)
        ..=(current_position + 1).min(ordered_len.saturating_sub(1))
    {
        let index = index_at(position);
        if index == current {
            continue;
        }
        let (_, effective_distance) = weighted_distance(position, current_position, direction);
        targets.push(PlanTarget {
            index,
            tier: Tier::Browse,
            class: 2,
            effective_distance,
            kind: PlanKind::Display,
        });
        if full
            .iter()
            .any(|selection| selection.required && selection.index == index)
        {
            targets.push(PlanTarget {
                index,
                tier: Tier::Full,
                class: 2,
                effective_distance,
                kind: PlanKind::Display,
            });
        }
    }
    targets.extend(
        adaptive_browse_selections(
            len,
            current,
            current_position,
            direction,
            sequence,
            &budgets.browse,
        )
        .into_iter()
        .map(|(index, effective_distance)| PlanTarget {
            index,
            tier: Tier::Browse,
            class: 4,
            effective_distance,
            kind: PlanKind::Display,
        }),
    );

    targets.extend(
        full.into_iter()
            .filter(|selection| !selection.required)
            .map(|selection| PlanTarget {
                index: selection.index,
                tier: Tier::Full,
                class: 5,
                effective_distance: selection.effective_distance,
                kind: PlanKind::Prefetch,
            }),
    );

    if include_warm {
        for index in 0..len {
            if index == current {
                continue;
            }
            let ahead = (index > current) == (direction >= 0);
            let distance = index.abs_diff(current).min(u32::MAX as usize) as u32;
            let effective_distance = if ahead {
                distance
            } else {
                distance.saturating_mul(3)
            };
            if effective_distance > BROWSE_WINDOW {
                targets.push(PlanTarget {
                    index,
                    tier: Tier::Browse,
                    class: 6,
                    effective_distance,
                    kind: PlanKind::Warm,
                });
            }
        }
    }

    targets
}

fn adaptive_browse_selections(
    len: usize,
    current: usize,
    current_position: usize,
    direction: i8,
    sequence: &[usize],
    budget: &BrowsePrefetchBudget,
) -> Vec<(usize, u32)> {
    let ordered_len = if sequence.is_empty() {
        len
    } else {
        sequence.len()
    };
    let current_position = current_position.min(ordered_len.saturating_sub(1));
    let index_at = |position: usize| {
        if sequence.is_empty() {
            position
        } else {
            sequence[position]
        }
    };
    let mut used_bytes = budget.bytes_for(current);
    for position in current_position.saturating_sub(1)
        ..=(current_position + 1).min(ordered_len.saturating_sub(1))
    {
        let index = index_at(position);
        if index != current {
            used_bytes = used_bytes.saturating_add(budget.bytes_for(index));
        }
    }

    let estimated_slots = budget
        .budget_bytes
        .checked_div(budget.fallback_bytes)
        .unwrap_or_default()
        .min((BROWSE_WINDOW + 1) as u64) as usize;
    let mut selected = Vec::with_capacity(estimated_slots.saturating_sub(3));
    let forward = direction >= 0;
    let mut ahead_distance = 2_usize;
    let mut behind_distance = 2_usize;
    loop {
        let ahead_position = if forward {
            current_position.checked_add(ahead_distance)
        } else {
            current_position.checked_sub(ahead_distance)
        }
        .filter(|position| *position < ordered_len);
        let behind_position = if forward {
            current_position.checked_sub(behind_distance)
        } else {
            current_position.checked_add(behind_distance)
        }
        .filter(|position| *position < ordered_len);
        let choose_ahead = match (ahead_position, behind_position) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(_), Some(_)) => ahead_distance <= behind_distance.saturating_mul(3),
        };
        let (position, effective_distance) = if choose_ahead {
            let position = ahead_position.expect("ahead candidate was selected");
            let effective_distance = ahead_distance.min(u32::MAX as usize) as u32;
            ahead_distance = ahead_distance.saturating_add(1);
            (position, effective_distance)
        } else {
            let position = behind_position.expect("behind candidate was selected");
            let effective_distance =
                (behind_distance.min(u32::MAX as usize) as u32).saturating_mul(3);
            behind_distance = behind_distance.saturating_add(1);
            (position, effective_distance)
        };
        if effective_distance > BROWSE_WINDOW {
            break;
        }
        let index = index_at(position);
        let bytes = budget.bytes_for(index);
        let Some(next_bytes) = used_bytes.checked_add(bytes) else {
            break;
        };
        if next_bytes > budget.budget_bytes {
            break;
        }
        used_bytes = next_bytes;
        selected.push((index, effective_distance));
    }
    selected
}

fn adaptive_full_selections(
    len: usize,
    current: usize,
    current_position: usize,
    direction: i8,
    sequence: &[usize],
    budget: &FullPrefetchBudget,
) -> Vec<FullSelection> {
    let ordered_len = if sequence.is_empty() {
        len
    } else {
        sequence.len()
    };
    let current_position = current_position.min(ordered_len.saturating_sub(1));
    let index_at = |position: usize| {
        if sequence.is_empty() {
            position
        } else {
            sequence[position]
        }
    };
    let estimated_slots = budget
        .budget_bytes
        .checked_div(budget.fallback_bytes)
        .unwrap_or_default()
        .min(ordered_len as u64) as usize;
    let estimated_slots = estimated_slots.min(1_024);
    let selection_capacity = estimated_slots.saturating_add(3);
    let mut selected = Vec::with_capacity(selection_capacity);
    let mut used_bytes = 0_u64;
    let mut add_required = |index: usize, effective_distance: u32| {
        if index < len {
            used_bytes = used_bytes.saturating_add(budget.bytes_for(index));
            selected.push(FullSelection {
                index,
                effective_distance,
                required: true,
            });
        }
    };
    add_required(current, 0);
    for position in current_position.saturating_sub(1)
        ..=(current_position + 1).min(ordered_len.saturating_sub(1))
    {
        let index = index_at(position);
        if index == current {
            continue;
        }
        let (_, effective_distance) = weighted_distance(position, current_position, direction);
        add_required(index, effective_distance);
    }

    let forward = direction >= 0;
    // Distance one is already part of the mandatory working set.
    let mut ahead_distance = 2_usize;
    let mut behind_distance = 2_usize;
    loop {
        let ahead_position = if forward {
            current_position.checked_add(ahead_distance)
        } else {
            current_position.checked_sub(ahead_distance)
        }
        .filter(|position| *position < ordered_len);
        let behind_position = if forward {
            current_position.checked_sub(behind_distance)
        } else {
            current_position.checked_add(behind_distance)
        }
        .filter(|position| *position < ordered_len);
        let choose_ahead = match (ahead_position, behind_position) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(_), Some(_)) => ahead_distance <= behind_distance.saturating_mul(3),
        };
        let (position, effective_distance) = if choose_ahead {
            let position = ahead_position.expect("ahead candidate was selected");
            let effective_distance = ahead_distance.min(u32::MAX as usize) as u32;
            ahead_distance = ahead_distance.saturating_add(1);
            (position, effective_distance)
        } else {
            let position = behind_position.expect("behind candidate was selected");
            let effective_distance =
                (behind_distance.min(u32::MAX as usize) as u32).saturating_mul(3);
            behind_distance = behind_distance.saturating_add(1);
            (position, effective_distance)
        };
        let index = index_at(position);
        if index >= len {
            continue;
        }
        let bytes = budget.bytes_for(index);
        let Some(next_bytes) = used_bytes.checked_add(bytes) else {
            break;
        };
        if next_bytes > budget.budget_bytes {
            break;
        }
        used_bytes = next_bytes;
        selected.push(FullSelection {
            index,
            effective_distance,
            required: false,
        });
    }
    selected
}

fn current_position(current: usize, sequence: &[usize]) -> usize {
    if sequence.is_empty() {
        current
    } else {
        sequence
            .iter()
            .position(|&index| index == current)
            .unwrap_or_default()
    }
}

fn normalize_filtered_sequence(len: usize, sequence: &[usize]) -> Vec<usize> {
    let mut seen = HashSet::with_capacity(sequence.len());
    let mut normalized = Vec::with_capacity(sequence.len());
    for &index in sequence {
        if index < len && seen.insert(index) {
            normalized.push(index);
        }
    }
    normalized
}

fn weighted_distance(position: usize, current_position: usize, direction: i8) -> (u32, u32) {
    let ahead = (position > current_position) == (direction >= 0);
    let distance = position.abs_diff(current_position).min(u32::MAX as usize) as u32;
    let effective_distance = if ahead {
        distance
    } else {
        distance.saturating_mul(3)
    };
    (distance, effective_distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn navigation_budgets(full: FullPrefetchBudget) -> NavigationPrefetchBudgets {
        NavigationPrefetchBudgets::new(full, BrowsePrefetchBudget::new(u64::MAX, 1, HashMap::new()))
    }

    /// Straightforward full scan retained in tests as a semantic oracle for
    /// the bounded interactive scan used in production.
    fn build_plan_targets_reference(
        len: usize,
        current: usize,
        direction: i8,
        zoomed: bool,
        sequence: &[usize],
        include_warm: bool,
    ) -> Vec<PlanTarget> {
        if len == 0 {
            return Vec::new();
        }
        let current = current.min(len - 1);
        let mut targets = Vec::new();
        {
            let mut display = |index, tier, class, effective_distance| {
                targets.push(PlanTarget {
                    index,
                    tier,
                    class,
                    effective_distance,
                    kind: PlanKind::Display,
                });
            };
            display(current, Tier::Browse, 0, 0);
            display(current, Tier::Full, u8::from(!zoomed), 0);

            let current_position = if sequence.is_empty() {
                current
            } else {
                sequence
                    .iter()
                    .position(|&index| index == current)
                    .unwrap_or_default()
            };
            for (position, index) in if sequence.is_empty() {
                (0..len).enumerate().collect::<Vec<_>>()
            } else {
                sequence.iter().copied().enumerate().collect()
            } {
                if index == current || index >= len {
                    continue;
                }
                let ahead = (position > current_position) == (direction >= 0);
                let distance = position.abs_diff(current_position).min(u32::MAX as usize) as u32;
                let effective_distance = if ahead {
                    distance
                } else {
                    distance.saturating_mul(3)
                };
                if distance <= FULL_NEIGHBOR_WINDOW {
                    display(index, Tier::Browse, 2, effective_distance);
                    display(index, Tier::Full, 3, effective_distance);
                } else if effective_distance <= BROWSE_WINDOW {
                    display(index, Tier::Browse, 4, effective_distance);
                }
            }
        }

        if include_warm {
            for index in 0..len {
                if index == current {
                    continue;
                }
                let ahead = (index > current) == (direction >= 0);
                let distance = index.abs_diff(current).min(u32::MAX as usize) as u32;
                let effective_distance = if ahead {
                    distance
                } else {
                    distance.saturating_mul(3)
                };
                if effective_distance > BROWSE_WINDOW {
                    targets.push(PlanTarget {
                        index,
                        tier: Tier::Browse,
                        class: 6,
                        effective_distance,
                        kind: PlanKind::Warm,
                    });
                }
            }
        }
        targets
    }

    fn target(targets: &[PlanTarget], index: usize, tier: Tier, kind: PlanKind) -> PlanTarget {
        *targets
            .iter()
            .find(|target| target.index == index && target.tier == tier && target.kind == kind)
            .unwrap_or_else(|| panic!("missing {kind:?} target {index}/{tier:?}: {targets:?}"))
    }

    #[test]
    fn empty_library_has_no_targets() {
        assert!(build_plan_targets(0, 0, 1, false, &[], true).is_empty());
    }

    #[test]
    fn bounded_scan_matches_full_scan_exhaustively() {
        for len in 0_usize..=50 {
            let sequences = [
                Vec::new(),
                (0..len).collect(),
                (0..len).filter(|index| index % 3 != 1).collect(),
            ];
            for current in 0..=len.saturating_add(1) {
                for direction in [-1, 0, 1] {
                    for zoomed in [false, true] {
                        for include_warm in [false, true] {
                            for sequence in &sequences {
                                assert_eq!(
                                    build_plan_targets(
                                        len,
                                        current,
                                        direction,
                                        zoomed,
                                        sequence,
                                        include_warm,
                                    ),
                                    build_plan_targets_reference(
                                        len,
                                        current,
                                        direction,
                                        zoomed,
                                        sequence,
                                        include_warm,
                                    ),
                                    "len={len}, current={current}, direction={direction}, \
                                     zoomed={zoomed}, include_warm={include_warm}, \
                                     sequence={sequence:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fit_preloads_full_while_zoom_prioritizes_it_and_clamps_index() {
        let fit = build_plan_targets(5, usize::MAX, 1, false, &[], false);
        assert_eq!(target(&fit, 4, Tier::Browse, PlanKind::Display).class, 0);
        assert_eq!(target(&fit, 4, Tier::Full, PlanKind::Display).class, 1);

        let zoomed = build_plan_targets(5, 4, 1, true, &[], false);
        assert_eq!(target(&zoomed, 4, Tier::Full, PlanKind::Display).class, 0);
    }

    #[test]
    fn fit_and_zoomed_waves_preload_current_and_both_immediate_neighbors() {
        let fit = build_plan_targets(100, 50, 1, false, &[], true);
        let fit_full: Vec<_> = fit
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.class, target.effective_distance))
            .collect();
        assert_eq!(fit_full, [(50, 1, 0), (49, 3, 3), (51, 3, 1)]);

        let zoomed = build_plan_targets(100, 50, 1, true, &[], true);
        let full: Vec<_> = zoomed
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.class, target.effective_distance))
            .collect();
        assert_eq!(full, [(50, 0, 0), (49, 3, 3), (51, 3, 1)]);
    }

    #[test]
    fn identity_wave_is_forward_biased_in_both_directions() {
        let forward = build_plan_targets(10, 4, 1, true, &[], false);
        assert_eq!(
            target(&forward, 5, Tier::Browse, PlanKind::Display).effective_distance,
            1
        );
        assert_eq!(
            target(&forward, 6, Tier::Browse, PlanKind::Display).effective_distance,
            2
        );
        assert!(
            forward
                .iter()
                .all(|target| !(target.index == 6 && target.tier == Tier::Full))
        );
        assert_eq!(
            target(&forward, 3, Tier::Browse, PlanKind::Display).effective_distance,
            3
        );
        assert_eq!(
            target(&forward, 3, Tier::Full, PlanKind::Display).effective_distance,
            3
        );

        let backward = build_plan_targets(10, 4, -1, true, &[], false);
        assert_eq!(
            target(&backward, 3, Tier::Full, PlanKind::Display).effective_distance,
            1
        );
        assert_eq!(
            target(&backward, 5, Tier::Browse, PlanKind::Display).effective_distance,
            3
        );
    }

    #[test]
    fn filtered_sequence_controls_interactive_neighbors() {
        let targets = build_plan_targets(10, 4, 1, true, &[1, 4, 8], false);
        assert!(targets.iter().any(|target| target.index == 8));
        assert!(targets.iter().any(|target| target.index == 1));
        assert!(
            targets
                .iter()
                .all(|target| ![2, 3, 5, 6, 7].contains(&target.index))
        );
        assert_eq!(
            target(&targets, 8, Tier::Full, PlanKind::Display).effective_distance,
            1
        );
        assert_eq!(
            target(&targets, 1, Tier::Browse, PlanKind::Display).effective_distance,
            3
        );
    }

    #[test]
    fn warm_targets_cover_only_the_far_identity_wave() {
        let targets = build_plan_targets(100, 50, 1, false, &[], true);
        assert!(
            targets
                .iter()
                .filter(|target| target.kind == PlanKind::Warm)
                .all(|target| {
                    target.tier == Tier::Browse
                        && target.class == 6
                        && target.effective_distance > BROWSE_WINDOW
                })
        );
        assert_eq!(
            target(&targets, 75, Tier::Browse, PlanKind::Warm).effective_distance,
            25
        );
        assert_eq!(
            target(&targets, 41, Tier::Browse, PlanKind::Warm).effective_distance,
            27
        );
        assert!(
            targets
                .iter()
                .all(|target| !(target.index == 42 && target.kind == PlanKind::Warm))
        );
    }

    #[test]
    fn adaptive_full_prefetch_fills_a_directional_prefix_to_its_byte_budget() {
        let budget = navigation_budgets(FullPrefetchBudget::new(700, 100, HashMap::new()));
        let targets = build_plan_targets_with_full_prefetch(100, 50, 1, false, &[], &budget, false);
        let full: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.kind, target.effective_distance))
            .collect();

        assert_eq!(
            full,
            [
                (50, PlanKind::Display, 0),
                (49, PlanKind::Display, 3),
                (51, PlanKind::Display, 1),
                (52, PlanKind::Prefetch, 2),
                (53, PlanKind::Prefetch, 3),
                (54, PlanKind::Prefetch, 4),
                (55, PlanKind::Prefetch, 5),
            ]
        );
    }

    #[test]
    fn browse_wave_is_capped_to_a_convergent_byte_prefix() {
        let budgets = NavigationPrefetchBudgets::new(
            FullPrefetchBudget::new(700, 100, HashMap::new()),
            BrowsePrefetchBudget::new(400, 100, HashMap::new()),
        );
        let targets =
            build_plan_targets_with_full_prefetch(100, 50, 1, false, &[], &budgets, false);
        let browse: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Browse)
            .map(|target| (target.index, target.effective_distance))
            .collect();

        assert_eq!(browse.len(), 4);
        assert!(browse.contains(&(50, 0)));
        assert!(browse.contains(&(49, 3)));
        assert!(browse.contains(&(51, 1)));
        assert!(browse.contains(&(52, 2)));
    }

    #[test]
    fn mandatory_full_targets_survive_a_budget_shortfall_without_extras() {
        let known = HashMap::from([(49, 200), (50, 200), (51, 200)]);
        let budget = navigation_budgets(FullPrefetchBudget::new(500, 100, known));
        let targets = build_plan_targets_with_full_prefetch(100, 50, 1, false, &[], &budget, false);
        let full: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.kind))
            .collect();

        assert_eq!(
            full,
            [
                (50, PlanKind::Display),
                (49, PlanKind::Display),
                (51, PlanKind::Display),
            ]
        );
    }

    #[test]
    fn adaptive_full_prefetch_does_not_skip_a_nearer_nonfitting_candidate() {
        let known = HashMap::from([(49, 100), (50, 100), (51, 100), (52, 300), (53, 50)]);
        let budget = navigation_budgets(FullPrefetchBudget::new(550, 100, known));
        let targets = build_plan_targets_with_full_prefetch(100, 50, 1, false, &[], &budget, false);

        assert!(targets.iter().all(|target| {
            target.tier != Tier::Full || !matches!(target.kind, PlanKind::Prefetch)
        }));
    }

    #[test]
    fn adaptive_full_prefetch_follows_filtered_order_in_both_directions() {
        let budget = navigation_budgets(FullPrefetchBudget::new(500, 100, HashMap::new()));
        let targets = build_plan_targets_with_full_prefetch(
            10,
            4,
            -1,
            false,
            &[9, 2, 7, 4, 8, 1],
            &budget,
            false,
        );
        let full: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.kind, target.effective_distance))
            .collect();

        assert_eq!(
            full,
            [
                (4, PlanKind::Display, 0),
                (7, PlanKind::Display, 1),
                (8, PlanKind::Display, 3),
                (2, PlanKind::Prefetch, 2),
                (9, PlanKind::Prefetch, 3),
            ]
        );
    }

    #[test]
    fn adaptive_full_count_scales_across_representative_sensor_sizes() {
        for megapixels in [12_u64, 24, 33, 61] {
            let full_bytes = megapixels * 1_000_000 * 4;
            for budget_bytes in [512_000_000_u64, 2_000_000_000, 4_000_000_000] {
                let budget = navigation_budgets(FullPrefetchBudget::new(
                    budget_bytes,
                    full_bytes,
                    HashMap::new(),
                ));
                let targets =
                    build_plan_targets_with_full_prefetch(100, 50, 1, false, &[], &budget, false);
                let actual = targets
                    .iter()
                    .filter(|target| target.tier == Tier::Full)
                    .count();
                let expected = (budget_bytes / full_bytes) as usize;
                assert_eq!(
                    actual,
                    expected.clamp(3, 100),
                    "{megapixels} MP with a {budget_bytes} byte Full budget"
                );
            }
        }
    }

    #[test]
    fn filtered_duplicate_indices_do_not_consume_full_budget_twice() {
        let budget = navigation_budgets(FullPrefetchBudget::new(400, 100, HashMap::new()));
        let targets = build_plan_targets_with_full_prefetch(
            10,
            5,
            1,
            false,
            &[4, 5, 5, 6, 7],
            &budget,
            false,
        );
        let full: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.kind))
            .collect();

        assert_eq!(
            full,
            [
                (5, PlanKind::Display),
                (4, PlanKind::Display),
                (6, PlanKind::Display),
                (7, PlanKind::Prefetch),
            ]
        );
    }

    #[test]
    fn invalid_filtered_indices_do_not_displace_visible_full_neighbors() {
        let budget = navigation_budgets(FullPrefetchBudget::new(400, 100, HashMap::new()));
        let targets = build_plan_targets_with_full_prefetch(
            10,
            5,
            1,
            false,
            &[usize::MAX, 4, 5, 5, usize::MAX - 1, 6, 7],
            &budget,
            false,
        );
        let full: Vec<_> = targets
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.kind))
            .collect();

        assert_eq!(
            full,
            [
                (5, PlanKind::Display),
                (4, PlanKind::Display),
                (6, PlanKind::Display),
                (7, PlanKind::Prefetch),
            ]
        );
    }

    #[test]
    fn all_invalid_filtered_indices_normalize_to_identity_order() {
        let budget = navigation_budgets(FullPrefetchBudget::new(400, 100, HashMap::new()));
        let identity = build_plan_targets_with_full_prefetch(10, 5, 1, false, &[], &budget, false);
        let invalid = build_plan_targets_with_full_prefetch(
            10,
            5,
            1,
            false,
            &[usize::MAX, usize::MAX - 1],
            &budget,
            false,
        );

        assert_eq!(invalid, identity);
    }
}
