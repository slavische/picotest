mod common;

use common::{run_cargo_test_in_plugin_workspace, LineMatcher};
use std::fs;
use std::path::PathBuf;

const CRATE_DIR: &str = "../tmp/test_standalone";
const TEST_SOURCE_FILE_PATH: &str = asset!("picotest_standalone_tests.rs");

/// Creates a crate which is not a plugin and uses `#[picotest_standalone]`.
fn create_standalone_crate() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CRATE_DIR);
    if crate_dir.exists() {
        fs::remove_dir_all(&crate_dir).expect("Failed to remove test crate directory");
    }
    fs::create_dir_all(crate_dir.join("src")).expect("Failed to create test crate directory");
    let crate_dir = fs::canonicalize(&crate_dir).unwrap();

    let manifest = format!(
        r#"[package]
name = "test-standalone"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "lib"]

[dependencies]
picotest = {{ path = "{}" }}

# Not a member of the picotest workspace.
[workspace]
"#,
        env!("CARGO_MANIFEST_DIR")
    );
    fs::write(crate_dir.join("Cargo.toml"), manifest).unwrap();
    fs::copy(TEST_SOURCE_FILE_PATH, crate_dir.join("src/lib.rs"))
        .expect("Failed to copy test sources to test crate");

    assert!(!crate_dir.join("topology.toml").exists());
    crate_dir
}

#[test]
fn run_standalone_tests_outside_plugin() {
    let crate_dir = create_standalone_crate();

    let mut line_matcher = LineMatcher::new("Hello from standalone_should_success");
    let exit_status =
        run_cargo_test_in_plugin_workspace(&crate_dir, "should_success", &mut line_matcher);
    assert!(
        exit_status.success(),
        "tests are supposed to finish successfully"
    );
    assert!(line_matcher.has_matched());

    let mut line_matcher = LineMatcher::new("Hello from standalone_should_fail");
    let exit_status =
        run_cargo_test_in_plugin_workspace(&crate_dir, "should_fail", &mut line_matcher);
    assert!(
        !exit_status.success(),
        "tests are supposed to finish with failure"
    );
    assert!(line_matcher.has_matched());
}
