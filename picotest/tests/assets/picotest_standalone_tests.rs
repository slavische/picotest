//! Tests for #\[picotest_standalone\] macro.
//!
//! They are expected to be executed inside a crate which is not
//! a plugin: no `topology.toml`, no manifest, no migrations.

pub mod should_success {
    #[picotest::picotest_standalone]
    fn a_test_should_success() {
        println!("Hello from standalone_should_success");
    }

    #[picotest::picotest_standalone]
    fn b_test_shares_instance_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // The cdylib is loaded once into the shared instance, so its
        // statics live across tests.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

pub mod should_fail {
    #[picotest::picotest_standalone]
    fn test_should_fail() {
        panic!("Hello from standalone_should_fail");
    }
}
