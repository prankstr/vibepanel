//! Shared formatting for GPU metrics displayed by widgets.

use crate::services::gpu::GpuDeviceSnapshot;
use crate::services::system::format_bytes_long;

pub(crate) fn device_title(snapshot: &GpuDeviceSnapshot, show_index: bool) -> String {
    match (show_index, snapshot.device_name.as_deref()) {
        (true, Some(name)) => format!("GPU {}: {name}", snapshot.device_index),
        (true, None) => format!("GPU {}", snapshot.device_index),
        (false, Some(name)) => format!("GPU: {name}"),
        (false, None) => "GPU".to_string(),
    }
}

pub(crate) fn vram(snapshot: &GpuDeviceSnapshot) -> Option<String> {
    match (snapshot.vram_used, snapshot.vram_total) {
        (Some(used), Some(total)) => Some(format!(
            "{} / {}",
            format_bytes_long(used),
            format_bytes_long(total)
        )),
        (Some(used), None) => Some(format!("{} used", format_bytes_long(used))),
        _ => None,
    }
}

pub(crate) fn power(snapshot: &GpuDeviceSnapshot) -> Option<String> {
    match (snapshot.power_watts, snapshot.power_limit_watts) {
        (Some(value), Some(limit)) => Some(format!("{value:.1} / {limit:.0} W")),
        (Some(value), None) => Some(format!("{value:.1} W")),
        _ => None,
    }
}

pub(crate) fn clock(snapshot: &GpuDeviceSnapshot) -> Option<String> {
    match (snapshot.clock_mhz, snapshot.max_clock_mhz) {
        (Some(value), Some(limit)) => Some(format!("{value} / {limit} MHz")),
        (Some(value), None) => Some(format!("{value} MHz")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{clock, device_title, power, vram};
    use crate::services::gpu::GpuDeviceSnapshot;

    #[test]
    fn formats_available_gpu_metrics() {
        let snapshot = GpuDeviceSnapshot {
            vram_used: Some(4 * 1024 * 1024 * 1024),
            vram_total: Some(8 * 1024 * 1024 * 1024),
            power_watts: Some(118.6),
            clock_mhz: Some(2430),
            ..Default::default()
        };

        assert_eq!(vram(&snapshot).as_deref(), Some("4.0 GB / 8.0 GB"));
        assert_eq!(power(&snapshot).as_deref(), Some("118.6 W"));
        assert_eq!(clock(&snapshot).as_deref(), Some("2430 MHz"));
    }

    #[test]
    fn formats_gpu_metrics_with_limits() {
        let snapshot = GpuDeviceSnapshot {
            power_watts: Some(120.0),
            power_limit_watts: Some(300.0),
            clock_mhz: Some(1500),
            max_clock_mhz: Some(2500),
            ..Default::default()
        };

        assert_eq!(power(&snapshot).as_deref(), Some("120.0 / 300 W"));
        assert_eq!(clock(&snapshot).as_deref(), Some("1500 / 2500 MHz"));
    }

    #[test]
    fn omits_unavailable_gpu_metrics() {
        let snapshot = GpuDeviceSnapshot::default();

        assert_eq!(vram(&snapshot), None);
        assert_eq!(power(&snapshot), None);
        assert_eq!(clock(&snapshot), None);
    }

    #[test]
    fn formats_device_titles_with_optional_index_and_name() {
        let named = GpuDeviceSnapshot {
            device_index: 3,
            device_name: Some("NVIDIA GeForce RTX 4090".to_string()),
            ..Default::default()
        };
        let unnamed = GpuDeviceSnapshot {
            device_index: 3,
            ..Default::default()
        };

        assert_eq!(device_title(&named, true), "GPU 3: NVIDIA GeForce RTX 4090");
        assert_eq!(device_title(&named, false), "GPU: NVIDIA GeForce RTX 4090");
        assert_eq!(device_title(&unnamed, true), "GPU 3");
        assert_eq!(device_title(&unnamed, false), "GPU");
    }
}
