//! The setup contract as Aria.app speaks it: `aria.setup.v1` messages,
//! one per length-delimited frame, over the stdio of `aria setup inspect`,
//! `aria setup apply`, `aria setup set-stt-engine` and
//! `aria setup set-stt-runs-on`.

use aria_voice::config::Section;
use std::io::{Read, Write};

use prost::Message;

use crate::supervisor::{DaemonState, Job, Status};
use aria_daemon::PairedServer;
use aria_settings::RunsOn;

use super::backends::ModelSource;
use super::models::ModelFiles;
use super::stt_engine::SetSttEngineSummary;
use super::stt_runs_on::SetSttRunsOnSummary;
use super::{
    ApplyOptions, Availability, Choice, Grant, Inspection, Missing, ModelState, Outcome, Pairing,
    Selections, SettingsWrite, SetupError, SetupEvent, Step, Summary, voice_model_present,
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

/// Reads one `SetSttEngineRequest`, then answers a response per
/// event and one more carrying the summary.
pub fn set_stt_engine(input: &mut impl Read, output: &mut impl Write) -> Result<(), SetupError> {
    use generated::set_stt_engine_response::Body;
    let mut frame = Vec::new();
    input.read_to_end(&mut frame).map_err(SetupError::Request)?;
    let request = generated::SetSttEngineRequest::decode_length_delimited(frame.as_slice())
        .map_err(SetupError::Decode)?;

    let summary = super::stt_engine::set_stt_engine(&request.engine_id, false, &mut |event| {
        let response = generated::SetSttEngineResponse {
            body: Some(Body::Event(event.into())),
        };
        write(output, &response)
    })?;
    let response = generated::SetSttEngineResponse {
        body: Some(Body::Summary((&summary).into())),
    };
    write(output, &response)
}

/// Reads one `SetSttRunsOnRequest`, then answers a response per
/// event and one more carrying the summary.
pub fn set_stt_runs_on(input: &mut impl Read, output: &mut impl Write) -> Result<(), SetupError> {
    use generated::set_stt_runs_on_response::Body;
    let mut frame = Vec::new();
    input.read_to_end(&mut frame).map_err(SetupError::Request)?;
    let request = generated::SetSttRunsOnRequest::decode_length_delimited(frame.as_slice())
        .map_err(SetupError::Decode)?;
    let runs_on = runs_on(request.runs_on)?;

    let summary = super::stt_runs_on::set_stt_runs_on(runs_on, false, &mut |event| {
        let response = generated::SetSttRunsOnResponse {
            body: Some(Body::Event(event.into())),
        };
        write(output, &response)
    })?;
    let response = generated::SetSttRunsOnResponse {
        body: Some(Body::Summary((&summary).into())),
    };
    write(output, &response)
}

fn runs_on(value: i32) -> Result<RunsOn, SetupError> {
    match generated::RunsOn::try_from(value) {
        Ok(generated::RunsOn::Local) => Ok(RunsOn::Local),
        Ok(generated::RunsOn::Server) => Ok(RunsOn::Server),
        Ok(generated::RunsOn::Unspecified) | Err(_) => Err(SetupError::Invalid(
            "no known place to run speech recognition",
        )),
    }
}

impl From<RunsOn> for generated::RunsOn {
    fn from(runs_on: RunsOn) -> Self {
        match runs_on {
            RunsOn::Local => Self::Local,
            RunsOn::Server => Self::Server,
        }
    }
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
        if [&selections.stt_engine_id, &selections.tts_engine_id]
            .iter()
            .any(|key| key.is_empty())
        {
            return Err(SetupError::Invalid("a required selection is empty"));
        }
        Ok((
            Selections {
                stt: selections.stt_engine_id,
                tts: selections.tts_engine_id,
                wake: selections.wake,
            },
            ApplyOptions {
                downloads: request.downloads,
                transfer_meter: false,
            },
        ))
    }
}

impl From<&Inspection> for generated::Inspection {
    fn from(inspection: &Inspection) -> Self {
        Self {
            settings_path: inspection.settings_path.display().to_string(),
            macos: inspection.macos,
            defaults: Some(generated::Selections::from(&Selections::defaults(
                inspection,
            ))),
            stt_choices: inspection
                .stt
                .iter()
                .map(|c| generated::Choice {
                    settable: c.settable(),
                    ..choice(c)
                })
                .collect(),
            tts_choices: inspection.tts.iter().map(choice).collect(),
            models: inspection.models.iter().map(Into::into).collect(),
            wakeword_installed: inspection.wakeword_installed,
            service: Some((&inspection.service).into()),
            grant: generated::Grant::from(inspection.grant) as i32,
            missing: inspection.missing().iter().map(Into::into).collect(),
            server: match &inspection.pairing {
                Pairing::Paired(paired) => Some(paired.into()),
                Pairing::Unpaired | Pairing::Unusable => None,
            },
            stt_runs_on: generated::RunsOn::from(inspection.stt_runs_on) as i32,
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
            stt_engine_id: selections.stt.clone(),
            tts_engine_id: selections.tts.clone(),
            wake: selections.wake,
        }
    }
}

fn choice(choice: &Choice) -> generated::Choice {
    let (availability, note) = match &choice.availability {
        Availability::Ready => (generated::Availability::Ready, String::new()),
        Availability::Missing { fix } => (generated::Availability::Missing, fix.clone()),
        Availability::Unsupported { why } => {
            (generated::Availability::Unsupported, (*why).to_owned())
        }
    };
    generated::Choice {
        id: choice.key.to_owned(),
        label: choice.label.to_owned(),
        availability: availability as i32,
        availability_detail: note,
        default: choice.default,
        model_source: generated::ModelSource::from(choice.model) as i32,
        settable: false,
    }
}

impl From<&ModelState> for generated::Model {
    fn from(model: &ModelState) -> Self {
        Self {
            id: model.name.to_owned(),
            title: model.title.to_owned(),
            engine_id: model.engine.unwrap_or_default().to_owned(),
            size_bytes: model.bytes,
            download_by_default: model.default,
            files: generated::ModelFiles::from(model.files) as i32,
            used_by: model
                .used_by
                .iter()
                .map(|&section| generated::ModelUse::from(section) as i32)
                .collect(),
            label: model.label.to_owned(),
        }
    }
}

impl From<Section> for generated::ModelUse {
    fn from(section: Section) -> Self {
        match section {
            Section::Stt => Self::Stt,
            Section::SystemTranscription => Self::SystemTranscription,
            Section::Tts => Self::Tts,
            Section::Speaker => Self::Speaker,
        }
    }
}

impl From<&Missing> for generated::Missing {
    fn from(missing: &Missing) -> Self {
        use generated::missing::Kind;
        let (kind, model) = match missing {
            Missing::Settings => (Kind::Settings, ""),
            Missing::Wakeword => (Kind::Wakeword, ""),
            Missing::Model(name) => (Kind::Model, *name),
            Missing::Service => (Kind::Service, ""),
            Missing::Grant => (Kind::Grant, ""),
            Missing::KeyRemapped => (Kind::KeyRemapped, ""),
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
        }
    }
}

impl From<ModelFiles> for generated::ModelFiles {
    fn from(files: ModelFiles) -> Self {
        match files {
            ModelFiles::Complete => Self::Complete,
            ModelFiles::Partial => Self::Partial,
            ModelFiles::Absent => Self::Absent,
            ModelFiles::External => Self::External,
        }
    }
}

impl From<Grant> for generated::Grant {
    fn from(grant: Grant) -> Self {
        match grant {
            Grant::Armed => Self::Armed,
            Grant::Waiting => Self::Waiting,
        }
    }
}

impl From<Step> for generated::Step {
    fn from(step: Step) -> Self {
        match step {
            Step::Settings => Self::Settings,
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
        Self {
            settings: generated::SettingsWrite::from(summary.settings) as i32,
            wakeword_dir: summary
                .wakeword
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            models: model_outcomes(&summary.models),
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

impl From<SettingsWrite> for generated::SettingsWrite {
    fn from(write: SettingsWrite) -> Self {
        match write {
            SettingsWrite::Unchanged => Self::Unchanged,
            SettingsWrite::Written => Self::Written,
        }
    }
}

fn model_outcomes(models: &[(&'static str, Outcome)]) -> Vec<generated::ModelOutcome> {
    models
        .iter()
        .map(|(model, outcome)| generated::ModelOutcome {
            model_id: (*model).to_owned(),
            outcome: generated::Outcome::from(*outcome) as i32,
        })
        .collect()
}

impl From<&SetSttEngineSummary> for generated::SetSttEngineSummary {
    fn from(summary: &SetSttEngineSummary) -> Self {
        Self {
            settings: generated::SettingsWrite::from(summary.settings) as i32,
            models: model_outcomes(&summary.models),
        }
    }
}

impl From<&SetSttRunsOnSummary> for generated::SetSttRunsOnSummary {
    fn from(summary: &SetSttRunsOnSummary) -> Self {
        Self {
            settings: generated::SettingsWrite::from(summary.settings) as i32,
            models: model_outcomes(&summary.models),
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
            downloads: vec!["whisper".to_owned()],
        }
    }

    fn requested(
        request: generated::ApplyRequest,
    ) -> Result<(Selections, ApplyOptions), SetupError> {
        request.try_into()
    }

    #[test]
    fn only_stt_choices_say_whether_they_can_be_set_in_place() {
        let inspection = super::super::tests::inspection(super::super::tests::configured());
        assert!(
            inspection
                .tts
                .iter()
                .any(|choice| choice.key == "kokoro" && choice.settable())
        );

        let wire = generated::Inspection::from(&inspection);

        let moonshine = wire
            .stt_choices
            .iter()
            .find(|choice| choice.id == "moonshine")
            .unwrap();
        assert!(moonshine.settable);
        let kokoro = wire
            .tts_choices
            .iter()
            .find(|choice| choice.id == "kokoro")
            .unwrap();
        assert!(!kokoro.settable);
    }

    #[test]
    fn the_inspection_marks_what_each_model_is_loaded_for() {
        let used_by = |text: &str, name: &str| {
            let file = aria_settings::testing::file(text);
            super::super::model_states(std::path::Path::new("/home/someone"), &file)
                .unwrap()
                .iter()
                .find(|model| model.name == name)
                .map(|model| generated::Model::from(model).used_by)
                .unwrap()
        };
        let tiny = "[stt]\nbackend = \"whisper-candle\"\n";
        let chosen = format!("{tiny}[transcription]\nbackend = \"parakeet\"\n");

        assert_eq!(
            used_by(tiny, "whisper-candle"),
            [generated::ModelUse::Stt as i32]
        );
        assert_eq!(
            used_by(tiny, "speaker"),
            [generated::ModelUse::Speaker as i32]
        );
        assert!(used_by(tiny, "moonshine").is_empty());
        assert!(
            used_by(tiny, "parakeet").is_empty(),
            "System transcription's fallback is no setting, so it can be deleted"
        );
        assert_eq!(
            used_by(&chosen, "parakeet"),
            [generated::ModelUse::SystemTranscription as i32]
        );
    }

    #[test]
    fn a_request_needs_its_selections() {
        let error = requested(request(None)).unwrap_err();
        assert!(matches!(error, SetupError::Invalid(_)), "{error}");

        let empty = generated::Selections {
            stt_engine_id: String::new(),
            tts_engine_id: "system".to_owned(),
            wake: false,
        };
        let error = requested(request(Some(empty))).unwrap_err();
        assert!(matches!(error, SetupError::Invalid(_)), "{error}");
    }

    #[test]
    fn a_request_carries_the_answers_and_the_downloads() {
        let selections = generated::Selections {
            stt_engine_id: "whisper".to_owned(),
            tts_engine_id: "system".to_owned(),
            wake: false,
        };

        let (selections, options) = requested(request(Some(selections))).unwrap();

        assert_eq!(selections.stt, "whisper");
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
