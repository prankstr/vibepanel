use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{DrawingArea, cairo};

const INSET: f64 = 2.0;
const LINE_WIDTH: f64 = 1.5;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // Automatic scaling is part of the shared API for later graph consumers.
pub(crate) enum HistoryScale {
    Fixed { min: f64, max: f64 },
    Automatic { min: f64, headroom: f64 },
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // Dashed series are supported even though the first consumer is solid-only.
pub(crate) enum LineStyle {
    Solid,
    Dashed,
}

#[derive(Debug, Clone)]
pub(crate) struct HistorySeries {
    pub values: Vec<Option<f64>>,
    pub style: LineStyle,
    pub alpha: f64,
}

impl HistorySeries {
    pub(crate) fn solid(values: Vec<Option<f64>>) -> Self {
        Self {
            values,
            style: LineStyle::Solid,
            alpha: 1.0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct HistoryGraph {
    area: DrawingArea,
    series: Rc<RefCell<Vec<HistorySeries>>>,
}

impl HistoryGraph {
    pub(crate) fn new(capacity: usize, height: i32, scale: HistoryScale) -> Self {
        let capacity = capacity.max(2);
        let series = Rc::new(RefCell::new(Vec::new()));
        let area = DrawingArea::new();
        area.set_size_request(-1, height);
        area.set_hexpand(true);
        area.set_can_target(false);

        let draw_series = series.clone();
        area.set_draw_func(move |area, cr, width, height| {
            draw_history(
                area,
                cr,
                width,
                height,
                capacity,
                scale,
                &draw_series.borrow(),
            );
        });

        Self { area, series }
    }

    pub(crate) fn widget(&self) -> &DrawingArea {
        &self.area
    }

    pub(crate) fn set_series(&self, series: Vec<HistorySeries>) {
        *self.series.borrow_mut() = series;
        self.area.queue_draw();
    }
}

fn draw_history(
    area: &DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    capacity: usize,
    scale: HistoryScale,
    series: &[HistorySeries],
) {
    if width < 2 || height < 2 {
        return;
    }

    let (min, max) = scale_bounds(scale, series);
    let width = f64::from(width);
    let height = f64::from(height);
    let plot_width = (width - INSET * 2.0).max(1.0);
    let plot_height = (height - INSET * 2.0).max(1.0);
    let x_step = plot_width / capacity.saturating_sub(1).max(1) as f64;
    let color = area.color();
    let color = (
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
        color.alpha() as f64,
    );

    for series in series {
        let values = if series.values.len() > capacity {
            &series.values[series.values.len() - capacity..]
        } else {
            &series.values
        };
        let first_x = width - INSET - x_step * values.len().saturating_sub(1) as f64;
        draw_series(
            cr,
            values,
            first_x,
            x_step,
            plot_height,
            min,
            max,
            color,
            series,
        );
    }
}

fn scale_bounds(scale: HistoryScale, series: &[HistorySeries]) -> (f64, f64) {
    match scale {
        HistoryScale::Fixed { min, max } if min.is_finite() && max.is_finite() && max > min => {
            (min, max)
        }
        HistoryScale::Fixed { min, .. } => (min, min + 1.0),
        HistoryScale::Automatic { min, headroom } => {
            let maximum = series
                .iter()
                .flat_map(|series| series.values.iter().flatten())
                .copied()
                .filter(|value| value.is_finite())
                .fold(min, f64::max);
            let max = min + (maximum - min) * (1.0 + headroom.max(0.0));
            (min, max.max(min + 1.0))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_series(
    cr: &cairo::Context,
    values: &[Option<f64>],
    first_x: f64,
    x_step: f64,
    plot_height: f64,
    min: f64,
    max: f64,
    color: (f64, f64, f64, f64),
    series: &HistorySeries,
) {
    let mut run = Vec::new();
    for (index, value) in values
        .iter()
        .copied()
        .chain(std::iter::once(None))
        .enumerate()
    {
        if let Some(value) = value.filter(|value| value.is_finite()) {
            let x = first_x + x_step * index as f64;
            let normalized = ((value.clamp(min, max) - min) / (max - min)).clamp(0.0, 1.0);
            run.push((x, INSET + plot_height * (1.0 - normalized)));
            continue;
        }

        if run.len() >= 2 {
            cr.move_to(run[0].0, run[0].1);
            for &(x, y) in run.iter().skip(1) {
                cr.line_to(x, y);
            }
            cr.set_source_rgba(
                color.0,
                color.1,
                color.2,
                series.alpha.clamp(0.0, 1.0) * color.3,
            );
            cr.set_line_width(LINE_WIDTH);
            match series.style {
                LineStyle::Solid => cr.set_dash(&[], 0.0),
                LineStyle::Dashed => cr.set_dash(&[4.0, 3.0], 0.0),
            }
            let _ = cr.stroke();
        }
        run.clear();
    }
    cr.set_dash(&[], 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_scale_is_shared_across_series() {
        let series = vec![
            HistorySeries::solid(vec![Some(10.0)]),
            HistorySeries {
                values: vec![Some(20.0), None],
                style: LineStyle::Dashed,
                alpha: 0.5,
            },
        ];
        assert_eq!(
            scale_bounds(
                HistoryScale::Automatic {
                    min: 0.0,
                    headroom: 0.1
                },
                &series
            ),
            (0.0, 22.0)
        );
    }
}
