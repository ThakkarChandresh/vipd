//! Keeps `examples/vipd.toml` valid as the config format evolves.

use std::path::Path;

use vipd::config::{Config, ConfigError};

#[test]
fn the_example_config_is_valid_once_its_key_is_replaced() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/vipd.toml");
    match Config::load(&path) {
        Err(ConfigError::Invalid(problems)) => {
            assert!(problems.iter().any(|p| p.contains("auth_key is the example key")), "{problems:?}")
        }
        other => panic!("the example key must be rejected, got {other:?}"),
    }

    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#""a-long-random-shared-secret""#, r#""a-test-key-that-is-long-enough""#);
    let config = Config::from_toml(&text).unwrap();
    assert_eq!(config.node_name, "web-a");
    assert_eq!(config.vips.len(), 1);
    assert_eq!(config.vips[0].prefix, 32, "the README steers VIPs to /32");
    assert_eq!(config.checks[0].weight, -60);
}
