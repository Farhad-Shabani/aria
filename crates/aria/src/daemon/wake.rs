//! The wake word as a dictation trigger, owned by a persistent daemon.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::audio::{WakeWordEvent, WakeWordMonitor};
use crate::dictation::{
    DeliveryMode, DictationError, DictationOutcome, DictationRequest, StopMode,
};
use crate::ipc::WAKE_WORD_HEALTH_SERVICE;

use super::dictation::DictationRuntimeError;
use super::dictation::{DictationCallError, DictationHandle};
use super::tasks::Supervised;

const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(super) struct WakeTrigger {
    monitor: WakeWordMonitor,
    dictation: DictationHandle,
    health: HealthReporter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WakeExit {
    Shutdown,
    Failed,
}

impl Supervised for WakeTrigger {
    async fn run(self, shutdown: CancellationToken) {
        let health = self.health.clone();
        let listener_shutdown = shutdown.clone();
        let mut listener = tokio::task::spawn_blocking(move || self.listen(listener_shutdown));

        let exit = tokio::select! {
            result = &mut listener => listener_exit(result),
            () = shutdown.cancelled() => listener_exit(listener.await),
        };
        report_exit(exit, &health).await;
    }
}

impl WakeTrigger {
    pub(super) fn new(
        monitor: WakeWordMonitor,
        dictation: DictationHandle,
        health: HealthReporter,
    ) -> Self {
        Self {
            monitor,
            dictation,
            health,
        }
    }

    /// Registers the initial status. Awaited before the daemon serves, so a
    /// health check can never find the wake service unregistered.
    pub(super) async fn announce(&self) {
        self.health
            .set_service_status(WAKE_WORD_HEALTH_SERVICE, ServingStatus::Serving)
            .await;
    }

    fn listen(self, shutdown: CancellationToken) -> WakeExit {
        while !shutdown.is_cancelled() {
            match self.monitor.wait_timeout(MONITOR_POLL_INTERVAL) {
                Ok(Some(WakeWordEvent::Detected)) => self.activate(),
                Ok(Some(WakeWordEvent::Failed)) => {
                    tracing::error!(
                        trigger = "wake_word",
                        "wake-word monitoring stopped after detector failure"
                    );
                    return WakeExit::Failed;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        trigger = "wake_word",
                        %error,
                        "wake-word monitoring stopped"
                    );
                    return WakeExit::Failed;
                }
            }
        }
        WakeExit::Shutdown
    }

    fn activate(&self) {
        tracing::info!(trigger = "wake_word", "wake word detected");
        let request = DictationRequest {
            output_mode: None,
            // Hands-free: always one insert at silence, never live keystrokes.
            delivery_mode: Some(DeliveryMode::Complete),
            stop_mode: Some(StopMode::OnSilence),
            transcript_only: false,
            project: crate::vocabulary::Context::Default,
        };
        // Daemon shutdown reaches the operation through the actor, which also
        // releases this blocking wait by answering it.
        let result = self.dictation.start_blocking(request);
        match result {
            Ok(DictationOutcome::Completed(_)) => tracing::info!(
                trigger = "wake_word",
                status = "completed",
                "wake-word dictation finished"
            ),
            Ok(DictationOutcome::StopAccepted) => tracing::warn!(
                trigger = "wake_word",
                status = "stop_accepted",
                "wake-word dictation finished without starting a recording"
            ),
            Err(DictationCallError::Runtime(DictationRuntimeError::Execution(
                DictationError::Cancelled,
            ))) => tracing::info!(
                trigger = "wake_word",
                status = "cancelled",
                "wake-word dictation finished"
            ),
            Err(DictationCallError::Unavailable) => tracing::info!(
                trigger = "wake_word",
                status = "unavailable",
                "wake-word dictation did not run; the daemon is stopping"
            ),
            Err(error @ (DictationCallError::Runtime(_) | DictationCallError::JobFailed)) => {
                tracing::warn!(
                    trigger = "wake_word",
                    status = "failed",
                    %error,
                    "wake-word dictation finished"
                );
                // A failure before the pipeline's suspension guard existed —
                // a preferences read, a coordinator build, a panic in that
                // window — leaves the monitor triggered and deaf. Resuming an
                // armed monitor is harmless, so every failure re-arms.
                self.resume_monitor();
            }
        }
    }

    fn resume_monitor(&self) {
        if let Err(error) = self.monitor.resume() {
            tracing::error!(
                trigger = "wake_word",
                %error,
                "wake-word monitoring could not resume"
            );
        }
    }
}

fn listener_exit(result: Result<WakeExit, tokio::task::JoinError>) -> WakeExit {
    match result {
        Ok(exit) => exit,
        Err(error) => {
            tracing::error!(%error, "wake-word listener ended unexpectedly");
            WakeExit::Failed
        }
    }
}

async fn report_exit(exit: WakeExit, health: &HealthReporter) {
    if exit == WakeExit::Failed {
        health
            .set_service_status(WAKE_WORD_HEALTH_SERVICE, ServingStatus::NotServing)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_server::Health;
    use tonic_health::server::HealthService;

    use super::*;

    #[tokio::test]
    async fn a_failed_exit_degrades_only_wake_health() {
        let (reporter, _server) = tonic_health::server::health_reporter();
        reporter
            .set_service_status(
                WAKE_WORD_HEALTH_SERVICE,
                tonic_health::ServingStatus::Serving,
            )
            .await;
        let service = HealthService::from_health_reporter(reporter.clone());

        report_exit(WakeExit::Failed, &reporter).await;

        for (name, expected) in [
            (WAKE_WORD_HEALTH_SERVICE, ServingStatus::NotServing),
            ("", ServingStatus::Serving),
        ] {
            let response = service
                .check(tonic::Request::new(HealthCheckRequest {
                    service: name.to_owned(),
                }))
                .await
                .expect("the health check should answer");
            assert_eq!(response.into_inner().status(), expected, "{name:?}");
        }
    }
}
