use clap::Subcommand;

use super::logging::log_filter;

#[derive(Subcommand)]
pub(super) enum RemoteCommand {
    #[command(about = "Print a secret invitation from the local inference server")]
    Invite {
        #[arg(long)]
        endpoint: String,
    },
    #[command(about = "Pair using a secret invitation read from stdin; does not enable uploads")]
    Pair {
        /// Print one JSON object, with a failure code on failure
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Revoke a client and cancel its active inference")]
    Revoke { fingerprint: String },
    #[command(about = "Forget the paired server and this machine's key for it")]
    Forget,
}

pub(super) fn run(command: RemoteCommand) -> anyhow::Result<()> {
    match command {
        RemoteCommand::Invite { endpoint } => {
            println!("{}", aria_daemon::remote_invite(&endpoint)?);
        }
        RemoteCommand::Pair { json: false } => {
            let fingerprint = crate::setup::pair_server(std::io::stdin().lock())?;
            println!("Paired client: {fingerprint}");
            println!("To use it, run `aria setup --stt-runs-on server`.");
        }
        RemoteCommand::Pair { json: true } => {
            let paired = crate::setup::pair_server(std::io::stdin().lock());
            println!("{}", pair_report(&paired));
            if paired.is_err() {
                std::process::exit(1);
            }
        }
        RemoteCommand::Revoke { fingerprint } => {
            aria_daemon::remote_revoke(&fingerprint)?;
            println!("Client revoked.");
        }
        RemoteCommand::Forget => {
            let forgotten = crate::setup::forget_server()?;
            match &forgotten {
                aria_daemon::Forgotten::NothingPaired => println!("No server is paired."),
                aria_daemon::Forgotten::Server { .. } => {
                    println!("Forgot the paired server.");
                }
                aria_daemon::Forgotten::UnfinishedPairing { .. } => {
                    println!("Removed the key from a pairing that did not finish.");
                }
                aria_daemon::Forgotten::Unreadable => {
                    println!("Removed an unreadable pairing file; its fingerprint is unknown.");
                }
            }
            if matches!(
                forgotten,
                aria_daemon::Forgotten::Server { .. }
                    | aria_daemon::Forgotten::UnfinishedPairing { .. }
            ) {
                println!("To block this machine on the server too, run there:");
                println!("  {}", forgotten.revoke_hint());
            }
        }
    }
    Ok(())
}

fn pair_report(paired: &Result<String, crate::setup::SetupError>) -> serde_json::Value {
    match paired {
        Ok(fingerprint) => serde_json::json!({ "paired": true, "fingerprint": fingerprint }),
        Err(error) => serde_json::json!({
            "paired": false,
            "error": {
                "code": pairing_failure(error).label(),
                "message": crate::primitives::error_chain(error),
            },
        }),
    }
}

fn pairing_failure(error: &crate::setup::SetupError) -> aria_daemon::PairingFailure {
    use crate::setup::SetupError;
    use aria_daemon::PairingFailure;

    match error {
        SetupError::Pair(error) => PairingFailure::from(error),
        SetupError::Busy => PairingFailure::Busy,
        SetupError::Home(_)
        | SetupError::CurrentExe(_)
        | SetupError::Signal(_)
        | SetupError::Io { .. }
        | SetupError::Prompt(_)
        | SetupError::UnknownModel { .. }
        | SetupError::UnknownEngine { .. }
        | SetupError::NotSettable { .. }
        | SetupError::ServerNotPaired
        | SetupError::PairingUnusable { .. }
        | SetupError::ServerInUse
        | SetupError::Forget(_)
        | SetupError::ModelInUse { .. }
        | SetupError::Config(_)
        | SetupError::ModelNotInstalled { .. }
        | SetupError::Request(_)
        | SetupError::Decode(_)
        | SetupError::Invalid(_)
        | SetupError::Respond(_)
        | SetupError::NotATerminal
        | SetupError::Spawn { .. }
        | SetupError::Wait { .. }
        | SetupError::Command { .. }
        | SetupError::Interrupted { .. }
        | SetupError::WrongHash { .. }
        | SetupError::Untrusted { .. }
        | SetupError::Service(_)
        | SetupError::Settings(_)
        | SetupError::SettingsRefused(_)
        | SetupError::SettingsChanging => PairingFailure::Failed,
    }
}

pub(super) fn serve(bind: std::net::SocketAddr) -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(log_filter(None))
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    aria_stt::whisper::install_logging_hooks();
    Ok(aria_daemon::serve_inference(bind)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_pairing_reports_its_code_and_cause() {
        let report = pair_report(&Err(crate::setup::SetupError::Pair(
            aria_daemon::RemoteClientError::Timeout,
        )));

        assert_eq!(
            report,
            serde_json::json!({
                "paired": false,
                "error": { "code": "timeout", "message": "remote operation timed out" },
            })
        );
    }

    #[test]
    fn a_pairing_that_waits_on_another_setup_reports_busy() {
        let report = pair_report(&Err(crate::setup::SetupError::Busy));

        assert_eq!(report["error"]["code"], "busy");
    }
}
