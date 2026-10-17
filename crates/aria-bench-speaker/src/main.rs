//! ```text
//! aria-bench-speaker <trials.txt> [model.onnx]
//! ```
//!
//! The model defaults to where `aria setup` puts it.

use std::path::PathBuf;
use std::process::ExitCode;

use aria_bench_speaker::{Report, measure, read_trials};
use aria_kit::speaker::{CLUSTER_THRESHOLD, ENROLLED_THRESHOLD, OWN_VOICE_THRESHOLD};

/// The thresholds the application runs with, labeled by constant name.
const GATES: [(&str, f32); 3] = [
    ("ENROLLED_THRESHOLD", ENROLLED_THRESHOLD),
    ("OWN_VOICE_THRESHOLD", OWN_VOICE_THRESHOLD),
    ("CLUSTER_THRESHOLD", CLUSTER_THRESHOLD),
];

/// Candidate enrollment consistency thresholds for comparing rejection rates.
const CONSISTENCY_FLOORS: [f32; 6] = [0.30, 0.35, 0.40, 0.45, 0.50, 0.55];

const DEFAULT_MODEL: &str = ".aria/models/wespeaker/en/model.onnx";

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(trials), model) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: aria-bench-speaker <trials.txt> [model.onnx]");
        return ExitCode::FAILURE;
    };
    let model = match model.map(PathBuf::from).or_else(default_model) {
        Some(model) => model,
        None => {
            eprintln!("no model given, and no home directory to look in");
            return ExitCode::FAILURE;
        }
    };

    match run(&PathBuf::from(trials), &model) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Error: {error}");
            let mut cause = std::error::Error::source(&error);
            while let Some(error) = cause {
                eprintln!("  {error}");
                cause = error.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(
    trials: &std::path::Path,
    model: &std::path::Path,
) -> Result<Report, aria_bench_speaker::VerifyError> {
    let trials = read_trials(trials)?;
    let measured = measure(model, &trials)?;
    Ok(Report::new(&measured, &GATES, &CONSISTENCY_FLOORS))
}

fn default_model() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_MODEL))
}
