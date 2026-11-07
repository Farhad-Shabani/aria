use std::process::ExitCode;

use aria::commands::{self, Cli};
use aria::primitives::error_chain;
use aria::setup::SetupError;
use clap::Parser;

/// `EX_TEMPFAIL`: another setup run holds the lock, so trying again later
/// can succeed. Aria.app tells it apart from a failure by this code.
const SETUP_BUSY: u8 = 75;

fn main() -> ExitCode {
    match commands::run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error_line(&error));
            ExitCode::from(status(&error))
        }
    }
}

/// One line, as cargo and git print theirs: anyhow's own report spreads
/// the causes over a `Caused by:` block.
fn error_line(error: &anyhow::Error) -> String {
    format!("error: {}", error_chain(error.as_ref()))
}

fn status(error: &anyhow::Error) -> u8 {
    let busy = error
        .chain()
        .any(|cause| matches!(cause.downcast_ref(), Some(SetupError::Busy)));
    if busy { SETUP_BUSY } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_prints_on_one_line_with_its_causes() {
        let error =
            anyhow::anyhow!("permission denied").context("failed to prepare daemon runtime paths");

        assert_eq!(
            error_line(&error),
            "error: failed to prepare daemon runtime paths: permission denied"
        );
    }

    #[test]
    fn a_setup_run_another_one_holds_off_exits_temporarily_failed() {
        let busy = anyhow::Error::new(SetupError::Busy).context("setup did not finish");
        let failed = anyhow::anyhow!("permission denied");

        assert_eq!(status(&busy), SETUP_BUSY);
        assert_eq!(status(&failed), 1);
    }
}
