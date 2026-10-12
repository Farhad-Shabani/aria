//! Black-box acceptance harness.
//!
//! Runs a compiled binary against a scenario catalog. Cases only speak the
//! product's public contract: config file, CLI, exit status, stdout, stderr.
//!
//! Infrastructure failures surface as typed errors; a case whose expectations
//! do not hold panics, which is how the test framework reports a failed case.
//!
//! `baseline` and `stats` spawn nothing. Benches use them to record a run
//! and judge the next one against it.

pub mod artifact;
pub mod baseline;
mod catalog;
mod daemon;
pub mod inputs;
pub mod log;
mod runner;
mod sandbox;
pub mod stats;

pub use catalog::CatalogError;
pub use daemon::{DaemonProcessError, RunningDaemon, ServiceStart, start_service};
pub use runner::{HarnessError, run_catalog};
pub use sandbox::{Sandbox, SandboxError, yaml_string};
