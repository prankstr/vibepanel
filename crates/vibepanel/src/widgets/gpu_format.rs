//! Shared formatting for GPU metrics displayed by widgets.

use tr::tr;

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
        (Some(used), None) => Some(tr!("{used} used", used = format_bytes_long(used))),
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
    fn formats_gpu_metrics() {
        type Formatter = fn(&GpuDeviceSnapshot) -> Option<String>;
        let available = GpuDeviceSnapshot {
            vram_used: Some(4 * 1024 * 1024 * 1024),
            vram_total: Some(8 * 1024 * 1024 * 1024),
            power_watts: Some(118.6),
            clock_mhz: Some(2430),
            ..Default::default()
        };
        let with_limits = GpuDeviceSnapshot {
            power_watts: Some(120.0),
            power_limit_watts: Some(300.0),
            clock_mhz: Some(1500),
            max_clock_mhz: Some(2500),
            ..Default::default()
        };
        let unavailable = GpuDeviceSnapshot::default();

        let cases: [(&str, Formatter, &GpuDeviceSnapshot, Option<&str>); 8] = [
            ("vram", vram, &available, Some("4.0 GB / 8.0 GB")),
            ("power", power, &available, Some("118.6 W")),
            ("clock", clock, &available, Some("2430 MHz")),
            (
                "power with limit",
                power,
                &with_limits,
                Some("120.0 / 300 W"),
            ),
            (
                "clock with limit",
                clock,
                &with_limits,
                Some("1500 / 2500 MHz"),
            ),
            ("vram unavailable", vram, &unavailable, None),
            ("power unavailable", power, &unavailable, None),
            ("clock unavailable", clock, &unavailable, None),
        ];

        for (name, format, snapshot, expected) in cases {
            assert_eq!(format(snapshot).as_deref(), expected, "{name}");
        }
    }

    #[test]
    fn formats_device_titles_with_optional_index_and_name() {
        let cases = [
            (
                "named with index",
                Some("NVIDIA GeForce RTX 4090"),
                true,
                "GPU 3: NVIDIA GeForce RTX 4090",
            ),
            (
                "named without index",
                Some("NVIDIA GeForce RTX 4090"),
                false,
                "GPU: NVIDIA GeForce RTX 4090",
            ),
            ("unnamed with index", None, true, "GPU 3"),
            ("unnamed without index", None, false, "GPU"),
        ];

        for (name, device_name, show_index, expected) in cases {
            let snapshot = GpuDeviceSnapshot {
                device_index: 3,
                device_name: device_name.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(device_title(&snapshot, show_index), expected, "{name}");
        }
    }
}
