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
    Pair,
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
        RemoteCommand::Pair => {
            let fingerprint = aria_daemon::remote_pair(std::io::stdin().lock())?;
            println!("Paired client: {fingerprint}");
            println!("To use it, choose the paired inference server in `aria setup`.");
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

pub(super) fn serve(bind: std::net::SocketAddr) -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(log_filter(None))
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    aria_stt::whisper::install_logging_hooks();
    Ok(aria_daemon::serve_inference(bind)?)
}
