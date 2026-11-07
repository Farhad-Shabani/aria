use super::{DictationRequest, DictationSettings, Rewriting};
use crate::history::HistorySource;
use crate::vocabulary::Context;
use aria_platform::delivery::{Application, TargetPolicy};
use aria_settings::{ApplicationOverride, DictationOverrides, Settings};

impl DictationSettings {
    /// `application` is the app in front as dictation is admitted. Each
    /// setting comes from the request, else that app's overrides, else `settings`.
    pub fn resolve(
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
        // The pipeline decides whether this can stream, since only it knows the engine.
        let delivery_mode = request
            .delivery_mode
            .or(application_delivery)
            .unwrap_or(settings.delivery_mode);
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
            source: HistorySource::Local,
            target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::StopMode;
    use aria_platform::delivery::OutputMode;
    use aria_settings::DeliveryMode;
    use aria_settings::Directory;
    use aria_settings::{ApplicationId, ApplicationOverrides};

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
            (OutputMode::Insert, DeliveryMode::Streaming)
        );
        assert_eq!(
            modes(DictationSettings::resolve(settings, request, None)),
            (OutputMode::Insert, DeliveryMode::Streaming)
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

    #[test]
    fn history_keep_audio_comes_from_the_settings() {
        for enabled in [true, false] {
            let settings = Settings {
                history_keep_audio: enabled,
                ..Settings::default()
            };
            let resolved = DictationSettings::resolve(
                settings,
                DictationRequest::new(StopMode::OnSilence),
                None,
            );
            assert_eq!(resolved.history_keep_audio, enabled);
        }
    }

    #[test]
    fn a_request_overrides_the_selected_vocabulary_only_while_vocabulary_is_on() {
        let selected = Directory::new("/projects/selected".into()).expect("absolute path");
        let explicit = Directory::new("/projects/explicit".into()).expect("absolute path");
        let settings = Settings {
            vocabulary_enabled: true,
            vocabulary_dir: Some(selected.clone()),
            ..Settings::default()
        };
        let project = |settings: &Settings, context| {
            DictationSettings::resolve(
                settings.clone(),
                DictationRequest {
                    project: context,
                    ..DictationRequest::new(StopMode::OnSilence)
                },
                None,
            )
            .project
        };

        assert_eq!(project(&settings, Context::Default), Some(selected));
        assert_eq!(
            project(&settings, Context::Directory(explicit.clone())),
            Some(explicit.clone())
        );
        assert_eq!(project(&settings, Context::Disabled), None);
        let disabled = Settings {
            vocabulary_enabled: false,
            ..settings
        };
        assert_eq!(project(&disabled, Context::Directory(explicit)), None);
    }
}
