use std::io::IsTerminal;

use crate::setup::{Enrolled, Grant, Options as SetupOptions, SettingsWrite, TryOut};
use anyhow::Context;
use clap::Subcommand;

use super::service::service_status_message;

/// Aria.app's side of setup: `aria.setup.v1` protobuf frames on stdio,
/// never a terminal.
#[derive(Subcommand)]
pub(super) enum SetupCommand {
    /// Write one Inspection
    #[command(hide = true)]
    Inspect,
    /// Read one ApplyRequest, write ApplyResponses until the summary
    #[command(hide = true)]
    Apply,
}

pub(super) fn run(yes: bool, skip_models: bool) -> anyhow::Result<()> {
    let options = SetupOptions {
        yes,
        skip_models,
        interactive: std::io::stdin().is_terminal(),
        color: std::io::stdout().is_terminal(),
    };
    let summary = crate::setup::run(
        options,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .context("setup did not finish")?;
    let selections = &summary.selections;
    println!();
    println!("speech       {}", selections.stt);
    println!("voice        {}", selections.tts);
    println!("wake word    {}", on_off(selections.wake));
    println!("hotkey       {}", on_off(selections.aria_key));
    println!("settings     {}", settings_write_message(&summary.settings));
    match &summary.wakeword {
        Some(dir) => println!("wake model   {}", dir.display()),
        None => println!("wake model   none shipped; set wake.model to your own"),
    }
    for (name, outcome) in &summary.models {
        println!("{name:<12} {}", outcome.as_str());
    }
    if let Some(unit) = &summary.legacy_agent {
        println!("removed      {} (the Python wake agent)", unit.display());
    }
    println!("{}", service_status_message(&summary.service));
    if let Some(voice) = enrolled_message(&summary.enrolled) {
        println!("voice        {voice}");
    }
    if let Some(press) = try_out_message(&summary.tried) {
        println!("test press   {press}");
    }
    if summary.grant == Grant::Waiting {
        println!("next         {}", crate::doctor::hotkey_unarmed_reason());
    }
    if cfg!(target_os = "macos") {
        println!("next         open Aria for the menu bar, then run `aria doctor`");
    } else {
        println!("next         run `aria doctor`");
    }
    Ok(())
}

pub(super) fn run_for_app(command: SetupCommand) -> anyhow::Result<()> {
    anyhow::ensure!(
        !std::io::stdout().is_terminal(),
        "this writes protobuf for Aria.app; run `aria setup` instead"
    );
    let mut stdout = std::io::stdout().lock();
    match command {
        SetupCommand::Inspect => crate::setup::wire::inspect(&mut stdout),
        SetupCommand::Apply => crate::setup::wire::apply(&mut std::io::stdin().lock(), &mut stdout),
    }
    .context("setup did not finish")
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn settings_write_message(write: &SettingsWrite) -> String {
    match write {
        SettingsWrite::Unchanged => "unchanged".to_owned(),
        SettingsWrite::Written => "written".to_owned(),
    }
}

/// What to say about the voice, or nothing when there is nothing to report.
/// A run that never asked stays quiet: the reason is usually that no model
/// was fetched, and setup already printed that line.
fn enrolled_message(enrolled: &Enrolled) -> Option<String> {
    match enrolled {
        Enrolled::NotOffered => None,
        Enrolled::Declined => {
            Some("skipped; enroll later with `aria speaker enroll <name>`".into())
        }
        Enrolled::Done { name } => Some(format!("saved for {name}")),
        Enrolled::Failed(why) => Some(format!("not enrolled: {why}")),
    }
}

/// What the test press proved, or nothing when there was none.
fn try_out_message(tried: &TryOut) -> Option<String> {
    match tried {
        TryOut::NotOffered => None,
        TryOut::Declined => Some("skipped; try one with `aria dictate`".into()),
        TryOut::Done { heard, spoke } if *spoke => Some(format!("heard {heard:?}")),
        TryOut::Done { heard, .. } => Some(format!("heard {heard:?}, but the voice did not play")),
        TryOut::Failed(why) => Some(format!("did not work: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn setup_takes_its_two_flags() {
        let parse = |arguments: &[&str]| {
            let cli = Cli::try_parse_from(arguments).expect("the setup command should parse");
            let Command::Setup {
                command: None,
                yes,
                skip_models,
            } = cli.command
            else {
                panic!("a setup invocation should parse as Setup");
            };
            (yes, skip_models)
        };

        assert_eq!(parse(&["aria", "setup"]), (false, false));
        assert_eq!(
            parse(&["aria", "setup", "--yes", "--skip-models"]),
            (true, true)
        );
    }

    #[test]
    fn setup_has_hidden_subcommands_for_the_app() {
        let cli = Cli::try_parse_from(["aria", "setup", "inspect"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                command: Some(SetupCommand::Inspect),
                ..
            }
        ));
        let cli = Cli::try_parse_from(["aria", "setup", "apply"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                command: Some(SetupCommand::Apply),
                ..
            }
        ));
    }
}
