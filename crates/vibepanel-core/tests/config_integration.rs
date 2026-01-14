//! Integration tests for config parsing against the real config.toml.

use std::path::PathBuf;
use vibepanel_core::Config;

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
fn test_load_real_config() {
    let config_path = project_root().join("config.toml");

    let config = Config::load(&config_path).expect("Failed to load config.toml");

    // Verify config loads and has expected structure
    // (specific values may change, so we test for validity rather than exact values)
    assert!(config.bar.size > 0, "Bar size should be positive");

    // Check widgets are loaded
    assert!(!config.widgets.left.is_empty(), "Expected left widgets");
    assert!(!config.widgets.right.is_empty(), "Expected right widgets");

    // Verify workspace config has valid backend
    assert!(
        ["auto", "mango", "hyprland", "niri"].contains(&config.workspace.backend.as_str()),
        "Workspace backend should be valid"
    );

    // Verify theme config has valid mode
    assert!(
        ["auto", "dark", "light"].contains(&config.theme.mode.as_str()),
        "Theme mode should be valid"
    );
}

#[test]
fn test_real_config_validates() {
    let config_path = project_root().join("config.toml");
    let config = Config::load(&config_path).unwrap();

    // The real config should pass validation
    config.validate().expect("Real config.toml should be valid");
}

#[test]
fn test_widget_names() {
    let config_path = project_root().join("config.toml");
    let config = Config::load(&config_path).unwrap();

    // Check known widget names (handles both single widgets and groups)
    let left_names: Vec<String> = config
        .widgets
        .left
        .iter()
        .flat_map(|w| w.display_names())
        .collect();
    let right_names: Vec<String> = config
        .widgets
        .right
        .iter()
        .flat_map(|w| w.display_names())
        .collect();

    assert!(
        left_names.iter().any(|n| n.contains("workspace")),
        "Expected workspace widget in left"
    );
    assert!(
        left_names.iter().any(|n| n.contains("window_title")),
        "Expected window_title widget in left"
    );

    assert!(
        right_names.iter().any(|n| n.contains("clock")),
        "Expected clock widget in right"
    );
    assert!(
        right_names.iter().any(|n| n.contains("battery")),
        "Expected battery widget in right"
    );
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
    assert!(summary.contains("Workspace:"));
    assert!(summary.contains("OSD:"));

    // Verify summary contains size (a stable value)
    assert!(summary.contains("size:"), "Summary should show bar size");
}

#[test]
fn test_find_and_load_with_explicit_path() {
    let config_path = project_root().join("config.toml");

    let result = Config::find_and_load(Some(&config_path)).unwrap();

    assert!(!result.used_defaults);
    assert!(result.source.is_some());
    assert_eq!(result.source.unwrap(), config_path);

    // Config should be valid (don't assert specific values that may change)
    result
        .config
        .validate()
        .expect("Loaded config should be valid");
}

#[test]
fn test_find_and_load_explicit_missing_fails() {
    let missing_path = PathBuf::from("/nonexistent/config.toml");

    // Explicit path that doesn't exist should fail (no fallback)
    let result = Config::find_and_load(Some(&missing_path));
    assert!(result.is_err());
}

#[test]
fn test_find_and_load_no_explicit_uses_search_chain() {
    // When no explicit path is given, should search XDG chain
    // In test environment, this may find ./config.toml or use defaults
    let result = Config::find_and_load(None).unwrap();

    // Config should be valid regardless of source
    result.config.validate().expect("Config should be valid");
}

#[test]
fn test_broken_config_returns_error_not_defaults() {
    use std::io::Write;

    // Create a temp directory and broken config file
    let temp_dir = std::env::temp_dir().join("vibepanel_test_broken_config");
    let _ = std::fs::remove_dir_all(&temp_dir); // Clean up any previous run
    std::fs::create_dir_all(&temp_dir).unwrap();

    let broken_config_path = temp_dir.join("config.toml");
    let mut file = std::fs::File::create(&broken_config_path).unwrap();
    writeln!(file, "this is not valid toml {{{{").unwrap();
    drop(file);

    // Loading the broken config directly should fail
    let result = Config::load(&broken_config_path);
    assert!(result.is_err(), "Broken config should fail to load");

    // Clean up
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[test]
fn test_default_config_toml_parses_without_error() {
    // The embedded DEFAULT_CONFIG_TOML should always parse successfully
    let config =
        Config::from_default_toml().expect("DEFAULT_CONFIG_TOML should parse without error");

    // And it should validate
    config
        .validate()
        .expect("DEFAULT_CONFIG_TOML should pass validation");
}

#[test]
fn test_validation_rejects_invalid_theme_mode() {
    let toml = r#"
        [theme]
        mode = "ultra_dark"
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    let result = config.validate();

    assert!(result.is_err(), "Invalid theme.mode should fail validation");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("theme.mode"),
        "Error should mention theme.mode"
    );
}

#[test]
fn test_validation_rejects_invalid_workspace_backend() {
    let toml = r#"
        [workspace]
        backend = "gnome_shell"
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    let result = config.validate();

    assert!(
        result.is_err(),
        "Invalid workspace.backend should fail validation"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("workspace.backend"),
        "Error should mention workspace.backend"
    );
}

#[test]
fn test_validation_rejects_invalid_osd_position() {
    let toml = r#"
        [osd]
        position = "middle"
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    let result = config.validate();

    assert!(
        result.is_err(),
        "Invalid osd.position should fail validation"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("osd.position"),
        "Error should mention osd.position"
    );
}

#[test]
fn test_validation_rejects_notch_enabled_with_center_widgets() {
    // When notch_enabled=true, using widgets.center should be rejected
    let toml = r#"
        [bar]
        notch_enabled = true
        
        [widgets]
        center = ["clock"]
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    let result = config.validate();

    assert!(
        result.is_err(),
        "notch_enabled=true with widgets.center should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("widgets.center"),
        "Error should mention widgets.center"
    );
    assert!(
        err.contains("notch_enabled"),
        "Error should mention notch_enabled"
    );
}

#[test]
fn test_validation_accepts_valid_enum_values() {
    // Test all valid enum combinations
    let toml = r#"
        [bar]
        notch_enabled = false
        
        [theme]
        mode = "dark"
        
        [workspace]
        backend = "hyprland"
        
        [osd]
        position = "bottom"
        
        [widgets]
        center = ["clock"]
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    config
        .validate()
        .expect("Valid config should pass validation");

    // Also test other valid values (notch mode with left/right sections)
    let toml2 = r#"
        [bar]
        notch_enabled = true
        
        [theme]
        mode = "light"
        
        [workspace]
        backend = "niri"
        
        [osd]
        position = "top"
        
        [widgets]
        left = ["clock"]
        right = ["battery"]
    "#;

    let config2: Config = toml::from_str(toml2).unwrap();
    config2
        .validate()
        .expect("Valid config should pass validation");
}

#[test]
fn test_validation_collects_multiple_errors() {
    // Multiple invalid values should all be reported
    let toml = r#"
        [bar]
        size = 0
        
        [theme]
        mode = "bad_mode"
        
        [osd]
        timeout_ms = 0
    "#;

    let config: Config = toml::from_str(toml).unwrap();
    let result = config.validate();

    assert!(result.is_err(), "Multiple invalid values should fail");
    let err = result.unwrap_err().to_string();

    // All errors should be present
    assert!(err.contains("bar.size"), "Should report bar.size error");
    assert!(err.contains("theme.mode"), "Should report theme.mode error");
    assert!(
        err.contains("osd.timeout_ms"),
        "Should report osd.timeout_ms error"
    );
}
