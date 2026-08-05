//! The shipped example configuration must stay loadable.
//!
//! Unknown keys are now a hard error, so a setting renamed in code but not
//! in `yanuget.example.toml` (or the reverse) breaks every operator who
//! starts from that file. This catches the drift at test time.

#[test]
fn shipped_example_config_parses() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/yanuget.example.toml"))
            .expect("example config is present");
    let config: yanuget::config::Config = toml::from_str(&text).expect("example config parses");

    // Spot-check that it really deserialized rather than falling back to
    // defaults for everything.
    assert!(!config.trusted_proxies.is_empty());
    assert!(config.rate_limit.enabled);
}
