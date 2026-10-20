//! Black-box acceptance harness for the Aria daemon and CLI.
//!
//! Runs the `aria` binary against a scenario catalog, one test per case, each
//! in a sandbox with its own home. Cases only speak the product's public
//! contract: config file, CLI, exit status, stdout, stderr. The case format is
//! documented beside the catalog, in `crates/aria/tests/cases/README.md`.
//!
//! `artifact` and `stats` spawn nothing. Benches use them to record and
//! summarize a run.

pub mod artifact;
mod catalog;
mod daemon;
pub mod inputs;
pub mod log;
mod runner;
mod sandbox;
pub mod stats;

pub use catalog::CatalogError;
pub use daemon::{DaemonProcessError, RunningDaemon, ServiceStart, start_service};
pub use runner::run_catalog;
pub use sandbox::{Sandbox, SandboxError, yaml_string};
