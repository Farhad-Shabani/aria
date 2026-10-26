//! The setup contract as Aria.app speaks it: `aria.setup.v1` messages,
//! one per length-delimited frame, over the stdio of `aria setup inspect`
//! and `aria setup apply`.

use std::io::{Read, Write};

use prost::Message;

use crate::daemon::PairedServer;
use crate::supervisor::{DaemonState, Job, Status};

use super::backends::ModelSource;
use super::models::OnDisk;
use super::{
    ApplyOptions, Availability, Choice, ConfigWrite, Grant, Inspection, Missing, ModelState,
    Outcome, Pairing, Selections, SetupError, SetupEvent, Step, Summary, voice_model_present,
};

#[allow(unreachable_pub, reason = "Prost generates public types")]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/aria.setup.v1.rs"));
}

use generated::apply_response::Body;

/// Answers one `Inspection`.
pub fn inspect(output: &mut impl Write) -> Result<(), SetupError> {
    let inspection = super::inspect()?;
    write(output, &generated::Inspection::from(&inspection))
}

/// Reads one `ApplyRequest`, then answers an `ApplyResponse` per event and
/// one more carrying the summary. A response that cannot be written ends
/// the run: the app that was reading has gone.
pub fn apply(input: &mut impl Read, output: &mut impl Write) -> Result<(), SetupError> {
    let mut frame = Vec::new();
    input.read_to_end(&mut frame).map_err(SetupError::Request)?;
    let request = generated::ApplyRequest::decode_length_delimited(frame.as_slice())
        .map_err(SetupError::Decode)?;
    let (selections, options) = request.try_into()?;
    let inspection = super::inspect()?;

    let summary = super::apply(&selections, &options, &inspection, &mut |event| {
        let response = generated::ApplyResponse {
            body: Some(Body::Event(event.into())),
        };
        write(output, &response)
    })?;
    let response = generated::ApplyResponse {
        body: Some(Body::Summary((&summary).into())),
    };
    write(output, &response)
}

fn write(output: &mut impl Write, message: &impl Message) -> Result<(), SetupError> {
    output
        .write_all(&message.encode_length_delimited_to_vec())
        .and_then(|()| output.flush())
        .map_err(SetupError::Respond)
}

impl TryFrom<generated::ApplyRequest> for (Selections, ApplyOptions) {
    type Error = SetupError;

    fn try_from(request: generated::ApplyRequest) -> Result<Self, SetupError> {
        let selections = request
            .selections
            .ok_or(SetupError::Invalid("no selections"))?;
        if [
            &selections.transcription_engine_id,
            &selections.synthesis_engine_id,
            &selections.assistant_id,
        ]
        .iter()
        .any(|key| key.is_empty())
        {
            return Err(SetupError::Invalid("an engine key is empty"));
        }
        Ok((
            Selections {
                stt: selections.transcription_engine_id,
                tts: selections.synthesis_engine_id,
                chat: selections.assistant_id,
                wake: selections.wake_enabled,
                hotkey: selections.hotkey_enabled,
            },
            ApplyOptions {
                downloads: request.model_ids,
                transfer_meter: false,
            },
        ))
    }
}

impl From<&Inspection> for generated::Inspection {
    fn from(inspection: &Inspection) -> Self {
        Self {
            config_path: inspection.config_path.display().to_string(),
            is_macos: inspection.macos,
            defaults: Some(generated::Selections::from(&Selections::defaults(
                inspection,
            ))),
            transcription_choices: inspection.stt.iter().map(Into::into).collect(),
            synthesis_choices: inspection.tts.iter().map(Into::into).collect(),
            assistant_choices: inspection.chat.iter().map(Into::into).collect(),
            models: inspection.models.iter().map(Into::into).collect(),
            wakeword_installed: inspection.wakeword_installed,
            service: Some((&inspection.service).into()),
            grant: generated::Grant::from(inspection.grant) as i32,
            server: match &inspection.pairing {
                Pairing::Paired(paired) => Some(paired.into()),
                Pairing::Unpaired | Pairing::Unusable => None,
            },
            missing: inspection.missing().iter().map(Into::into).collect(),
        }
    }
}

impl From<&PairedServer> for generated::PairedServer {
    fn from(server: &PairedServer) -> Self {
        Self {
            endpoint: server.endpoint.to_string(),
            client_fingerprint: server.client_fingerprint.clone(),
        }
    }
}

impl From<&Selections> for generated::Selections {
    fn from(selections: &Selections) -> Self {
        Self {
            transcription_engine_id: selections.stt.clone(),
            synthesis_engine_id: selections.tts.clone(),
            assistant_id: selections.chat.clone(),
            wake_enabled: selections.wake,
            hotkey_enabled: selections.hotkey,
        }
    }
}

impl From<&Choice> for generated::Choice {
    fn from(choice: &Choice) -> Self {
        let (availability, note) = match &choice.availability {
            Availability::Ready => (generated::Availability::Ready, String::new()),
            Availability::Missing { fix } => (generated::Availability::Missing, fix.clone()),
            Availability::Unsupported { why } => {
                (generated::Availability::Unsupported, (*why).to_owned())
            }
        };
        Self {
            id: choice.key.to_owned(),
            name: choice.label.to_owned(),
            availability: availability as i32,
            availability_detail: note,
            is_default: choice.default,
            model_source: generated::ModelSource::from(choice.model) as i32,
        }
    }
}

impl From<&ModelState> for generated::Model {
    fn from(model: &ModelState) -> Self {
        Self {
            id: model.name.to_owned(),
            name: model.title.to_owned(),
            engine_id: model.engine.unwrap_or_default().to_owned(),
            size_bytes: model.bytes,
            download_by_default: model.default,
            on_disk: generated::OnDisk::from(model.on_disk) as i32,
            external: model.external,
        }
    }
}

impl From<&Missing> for generated::Missing {
    fn from(missing: &Missing) -> Self {
        use generated::missing::Kind;
        let (kind, model) = match missing {
            Missing::Config => (Kind::Config, ""),
            Missing::Wakeword => (Kind::Wakeword, ""),
            Missing::Model(name) => (Kind::Model, *name),
            Missing::Service => (Kind::Service, ""),
            Missing::Grant => (Kind::Grant, ""),
        };
        Self {
            kind: kind as i32,
            model_id: model.to_owned(),
        }
    }
}

impl From<&Status> for generated::ServiceStatus {
    fn from(status: &Status) -> Self {
        let (job, job_pid) = match status.job {
            Job::Absent => (generated::Job::Absent, 0),
            Job::Idle => (generated::Job::Idle, 0),
            Job::Running(pid) => (generated::Job::Running, pid),
        };
        let (daemon, daemon_pid) = match status.daemon {
            DaemonState::Ready { pid } => (generated::DaemonState::Ready, pid),
            DaemonState::Starting { pid } => (generated::DaemonState::Starting, pid),
            DaemonState::NotRunning => (generated::DaemonState::NotRunning, None),
        };
        Self {
            unit_path: status.unit.display().to_string(),
            job: job as i32,
            job_pid,
            socket_path: status.socket.display().to_string(),
            daemon: daemon as i32,
            daemon_pid: daemon_pid.unwrap_or(0),
        }
    }
}

impl From<ModelSource> for generated::ModelSource {
    fn from(model: ModelSource) -> Self {
        match model {
            ModelSource::Downloads(_) => Self::Downloads,
            ModelSource::FetchedOnFirstUse => Self::FetchedOnFirstUse,
            ModelSource::BuiltIn => Self::BuiltIn,
            ModelSource::Remote => Self::Remote,
        }
    }
}

impl From<OnDisk> for generated::OnDisk {
    fn from(on_disk: OnDisk) -> Self {
        match on_disk {
            OnDisk::Complete => Self::Complete,
            OnDisk::Partial => Self::Partial,
            OnDisk::Absent => Self::Absent,
        }
    }
}

impl From<Grant> for generated::Grant {
    fn from(grant: Grant) -> Self {
        match grant {
            Grant::NotNeeded => Self::NotNeeded,
            Grant::Armed => Self::Armed,
            Grant::Waiting => Self::Waiting,
        }
    }
}

impl From<Step> for generated::Step {
    fn from(step: Step) -> Self {
        match step {
            Step::Config => Self::Config,
            Step::Wakeword => Self::Wakeword,
            Step::Models => Self::Models,
            Step::Service => Self::Service,
        }
    }
}

impl From<SetupEvent> for generated::SetupEvent {
    fn from(event: SetupEvent) -> Self {
        use generated::setup_event::Event;
        let event = match event {
            SetupEvent::Started(step) => Event::Started(generated::Step::from(step) as i32),
            SetupEvent::Download {
                model,
                received,
                total,
            } => Event::Download(generated::DownloadProgress {
                model_id: model.to_owned(),
                received_bytes: received,
                total_bytes: total,
            }),
        };
        Self { event: Some(event) }
    }
}

impl From<&Summary> for generated::Summary {
    fn from(summary: &Summary) -> Self {
        let (config, config_backup) = match &summary.config {
            ConfigWrite::Unchanged => (generated::ConfigWrite::Unchanged, String::new()),
            ConfigWrite::Written { backup } => (
                generated::ConfigWrite::Written,
                backup
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
        };
        Self {
            config_write: config as i32,
            config_backup_path: config_backup,
            wakeword_directory: summary
                .wakeword
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            model_outcomes: summary
                .models
                .iter()
                .map(|(model, outcome)| generated::ModelOutcome {
                    model_id: (*model).to_owned(),
                    outcome: generated::Outcome::from(*outcome) as i32,
                })
                .collect(),
            legacy_agent_path: summary
                .legacy_agent
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            service: Some((&summary.service).into()),
            grant: generated::Grant::from(summary.grant) as i32,
            voice_model_present: voice_model_present(&summary.models),
        }
    }
}

impl From<Outcome> for generated::Outcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Downloaded => Self::Downloaded,
            Outcome::Present => Self::Present,
            Outcome::Skipped => Self::Skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn request(selections: Option<generated::Selections>) -> generated::ApplyRequest {
        generated::ApplyRequest {
            selections,
            model_ids: vec!["whisper".to_owned()],
        }
    }

    #[test]
    fn a_request_needs_its_selections() {
        let error = <(Selections, ApplyOptions)>::try_from(request(None)).unwrap_err();
        assert!(matches!(error, SetupError::Invalid(_)), "{error}");

        let empty = generated::Selections {
            transcription_engine_id: String::new(),
            synthesis_engine_id: "system".to_owned(),
            assistant_id: "codex".to_owned(),
            wake_enabled: false,
            hotkey_enabled: true,
        };
        let error = <(Selections, ApplyOptions)>::try_from(request(Some(empty))).unwrap_err();
        assert!(matches!(error, SetupError::Invalid(_)), "{error}");
    }

    #[test]
    fn a_request_carries_the_answers_and_the_downloads() {
        let selections = generated::Selections {
            transcription_engine_id: "whisper".to_owned(),
            synthesis_engine_id: "system".to_owned(),
            assistant_id: "codex".to_owned(),
            wake_enabled: false,
            hotkey_enabled: true,
        };

        let (selections, options) =
            <(Selections, ApplyOptions)>::try_from(request(Some(selections))).unwrap();

        assert_eq!(selections.stt, "whisper");
        assert!(selections.hotkey);
        assert_eq!(options.downloads, ["whisper"]);
        assert!(
            !options.transfer_meter,
            "curl's bar has no terminal to draw on"
        );
    }

    /// The app reads frames until the summary, so two frames on one
    /// stream have to come back apart.
    #[test]
    fn responses_frame_one_after_another() {
        let mut stream = Vec::new();
        let event = generated::ApplyResponse {
            body: Some(Body::Event(
                SetupEvent::Download {
                    model: "whisper",
                    received: 10,
                    total: 20,
                }
                .into(),
            )),
        };
        let summary = generated::ApplyResponse {
            body: Some(Body::Summary(generated::Summary::default())),
        };
        write(&mut stream, &event).unwrap();
        write(&mut stream, &summary).unwrap();

        let mut bytes = stream.as_slice();
        let first = generated::ApplyResponse::decode_length_delimited(&mut bytes).unwrap();
        let second = generated::ApplyResponse::decode_length_delimited(&mut bytes).unwrap();

        assert_eq!(first, event);
        assert_eq!(second, summary);
        assert!(bytes.is_empty());
    }

    #[test]
    fn a_running_service_reports_both_pids() {
        let status = Status {
            unit: PathBuf::from("/Library/LaunchAgents/io.github.farhad-shabani.aria.daemon.plist"),
            app_agent: false,
            job: Job::Running(41),
            socket: PathBuf::from("/tmp/aria-501/daemon.sock"),
            daemon: DaemonState::Ready { pid: Some(42) },
        };

        let wire = generated::ServiceStatus::from(&status);

        assert_eq!(wire.job(), generated::Job::Running);
        assert_eq!(wire.job_pid, 41);
        assert_eq!(wire.daemon(), generated::DaemonState::Ready);
        assert_eq!(wire.daemon_pid, 42);
    }
}
