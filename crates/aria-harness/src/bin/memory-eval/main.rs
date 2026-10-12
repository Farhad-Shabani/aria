use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;

mod compare;
mod corpus;
mod judge;

#[derive(Parser)]
#[command(about = "Prepare, grade, and compare reference memory evaluations")]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Prepare {
        dataset: PathBuf,
        output: PathBuf,
    },
    Export {
        report: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        repetition: usize,
        #[arg(long, value_enum)]
        condition: Condition,
        #[arg(long)]
        allow_extraction_failures: bool,
    },
    Judge {
        references: PathBuf,
        hypotheses: PathBuf,
        rubric: PathBuf,
        output: PathBuf,
        #[arg(long, default_value = "opus")]
        model: String,
    },
    Compare {
        references: PathBuf,
        before: PathBuf,
        after: PathBuf,
        output: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Condition {
    Before,
    After,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let output = match &cli.command {
        Operation::Prepare { dataset, output } => {
            corpus::prepare(dataset, output)?;
            output
        }
        Operation::Export {
            report,
            output,
            repetition,
            condition,
            allow_extraction_failures,
        } => {
            corpus::export(
                report,
                output,
                *repetition,
                matches!(condition, Condition::After),
                *allow_extraction_failures,
            )?;
            output
        }
        Operation::Judge {
            references,
            hypotheses,
            rubric,
            output,
            model,
        } => {
            ensure!(!model.trim().is_empty(), "model must be nonempty text");
            judge::run(references, hypotheses, rubric, output, model)?;
            output
        }
        Operation::Compare {
            references,
            before,
            after,
            output,
        } => {
            compare::run(references, before, after, output)?;
            output
        }
    };
    println!("{}", output.display());
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).with_context(|| path.display().to_string())?)
        .with_context(|| format!("invalid JSON: {}", path.display()))
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(path)
        .with_context(|| path.display().to_string())?
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON: {}:{}", path.display(), index + 1))
        })
        .collect()
}

fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .context("expected nonempty text")
}

fn index_rows<'a>(rows: &'a [Value], name: &str) -> Result<BTreeMap<&'a str, &'a Value>> {
    ensure!(!rows.is_empty(), "{name} must be a nonempty array");
    let mut indexed = BTreeMap::new();
    for row in rows {
        let id = text(&row["question_id"])?;
        ensure!(
            indexed.insert(id, row).is_none(),
            "duplicate {name} question ID: {id}"
        );
    }
    Ok(indexed)
}

fn metadata_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".metadata.json");
    PathBuf::from(name)
}
