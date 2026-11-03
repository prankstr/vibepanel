//! Where a bar tooltip goes, kept free of GTK so it can be unit tested.

use vibepanel_core::config::BarPosition;

/// Gap between the cursor and a tooltip on a top or bottom bar.
const CURSOR_GAP: i32 = 10;

/// Gap between a side bar and its tooltips. Smaller than `CURSOR_GAP`
/// because the tooltip already starts past the bar's exclusive zone.
const SIDE_BAR_GAP: i32 = 4;

/// Minimum distance between a tooltip and the monitor edges.
const EDGE_MARGIN: i32 = 8;

/// Layer-shell margins for a tooltip anchored to the bar's monitor edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placement {
    /// Distance from the monitor edge the bar sits on.
    pub inward: i32,
    /// Position along the bar.
    pub along: Along,
}

/// Position along the bar, as a margin from its start (left or top) or its
/// end (right or bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Along {
    Start(i32),
    End(i32),
}

/// Place a tooltip of `size` for a pointer at `pointer`. Only the pointer's
/// position along the bar is used. `bar_offset` is the bar thickness the
/// compositor doesn't already reserve.
pub(super) fn place(
    position: BarPosition,
    monitor: (i32, i32),
    pointer: (i32, i32),
    size: (i32, i32),
    bar_offset: i32,
) -> Placement {
    if position.is_horizontal() {
        let (monitor_width, _) = monitor;
        let (x, _) = pointer;
        let (width, _) = size;

        // Right of the cursor, or left of it when that would run off screen.
        let start = x + CURSOR_GAP;
        let along = if start + width > monitor_width - EDGE_MARGIN {
            Along::End((monitor_width - x + CURSOR_GAP).max(EDGE_MARGIN))
        } else {
            Along::Start(start.max(EDGE_MARGIN))
        };
        Placement {
            inward: bar_offset,
            along,
        }
    } else {
        let (_, monitor_height) = monitor;
        let (_, y) = pointer;
        let (_, height) = size;

        let max_top = (monitor_height - height - EDGE_MARGIN).max(EDGE_MARGIN);
        let top = (y - height / 2).clamp(EDGE_MARGIN, max_top);
        Placement {
            inward: SIDE_BAR_GAP + bar_offset,
            along: Along::Start(top),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: (i32, i32) = (1000, 800);
    const SIZE: (i32, i32) = (100, 30);

    #[test]
    fn horizontal_bar_places_tooltip_right_of_cursor() {
        let placement = place(BarPosition::Top, MONITOR, (200, 10), SIZE, 32);
        assert_eq!(placement.along, Along::Start(210));
        assert_eq!(placement.inward, 32);
    }

    #[test]
    fn horizontal_bar_flips_left_of_cursor_near_right_edge() {
        let placement = place(BarPosition::Bottom, MONITOR, (950, 790), SIZE, 0);
        assert_eq!(placement.along, Along::End(60));
    }

    #[test]
    fn side_bar_centres_tooltip_on_cursor() {
        let placement = place(BarPosition::Left, MONITOR, (10, 400), SIZE, 32);
        assert_eq!(placement.along, Along::Start(385));
        assert_eq!(placement.inward, SIDE_BAR_GAP + 32);
    }

    #[test]
    fn side_bar_clamps_to_monitor_edges() {
        let top = place(BarPosition::Right, MONITOR, (990, 5), SIZE, 0);
        assert_eq!(top.along, Along::Start(EDGE_MARGIN));
        let bottom = place(BarPosition::Right, MONITOR, (990, 799), SIZE, 0);
        assert_eq!(bottom.along, Along::Start(800 - 30 - EDGE_MARGIN));
    }
}
