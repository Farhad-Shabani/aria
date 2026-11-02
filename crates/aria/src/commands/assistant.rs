use aria_daemon::{AssistantId, DaemonSession};
use aria_platform::runtime::PreparedRuntimePaths;
use clap::Subcommand;

use super::assistant_parser;

#[derive(Subcommand)]
pub(super) enum AssistantCommand {
    /// List the models the installed assistant offers. Choose one with
    /// `aria settings set assistant.models.ASSISTANT MODEL_ID`
    Models {
        /// Which assistant's models; defaults to the selected assistant
        #[arg(long = "assistant", value_parser = assistant_parser(), value_name = "ASSISTANT")]
        assistant_id: Option<AssistantId>,
        /// Print the model list as JSON
        #[arg(long)]
        json: bool,
    },
}

pub(super) fn models(assistant_id: Option<AssistantId>, json: bool) -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()?;
    let session = DaemonSession::connect_at(&paths)?;
    let assistant_id = match assistant_id {
        Some(assistant_id) => assistant_id,
        None => session.get_settings()?.settings.resolve_assistant_id(),
    };
    let models = session.list_assistant_models(assistant_id)?;
    if json {
        println!("{}", serde_json::to_string(&models)?);
    } else {
        println!("Default");
        for model in models {
            println!("{}\t{}", model.id, model.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn assistant_models_lists_and_never_writes() {
        assert!(matches!(
            Cli::try_parse_from(["aria", "assistant", "models", "--json"])
                .unwrap()
                .command,
            Command::Assistant {
                command: AssistantCommand::Models { json: true, .. },
            }
        ));
        for args in [
            vec!["aria", "assistant", "model", "--list"],
            vec!["aria", "assistant", "models", "example"],
            vec!["aria", "assistant", "models", "--default"],
            vec!["aria", "assistant", "codex"],
            vec!["aria", "assistant"],
            vec!["aria", "model"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }
}
