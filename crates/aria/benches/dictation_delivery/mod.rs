use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use aria_harness::{Sandbox, artifact, inputs};
use serde_json::{Value, json};
use tonic::transport::{Channel, Endpoint};

#[allow(dead_code, unreachable_pub)]
mod wire {
    include!(concat!(env!("OUT_DIR"), "/aria.daemon.v1.rs"));
}

pub(super) mod target;

pub(super) struct Preparation {
    project: Option<PathBuf>,
    personal: Option<String>,
}

impl Preparation {
    pub(super) fn load(streaming: bool) -> anyhow::Result<Self> {
        let project = inputs::path_input("ARIA_BENCH_VOCABULARY_DIR")?;
        if let Some(project) = &project {
            ensure!(
                project.is_absolute() && project.is_dir(),
                "vocabulary must be an absolute directory"
            );
        }
        let personal = inputs::path_input("ARIA_BENCH_PERSONAL_TERMS")?
            .map(std::fs::read_to_string)
            .transpose()?;
        if streaming {
            ensure!(
                std::env::var_os("ARIA_BENCH_TARGET_TEXT_FILE").is_none()
                    && std::env::var_os("ARIA_BENCH_TARGET_PID").is_none(),
                "the benchmark owns its target; unset ARIA_BENCH_TARGET_TEXT_FILE and ARIA_BENCH_TARGET_PID"
            );
        }
        Ok(Self { project, personal })
    }

    pub(super) fn personal_identity(&self) -> anyhow::Result<String> {
        Ok(artifact::fingerprint_value(&self.personal)?)
    }

    pub(super) fn seed(&self, sandbox: &Sandbox) -> anyhow::Result<()> {
        if let Some(personal) = &self.personal {
            let memory = sandbox.home_path().join(".aria/memory");
            std::fs::create_dir_all(&memory)?;
            std::fs::write(memory.join("lexicon.txt"), personal)?;
        }
        Ok(())
    }

    pub(super) async fn prepare(&self, channel: Channel) -> anyhow::Result<Option<Value>> {
        let Some(project) = &self.project else {
            return Ok(None);
        };
        let directory = project
            .to_str()
            .context("non-UTF8 benchmark directory")?
            .to_owned();
        let mut preferences =
            wire::preferences_service_client::PreferencesServiceClient::new(channel.clone());
        let snapshot = preferences
            .get(wire::GetPreferencesRequest {})
            .await?
            .into_inner();
        let updated = preferences
            .update(wire::UpdatePreferencesRequest {
                expected_revision: snapshot.revision,
                patch: Some(wire::PreferencePatch {
                    project_vocabulary: Some(true),
                    vocabulary_directory: Some(directory.clone()),
                    ..Default::default()
                }),
            })
            .await?
            .into_inner();
        let Some(wire::update_preferences_response::Result::Snapshot(snapshot)) = updated.result
        else {
            anyhow::bail!("vocabulary preference update conflicted");
        };
        ensure!(
            snapshot
                .preferences
                .is_some_and(|preferences| preferences.project_vocabulary),
            "daemon did not enable vocabulary"
        );
        let mut vocabulary = wire::vocabulary_service_client::VocabularyServiceClient::new(channel);
        let status = loop {
            let status = vocabulary
                .get_status(wire::VocabularyRequest {
                    directory: Some(directory.clone()),
                })
                .await?
                .into_inner();
            if status.state == wire::VocabularyState::Ready as i32 {
                break status;
            }
            ensure!(
                status.error_category.is_empty(),
                "vocabulary preparation failed: {}",
                status.error_category
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        ensure!(
            status.term_count > 0 && status.runtime_revision > 0,
            "vocabulary did not prepare a nonempty snapshot: {}",
            status.error_category
        );
        Ok(Some(
            json!({"catalog_revision": status.catalog_revision, "runtime_revision": status.runtime_revision, "terms": status.term_count}),
        ))
    }
}

pub(super) async fn connect(socket: &Path) -> anyhow::Result<Channel> {
    Ok(
        Endpoint::from_shared(format!("unix://{}", socket.display()))?
            .connect_timeout(Duration::from_secs(10))
            .connect()
            .await?,
    )
}

pub(super) async fn record(
    channel: Channel,
    mut target: Option<&mut target::Target>,
) -> anyhow::Result<Value> {
    tokio::time::timeout(Duration::from_secs(330), async {
        let mut client = wire::dictation_service_client::DictationServiceClient::new(channel);
        if let Some(target) = target.as_deref_mut() {
            target.reset("", 0, 0)?;
        }
        let streaming = target.is_some();
        let started_unix_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?.as_micros();
        let started_ns = streaming.then(target::uptime_ns).transpose()?;
        let started = Instant::now();
        let mut stream = client.start(wire::StartDictationRequest {
            output_mode: Some(if streaming { wire::OutputMode::Insert } else { wire::OutputMode::Copy } as i32),
            delivery_mode: Some(if streaming { wire::DeliveryMode::Streaming } else { wire::DeliveryMode::Complete } as i32),
            stop_mode: if streaming { wire::StopMode::Manual } else { wire::StopMode::OnSilence } as i32,
            ..Default::default()
        }).await?.into_inner();
        let mut events = Vec::new();
        let mut recording_ms = None;
        let mut delivered_ms = None;
        let mut text = None;
        let mut segments = Vec::new();
        let mut segment_deliveries = 0;
        while let Some(message) = stream.message().await? {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match message.body {
                Some(wire::start_dictation_response::Body::Progress(stage)) => {
                    match wire::DictationStage::try_from(stage.stage) {
                        Ok(wire::DictationStage::Recording) => recording_ms = Some(elapsed_ms),
                        Ok(wire::DictationStage::Delivered) => delivered_ms = Some(elapsed_ms),
                        Ok(wire::DictationStage::SegmentTranscribed) => {
                            ensure!(!stage.text.is_empty(), "segment event has no text");
                            segments.push((elapsed_ms, stage.text.clone()));
                        }
                        Ok(wire::DictationStage::SegmentDelivered) => segment_deliveries += 1,
                        _ => {}
                    }
                    events.push(json!({"stage": stage.stage, "elapsed_ms": elapsed_ms, "text": stage.text}));
                }
                Some(wire::start_dictation_response::Body::Result(result)) => { text = Some(result.text); break; }
                Some(wire::start_dictation_response::Body::StopAccepted(_)) => anyhow::bail!("benchmark unexpectedly stopped another recording"),
                None => {}
            }
        }
        let client_ms = started.elapsed().as_millis() as u64;
        let text = text.context("dictation ended without result")?;

        let first_delivered_ms = delivered_ms.context("delivery event was not observed")?
            .checked_sub(recording_ms.context("recording event was not observed")?).context("delivery preceded recording")?;
        let mut trace = json!({"started_unix_us": started_unix_us, "text": text, "capture_to_delivered_ms": first_delivered_ms, "client_ms": client_ms, "events": events});
        if let Some(target) = target {
            ensure!(segment_deliveries == segments.len(), "segment delivery events missing");
            let metrics = segment_metrics(&segments, recording_ms.context("recording event missing")?, &text)?;
            trace["capture_to_first_segment_ms"] = metrics["capture_to_first_segment_ms"].clone();
            trace["corrected_segments"] = metrics["corrected_segments"].clone();
            trace["segment_text_kind"] = json!("corrected output; not raw capture ASR");
            trace["target"] = target.finish(&text, started_ns.context("missing trial start")?).await?;
        } else {
            let clipboard = clipboard_text()?;
            ensure!(clipboard == text, "clipboard differs from completed dictation result");
            trace["clipboard_text"] = json!(clipboard);
        }
        Ok(trace)
    }).await.context("dictation delivery observation timed out")?
}

fn segment_metrics(
    segments: &[(u64, String)],
    recording_ms: u64,
    result: &str,
) -> anyhow::Result<Value> {
    let first = segments
        .first()
        .context("no corrected segment events observed")?;
    let first_ms = first
        .0
        .checked_sub(recording_ms)
        .context("segment preceded recording")?;
    ensure!(
        segments
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            == result,
        "corrected segment reconstruction differs from completed result"
    );
    let mut previous = None;
    let mut corrected = Vec::new();
    for (elapsed_ms, text) in segments {
        let gap = previous
            .map(|previous| {
                elapsed_ms
                    .checked_sub(previous)
                    .context("segment events are out of order")
            })
            .transpose()?;
        corrected.push(json!({"elapsed_ms": elapsed_ms, "text": text, "gap_ms": gap}));
        previous = Some(*elapsed_ms);
    }
    Ok(json!({"capture_to_first_segment_ms": first_ms, "corrected_segments": corrected}))
}

#[cfg(target_os = "macos")]
fn clipboard_text() -> anyhow::Result<String> {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;
    autoreleasepool(|_| {
        NSPasteboard::generalPasteboard()
            .stringForType(&NSString::from_str("public.utf8-plain-text"))
            .map(|text| text.to_string())
            .context("clipboard contains no delivered text")
    })
}

#[cfg(not(target_os = "macos"))]
fn clipboard_text() -> anyhow::Result<String> {
    anyhow::bail!("delivery benchmark readback currently requires macOS")
}

#[cfg(test)]
mod tests {
    #[test]
    fn reconstructs_corrected_segments_with_spaces_and_relative_timings() {
        let segments = vec![(125, "héllo".into()), (180, "world".into())];
        let metrics = super::segment_metrics(&segments, 100, "héllo world").unwrap();
        assert_eq!(metrics["capture_to_first_segment_ms"], 25);
        assert!(metrics["corrected_segments"][0]["gap_ms"].is_null());
        assert_eq!(metrics["corrected_segments"][1]["gap_ms"], 55);
        assert!(super::segment_metrics(&segments, 100, "hélloworld").is_err());
        assert!(super::segment_metrics(&segments, 130, "héllo world").is_err());
        assert!(super::segment_metrics(&[], 100, "").is_err());
        assert!(
            super::segment_metrics(&[(125, "a".into()), (120, "b".into())], 100, "a b").is_err()
        );
    }
}
