//! The system transcription service: wire translation over the daemon's
//! system transcription slot.

use std::io::{self, Read};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aria_settings::rpc::RpcFailure;
use aria_voice::system_transcription::unsaved::UnsavedError;
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use crate::ipc::wire;

use super::super::system_transcription::SystemTranscriptionState;

pub(in crate::daemon) struct SystemTranscriptionRpc {
    state: Arc<SystemTranscriptionState>,
}

impl SystemTranscriptionRpc {
    pub(in crate::daemon) fn new(state: Arc<SystemTranscriptionState>) -> Self {
        Self { state }
    }

    /// The slot itself, for shutdown: draining has to stop a live session on
    /// a blocking task, rather than leave it to a Drop that lands on the
    /// runtime's driver thread once the grace period is already spent.
    pub(in crate::daemon) fn state(&self) -> Arc<SystemTranscriptionState> {
        Arc::clone(&self.state)
    }
}

#[tonic::async_trait]
impl wire::system_transcription_service_server::SystemTranscriptionService
    for SystemTranscriptionRpc
{
    async fn start(
        &self,
        request: Request<wire::StartSystemTranscriptionRequest>,
    ) -> Result<Response<wire::StartSystemTranscriptionResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/Start";
        let output = Option::<PathBuf>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.start(output)).await? {
            Ok(started) => {
                super::served(METHOD);
                Ok(Response::new(started.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn stop(
        &self,
        _request: Request<wire::StopSystemTranscriptionRequest>,
    ) -> Result<Response<wire::StopSystemTranscriptionResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/Stop";
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.stop()).await? {
            Ok(stopped) => {
                super::served(METHOD);
                Ok(Response::new(stopped.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn get_status(
        &self,
        _request: Request<wire::GetSystemTranscriptionStatusRequest>,
    ) -> Result<Response<wire::GetSystemTranscriptionStatusResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/GetStatus";
        let state = Arc::clone(&self.state);
        let status = super::blocking(METHOD, move || state.status()).await?;
        // The app polls this, so info would fill the log.
        tracing::debug!(method = METHOD, status = "ok", "request served");
        Ok(Response::new(status.into()))
    }

    async fn list_unsaved(
        &self,
        _request: Request<wire::ListUnsavedTranscriptsRequest>,
    ) -> Result<Response<wire::ListUnsavedTranscriptsResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/ListUnsaved";
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.unsaved()).await? {
            Ok(unsaved) => {
                super::served(METHOD);
                Ok(Response::new(unsaved.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn read_unsaved(
        &self,
        request: Request<wire::ReadUnsavedTranscriptRequest>,
    ) -> Result<Response<wire::ReadUnsavedTranscriptResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/ReadUnsaved";
        let request = request.into_inner();
        let name = request.name;
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.read_unsaved(&name)).await? {
            Ok(text) => {
                // The app reads this as the page opens, so info would fill the log.
                tracing::debug!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(text.into()))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    async fn discard_unsaved(
        &self,
        request: Request<wire::DiscardUnsavedTranscriptRequest>,
    ) -> Result<Response<wire::DiscardUnsavedTranscriptResponse>, Status> {
        const METHOD: &str = "SystemTranscriptionService/DiscardUnsaved";
        let request = request.into_inner();
        let name = request.name;
        let state = Arc::clone(&self.state);
        match super::blocking(METHOD, move || state.discard_unsaved(&name)).await? {
            Ok(()) => {
                super::served(METHOD);
                Ok(Response::new(wire::DiscardUnsavedTranscriptResponse {}))
            }
            Err(failure) => Err(super::failed(METHOD, failure)),
        }
    }

    type ExportUnsavedStream = ExportStream;

    async fn export_unsaved(
        &self,
        request: Request<wire::ExportUnsavedTranscriptRequest>,
    ) -> Result<Response<Self::ExportUnsavedStream>, Status> {
        const METHOD: &str = "SystemTranscriptionService/ExportUnsaved";
        let (name, destination) = <(String, PathBuf)>::try_from(request.into_inner())
            .map_err(|error| super::invalid_request(METHOD, &error))?;
        let state = Arc::clone(&self.state);
        let source =
            match super::blocking(METHOD, move || state.export_unsaved(&name, &destination)).await?
            {
                Ok(source) => source,
                Err(failure) => return Err(super::failed(METHOD, failure)),
            };
        let (chunks, receiver) = mpsc::channel(EXPORT_CAPACITY);
        tokio::task::spawn_blocking(move || {
            let sent = send_chunks(source, |chunk| {
                chunks
                    .blocking_send(Ok(wire::ExportUnsavedTranscriptResponse { chunk }))
                    .is_ok()
            });
            match sent {
                Ok(Exported::Whole) => super::served(METHOD),
                Ok(Exported::Abandoned) => {
                    tracing::debug!(method = METHOD, "the caller left before the export ended");
                }
                Err(error) => {
                    let failure = RpcFailure::from(&UnsavedError::Io(error));
                    let _ = chunks.blocking_send(Err(super::failed(METHOD, failure)));
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// How much of a transcript one export message carries.
const EXPORT_CHUNK_BYTES: u64 = 64 * 1024;
/// Chunks queued for a slow caller, so one that stalls holds little memory.
const EXPORT_CAPACITY: usize = 4;

type ExportStream =
    Pin<Box<dyn Stream<Item = Result<wire::ExportUnsavedTranscriptResponse, Status>> + Send>>;

/// How an export ended without a read error.
#[derive(Debug, PartialEq, Eq)]
enum Exported {
    Whole,
    /// The caller stopped taking chunks, as when it hung up.
    Abandoned,
}

/// Reads `source` to its end, a chunk at a time, and hands each to `send`,
/// which says whether the caller took it.
fn send_chunks(
    mut source: impl Read,
    mut send: impl FnMut(Vec<u8>) -> bool,
) -> io::Result<Exported> {
    loop {
        let mut chunk = Vec::new();
        (&mut source)
            .take(EXPORT_CHUNK_BYTES)
            .read_to_end(&mut chunk)?;
        if chunk.is_empty() {
            return Ok(Exported::Whole);
        }
        if !send(chunk) {
            return Ok(Exported::Abandoned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_sends_the_whole_file_in_bounded_chunks() {
        let text: Vec<u8> = (0..200 * 1024).map(|byte| (byte % 251) as u8).collect();
        let mut chunks = Vec::new();

        let exported = send_chunks(text.as_slice(), |chunk| {
            chunks.push(chunk);
            true
        })
        .unwrap();

        assert_eq!(exported, Exported::Whole);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() as u64 <= EXPORT_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn an_export_stops_once_the_caller_leaves() {
        let text = vec![b'a'; 200 * 1024];
        let mut sent = 0;

        let exported = send_chunks(text.as_slice(), |_| {
            sent += 1;
            false
        })
        .unwrap();

        assert_eq!(exported, Exported::Abandoned);
        assert_eq!(sent, 1);
    }
}
