//! Keeps `examples/vipd.toml` valid as the config format evolves.

use std::path::Path;

use vipd::config::Config;

#[test]
fn the_example_config_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/vipd.toml");
    let config = Config::load(&path).unwrap();
    assert_eq!(config.node_name, "web-a");
    assert_eq!(config.vips.len(), 1);
    assert_eq!(config.vips[0].prefix, 32, "the README steers VIPs to /32");
    assert_eq!(config.checks[0].weight, -60);
}
