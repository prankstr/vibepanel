//! Debug mock for simulating mobile/cellular state without real hardware.
//!
//! **Opt-in**: set `VIBEPANEL_MOCK_MOBILE=1` in the environment and create
//! `/tmp/vibepanel-debug-mobile` with a state string. The panel polls the file
//! every ~2 seconds and reacts to content changes. Without the env var, debug
//! builds behave identically to release builds.
//!
//! # Supported states
//!
//! | File content   | Description                                      |
//! |----------------|--------------------------------------------------|
//! | `disabled`     | Modem present, WWAN disabled                     |
//! | `enabled`      | WWAN enabled, not registered on a network        |
//! | `registered`   | Registered on network, not connected             |
//! | `connecting`   | Connection in progress                           |
//! | `connected`    | Fully connected                                  |
//!
//! # Optional key=value overrides
//!
//! Append lines after the state to customise the mock:
//! ```text
//! connected
//! signal=85
//! operator=MyCarrier
//! tech=5g
//! ```
//!
//! Supported keys: `signal` (0-100), `operator` (string),
//! `tech` (2g|edge|3g|hspa|hspa+|lte|lte+|5g).
//!
//! # Quick usage
//!
//! ```sh
//! export VIBEPANEL_MOCK_MOBILE=1                        # enable mock (once per session)
//! echo "connected" > /tmp/vibepanel-debug-mobile    # simulate connected modem
//! echo "disabled"  > /tmp/vibepanel-debug-mobile     # simulate disabled modem
//! rm /tmp/vibepanel-debug-mobile                     # back to normal
//! ```

use std::cell::RefCell;
use std::path::Path;

use tracing::debug;

use super::{
    DEBUG_MOBILE_MOCK_FILE, MM_ACCESS_TECH_EDGE, MM_ACCESS_TECH_GPRS, MM_ACCESS_TECH_GSM,
    MM_ACCESS_TECH_HSDPA, MM_ACCESS_TECH_HSPA_PLUS, MM_ACCESS_TECH_HSUPA, MM_ACCESS_TECH_LTE,
    MM_ACCESS_TECH_LTE_CAT_M, MM_ACCESS_TECH_NR5G, MM_ACCESS_TECH_UMTS, NmUpdate,
    mobile::access_technology_label, send_nm_update,
};

/// Parsed state from the debug mock file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockMobileState {
    pub state: MockModemState,
    pub signal: u32,
    pub operator: String,
    pub tech: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockModemState {
    Disabled,
    Enabled,
    Registered,
    Connecting,
    Connected,
}

impl MockModemState {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "disabled" => Some(Self::Disabled),
            "enabled" => Some(Self::Enabled),
            "registered" => Some(Self::Registered),
            "connecting" => Some(Self::Connecting),
            "connected" => Some(Self::Connected),
            _ => None,
        }
    }

    /// Whether WWAN/modem is considered enabled in this state.
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether a mobile data connection is active.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Whether the modem is currently connecting (activating).
    pub fn is_connecting(self) -> bool {
        matches!(self, Self::Connecting)
    }

    /// Whether the modem is registered on a network (has operator/signal).
    pub fn is_registered(self) -> bool {
        matches!(self, Self::Registered | Self::Connecting | Self::Connected)
    }
}

/// Check whether mock mobile simulation is enabled.
///
/// Requires **both** conditions:
/// 1. The `VIBEPANEL_MOCK_MOBILE=1` environment variable is set.
/// 2. The debug mock file (`/tmp/vibepanel-debug-mobile`) exists.
///
/// The env-var gate ensures that debug builds don't unconditionally poll the
/// filesystem every 2 seconds — the mock is opt-in even in development.
pub fn is_enabled() -> bool {
    std::env::var("VIBEPANEL_MOCK_MOBILE")
        .map(|v| v == "1")
        .unwrap_or(false)
        && Path::new(DEBUG_MOBILE_MOCK_FILE).exists()
}

/// Parse the debug mock file. Returns `None` if the file doesn't exist or
/// doesn't contain a recognised state on the first line.
pub fn read_state() -> Option<MockMobileState> {
    let content = std::fs::read_to_string(DEBUG_MOBILE_MOCK_FILE).ok()?;
    let mut lines = content.lines().map(str::trim).filter(|l| !l.is_empty());

    let state_str = lines.next()?;
    let state = MockModemState::from_str(state_str)?;

    // Defaults.
    let mut signal: u32 = 75;
    let mut operator = "Mock Carrier".to_string();
    let mut tech_bits: u32 = MM_ACCESS_TECH_LTE;

    // Parse optional key=value overrides.
    for line in lines {
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "signal" => {
                    if let Ok(v) = value.trim().parse::<u32>() {
                        signal = v.min(100);
                    }
                }
                "operator" => {
                    operator = value.trim().to_string();
                }
                "tech" => {
                    tech_bits = parse_tech(value.trim());
                }
                _ => {}
            }
        }
    }

    let tech_label = access_technology_label(tech_bits).to_string();

    Some(MockMobileState {
        state,
        signal,
        operator,
        tech: tech_label,
    })
}

fn parse_tech(s: &str) -> u32 {
    let hspa = MM_ACCESS_TECH_HSDPA | MM_ACCESS_TECH_HSUPA;
    match s.to_ascii_lowercase().as_str() {
        "2g" => MM_ACCESS_TECH_GSM | MM_ACCESS_TECH_GPRS,
        "edge" => MM_ACCESS_TECH_EDGE,
        "3g" => MM_ACCESS_TECH_UMTS,
        "hspa" => hspa,
        "hspa+" => MM_ACCESS_TECH_HSPA_PLUS,
        "lte" => MM_ACCESS_TECH_LTE,
        "lte+" => MM_ACCESS_TECH_LTE_CAT_M,
        "5g" => MM_ACCESS_TECH_NR5G,
        _ => MM_ACCESS_TECH_LTE,
    }
}

/// Send mock mobile state as network updates, mimicking what the real
/// `fetch_mobile_device_info` would produce.
pub fn send_mock_updates(mock: &MockMobileState) {
    debug!("Using mock mobile state: {:?}", mock.state);

    // Always report modem device presence.
    send_nm_update(NmUpdate::ModemDeviceExists);

    let (operator, tech, signal) = if mock.state.is_registered() {
        (
            Some(mock.operator.clone()),
            Some(mock.tech.clone()),
            Some(mock.signal),
        )
    } else {
        (None, None, None)
    };

    send_nm_update(NmUpdate::MobileDeviceInfo {
        conn_name: if mock.state.is_registered() {
            Some("Mock Mobile".to_string())
        } else {
            None
        },
        operator_name: operator,
        access_technology: tech,
        signal_quality: signal,
        active: mock.state.is_active(),
        connecting: mock.state.is_connecting(),
        supported: true,
        has_modem: true,
    });

    send_nm_update(NmUpdate::MobileEnabled(mock.state.is_enabled()));
}

thread_local! {
    static LAST_MOCK_STATE: RefCell<Option<MockMobileState>> = const { RefCell::new(None) };
}

/// Start a polling timer that re-reads the mock file every ~2 seconds.
/// When the state changes (or the file is removed), it sends appropriate
/// network updates. Stops polling when the file is deleted.
pub fn start_polling() {
    gtk4::glib::timeout_add_local(std::time::Duration::from_secs(2), || {
        match read_state() {
            Some(mock) => {
                let changed = LAST_MOCK_STATE.with(|cell| {
                    let prev = cell.borrow();
                    prev.as_ref() != Some(&mock)
                });
                if changed {
                    send_mock_updates(&mock);
                    LAST_MOCK_STATE.with(|cell| {
                        *cell.borrow_mut() = Some(mock);
                    });
                }
                gtk4::glib::ControlFlow::Continue
            }
            None => {
                // File removed or invalid — stop polling.
                debug!("Mock mobile file removed, stopping poll");
                LAST_MOCK_STATE.with(|cell| {
                    *cell.borrow_mut() = None;
                });
                gtk4::glib::ControlFlow::Break
            }
        }
    });
}

/// Walk through a sequence of mock states with delays between them,
/// simulating realistic modem transition times.
///
/// Each entry is `(state_str, delay_ms_before_next)`. The first state is
/// applied immediately, then each subsequent state is applied after the
/// preceding delay elapses.
pub fn transition_through_states(steps: &'static [(&'static str, u64)]) {
    if steps.is_empty() {
        return;
    }

    // Apply the first state immediately.
    let _ = std::fs::write(DEBUG_MOBILE_MOCK_FILE, steps[0].0);
    if let Some(mock) = read_state() {
        send_mock_updates(&mock);
    }

    // Schedule remaining states with cumulative delays.
    let mut cumulative_ms: u64 = steps[0].1;
    for &(state_str, delay_after) in &steps[1..] {
        let delay = cumulative_ms;
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
            let _ = std::fs::write(DEBUG_MOBILE_MOCK_FILE, state_str);
            if let Some(mock) = read_state() {
                send_mock_updates(&mock);
            }
        });
        cumulative_ms += delay_after;
    }
}
