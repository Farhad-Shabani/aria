use clap::Subcommand;

#[derive(Subcommand)]
pub(super) enum ModelsCommand {
    /// Show each model, its size, and what uses it
    List {
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
    },
    /// Delete a downloaded model that is not in use
    #[command(visible_alias = "rm")]
    Remove { name: String },
}

pub(super) fn run(command: ModelsCommand) -> anyhow::Result<()> {
    match command {
        ModelsCommand::List { json } => {
            let models = crate::setup::models::list()?;
            if json {
                let models: Vec<_> = models.iter().map(model_json).collect();
                println!("{}", serde_json::Value::Array(models));
                return Ok(());
            }
            for model in models {
                let used_by = if model.used_by.is_empty() {
                    String::new()
                } else {
                    format!("in use: {}", crate::setup::tables(&model.used_by))
                };
                let row = format!(
                    "{:<16}{:>8}   {:<11} {used_by}",
                    model.name,
                    crate::setup::size(model.bytes),
                    model.files.as_str()
                );
                println!("{}", row.trim_end());
            }
        }
        ModelsCommand::Remove { name } => {
            let freed = crate::setup::models::remove(&name)?;
            println!("removed {name}, freed {}", crate::setup::size(freed));
        }
    }
    Ok(())
}

fn model_json(model: &crate::setup::models::Model) -> serde_json::Value {
    use crate::setup::models::ModelFiles;
    let files = match model.files {
        ModelFiles::Complete => "complete",
        ModelFiles::Partial => "partial",
        ModelFiles::Absent => "absent",
        ModelFiles::External => "external",
    };
    serde_json::json!({
        "name": model.name,
        "bytes": model.bytes,
        "files": files,
        "used_by": model.used_by.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn models_remove_parses_a_name() {
        for verb in ["remove", "rm"] {
            assert!(matches!(
                Cli::try_parse_from(["aria", "models", verb, "parakeet"])
                    .unwrap()
                    .command,
                Command::Models { command: ModelsCommand::Remove { name } } if name == "parakeet"
            ));
        }
    }
}
