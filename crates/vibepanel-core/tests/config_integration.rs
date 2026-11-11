//! Integration tests for config parsing against the real config.toml.

use std::path::PathBuf;
use vibepanel_core::Config;
use vibepanel_core::config::{DEFAULT_BATTERY_CRITICAL_THRESHOLD, DEFAULT_BATTERY_LOW_THRESHOLD};

fn project_root() -> PathBuf {
    // Navigate from crates/vibepanel-core/ up to project root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .unwrap()
        .parent() // vibepanel/
        .unwrap()
        .to_path_buf()
}

#[test]
fn find_and_load_explicit_path_loads_and_validates() {
    // config.toml is also the embedded DEFAULT_CONFIG_TOML
    let config_path = project_root().join("config.toml");

    let result = Config::find_and_load(Some(&config_path)).unwrap();

    assert!(!result.used_defaults);
    assert_eq!(result.source, Some(config_path));

    // Specific values may change, so test for validity rather than exact values
    let config = result.config;
    assert!(config.bar.size > 0, "Bar size should be positive");
    assert!(!config.widgets.left.is_empty(), "Expected left widgets");
    assert!(!config.widgets.right.is_empty(), "Expected right widgets");
    config.validate().expect("Real config.toml should be valid");
}

#[test]
fn test_config_summary() {
    let config_path = project_root().join("config.toml");
    let config = Config::load(&config_path).unwrap();

    let summary = config.summary();

    // Verify summary contains key sections
    assert!(summary.contains("Bar Configuration:"));
    assert!(summary.contains("Widgets:"));
    assert!(summary.contains("Theme:"));
    assert!(summary.contains("Advanced:"));
    assert!(summary.contains("OSD:"));

    // Verify summary contains size (a stable value)
    assert!(summary.contains("size:"), "Summary should show bar size");
}

#[test]
fn test_find_and_load_explicit_missing_fails() {
    let missing_path = PathBuf::from("/nonexistent/config.toml");

    // Explicit path that doesn't exist should fail (no fallback)
    let result = Config::find_and_load(Some(&missing_path));
    assert!(result.is_err());
}

#[test]
fn test_broken_config_returns_error_not_defaults() {
    let temp_dir = std::env::temp_dir().join("vibepanel_test_broken_config");
    let _ = std::fs::remove_dir_all(&temp_dir); // Clean up any previous run
    std::fs::create_dir_all(&temp_dir).unwrap();

    let broken_config_path = temp_dir.join("config.toml");
    std::fs::write(&broken_config_path, "this is not valid toml {{{{\n").unwrap();

    let result = Config::find_and_load(Some(&broken_config_path));
    assert!(result.is_err(), "Broken config should fail to load");

    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[test]
fn battery_alert_config_cases() {
    struct Case {
        name: &'static str,
        toml: &'static str,
        enabled: bool,
        thresholds: Option<(u32, u32)>,
    }
    let cases = [
        Case {
            name: "explicit options",
            toml: r#"
                [widgets]
                right = ["battery"]

                [widgets.battery]
                alerts = true
                low_threshold = 15
                critical_threshold = 5
            "#,
            enabled: true,
            thresholds: Some((15, 5)),
        },
        Case {
            name: "defaults with active widget",
            toml: r#"
                [widgets]
                right = ["battery"]
            "#,
            enabled: true,
            thresholds: Some((
                DEFAULT_BATTERY_LOW_THRESHOLD,
                DEFAULT_BATTERY_CRITICAL_THRESHOLD,
            )),
        },
        Case {
            name: "widget not placed",
            toml: r#"
                [widgets]
                right = ["clock"]

                [widgets.battery]
                alerts = true
            "#,
            enabled: false,
            thresholds: None,
        },
        Case {
            name: "widget disabled",
            toml: r#"
                [widgets]
                right = ["battery"]

                [widgets.battery]
                disabled = true
            "#,
            enabled: false,
            thresholds: None,
        },
    ];
    for case in cases {
        let config: Config = toml::from_str(case.toml).unwrap();
        let alert = config.battery_alert_config();
        assert_eq!(alert.enabled, case.enabled, "{}: enabled", case.name);
        if let Some((low, critical)) = case.thresholds {
            assert_eq!(alert.low_threshold, low, "{}: low_threshold", case.name);
            assert_eq!(
                alert.critical_threshold, critical,
                "{}: critical_threshold",
                case.name
            );
        }
        if let Err(err) = config.validate() {
            panic!("{}: expected valid config: {err}", case.name);
        }
    }
}

#[test]
fn battery_validation_rejects() {
    let cases = [
        (
            "zero low threshold",
            "low_threshold = 0",
            "widgets.battery.low_threshold",
        ),
        (
            "u32 overflow",
            "low_threshold = 4294967396",
            "widgets.battery.low_threshold",
        ),
        (
            "non-bool alerts",
            "alerts = \"yes\"",
            "widgets.battery.alerts",
        ),
        (
            "critical above low",
            "low_threshold = 10\ncritical_threshold = 15",
            "widgets.battery.critical_threshold",
        ),
    ];
    for (name, battery, field) in cases {
        let toml = format!("[widgets]\nright = [\"battery\"]\n\n[widgets.battery]\n{battery}\n");
        let config: Config = toml::from_str(&toml).unwrap();
        let err = match config.validate() {
            Ok(()) => panic!("{name}: expected validation error"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains(field), "{name}: should report {field}: {err}");
    }
}

#[test]
fn audio_overdrive_policy_cases() {
    let temp_dir = std::env::temp_dir().join("vibepanel_test_audio_policy");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    // `None` contents means the file doesn't exist.
    let cases = [
        (
            "explicit true",
            Some("[audio]\nallow_overdrive = true\n"),
            true,
        ),
        (
            "explicit false",
            Some("[audio]\nallow_overdrive = false\n"),
            false,
        ),
        ("missing file", None, false),
        (
            "malformed toml",
            Some("this is not valid toml {{{{\n"),
            false,
        ),
        ("missing audio section", Some("[bar]\nsize = 40\n"), false),
        (
            "wrong type",
            Some("[audio]\nallow_overdrive = \"yes\"\n"),
            false,
        ),
        (
            "ignores unrelated validation errors",
            Some("[audio]\nallow_overdrive = true\n\n[theme]\nmode = \"invalid\"\n"),
            true,
        ),
    ];
    for (i, (name, contents, expected)) in cases.into_iter().enumerate() {
        let path = temp_dir.join(format!("config-{i}.toml"));
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        assert_eq!(
            Config::read_audio_allow_overdrive(Some(&path)),
            expected,
            "{name}"
        );
    }

    std::fs::remove_dir_all(&temp_dir).unwrap();
}
