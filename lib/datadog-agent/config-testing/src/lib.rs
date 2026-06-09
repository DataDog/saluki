// This crate's build.rs generates both the test-support config registry annotations AND the
// configuration documentation markdown. Doc generation lives here (not in the config crate)
// because it is a dev/CI concern, not a prod build artifact.
pub mod config_registry;

mod smoke_test;
pub use smoke_test::run_config_smoke_tests;
