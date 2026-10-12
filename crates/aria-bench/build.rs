use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;

use toml_edit::{ArrayOfTables, DocumentMut, Table};

const NORMALIZER_DEPENDENCIES: [&str; 3] = ["regex", "unicode-normalization", "unicode-properties"];

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
    let lock = std::fs::read_to_string("../../Cargo.lock")?;
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    std::fs::write(
        output.join("scorer-dependencies.txt"),
        dependency_identity(&lock)?,
    )?;
    Ok(())
}

fn dependency_identity(lock: &str) -> Result<String> {
    let document = lock.parse::<DocumentMut>()?;
    let packages = document["package"]
        .as_array_of_tables()
        .ok_or("Cargo.lock has no package tables")?;
    let root = resolve("aria-bench", packages)?;
    let dependencies = root["dependencies"]
        .as_array()
        .ok_or("aria-bench has no dependencies")?;
    let mut pending = Vec::new();
    for name in NORMALIZER_DEPENDENCIES {
        let reference = dependencies
            .iter()
            .filter_map(|value| value.as_str())
            .find(|reference| reference.split_whitespace().next() == Some(name))
            .ok_or_else(|| format!("normalizer dependency {name} is missing from Cargo.lock"))?;
        pending.push(reference);
    }
    let mut selected = BTreeSet::new();
    while let Some(reference) = pending.pop() {
        let package = resolve(reference, packages)?;
        let name = field(package, "name")?;
        let version = field(package, "version")?;
        let source = field(package, "source")?;
        let checksum = package
            .get("checksum")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !(source.starts_with("registry+") && !checksum.is_empty()
            || source.starts_with("git+")
                && source
                    .rsplit_once('#')
                    .is_some_and(|(_, revision)| !revision.is_empty()))
        {
            return Err(format!(
                "normalizer dependency {name} needs a checksummed registry or pinned git source"
            )
            .into());
        }
        if !selected.insert((name, version, source, checksum)) {
            continue;
        }
        if let Some(dependencies) = package.get("dependencies") {
            for dependency in dependencies
                .as_array()
                .ok_or("invalid package dependencies")?
            {
                pending.push(dependency.as_str().ok_or("invalid dependency reference")?);
            }
        }
    }
    Ok(selected
        .into_iter()
        .map(|identity| format!("{identity:?}\n"))
        .collect())
}

fn field<'a>(package: &'a Table, name: &str) -> Result<&'a str> {
    package
        .get(name)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("normalizer dependency has no {name}").into())
}

fn resolve<'a>(reference: &str, packages: &'a ArrayOfTables) -> Result<&'a Table> {
    let mut parts = reference.splitn(3, ' ');
    let name = parts.next().ok_or("empty dependency reference")?;
    let version = parts.next();
    let source = parts
        .next()
        .map(|source| source.trim_start_matches('(').trim_end_matches(')'));
    let mut matches = packages.iter().filter(|package| {
        package.get("name").and_then(|value| value.as_str()) == Some(name)
            && version.is_none_or(|version| {
                package.get("version").and_then(|value| value.as_str()) == Some(version)
            })
            && source.is_none_or(|source| {
                package.get("source").and_then(|value| value.as_str()) == Some(source)
            })
    });
    let package = matches
        .next()
        .ok_or_else(|| format!("unresolved normalizer dependency {reference}"))?;
    if matches.next().is_some() {
        return Err(format!("ambiguous normalizer dependency {reference}").into());
    }
    Ok(package)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4
[[package]]
name = "aria-bench"
version = "0.1.0"
dependencies = ["regex", "unicode-normalization", "unicode-properties", "engine"]
[[package]]
name = "regex"
version = "1.0.0"
source = "registry+https://registry.example/index"
checksum = "regex-checksum"
dependencies = ["regex-syntax 1.0.0"]
[[package]]
name = "regex-syntax"
version = "1.0.0"
source = "registry+https://registry.example/index"
checksum = "syntax-checksum"
[[package]]
name = "regex-syntax"
version = "2.0.0"
source = "registry+https://registry.example/index"
checksum = "unrelated-syntax-checksum"
[[package]]
name = "unicode-normalization"
version = "0.1.0"
source = "registry+https://registry.example/index"
checksum = "normalization-checksum"
[[package]]
name = "unicode-properties"
version = "0.1.0"
source = "registry+https://registry.example/index"
checksum = "properties-checksum"
[[package]]
name = "engine"
version = "1.0.0"
source = "registry+https://registry.example/index"
checksum = "engine-checksum"
dependencies = ["regex-syntax 2.0.0"]
"#;

    #[test]
    fn resolved_normalizer_changes_alter_identity_but_engine_changes_do_not() {
        let original = dependency_identity(LOCK).unwrap();
        for checksum in [
            "regex-checksum",
            "syntax-checksum",
            "normalization-checksum",
            "properties-checksum",
        ] {
            assert_ne!(
                original,
                dependency_identity(&LOCK.replace(&format!("\"{checksum}\""), "\"changed\""))
                    .unwrap()
            );
        }
        for checksum in ["engine-checksum", "unrelated-syntax-checksum"] {
            assert_eq!(
                original,
                dependency_identity(&LOCK.replace(checksum, "changed")).unwrap()
            );
        }
        assert_ne!(
            original,
            dependency_identity(&LOCK.replace("regex-syntax 1.0.0", "regex-syntax 2.0.0")).unwrap()
        );
        assert_eq!(
            original,
            dependency_identity(&format!("# formatting only\n{LOCK}")).unwrap()
        );
    }

    #[test]
    fn ambiguous_missing_and_unpinned_dependencies_are_rejected() {
        assert!(dependency_identity(&LOCK.replace("regex-syntax 1.0.0", "regex-syntax")).is_err());
        assert!(
            dependency_identity(&LOCK.replace("regex-syntax 1.0.0", "missing-package")).is_err()
        );
        assert!(
            dependency_identity(&LOCK.replace(
                "registry+https://registry.example/index",
                "git+https://example/repository"
            ))
            .is_err()
        );
        assert!(
            dependency_identity(&LOCK.replace(
                "registry+https://registry.example/index",
                "git+https://example/repository#revision"
            ))
            .is_ok()
        );
    }
}
