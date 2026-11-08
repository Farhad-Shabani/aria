use std::io::{IsTerminal, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use aria_assistant::chat::read_instructions;
use aria_assistant::prompts::{self, NewSource, PromptDraft};
use aria_daemon::{DaemonSession, Instructions, PromptCall, PromptChoice, PromptInput};
use aria_settings::{Chord, Prompt, PromptKind, Prompts, SettingsCache, SettingsStore};
use clap::{Args, Subcommand, ValueEnum};

use super::ask::{print_reply, wait_for_reply};
use super::{CliOutputMode, runtime_paths};
use crate::cli::Interrupt;

#[derive(Subcommand)]
pub(super) enum PromptCommand {
    /// List saved prompts: ID, name, kind, on or off, chord, and file
    List {
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print a prompt's instructions
    Show {
        /// The prompt's ID or name
        prompt: String,
    },
    /// Save a new prompt and print its ID
    #[command(
        long_about = "Save a new prompt and print its ID. Without --file, the instructions go in a new file in ~/.aria/prompts named after the prompt, never over a file already there. Without --file or --instructions, they are read from stdin. With --file and no --name, the prompt is named after the file."
    )]
    Add {
        /// The name Aria shows. Defaults to the file's name
        #[arg(long, required_unless_present = "file")]
        name: Option<String>,
        /// What the prompt makes of your words
        #[arg(long, value_enum, default_value_t = Kind::Rewrite)]
        kind: Kind,
        #[command(flatten)]
        source: Source,
    },
    /// Change a prompt's name, kind, file or instructions
    #[command(
        long_about = "Change a prompt's name, kind, file or instructions. New instructions replace its file's text; a file reached through a symlink keeps the link. With no option, piped stdin is the new instructions. The prompt keeps its chord and whether it is on."
    )]
    Edit {
        /// The prompt's ID or name
        prompt: String,
        /// A new name
        #[arg(long)]
        name: Option<String>,
        /// What the prompt makes of your words now
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        #[command(flatten)]
        source: Source,
    },
    /// Take a prompt off the list and leave its file where it is
    Remove {
        /// The prompt's ID or name
        prompt: String,
    },
    /// Run a prompt on text or a history entry and print the reply
    #[command(
        long_about = "Run a prompt on text or a history entry and print the reply. With --mode the reply is typed or copied instead, as a dictation is. A reply to an entry is saved under it while history is on. Esc or Ctrl-C cancels the run. Needs the daemon the app or `aria service` keeps running."
    )]
    Run(RunArgs),
}

#[derive(Args)]
pub(super) struct RunArgs {
    /// The prompt's ID or name
    #[arg(required_unless_present = "instructions")]
    prompt: Option<String>,
    /// Instructions for this run alone, run as compose, instead of a saved prompt
    #[arg(long, conflicts_with = "prompt")]
    instructions: Option<String>,
    /// The history entry to run on, by ID
    #[arg(
        long,
        value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64),
        conflicts_with = "text"
    )]
    entry: Option<u64>,
    /// The text to run on. Without it or --entry, it is read from stdin
    #[arg(long)]
    text: Option<String>,
    /// Deliver the reply like dictated text instead of printing it
    #[arg(long, value_enum)]
    mode: Option<CliOutputMode>,
}

#[derive(Args)]
pub(super) struct Source {
    /// Read the instructions from this Markdown file, where it is
    #[arg(long, conflicts_with = "instructions")]
    file: Option<PathBuf>,
    /// The instructions
    #[arg(long)]
    instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum Kind {
    /// The same words and meaning, cleaned up or restyled
    Rewrite,
    /// New text made from what was said, such as a reply or a summary
    Compose,
}

impl From<Kind> for PromptKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Rewrite => Self::Rewrite,
            Kind::Compose => Self::Compose,
        }
    }
}

/// What a source gave: a file to use where it is, or instructions for the
/// prompt's file.
struct Given {
    file: Option<PathBuf>,
    instructions: Option<Instructions>,
}

pub(super) fn run(command: PromptCommand) -> anyhow::Result<()> {
    let home = aria_settings::config::home_directory()?;
    match command {
        PromptCommand::List { json } => list(&open_prompts(&home)?.1, json),
        PromptCommand::Show { prompt } => {
            let (_store, prompts) = open_prompts(&home)?;
            let prompt = find(&prompts, &prompt)?;
            let text = read_instructions(prompt.file())?;
            print!("{}", text.as_str());
            if !text.as_str().ends_with('\n') {
                println!();
            }
        }
        PromptCommand::Add { name, kind, source } => {
            let (store, _prompts) = open_prompts(&home)?;
            let given = given(source, true)?;
            let source = match given.file {
                Some(file) => NewSource::Chosen {
                    file,
                    name,
                    instructions: given.instructions,
                },
                None => NewSource::Written {
                    name: name.context("give the prompt a --name, or a --file to name it after")?,
                    instructions: given.instructions.context(
                        "give the instructions with --instructions or --file, or pipe them on stdin",
                    )?,
                },
            };
            let saved = prompts::save(
                &home,
                &store,
                PromptDraft::New {
                    kind: kind.into(),
                    source,
                },
            )?;
            eprintln!("saved {} in {}", saved.name(), saved.file().display());
            println!("{}", saved.id().as_str());
        }
        PromptCommand::Edit {
            prompt,
            name,
            kind,
            source,
        } => {
            let (store, prompts) = open_prompts(&home)?;
            let stored = find(&prompts, &prompt)?;
            // Stdin is read only when it is the whole edit, so a scripted
            // rename never waits on a pipe nobody writes to.
            let reads_stdin = name.is_none() && kind.is_none();
            let given = given(source, reads_stdin)?;
            anyhow::ensure!(
                name.is_some()
                    || kind.is_some()
                    || given.file.is_some()
                    || given.instructions.is_some(),
                "nothing to change; give --name, --kind, --file or --instructions, or pipe new instructions"
            );
            let saved = prompts::save(
                &home,
                &store,
                PromptDraft::Edit {
                    id: stored.id().clone(),
                    name,
                    kind: kind.map(Into::into),
                    file: given.file,
                    instructions: given.instructions,
                },
            )?;
            eprintln!("saved {} in {}", saved.name(), saved.file().display());
        }
        PromptCommand::Remove { prompt } => {
            let (store, prompts) = open_prompts(&home)?;
            let stored = find(&prompts, &prompt)?;
            let removed = prompts::remove(&store, stored.id())?;
            println!(
                "Removed {}. Its file stays at {}",
                removed.name(),
                removed.file().display()
            );
        }
        PromptCommand::Run(args) => run_prompt(&home, args)?,
    }
    Ok(())
}

/// Runs through the daemon, which reads the prompt's file itself.
fn run_prompt(home: &Path, args: RunArgs) -> anyhow::Result<()> {
    let prompt = match (args.instructions, args.prompt) {
        (Some(text), _) => PromptChoice::OneOff(Instructions::new(text)?),
        (None, Some(wanted)) => {
            let (_store, prompts) = open_prompts(home)?;
            PromptChoice::Stored(find(&prompts, &wanted)?.id().clone())
        }
        (None, None) => anyhow::bail!("name a prompt, or give --instructions"),
    };
    let input = match (args.entry, args.text) {
        (Some(id), _) => PromptInput::Entry(id),
        (None, Some(text)) => PromptInput::Text(text),
        (None, None) if !std::io::stdin().is_terminal() => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("failed to read the text from stdin")?;
            PromptInput::Text(text)
        }
        (None, None) => anyhow::bail!(
            "give the text with --text or a history entry with --entry, or pipe it on stdin"
        ),
    };
    if let PromptInput::Text(text) = &input {
        anyhow::ensure!(!text.trim().is_empty(), "nothing to run the prompt on");
    }

    let paths = runtime_paths()?;
    let session = Arc::new(DaemonSession::connect_at(&paths)?);
    let mut interrupt = Interrupt::install(&session)?;
    let pending = session.run_prompt(PromptCall {
        input,
        prompt,
        application: None,
        tap_cancels: false,
        output_mode: args.mode.map(Into::into),
    })?;
    eprintln!("running {}", pending.ticket());
    let reply = wait_for_reply(pending, &mut interrupt)?;
    print_reply(&reply, args.mode);
    Ok(())
}

/// A store over the settings file with no daemon involved, and the prompts it holds.
fn open_prompts(home: &Path) -> anyhow::Result<(SettingsStore, Prompts)> {
    let cache = SettingsCache::open(aria_settings::default_path(home), home.to_path_buf())?;
    let store = SettingsStore::new(Arc::new(cache));
    let prompts = store.snapshot()?.settings.prompts;
    Ok((store, prompts))
}

/// A prompt by its ID, else by its name, ignoring case. A name two prompts share matches neither.
fn find<'a>(prompts: &'a Prompts, wanted: &str) -> anyhow::Result<&'a Prompt> {
    if let Some(prompt) = prompts.find(wanted) {
        return Ok(prompt);
    }
    let wanted_name = wanted.to_lowercase();
    let named: Vec<&Prompt> = prompts
        .entries()
        .iter()
        .filter(|prompt| prompt.name().to_lowercase() == wanted_name)
        .collect();
    match named.as_slice() {
        [prompt] => Ok(prompt),
        [] => anyhow::bail!("no prompt has the ID or name {wanted:?}; see `aria prompt list`"),
        several => {
            let ids: Vec<&str> = several.iter().map(|prompt| prompt.id().as_str()).collect();
            anyhow::bail!(
                "{} prompts are named {wanted:?}; name one by its ID: {}",
                several.len(),
                ids.join(", ")
            )
        }
    }
}

/// Piped stdin is read only when neither flag is given and `reads_stdin`
/// allows, and blank stdin gives nothing.
fn given(source: Source, reads_stdin: bool) -> anyhow::Result<Given> {
    if let Some(file) = source.file {
        let file = std::path::absolute(&file)
            .with_context(|| format!("failed to resolve {}", file.display()))?;
        return Ok(Given {
            file: Some(file),
            instructions: None,
        });
    }
    let text = match source.instructions {
        Some(text) => Some(text),
        None if !reads_stdin || std::io::stdin().is_terminal() => None,
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("failed to read the instructions from stdin")?;
            Some(text).filter(|text| !text.trim().is_empty())
        }
    };
    Ok(Given {
        file: None,
        instructions: text.map(Instructions::new).transpose()?,
    })
}

fn list(prompts: &Prompts, json: bool) {
    if json {
        let entries: Vec<serde_json::Value> = prompts.entries().iter().map(prompt_json).collect();
        println!("{}", serde_json::Value::Array(entries));
        return;
    }
    if prompts.entries().is_empty() {
        println!("no prompts");
    }
    for prompt in prompts.entries() {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            prompt.id().as_str(),
            prompt.name(),
            prompt.kind().as_str(),
            if prompt.enabled() { "on" } else { "off" },
            prompt.chord().map_or_else(|| "-".to_owned(), chord_label),
            prompt.file().display()
        );
    }
}

fn prompt_json(prompt: &Prompt) -> serde_json::Value {
    serde_json::json!({
        "id": prompt.id().as_str(),
        "name": prompt.name(),
        "kind": prompt.kind().as_str(),
        "enabled": prompt.enabled(),
        "chord": prompt.chord().map(|chord| {
            let modifiers = chord.modifiers();
            serde_json::json!({
                "key_code": chord.key_code(),
                "control": modifiers.control,
                "option": modifiers.option,
                "shift": modifiers.shift,
                "command": modifiers.command,
            })
        }),
        "file": prompt.file(),
    })
}

/// The modifiers by name, and the key by its code: key names belong to the
/// keyboard layout, which the app reads.
fn chord_label(chord: Chord) -> String {
    let modifiers = chord.modifiers();
    let mut parts: Vec<String> = [
        (modifiers.control, "control"),
        (modifiers.option, "option"),
        (modifiers.shift, "shift"),
        (modifiers.command, "command"),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .map(|(_, name)| name.to_owned())
    .collect();
    parts.push(format!("key {}", chord.key_code()));
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use aria_settings::PromptId;
    use clap::Parser;

    fn parse(arguments: &[&str]) -> PromptCommand {
        match Cli::try_parse_from(arguments).expect("parses").command {
            Command::Prompt { command } => command,
            _ => panic!("not a prompt command"),
        }
    }

    fn prompts(names: &[(&str, &str)]) -> Prompts {
        Prompts::new(
            names
                .iter()
                .map(|(id, name)| {
                    Prompt::new(
                        PromptId::new(*id).expect("id"),
                        (*name).to_owned(),
                        PromptKind::Rewrite,
                        PathBuf::from("/prompts/p.md"),
                        true,
                        None,
                    )
                    .expect("prompt")
                })
                .collect(),
        )
        .expect("prompts")
    }

    #[test]
    fn prompt_commands_parse() {
        assert!(matches!(
            parse(&["aria", "prompt", "list", "--json"]),
            PromptCommand::List { json: true }
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "show", "Summary"]),
            PromptCommand::Show { prompt } if prompt == "Summary"
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "add", "--name", "Summary", "--instructions", "Three bullets."]),
            PromptCommand::Add {
                name: Some(name),
                kind: Kind::Rewrite,
                source: Source { file: None, instructions: Some(text) },
            } if name == "Summary" && text == "Three bullets."
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "add", "--name", "Reply", "--kind", "compose", "--file", "reply.md"]),
            PromptCommand::Add {
                kind: Kind::Compose,
                source: Source { file: Some(file), instructions: None },
                ..
            } if file == Path::new("reply.md")
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "add", "--file", "reply.md"]),
            PromptCommand::Add {
                name: None,
                source: Source { file: Some(_), .. },
                ..
            }
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "edit", "Summary", "--name", "Brief"]),
            PromptCommand::Edit { prompt, name: Some(name), kind: None, .. }
                if prompt == "Summary" && name == "Brief"
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "remove", "Summary"]),
            PromptCommand::Remove { prompt } if prompt == "Summary"
        ));
        assert!(matches!(
            parse(&["aria", "prompt", "run", "Summary", "--entry", "42"]),
            PromptCommand::Run(RunArgs { prompt: Some(prompt), instructions: None, entry: Some(42), text: None, mode: None })
                if prompt == "Summary"
        ));
        let PromptCommand::Run(RunArgs {
            mode: Some(mode), ..
        }) = parse(&[
            "aria",
            "prompt",
            "run",
            "Summary",
            "--text",
            "Hi.",
            "--mode",
            "insert-or-copy",
        ])
        else {
            panic!("expected an output mode");
        };
        assert_eq!(
            aria_daemon::OutputMode::from(mode),
            aria_daemon::OutputMode::InsertOrCopy
        );
        assert!(matches!(
            parse(&["aria", "prompt", "run", "--instructions", "List the action items", "--text", "We met."]),
            PromptCommand::Run(RunArgs { prompt: None, instructions: Some(_), entry: None, text: Some(text), .. })
                if text == "We met."
        ));
        for arguments in [
            vec!["aria", "prompt", "run"],
            vec![
                "aria",
                "prompt",
                "run",
                "Summary",
                "--instructions",
                "Both.",
            ],
            vec![
                "aria", "prompt", "run", "Summary", "--entry", "1", "--text", "Both.",
            ],
            vec!["aria", "prompt", "run", "Summary", "--entry", "0"],
            vec!["aria", "prompt", "add", "--instructions", "No name."],
            vec![
                "aria",
                "prompt",
                "add",
                "--name",
                "N",
                "--file",
                "a.md",
                "--instructions",
                "b",
            ],
            vec!["aria", "prompt", "add", "--name", "N", "--kind", "summary"],
            vec!["aria", "prompt", "edit"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn a_prompt_is_found_by_id_or_by_its_name_in_any_case() {
        let prompts = prompts(&[("a1", "Summary"), ("b2", "Reply")]);

        assert_eq!(find(&prompts, "b2").expect("by id").name(), "Reply");
        assert_eq!(
            find(&prompts, "summary").expect("by name").id().as_str(),
            "a1"
        );
        assert!(find(&prompts, "Translate").is_err());
    }

    #[test]
    fn a_name_two_prompts_share_is_refused_with_both_ids() {
        let prompts = prompts(&[("a1", "Summary"), ("b2", "summary")]);

        let error = find(&prompts, "SUMMARY")
            .expect_err("ambiguous")
            .to_string();

        assert!(error.contains("a1") && error.contains("b2"), "{error}");
    }

    #[test]
    fn a_chord_names_its_modifiers_and_key_code() {
        let chord = Chord::new(
            1,
            aria_settings::Modifiers {
                control: true,
                command: true,
                ..Default::default()
            },
        )
        .expect("chord");

        assert_eq!(chord_label(chord), "control+command+key 1");
    }
}
