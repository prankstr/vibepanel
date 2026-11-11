//! Pure layout math functions for center-priority bar layout.
//!
//! These functions contain no GTK dependencies and can be unit tested directly.

/// Clamp a widget's size to fit within available space.
///
/// Returns:
/// - `natural` if it fits within `available`
/// - Otherwise shrinks toward `min`, but never below 0
/// - If `available <= 0`, returns 0
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
/// right gets priority and left shrinks.
///
/// # Arguments
///
/// * `interior` - Total usable width (after edge margins)
/// * `spacing` - Gap between left and right sections
/// * `left` - Size requirements for left section (None if not present)
/// * `left_expand` - Whether left section should expand to fill available space
/// * `right` - Size requirements for right section (None if not present)
/// * `right_expand` - Whether right section should expand to fill available space
pub fn compute_linear_allocation(
    interior: i32,
    spacing: i32,
    left: Option<SectionSizes>,
    left_expand: bool,
    right: Option<SectionSizes>,
    right_expand: bool,
) -> LinearAllocation {
    let (left_min, left_nat) = left.map_or((0, 0), |s| (s.min, s.natural));
    let (right_min, right_nat) = right.map_or((0, 0), |s| (s.min, s.natural));

    let available = match (left.is_some(), right.is_some()) {
        (true, true) => (interior - spacing).max(0),
        (true, false) | (false, true) => interior.max(0),
        (false, false) => 0,
    };

    let (mut left_width, mut right_width) = match (left.is_some(), right.is_some()) {
        (true, true) => {
            let total_natural = left_nat + right_nat;
            if total_natural <= available {
                (left_nat, right_nat)
            } else {
                let rw = right_nat.min(available);
                let lw = (available - rw).max(0);
                (lw, rw)
            }
        }
        (true, false) => {
            let lw = clamp_width(available, left_min, left_nat);
            (lw, 0)
        }
        (false, true) => {
            let rw = clamp_width(available, right_min, right_nat);
            (0, rw)
        }
        (false, false) => (0, 0),
    };

    let extra = (available - left_width - right_width).max(0);
    match (
        left.is_some() && left_expand,
        right.is_some() && right_expand,
    ) {
        (true, true) => {
            let left_extra = extra / 2;
            left_width += left_extra;
            right_width += extra - left_extra;
        }
        (true, false) => left_width += extra,
        (false, true) => right_width += extra,
        (false, false) => {}
    }

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

    fn sizes(min: i32, natural: i32) -> Option<SectionSizes> {
        Some(SectionSizes { min, natural })
    }

    #[test]
    fn clamp_width_cases() {
        // (name, available, min, natural, expected)
        let cases = [
            ("natural fits", 100, 50, 80, 80),
            ("natural too large", 60, 50, 80, 60),
            // Can't create space that isn't there, even below min.
            ("available below min", 30, 50, 80, 30),
            ("no space", 0, 50, 80, 0),
            ("negative space", -10, 50, 80, 0),
            ("exact fit", 80, 50, 80, 80),
        ];

        for (name, available, min, natural, expected) in cases {
            assert_eq!(
                clamp_width(available, min, natural),
                expected,
                "{name}: clamp_width({available}, {min}, {natural})"
            );
        }
    }

    #[test]
    fn center_priority_allocation_cases() {
        struct Case {
            name: &'static str,
            interior: i32,
            left: Option<SectionSizes>,
            left_expand: bool,
            center: SectionSizes,
            right: Option<SectionSizes>,
            right_expand: bool,
            /// (left_x, left_width, center_x, center_width, right_x, right_width)
            expected: (i32, i32, i32, i32, i32, i32),
        }

        let center = SectionSizes {
            min: 50,
            natural: 100,
        };
        let cases = [
            Case {
                name: "only center is anchored to the true center",
                interior: 400,
                left: None,
                left_expand: false,
                center,
                right: None,
                right_expand: false,
                expected: (0, 0, 150, 100, 400, 0),
            },
            Case {
                name: "left and right clamp to natural without expand",
                interior: 400,
                left: sizes(50, 100),
                left_expand: false,
                center,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 100, 150, 100, 300, 100),
            },
            Case {
                name: "both expand get the full budget",
                interior: 400,
                left: sizes(50, 100),
                left_expand: true,
                center,
                right: sizes(50, 100),
                right_expand: true,
                expected: (0, 142, 150, 100, 258, 142),
            },
            Case {
                name: "only left expands",
                interior: 400,
                left: sizes(50, 100),
                left_expand: true,
                center,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 142, 150, 100, 300, 100),
            },
            Case {
                name: "constrained sides clamp to budget",
                interior: 200,
                left: sizes(30, 80),
                left_expand: false,
                center,
                right: sizes(30, 80),
                right_expand: false,
                expected: (0, 42, 50, 100, 158, 42),
            },
        ];

        for case in cases {
            let alloc = compute_center_priority_allocation(
                case.interior,
                8,
                case.left,
                case.left_expand,
                case.center,
                case.right,
                case.right_expand,
            );
            assert_eq!(
                (
                    alloc.left_x,
                    alloc.left_width,
                    alloc.center_x,
                    alloc.center_width,
                    alloc.right_x,
                    alloc.right_width,
                ),
                case.expected,
                "{}: (left_x, left_width, center_x, center_width, right_x, right_width)",
                case.name
            );
        }
    }

    #[test]
    fn linear_allocation_cases() {
        struct Case {
            name: &'static str,
            interior: i32,
            left: Option<SectionSizes>,
            left_expand: bool,
            right: Option<SectionSizes>,
            right_expand: bool,
            /// (left_x, left_width, right_x, right_width)
            expected: (i32, i32, i32, i32),
        }

        let cases = [
            Case {
                name: "both fit",
                interior: 400,
                left: sizes(50, 100),
                left_expand: false,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 100, 300, 100),
            },
            Case {
                name: "right keeps priority, left truncates (regression)",
                interior: 200,
                left: sizes(50, 300),
                left_expand: false,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 92, 100, 100),
            },
            Case {
                name: "constrained left shrinks",
                interior: 150,
                left: sizes(50, 100),
                left_expand: false,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 42, 50, 100),
            },
            Case {
                name: "only left",
                interior: 400,
                left: sizes(50, 100),
                left_expand: false,
                right: None,
                right_expand: false,
                expected: (0, 100, 400, 0),
            },
            Case {
                name: "only right",
                interior: 400,
                left: None,
                left_expand: false,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 0, 300, 100),
            },
            Case {
                name: "empty",
                interior: 400,
                left: None,
                left_expand: false,
                right: None,
                right_expand: false,
                expected: (0, 0, 400, 0),
            },
            Case {
                name: "right expand fills remaining space",
                interior: 400,
                left: sizes(50, 100),
                left_expand: false,
                right: sizes(0, 0),
                right_expand: true,
                expected: (0, 100, 108, 292),
            },
            Case {
                name: "left expand fills remaining space",
                interior: 400,
                left: sizes(0, 0),
                left_expand: true,
                right: sizes(50, 100),
                right_expand: false,
                expected: (0, 292, 300, 100),
            },
            Case {
                name: "both expand share remaining space",
                interior: 400,
                left: sizes(50, 100),
                left_expand: true,
                right: sizes(50, 100),
                right_expand: true,
                expected: (0, 196, 204, 196),
            },
            Case {
                name: "single expander fills section",
                interior: 400,
                left: None,
                left_expand: false,
                right: sizes(0, 0),
                right_expand: true,
                expected: (0, 0, 0, 400),
            },
        ];

        for case in cases {
            let alloc = compute_linear_allocation(
                case.interior,
                8,
                case.left,
                case.left_expand,
                case.right,
                case.right_expand,
            );
            assert_eq!(
                (
                    alloc.left_x,
                    alloc.left_width,
                    alloc.right_x,
                    alloc.right_width
                ),
                case.expected,
                "{}: (left_x, left_width, right_x, right_width)",
                case.name
            );
        }
    }
}
