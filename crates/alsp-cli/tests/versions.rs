//! A release must change the crate and the plugin manifests together: Claude Code refreshes an
#![allow(non_snake_case)]
//! installed plugin only when its version changes, so a stale manifest strands users on old skills.
use std::path::Path;

fn manifest_version(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    v["version"].as_str().unwrap_or_else(|| panic!("{rel} has no version")).to_string()
}

#[test]
fn DW_V_2_plugin_manifest_versions_match_the_crate() {
    for rel in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
        assert_eq!(manifest_version(rel), env!("CARGO_PKG_VERSION"), "{rel} must match the alsp-cli crate version");
    }
}

#[test]
fn marketplace_entries_carry_no_version_that_could_drift() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.claude-plugin/marketplace.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    for plugin in v["plugins"].as_array().unwrap() {
        if let Some(ver) = plugin.get("version") {
            assert_eq!(ver, env!("CARGO_PKG_VERSION"));
        }
    }
}
