//! The events service: a stream of everything the daemon announces.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::ipc::wire;
use crate::rpc::RpcFailure;

use super::super::subscriptions::Subscriptions;
use super::failed;

pub(in crate::daemon) struct EventsService {
    subscriptions: Arc<Subscriptions>,
}

impl EventsService {
    pub(in crate::daemon) fn new(subscriptions: Arc<Subscriptions>) -> Self {
        Self { subscriptions }
    }

    /// The server ends every stream when it drains, and this service is where
    /// the registry already lives.
    pub(in crate::daemon) fn subscriptions(&self) -> Arc<Subscriptions> {
        Arc::clone(&self.subscriptions)
    }
}

#[tonic::async_trait]
impl wire::events_service_server::EventsService for EventsService {
    type SubscribeStream = super::super::subscriptions::EventStream;

    async fn subscribe(
        &self,
        _request: Request<wire::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        const METHOD: &str = "EventsService/Subscribe";
        match self.subscriptions.subscribe() {
            Ok(stream) => {
                tracing::info!(method = METHOD, status = "ok", "request served");
                Ok(Response::new(stream))
            }
            Err(refusal) => Err(failed(
                METHOD,
                RpcFailure::new((&refusal).into(), refusal.to_string()),
            )),
        }
    }
}
