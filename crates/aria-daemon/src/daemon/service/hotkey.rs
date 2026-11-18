//! The Aria key's state, read from the hotkey trigger.

use tokio::sync::watch;
use tonic::{Request, Response, Status};

use crate::ipc::wire;
use aria_platform::hotkey::HotkeyState;

#[derive(Clone)]
pub(in crate::daemon) struct HotkeyRpc {
    state: watch::Receiver<Option<HotkeyState>>,
}

impl HotkeyRpc {
    pub(in crate::daemon) fn new(state: watch::Receiver<Option<HotkeyState>>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl wire::hotkey_service_server::HotkeyService for HotkeyRpc {
    async fn get_status(
        &self,
        _request: Request<wire::GetHotkeyStatusRequest>,
    ) -> Result<Response<wire::GetHotkeyStatusResponse>, Status> {
        const METHOD: &str = "HotkeyService/GetStatus";
        let response = wire::GetHotkeyStatusResponse::from(*self.state.borrow());
        super::served(METHOD);
        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::testing::HostedDaemon;
    use super::*;
    use wire::hotkey_service_server::HotkeyService;

    #[tokio::test]
    async fn the_status_is_the_triggers_latest_state() {
        let (state, receiver) = watch::channel(None);
        let rpc = HotkeyRpc::new(receiver);
        state.send_replace(Some(HotkeyState::AwaitingListenAccess));

        let response = rpc
            .get_status(Request::new(wire::GetHotkeyStatusRequest {}))
            .await
            .expect("the status always answers")
            .into_inner();

        assert_eq!(response.state(), wire::HotkeyState::AwaitingListenAccess);
    }

    #[test]
    fn a_daemon_without_the_hotkey_serves_an_unknown_state() {
        let daemon = HostedDaemon::configured(|_| String::new());
        let client = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the client runtime builds");

        let state = client.block_on(async {
            wire::hotkey_service_client::HotkeyServiceClient::new(daemon.channel().await)
                .get_status(wire::GetHotkeyStatusRequest {})
                .await
                .expect("the unix socket serves the status")
                .into_inner()
                .state()
        });
        daemon.stop();

        assert_eq!(state, wire::HotkeyState::Unspecified);
    }
}
