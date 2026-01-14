//! Pure layout math functions for center-priority bar layout.
//!
//! These functions contain no GTK dependencies and can be unit tested directly.

/// Clamp a widget's size to fit within available space.
///
/// Returns:
/// - `natural` if it fits within `available`
/// - Otherwise shrinks toward `min`, but never below 0
/// - If `available <= 0`, returns 0
///
/// # Examples
///
/// ```
/// use vibepanel::layout_math::clamp_width;
///
/// // Natural size fits
/// assert_eq!(clamp_width(100, 50, 80), 80);
///
/// // Natural too large, use available
/// assert_eq!(clamp_width(60, 50, 80), 60);
///
/// // Available less than min, use available (can't fit)
/// assert_eq!(clamp_width(30, 50, 80), 30);
///
/// // No space available
/// assert_eq!(clamp_width(0, 50, 80), 0);
/// ```
pub fn clamp_width(available: i32, min_size: i32, nat_size: i32) -> i32 {
    if available <= 0 {
        return 0;
    }
    let mut target = nat_size.min(available);
    if target < min_size {
        target = min_size.min(available);
    }
    target.max(0)
}

/// Input sizes for a section (min and natural width).
#[derive(Debug, Clone, Copy, Default)]
pub struct SectionSizes {
    pub min: i32,
    pub natural: i32,
}

/// Results of center-priority allocation.
#[derive(Debug, Clone, Copy)]
pub struct CenterPriorityAllocation {
    /// X position for left section (relative to interior start)
    pub left_x: i32,
    /// Width allocated to left section
    pub left_width: i32,
    /// X position for center section (relative to interior start)
    pub center_x: i32,
    /// Width allocated to center section
    pub center_width: i32,
    /// X position for right section (relative to interior start)
    pub right_x: i32,
    /// Width allocated to right section
    pub right_width: i32,
}

/// Calculate center-priority layout allocations.
///
/// The center section is anchored to the true horizontal center.
/// Left and right sections get the remaining space after center is placed.
///
/// # Arguments
///
/// * `interior` - Total usable width (after edge margins)
/// * `spacing` - Gap between adjacent sections
/// * `left` - Size requirements for left section (None if not present)
/// * `left_expand` - Whether left section should expand to fill available space
/// * `center` - Size requirements for center section
/// * `right` - Size requirements for right section (None if not present)
/// * `right_expand` - Whether right section should expand to fill available space
///
/// # Returns
///
/// Allocation with positions and widths for all sections.
pub fn compute_center_priority_allocation(
    interior: i32,
    spacing: i32,
    left: Option<SectionSizes>,
    left_expand: bool,
    center: SectionSizes,
    right: Option<SectionSizes>,
    right_expand: bool,
) -> CenterPriorityAllocation {
    // Calculate center width and position (anchored to true center)
    let center_width = clamp_width(interior, center.min, center.natural);
    let center_start = ((interior - center_width) / 2).max(0);
    let center_end = center_start + center_width;

    // Calculate budgets for left and right
    let gap_left = if left.is_some() { spacing } else { 0 };
    let gap_right = if right.is_some() { spacing } else { 0 };
    let left_budget = (center_start - gap_left).max(0);
    let right_budget = (interior - center_end - gap_right).max(0);

    // Calculate widths
    // If section has an expander (like a flexible spacer), give it full budget
    // so GTK's Box can distribute space to hexpand children.
    // Otherwise, clamp to natural size for normal layout behavior.
    let left_width = match (left, left_expand) {
        (Some(_), true) => left_budget,
        (Some(s), false) => clamp_width(left_budget, s.min, s.natural),
        (None, _) => 0,
    };
    let right_width = match (right, right_expand) {
        (Some(_), true) => right_budget,
        (Some(s), false) => clamp_width(right_budget, s.min, s.natural),
        (None, _) => 0,
    };

    // Calculate positions
    let left_x = 0;
    let center_x = center_start;
    let right_x = interior - right_width;

    CenterPriorityAllocation {
        left_x,
        left_width,
        center_x,
        center_width,
        right_x,
        right_width,
    }
}

/// Results of linear (no-center) allocation.
#[derive(Debug, Clone, Copy)]
pub struct LinearAllocation {
    /// X position for left section (relative to interior start)
    pub left_x: i32,
    /// Width allocated to left section
    pub left_width: i32,
    /// X position for right section (relative to interior start)
    pub right_x: i32,
    /// Width allocated to right section
    pub right_width: i32,
}

/// Calculate linear layout allocations (when no center section exists).
///
/// Left anchors to start, right anchors to end. When space is constrained,
/// they compete proportionally.
///
/// # Arguments
///
/// * `interior` - Total usable width (after edge margins)
/// * `spacing` - Gap between left and right sections
/// * `left` - Size requirements for left section (None if not present)
/// * `right` - Size requirements for right section (None if not present)
pub fn compute_linear_allocation(
    interior: i32,
    spacing: i32,
    left: Option<SectionSizes>,
    right: Option<SectionSizes>,
) -> LinearAllocation {
    let (left_min, left_nat) = left.map_or((0, 0), |s| (s.min, s.natural));
    let (right_min, right_nat) = right.map_or((0, 0), |s| (s.min, s.natural));

    let (left_width, right_width) = match (left.is_some(), right.is_some()) {
        (true, true) => {
            let available = (interior - spacing).max(0);

            let mut lw = left_nat.min(available);
            let mut rw = (available - lw).max(right_min);

            if rw > right_nat {
                rw = right_nat;
                lw = available - rw;
            }

            if lw < left_min {
                lw = left_min;
                rw = (available - lw).max(right_min);
            }

            (lw, rw)
        }
        (true, false) => {
            let lw = clamp_width(interior, left_min, left_nat);
            (lw, 0)
        }
        (false, true) => {
            let rw = clamp_width(interior, right_min, right_nat);
            (0, rw)
        }
        (false, false) => (0, 0),
    };

    LinearAllocation {
        left_x: 0,
        left_width,
        right_x: interior - right_width,
        right_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_width_natural_fits() {
        assert_eq!(clamp_width(100, 50, 80), 80);
    }

    #[test]
    fn test_clamp_width_natural_too_large() {
        assert_eq!(clamp_width(60, 50, 80), 60);
    }

    #[test]
    fn test_clamp_width_available_less_than_min() {
        // When available < min, we still use available (can't magically create space)
        assert_eq!(clamp_width(30, 50, 80), 30);
    }

    #[test]
    fn test_clamp_width_no_space() {
        assert_eq!(clamp_width(0, 50, 80), 0);
        assert_eq!(clamp_width(-10, 50, 80), 0);
    }

    #[test]
    fn test_clamp_width_exact_fit() {
        assert_eq!(clamp_width(80, 50, 80), 80);
    }

    #[test]
    fn test_center_priority_center_anchored() {
        // With 400px interior, center should be at 150-250 (centered 100px widget)
        let alloc = compute_center_priority_allocation(
            400,
            8,
            None,
            false,
            SectionSizes {
                min: 50,
                natural: 100,
            },
            None,
            false,
        );

        assert_eq!(alloc.center_width, 100);
        assert_eq!(alloc.center_x, 150); // (400 - 100) / 2
    }

    #[test]
    fn test_center_priority_with_left_and_right_no_expand() {
        // 400px interior, center 100px, left and right each want 100px
        // Without expand, sections get clamped to natural size
        let alloc = compute_center_priority_allocation(
            400,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            false,
            SectionSizes {
                min: 50,
                natural: 100,
            },
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            false,
        );

        // Center at 150-250
        assert_eq!(alloc.center_width, 100);
        assert_eq!(alloc.center_x, 150);

        // Left gets clamped to natural size (100)
        assert_eq!(alloc.left_width, 100);
        assert_eq!(alloc.left_x, 0);

        // Right gets clamped to natural size (100), positioned at right edge
        assert_eq!(alloc.right_width, 100);
        assert_eq!(alloc.right_x, 300); // 400 - 100
    }

    #[test]
    fn test_center_priority_with_left_and_right_both_expand() {
        // 400px interior, center 100px, left and right each want 100px
        // With expand, sections get full budget
        let alloc = compute_center_priority_allocation(
            400,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            true,
            SectionSizes {
                min: 50,
                natural: 100,
            },
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            true,
        );

        // Center at 150-250
        assert_eq!(alloc.center_width, 100);
        assert_eq!(alloc.center_x, 150);

        // Left gets full budget of 150 - 8 = 142
        assert_eq!(alloc.left_width, 142);
        assert_eq!(alloc.left_x, 0);

        // Right gets full budget of 400 - 250 - 8 = 142
        assert_eq!(alloc.right_width, 142);
        assert_eq!(alloc.right_x, 258); // 400 - 142
    }

    #[test]
    fn test_center_priority_left_expand_only() {
        // Only left section has expander
        let alloc = compute_center_priority_allocation(
            400,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            true, // left expands
            SectionSizes {
                min: 50,
                natural: 100,
            },
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            false, // right does not expand
        );

        // Left gets full budget
        assert_eq!(alloc.left_width, 142);

        // Right gets clamped to natural
        assert_eq!(alloc.right_width, 100);
        assert_eq!(alloc.right_x, 300); // 400 - 100
    }

    #[test]
    fn test_center_priority_constrained() {
        // 200px interior, center wants 100px, left wants 80px, right wants 80px
        // Budget for each side: (200 - 100) / 2 - 8 = 42px
        // Without expand, sections are clamped (42 < 80, so they get 42)
        let alloc = compute_center_priority_allocation(
            200,
            8,
            Some(SectionSizes {
                min: 30,
                natural: 80,
            }),
            false,
            SectionSizes {
                min: 50,
                natural: 100,
            },
            Some(SectionSizes {
                min: 30,
                natural: 80,
            }),
            false,
        );

        // Center should be at 50-150
        assert_eq!(alloc.center_width, 100);
        assert_eq!(alloc.center_x, 50);

        // Left and right get clamped to budget (42px each)
        assert_eq!(alloc.left_width, 42); // budget: 50 - 8 = 42
        assert_eq!(alloc.right_width, 42); // budget: 200 - 150 - 8 = 42
    }

    #[test]
    fn test_center_priority_only_center() {
        let alloc = compute_center_priority_allocation(
            400,
            8,
            None,
            false,
            SectionSizes {
                min: 50,
                natural: 100,
            },
            None,
            false,
        );

        assert_eq!(alloc.center_width, 100);
        assert_eq!(alloc.center_x, 150);
        assert_eq!(alloc.left_width, 0);
        assert_eq!(alloc.right_width, 0);
    }

    #[test]
    fn test_linear_both_fit() {
        // 400px interior, both want 100px, spacing 8px
        // available = 400 - 8 = 392
        // Linear gives left its natural, then right gets remainder
        // But since remainder (292) > right_nat (100), right gets 100
        // and left expands to fill: 392 - 100 = 292
        let alloc = compute_linear_allocation(
            400,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
        );

        // Left expands to fill available space after right gets its natural
        assert_eq!(alloc.left_width, 292);
        assert_eq!(alloc.left_x, 0);
        assert_eq!(alloc.right_width, 100);
        assert_eq!(alloc.right_x, 300); // 400 - 100
    }

    #[test]
    fn test_linear_only_left() {
        let alloc = compute_linear_allocation(
            400,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            None,
        );

        assert_eq!(alloc.left_width, 100);
        assert_eq!(alloc.left_x, 0);
        assert_eq!(alloc.right_width, 0);
    }

    #[test]
    fn test_linear_only_right() {
        let alloc = compute_linear_allocation(
            400,
            8,
            None,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
        );

        assert_eq!(alloc.left_width, 0);
        assert_eq!(alloc.right_width, 100);
        assert_eq!(alloc.right_x, 300);
    }

    #[test]
    fn test_linear_constrained() {
        // Only 150px available, both want 100px with 8px gap
        // Available = 150 - 8 = 142px
        // Left gets natural (100)
        // Right gets max(142 - 100, right_min) = max(42, 50) = 50
        let alloc = compute_linear_allocation(
            150,
            8,
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
            Some(SectionSizes {
                min: 50,
                natural: 100,
            }),
        );

        // Left gets natural (100), right gets its minimum (50)
        assert_eq!(alloc.left_width, 100);
        assert_eq!(alloc.right_width, 50);
    }

    #[test]
    fn test_linear_empty() {
        let alloc = compute_linear_allocation(400, 8, None, None);

        assert_eq!(alloc.left_width, 0);
        assert_eq!(alloc.right_width, 0);
    }
}
