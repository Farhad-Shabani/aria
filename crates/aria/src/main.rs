use std::process::ExitCode;

use aria::commands::{self, Cli};
use aria::primitives::error_chain;
use clap::Parser;

fn main() -> ExitCode {
    match commands::run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error_line(&error));
            ExitCode::FAILURE
        }
    }
}

/// One line, as cargo and git print theirs: anyhow's own report spreads
/// the causes over a `Caused by:` block.
fn error_line(error: &anyhow::Error) -> String {
    format!("error: {}", error_chain(error.as_ref()))
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
}
