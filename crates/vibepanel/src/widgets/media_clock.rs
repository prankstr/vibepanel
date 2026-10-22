//! One timer for the bar's scrolling titles that also redraws the waveform, so both
//! share frames. Without a scrolling title the waveform redraws from cava.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib::{self, SourceId};

use crate::widgets::marquee_label::{MarqueeLabel, TICK_INTERVAL};
use crate::widgets::media_visualizer::BarVisualizer;

pub struct MediaClock {
    labels: Vec<Rc<MarqueeLabel>>,
    visualizer: Option<BarVisualizer>,
    timer: RefCell<Option<SourceId>>,
}

impl MediaClock {
    pub fn new(labels: Vec<Rc<MarqueeLabel>>, visualizer: Option<BarVisualizer>) -> Rc<Self> {
        let clock = Rc::new(Self {
            labels,
            visualizer,
            timer: RefCell::new(None),
        });
        for label in &clock.labels {
            let clock = Rc::downgrade(&clock);
            label.set_external_clock(move || {
                if let Some(clock) = clock.upgrade() {
                    clock.ensure_running();
                }
            });
        }
        clock
    }

    /// Start ticking if not already. The timer stops itself once no title scrolls.
    fn ensure_running(self: &Rc<Self>) {
        if self.timer.borrow().is_some() {
            return;
        }
        self.set_drives_visualizer(true);
        let clock = Rc::downgrade(self);
        let id = glib::timeout_add_local(TICK_INTERVAL, move || {
            let Some(clock) = clock.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if clock.tick() {
                glib::ControlFlow::Continue
            } else {
                clock.timer.borrow_mut().take();
                clock.set_drives_visualizer(false);
                glib::ControlFlow::Break
            }
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn tick(&self) -> bool {
        let mut scrolled = false;
        for label in &self.labels {
            scrolled |= label.advance();
        }
        if scrolled && let Some(visualizer) = &self.visualizer {
            visualizer.redraw();
        }
        scrolled
    }

    fn set_drives_visualizer(&self, driving: bool) {
        if let Some(visualizer) = &self.visualizer {
            visualizer.set_clock_driven(driving);
        }
    }
}

impl Drop for MediaClock {
    fn drop(&mut self) {
        if let Some(id) = self.timer.get_mut().take() {
            id.remove();
        }
        self.set_drives_visualizer(false);
    }
}
