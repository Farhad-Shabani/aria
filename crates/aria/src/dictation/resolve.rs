use super::{DictationRequest, DictationSettings, Rewriting};
use crate::delivery::{Application, OutputMode, PlatformTextDelivery, TargetPolicy, TextDelivery};
use crate::settings::{ApplicationOverride, DeliveryMode, DictationOverrides, Settings};
use crate::vocabulary::Context;

impl DictationSettings {
    /// `application` is the app in front as dictation is admitted. Each
    /// setting comes from the request, else that app's overrides, else `settings`.
    pub(crate) fn resolve(
        settings: Settings,
        request: DictationRequest,
        application: Option<Application>,
    ) -> Self {
        let application_id = application
            .as_ref()
            .and_then(|application| application.bundle_id.clone());
        let application_overrides = application_id
            .as_deref()
            .and_then(|id| settings.application_overrides.get(id));
        let target = TargetPolicy {
            overridden_for: application_overrides.and(application_id),
            excluded: settings.application_overrides.excluded_ids(),
        };
        // Exclusion of every app, not just this one, travels in `target`.
        let ApplicationOverride {
            excluded: _,
            overrides,
        } = application_overrides.unwrap_or_default();
        // Destructured so a new override field left unused here fails lint.
        let DictationOverrides {
            output_mode: application_output,
            delivery_mode: application_delivery,
        } = overrides;
        let output_mode = request
            .output_mode
            .or(application_output)
            .unwrap_or(settings.output_mode);
        // Copy cannot stream, nor can a platform that pastes, so they deliver
        // after recording stops. A request that asks to stream keeps its
        // choice, and delivery refuses it.
        let delivery_mode = match (output_mode, request.delivery_mode) {
            (OutputMode::Copy, None) => DeliveryMode::Complete,
            (_, None) if !PlatformTextDelivery::STREAMS => DeliveryMode::Complete,
            (_, requested) => requested
                .or(application_delivery)
                .unwrap_or(settings.delivery_mode),
        };
        Self {
            history_enabled: settings.history_enabled,
            history_keep_audio: settings.history_keep_audio,
            mute_sound: settings.mute_sound,
            stt_language: settings.stt_language,
            output_mode,
            delivery_mode,
            stop_mode: request.stop_mode,
            silence_duration: settings.silence_duration,
            auto_stop: settings.auto_stop,
            own_voice_only: settings.own_voice_only,
            rewriting: Rewriting {
                cleanup: settings.cleanup,
                backtrack: settings.backtrack,
            },
            transcript_only: request.transcript_only,
            project: if settings.vocabulary_enabled {
                match request.project {
                    Context::Default => settings.vocabulary_dir,
                    Context::Directory(directory) => Some(directory),
                    Context::Disabled => None,
                }
            } else {
                None
            },
            application,
            target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::StopMode;
    use crate::settings::{ApplicationId, ApplicationOverrides};

    const SLACK: &str = "com.tinyspeck.slackmacgap";

    fn front(bundle_id: &str) -> Option<Application> {
        Some(Application {
            name: "App".into(),
            bundle_id: Some(bundle_id.into()),
        })
    }

    fn with_slack(overrides: DictationOverrides) -> Settings {
        Settings {
            output_mode: OutputMode::Insert,
            delivery_mode: DeliveryMode::Streaming,
            application_overrides: ApplicationOverrides::new([(
                ApplicationId::new(SLACK).expect("valid"),
                ApplicationOverride {
                    excluded: false,
                    overrides,
                },
            )])
            .expect("valid"),
            ..Settings::default()
        }
    }

    const STREAMING: DeliveryMode = if PlatformTextDelivery::STREAMS {
        DeliveryMode::Streaming
    } else {
        DeliveryMode::Complete
    };

    fn modes(settings: DictationSettings) -> (OutputMode, DeliveryMode) {
        (settings.output_mode, settings.delivery_mode)
    }

    #[test]
    fn the_front_apps_overrides_replace_settings() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: Some(DeliveryMode::Complete),
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            modes(DictationSettings::resolve(
                settings.clone(),
                request.clone(),
                front(SLACK)
            )),
            (OutputMode::InsertAndSubmit, DeliveryMode::Complete)
        );
        assert_eq!(
            modes(DictationSettings::resolve(
                settings.clone(),
                request.clone(),
                front("com.apple.Notes")
            )),
            (OutputMode::Insert, STREAMING)
        );
        assert_eq!(
            modes(DictationSettings::resolve(settings, request, None)),
            (OutputMode::Insert, STREAMING)
        );
    }

    #[test]
    fn unset_overrides_inherit_and_requests_win() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::InsertAndSubmit),
            delivery_mode: None,
        });
        let request = DictationRequest {
            output_mode: Some(OutputMode::InsertOrCopy),
            ..DictationRequest::new(StopMode::OnSilence)
        };

        assert_eq!(
            modes(DictationSettings::resolve(settings, request, front(SLACK))),
            (OutputMode::InsertOrCopy, STREAMING)
        );
    }

    #[test]
    fn copy_delivers_after_recording_unless_the_request_asks_to_stream() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: Some(DeliveryMode::Streaming),
        });

        assert_eq!(
            modes(DictationSettings::resolve(
                settings.clone(),
                DictationRequest::new(StopMode::OnSilence),
                front(SLACK)
            )),
            (OutputMode::Copy, DeliveryMode::Complete)
        );
        let streaming = DictationRequest {
            delivery_mode: Some(DeliveryMode::Streaming),
            ..DictationRequest::new(StopMode::OnSilence)
        };
        assert_eq!(
            modes(DictationSettings::resolve(
                settings,
                streaming,
                front(SLACK)
            )),
            (OutputMode::Copy, DeliveryMode::Streaming)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_platform_that_pastes_streams_only_when_asked() {
        let settings = Settings {
            output_mode: OutputMode::InsertOrCopy,
            delivery_mode: DeliveryMode::Streaming,
            ..Settings::default()
        };
        let streaming = DictationRequest {
            delivery_mode: Some(DeliveryMode::Streaming),
            ..DictationRequest::new(StopMode::OnSilence)
        };

        assert_eq!(
            modes(DictationSettings::resolve(
                settings.clone(),
                DictationRequest::new(StopMode::OnSilence),
                None
            )),
            (OutputMode::InsertOrCopy, DeliveryMode::Complete)
        );
        assert_eq!(
            modes(DictationSettings::resolve(settings, streaming, None)),
            (OutputMode::InsertOrCopy, DeliveryMode::Streaming)
        );
    }

    #[test]
    fn the_front_app_is_kept_for_history() {
        let settings = DictationSettings::resolve(
            Settings::default(),
            DictationRequest::new(StopMode::OnSilence),
            front(SLACK),
        );

        assert_eq!(settings.application, front(SLACK));
    }

    #[test]
    fn only_the_front_app_with_aria_off_is_excluded() {
        let mut settings = with_slack(DictationOverrides::default());
        settings.application_overrides = ApplicationOverrides::new([(
            ApplicationId::new(SLACK).expect("valid"),
            ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides::default(),
            },
        )])
        .expect("valid");
        let request = DictationRequest::new(StopMode::OnSilence);

        assert!(
            DictationSettings::resolve(settings.clone(), request.clone(), front(SLACK))
                .application_excluded()
        );
        assert!(
            !DictationSettings::resolve(settings, request, front("com.apple.Notes"))
                .application_excluded()
        );
    }

    #[test]
    fn delivery_is_pinned_only_to_an_app_with_overrides() {
        let settings = with_slack(DictationOverrides {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: None,
        });
        let request = DictationRequest::new(StopMode::OnSilence);

        assert_eq!(
            DictationSettings::resolve(settings.clone(), request.clone(), front(SLACK))
                .target
                .overridden_for
                .as_deref(),
            Some(SLACK)
        );
        assert_eq!(
            DictationSettings::resolve(settings, request, front("com.apple.Notes"))
                .target
                .overridden_for,
            None
        );
    }
}
