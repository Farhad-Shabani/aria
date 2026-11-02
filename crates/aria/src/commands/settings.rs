use anyhow::Context;
use clap::Subcommand;

#[derive(Subcommand)]
pub(super) enum SettingsCommand {
    /// Print each setting: its value, set or default, where it comes from,
    /// and whether it applies live or after a restart
    Show {
        /// One key, such as tts.rate
        key: Option<String>,
        /// Print JSON instead
        #[arg(long)]
        json: bool,
    },
    /// Write keys to settings.toml in one change, whether or not the daemon
    /// runs. A switch takes on or off; other values read as TOML
    Set {
        /// KEY VALUE, repeated for keys that need each other
        #[arg(required = true, num_args = 2.., value_names = ["KEY", "VALUE"])]
        pairs: Vec<String>,
        /// Print JSON instead
        #[arg(long)]
        json: bool,
    },
    /// Remove a key from settings.toml, so its default applies
    Reset { key: String },
    /// Create settings.toml if it is missing, and print its path
    Create,
    /// Check settings.toml without a daemon
    Check {
        /// Print JSON instead
        #[arg(long)]
        json: bool,
    },
    /// Print where settings.toml is, past any symlink
    Path,
    /// Print a JSON Schema for settings.toml, for editors
    Schema,
}

pub(super) fn run(command: SettingsCommand) -> anyhow::Result<()> {
    match command {
        SettingsCommand::Show { key, json } => {
            let pending = aria_daemon::restart_required_keys()
                .context("cannot read the running daemon's settings status")?;
            let rows = aria_settings::command::show(&pending, key.as_deref())?;
            if json {
                let rows = rows.iter().map(row_json).collect();
                println!("{}", serde_json::Value::Array(rows));
            } else {
                print_rows(&rows);
            }
            Ok(())
        }
        SettingsCommand::Set { pairs, json } => {
            report_write(aria_settings::command::set(&pairs)?, json)
        }
        SettingsCommand::Reset { key } => report_write(aria_settings::command::reset(&key)?, false),
        SettingsCommand::Check { json } => {
            let fault = aria_settings::command::validate()?;
            if json {
                let report = serde_json::json!({
                    "valid": fault.is_none(),
                    "error": fault.as_ref().map(fault_json),
                });
                println!("{report}");
            } else if let Some(fault) = &fault {
                eprintln!("settings.toml is invalid: {}", fault_line(fault));
            } else {
                println!("settings.toml is valid");
            }
            if fault.is_some() {
                std::process::exit(1);
            }
            Ok(())
        }
        SettingsCommand::Create => {
            println!("{}", aria_settings::command::create()?.display());
            Ok(())
        }
        SettingsCommand::Path => {
            println!("{}", aria_settings::command::path()?.display());
            Ok(())
        }
        SettingsCommand::Schema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&aria_settings::command::schema())?
            );
            Ok(())
        }
    }
}

fn apply_label(apply: aria_settings::Apply) -> &'static str {
    match apply {
        aria_settings::Apply::Live => "live",
        aria_settings::Apply::Restart => "restart",
    }
}

/// Text unquoted, everything else as JSON, absent as a dash.
fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "-".to_owned(),
        serde_json::Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}

fn print_rows(rows: &[aria_settings::command::Row]) {
    let values: Vec<String> = rows.iter().map(|row| value_text(&row.value)).collect();
    let key_width = rows
        .iter()
        .map(|row| row.path().len())
        .max()
        .unwrap_or_default();
    let value_width = values.iter().map(String::len).max().unwrap_or_default();
    for (row, value) in rows.iter().zip(&values) {
        let state = if row.set { "set" } else { "default" };
        let pending = if row.restart_required {
            ", restart required"
        } else {
            ""
        };
        println!(
            "{key:key_width$}  {value:value_width$}  {state}, {apply}{pending}",
            key = row.path(),
            apply = apply_label(row.apply()),
        );
    }
    if let [row] = rows {
        println!("{}", row.description());
    }
}

fn row_json(row: &aria_settings::command::Row) -> serde_json::Value {
    serde_json::json!({
        "key": row.path(),
        "value": row.value,
        "state": if row.set { "set" } else { "default" },
        "apply": apply_label(row.apply()),
        "restart_required": row.restart_required,
        "description": row.description(),
    })
}

fn fault_json(fault: &aria_settings::SettingsFault) -> serde_json::Value {
    serde_json::json!({
        "message": fault.message,
        "line": fault.position.map(|position| position.line),
        "column": fault.position.map(|position| position.column),
    })
}

fn fault_line(fault: &aria_settings::SettingsFault) -> String {
    match fault.position {
        Some(position) => format!("{position}: {}", fault.message),
        None => fault.message.clone(),
    }
}

/// `applied`, `applied, restart key changed`, or the refusal, which exits 1.
fn report_write(outcome: aria_settings::command::Outcome, json: bool) -> anyhow::Result<()> {
    let (restart_key_changed, fault) = match &outcome {
        aria_settings::command::Outcome::Applied {
            restart_key_changed,
        } => (*restart_key_changed, None),
        aria_settings::command::Outcome::Refused(fault) => (false, Some(fault)),
    };
    if json {
        let report = serde_json::json!({
            "applied": fault.is_none(),
            "restart_key_changed": restart_key_changed,
            "error": fault.map(fault_json),
        });
        println!("{report}");
    } else if let Some(fault) = fault {
        eprintln!("refused: {}", fault_line(fault));
    } else if restart_key_changed {
        println!("applied, restart key changed");
    } else {
        println!("applied");
    }
    if fault.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn settings_commands_parse() {
        let parse = |args: &[&str]| {
            let Command::Settings { command } = Cli::try_parse_from(args).unwrap().command else {
                panic!("{args:?} is a settings command");
            };
            command
        };
        assert!(matches!(
            parse(&["aria", "settings", "show"]),
            SettingsCommand::Show {
                key: None,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["aria", "settings", "show", "tts.rate", "--json"]),
            SettingsCommand::Show { key: Some(key), json: true } if key == "tts.rate"
        ));
        assert!(matches!(
            parse(&["aria", "settings", "set", "phone.enabled", "on", "--json"]),
            SettingsCommand::Set { pairs, json: true } if pairs == ["phone.enabled", "on"]
        ));
        assert!(matches!(
            parse(&["aria", "settings", "set", "shortcuts.ask", "{ key_code = 49, shift = true }"]),
            SettingsCommand::Set { pairs, .. } if pairs[1] == "{ key_code = 49, shift = true }"
        ));
        assert!(matches!(
            parse(&["aria", "settings", "reset", "memory.enabled"]),
            SettingsCommand::Reset { key } if key == "memory.enabled"
        ));
        assert!(matches!(
            parse(&["aria", "settings", "check", "--json"]),
            SettingsCommand::Check { json: true }
        ));
        assert!(matches!(
            parse(&["aria", "settings", "create"]),
            SettingsCommand::Create
        ));
        assert!(matches!(
            parse(&["aria", "settings", "path"]),
            SettingsCommand::Path
        ));
        assert!(matches!(
            parse(&["aria", "settings", "schema"]),
            SettingsCommand::Schema
        ));
        for args in [
            vec!["aria", "settings", "set", "phone.enabled"],
            vec!["aria", "settings", "reset"],
            vec!["aria", "settings", "show", "a", "b"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn phone_pairing_parses_as_the_app_sends_it() {
        assert!(matches!(
            Cli::try_parse_from(["aria", "settings", "set", "phone.enabled", "off"])
                .unwrap()
                .command,
            Command::Settings {
                command: SettingsCommand::Set { pairs, json: false }
            } if pairs == ["phone.enabled", "off"]
        ));
    }
}
