//! The daemon's shared wire contract: generated messages, wire-domain
//! conversions, and the bundled client.

mod client;
pub(crate) mod protocol;
#[allow(
    unreachable_pub,
    reason = "Prost and tonic generate public types and service modules"
)]
pub(crate) mod wire;

/// The health-reporter name a daemon serves its wake-word status under. Not a
/// gRPC service, so it never appears in reflection — only in health checks.
pub(crate) const WAKE_WORD_HEALTH_SERVICE: &str = "aria.wake_word";

/// The health-reporter name for the global dictation hotkey, by the same
/// rule: health checks only, never reflection.
pub(crate) const HOTKEY_HEALTH_SERVICE: &str = "aria.hotkey";

pub use client::{ClientError, ConnectError, DaemonEvent, EventStream};
pub(crate) use client::{DaemonClient, Liveness, ProbeError, probe_daemon};
