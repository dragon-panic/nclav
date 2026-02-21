use nclav_config::load_enclaves;
use std::path::Path;

#[test]
fn load_valid_fixture() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let enclaves = load_enclaves(&dir).expect("should load without error");
    assert!(!enclaves.is_empty(), "expected at least one enclave");

    let enc = &enclaves[0];
    assert_eq!(enc.id.as_str(), "test-enclave");
    assert_eq!(enc.cloud, Some(nclav_domain::CloudTarget::Local));
}

#[test]
fn load_real_enclaves_fixture() {
    // Use the workspace-level enclaves directory
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../enclaves");
    if !dir.exists() {
        return; // skip if not present
    }
    let enclaves = load_enclaves(&dir).expect("should load without error");
    assert!(!enclaves.is_empty());
}

#[test]
fn missing_dir_returns_error() {
    let dir = Path::new("/nonexistent/path/does/not/exist");
    assert!(load_enclaves(dir).is_err());
}
