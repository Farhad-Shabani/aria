//! The microphone service: which input devices a press can record on.

use std::sync::Arc;

use aria_settings::SettingsCache;
use tonic::{Request, Response, Status};

use crate::ipc::{protocol, wire};

use super::super::phone::PhoneMicrophone;
use super::super::{capture, microphone};

pub(in crate::daemon) struct MicrophoneRpc {
    cache: Arc<SettingsCache>,
    phones: Arc<PhoneMicrophone>,
}

impl MicrophoneRpc {
    pub(in crate::daemon) fn new(cache: Arc<SettingsCache>, phones: Arc<PhoneMicrophone>) -> Self {
        Self { cache, phones }
    }
}

#[tonic::async_trait]
impl wire::microphone_service_server::MicrophoneService for MicrophoneRpc {
    async fn list(
        &self,
        _request: Request<wire::ListMicrophonesRequest>,
    ) -> Result<Response<wire::ListMicrophonesResponse>, Status> {
        const METHOD: &str = "MicrophoneService/List";
        let preference = self.cache.microphone().borrow().clone();
        let attached = self.phones.is_attached();
        match super::blocking(METHOD, capture::list_microphones).await? {
            Ok(devices) => {
                let current = microphone::current(&preference, attached, &devices);
                super::served(METHOD);
                Ok(Response::new(protocol::microphones(&devices, current)))
            }
            Err(error) => Err(super::failed(METHOD, &error)),
        }
    }
}
