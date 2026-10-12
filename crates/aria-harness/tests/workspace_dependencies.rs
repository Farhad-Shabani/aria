use std::collections::BTreeSet;
use std::io::Seek;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aria_primitives::process::{ChildProcess, WaitOutcome};
use serde::Deserialize;

#[derive(Deserialize)]
struct Workspace {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
}

fn forbidden_dependencies(workspace: &Workspace) -> Vec<String> {
    let members: BTreeSet<_> = workspace
        .packages
        .iter()
        .map(|package| &package.name)
        .collect();
    let mut violations = Vec::new();
    for package in &workspace.packages {
        let allowed: &[&str] = match package.name.as_str() {
            "aria-primitives" => &[],
            "aria-kit" => &["aria-primitives"],
            "aria-stt" | "aria-tts" | "aria-wake" | "aria-speaker" | "aria-segmentation" => {
                &["aria-kit", "aria-primitives"]
            }
            _ => continue,
        };
        for dependency in &package.dependencies {
            if dependency.kind.as_deref() != Some("dev")
                && members.contains(&dependency.name)
                && !allowed.contains(&dependency.name.as_str())
            {
                violations.push(format!("{} -> {}", package.name, dependency.name));
            }
        }
    }
    violations
}

#[test]
fn workspace_respects_dependency_boundaries() {
    let mut output = tempfile::tempfile().unwrap();
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(Stdio::inherit());
    let mut process = ChildProcess::spawn(&mut command).expect("cargo metadata should start");
    assert!(
        matches!(
            process.wait_until(Instant::now() + Duration::from_secs(30), || false).unwrap(),
            WaitOutcome::Exited(status) if status.success()
        ),
        "cargo metadata failed or timed out"
    );
    output.rewind().unwrap();
    let workspace = serde_json::from_reader(output).expect("cargo metadata should be valid JSON");
    assert_eq!(forbidden_dependencies(&workspace), Vec::<String>::new());
}
