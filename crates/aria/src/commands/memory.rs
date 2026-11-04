use std::path::PathBuf;

use aria_assistant::memory::Store;
use clap::Subcommand;
use tracing_subscriber::EnvFilter;

use super::answer;

#[derive(Subcommand)]
pub(super) enum MemoryCommand {
    /// List remembered facts
    List {
        /// Show facts waiting for review instead
        #[arg(long)]
        pending: bool,
    },
    /// Show where a fact came from
    Why { id: i64 },
    /// Delete a fact for good
    Forget { id: i64 },
    /// Delete every fact, learned substitution, and remembered ask
    Clear,
    /// Approve or reject each fact waiting for review
    Review,
    /// Extract facts from every session with new turns, now
    Consolidate,
    /// Show the word list and the substitutions learned from it
    Lexicon {
        /// Delete one learned substitution by id
        #[arg(long)]
        forget_pair: Option<i64>,
        /// Print terms and pairs as one JSON object instead
        #[arg(long, conflicts_with = "forget_pair")]
        json: bool,
    },
}

fn open_store() -> anyhow::Result<(Store, PathBuf)> {
    let home = aria_settings::config::home_directory()?;
    let settings = aria_settings::load(&home)?;
    let dir = aria_settings::config::memory_directory(&settings.memory, &home);
    let store = aria_assistant::memory::open(&dir)?;
    Ok((store, dir))
}

pub(super) fn run(command: MemoryCommand) -> anyhow::Result<()> {
    match command {
        MemoryCommand::List { pending } => {
            let (store, _) = open_store()?;
            let facts = match pending {
                true => store.pending_facts()?,
                false => store.active_facts()?,
            };
            if facts.is_empty() {
                println!(
                    "{}",
                    if pending {
                        "No pending facts"
                    } else {
                        "No memories yet"
                    }
                );
            }
            for fact in facts {
                println!("{:>4}  [{}]  {}", fact.id, fact.kind, fact.text);
            }
        }
        MemoryCommand::Why { id } => {
            let (store, _) = open_store()?;
            let fact = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("no fact with id {id}"))?;
            println!("[{}] {}", fact.kind, fact.text);
            println!("  said: \"{}\"", fact.source_quote);
            println!(
                "  session: {}  confidence: {}",
                fact.source_session, fact.confidence
            );
            println!("  status: {}  created: {}", fact.status, fact.created_at);
        }
        MemoryCommand::Forget { id } => {
            let (store, dir) = open_store()?;
            let fact = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("no fact with id {id}"))?;
            if answer(&format!("Forget \"{}\"? [y/N] ", fact.text))? != "y" {
                return Ok(());
            }
            store.forget(id)?;
            aria_assistant::memory::write_profile(&store, &dir)?;
            println!("Forgotten");
        }
        MemoryCommand::Clear => {
            let (store, dir) = open_store()?;
            if answer("Delete every fact, learned substitution, and remembered ask? [y/N] ")? != "y"
            {
                return Ok(());
            }
            store.clear()?;
            aria_assistant::memory::write_profile(&store, &dir)?;
            println!("Memory cleared");
        }
        MemoryCommand::Review => {
            let (store, dir) = open_store()?;
            let pending = store.pending_facts()?;
            if pending.is_empty() {
                println!("Nothing to review");
                return Ok(());
            }
            for fact in pending {
                println!(
                    "\n[{}] {}\n  said: \"{}\"",
                    fact.kind, fact.text, fact.source_quote
                );
                match answer("  (a)pprove / (r)eject / (s)kip [s]: ")?.as_str() {
                    "a" => {
                        store.approve(fact.id)?;
                    }
                    "r" => {
                        store.forget(fact.id)?;
                    }
                    _ => {}
                }
            }
            aria_assistant::memory::write_profile(&store, &dir)?;
        }
        MemoryCommand::Lexicon { forget_pair, json } => {
            let (store, dir) = open_store()?;
            if let Some(id) = forget_pair {
                anyhow::ensure!(store.forget_pair(id)?, "No pair with id {id}");
                println!("Pair forgotten");
                return Ok(());
            }
            let path = dir.join(aria_assistant::memory::LEXICON_FILE);
            let terms = aria_assistant::memory::load_terms(&path);
            if json {
                println!("{}", lexicon_json(&terms, &store.list_pairs()?));
                return Ok(());
            }
            if terms.is_empty() {
                println!("No lexicon terms ({} missing or empty)", path.display());
            } else {
                println!("Terms ({}):", path.display());
                for term in terms {
                    println!("  {term}");
                }
            }
            let pairs = store.list_pairs()?;
            if !pairs.is_empty() {
                println!("Derived pairs:");
                for pair in pairs {
                    println!(
                        "  {:>4}  {} → {}  [{}]",
                        pair.id, pair.misheard, pair.correct, pair.source
                    );
                }
            }
        }
        MemoryCommand::Consolidate => consolidate()?,
    }
    Ok(())
}

fn lexicon_json(
    terms: &[String],
    pairs: &[aria_assistant::memory::DerivedPair],
) -> serde_json::Value {
    let pairs: Vec<_> = pairs
        .iter()
        .map(|pair| {
            serde_json::json!({
                "id": pair.id,
                "misheard": pair.misheard,
                "correct": pair.correct,
                "source": pair.source,
            })
        })
        .collect();
    serde_json::json!({ "terms": terms, "pairs": pairs })
}

fn consolidate() -> anyhow::Result<()> {
    // Extraction reports what it skipped as warnings; here they belong on
    // the terminal, not in a daemon log.
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .compact()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    let home = aria_settings::config::home_directory()?;
    let settings = aria_settings::load(&home)?;
    let report = aria_assistant::memory::run_backlog(
        aria_daemon::config::load_memory(&settings.memory, &home),
        &home,
        &aria_kit::cancel::CancelToken::never(),
    )?;
    let results = report.results;
    if results.is_empty() && report.failed == 0 {
        println!("Nothing to consolidate (or memory disabled)");
        return Ok(());
    }
    let added: u32 = results.iter().map(|result| result.added).sum();
    let pending: u32 = results.iter().map(|result| result.pending).sum();
    println!(
        "Consolidated {} session(s): {added} added, {pending} pending",
        results.len()
    );
    if pending > 0 {
        println!("Run `aria memory review` to triage pending facts");
    }
    anyhow::ensure!(
        report.failed == 0,
        "{} session(s) could not be consolidated; see warnings above",
        report.failed
    );
    Ok(())
}
