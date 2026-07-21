//! Pure navigation-wave planning, separated from cache and worker state so
//! scheduling behavior can be tested and benchmarked deterministically.

use crate::types::Tier;

const BROWSE_WINDOW: u32 = 24;
const FULL_WINDOW: u32 = 2;
const INTERACTIVE_TARGET_CAPACITY: usize =
    2 + BROWSE_WINDOW as usize + (BROWSE_WINDOW / 3) as usize + FULL_WINDOW as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// A display candidate. The engine chooses develop versus rehydrate from
    /// current cache state.
    Display,
    /// A far browse render that should warm ring 2 and disk without entering
    /// the decoded RGBA ring.
    Warm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanTarget {
    pub index: usize,
    pub tier: Tier,
    pub class: u8,
    pub effective_distance: u32,
    pub kind: PlanKind,
}

/// Build the desired navigation wave without inspecting cache state.
///
/// `sequence` is display order after filtering. An empty slice means identity
/// order. `include_warm` adds browse-only work outside the interactive window
/// for a configured disk cache.
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
        if zoomed {
            display(current, Tier::Full, 0, 0);
        }

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
            if effective_distance <= FULL_WINDOW {
                display(index, Tier::Browse, 2, effective_distance);
                if zoomed {
                    display(index, Tier::Full, 3, effective_distance);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            if zoomed {
                display(current, Tier::Full, 0, 0);
            }

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
                if effective_distance <= FULL_WINDOW {
                    display(index, Tier::Browse, 2, effective_distance);
                    if zoomed {
                        display(index, Tier::Full, 3, effective_distance);
                    }
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
    fn fit_omits_full_while_zoomed_prioritizes_it_and_clamps_index() {
        let fit = build_plan_targets(5, usize::MAX, 1, false, &[], false);
        assert_eq!(target(&fit, 4, Tier::Browse, PlanKind::Display).class, 0);
        assert!(fit.iter().all(|target| target.tier != Tier::Full));

        let zoomed = build_plan_targets(5, 4, 1, true, &[], false);
        assert_eq!(target(&zoomed, 4, Tier::Full, PlanKind::Display).class, 0);
    }

    #[test]
    fn fit_wave_has_no_full_work_but_zoomed_wave_preserves_prefetch() {
        let fit = build_plan_targets(100, 50, 1, false, &[], true);
        assert_eq!(
            fit.iter()
                .filter(|target| target.tier == Tier::Full)
                .count(),
            0
        );

        let zoomed = build_plan_targets(100, 50, 1, true, &[], true);
        let full: Vec<_> = zoomed
            .iter()
            .filter(|target| target.tier == Tier::Full)
            .map(|target| (target.index, target.class, target.effective_distance))
            .collect();
        assert_eq!(full, [(50, 0, 0), (51, 3, 1), (52, 3, 2)]);
    }

    #[test]
    fn identity_wave_is_forward_biased_in_both_directions() {
        let forward = build_plan_targets(10, 4, 1, true, &[], false);
        assert_eq!(
            target(&forward, 5, Tier::Browse, PlanKind::Display).effective_distance,
            1
        );
        assert_eq!(
            target(&forward, 6, Tier::Full, PlanKind::Display).effective_distance,
            2
        );
        assert_eq!(
            target(&forward, 3, Tier::Browse, PlanKind::Display).effective_distance,
            3
        );
        assert!(
            forward
                .iter()
                .all(|target| !(target.index == 3 && target.tier == Tier::Full))
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
}
