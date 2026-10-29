//! Machine-local test inputs passed through the environment.
//!
//! Fixture paths (a model, a recording) differ per machine, so they arrive
//! as environment variables rather than checked-in configuration. An unset
//! variable yields `None` so the caller can skip instead of failing.

use std::env::VarError;
use std::path::PathBuf;

const RUNS_ENV: &str = "ARIA_BENCH_RUNS";

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("HOME is not set; cannot expand ~ in {variable}")]
    HomeNotSet { variable: String },

    #[error("{RUNS_ENV} must be a positive integer, got {value:?}")]
    InvalidRunCount { value: String },

    #[error("{variable} must be 0 or 1")]
    InvalidSwitch { variable: String },
}

/// The run count the environment asks for, or `default`. Zero and non-numbers
/// are rejected.
pub fn run_count(default: u32) -> Result<u32, InputError> {
    match std::env::var(RUNS_ENV) {
        Ok(value) => match value.parse() {
            Ok(runs) if runs > 0 => Ok(runs),
            _ => Err(InputError::InvalidRunCount { value }),
        },
        Err(_) => Ok(default),
    }
}

/// A missing fixture skips the bench instead of failing it.
pub fn fixture(bench: &str, variable: &str) -> Result<Option<PathBuf>, InputError> {
    let path = path_input(variable)?;
    if path.is_none() {
        println!("skipping the {bench} bench: {variable} is not set");
    }
    Ok(path)
}

/// A path from the environment, with a leading `~` expanded against the
/// real home. Unset or blank means the machine has no such fixture.
pub fn path_input(variable: &str) -> Result<Option<PathBuf>, InputError> {
    match std::env::var(variable) {
        Ok(value) if !value.trim().is_empty() => expand_home(&value, variable).map(Some),
        _ => Ok(None),
    }
}

/// Whether the environment turns `variable` on. Unset is off; anything but
/// `0` or `1` is rejected rather than read as either.
pub fn switch(variable: &str) -> Result<bool, InputError> {
    parse_switch(variable, std::env::var(variable))
}

fn parse_switch(variable: &str, value: Result<String, VarError>) -> Result<bool, InputError> {
    match value.as_deref() {
        Err(VarError::NotPresent) | Ok("0") => Ok(false),
        Ok("1") => Ok(true),
        _ => Err(InputError::InvalidSwitch {
            variable: variable.to_owned(),
        }),
    }
}

fn expand_home(value: &str, variable: &str) -> Result<PathBuf, InputError> {
    let home = || {
        std::env::var("HOME").map_err(|_unset| InputError::HomeNotSet {
            variable: variable.to_owned(),
        })
    };
    if value == "~" {
        return Ok(PathBuf::from(home()?));
    }
    match value.strip_prefix("~/") {
        Some(relative) => Ok(PathBuf::from(home()?).join(relative)),
        None => Ok(PathBuf::from(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_off_unless_set_to_one_and_rejects_anything_else() {
        let parse = |value| parse_switch("ARIA_BENCH_SWITCH", value);
        assert!(!parse(Err(VarError::NotPresent)).unwrap());
        assert!(!parse(Ok("0".into())).unwrap());
        assert!(parse(Ok("1".into())).unwrap());
        for invalid in ["", "true", "false", "2", " 1"] {
            assert!(parse(Ok(invalid.into())).is_err());
        }
        assert!(parse(Err(VarError::NotUnicode("1".into()))).is_err());
    }
}
