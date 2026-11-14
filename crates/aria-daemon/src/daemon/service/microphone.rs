//! The microphone service: which input devices a press can record on.

use std::sync::Arc;

use aria_kit::audio::resolve_input;
use aria_settings::SettingsCache;
use tonic::{Request, Response, Status};

use crate::ipc::{protocol, wire};

use super::super::capture;

pub(in crate::daemon) struct MicrophoneRpc {
    cache: Arc<SettingsCache>,
}

impl MicrophoneRpc {
    pub(in crate::daemon) fn new(cache: Arc<SettingsCache>) -> Self {
        Self { cache }
    }
}

#[tonic::async_trait]
impl wire::microphone_service_server::MicrophoneService for MicrophoneRpc {
    async fn list(
        &self,
        _request: Request<wire::ListMicrophonesRequest>,
    ) -> Result<Response<wire::ListMicrophonesResponse>, Status> {
        const METHOD: &str = "MicrophoneService/List";
        let chosen = capture::input(&self.cache.microphone().borrow());
        match super::blocking(METHOD, capture::list_microphones).await? {
            Ok(devices) => {
                let current = resolve_input(chosen.as_ref(), &devices);
                super::served(METHOD);
                Ok(Response::new(protocol::microphones(&devices, current)))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }
}
