//! Built-in test framework for Lua code running in mlua.
//!
//! Embeds a forked copy of [lust](https://github.com/bjornbytes/lust)
//! (MIT, single-file, zero-dependency) and provides Rust APIs for
//! executing tests and collecting structured results.
//!
//! # Quick start
//!
//! ```rust
//! use mlua_probe_core::testing;
//!
//! let summary = testing::framework::run_tests(r#"
//!     local describe, it, expect = lust.describe, lust.it, lust.expect
//!     describe('example', function()
//!         it('works', function()
//!             expect(1 + 1).to.equal(2)
//!         end)
//!     end)
//! "#, "@test.lua").unwrap();
//!
//! assert_eq!(summary.passed, 1);
//! ```
//!
//! # Integration with the debug engine
//!
//! The testing module is independent of [`crate::DebugSession`].
//! To debug a failing test, pass the same Lua code to
//! [`crate::DebugSession`] via `debug_launch` — breakpoints and
//! stepping work inside test code as usual.
//!
//! # Test doubles
//!
//! The [`doubles`] module provides Rust-backed spy, stub, and mock
//! objects exposed as Lua `UserData`.  These are registered
//! automatically by [`framework::run_tests`] as the `test_doubles`
//! global table.
//!
//! # Granular control
//!
//! For advanced use cases (e.g. registering lust on a pre-existing
//! VM or running multiple test suites in sequence), use
//! [`framework::register`] and [`framework::collect_results`]
//! directly instead of [`framework::run_tests`].

pub mod doubles;
pub mod framework;
mod types;

pub use types::{TestResult, TestSummary};
