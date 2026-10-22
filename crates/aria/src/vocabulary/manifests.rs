use std::{borrow::Cow, io, ops::Range};

use serde::Deserialize;
use toml_edit::{Document, Item};

const MAX_MEMBERS: usize = 1024;

pub(super) struct Manifest {
    pub name: Option<(String, Range<usize>)>,
    pub members: Vec<String>,
    pub exclude: Vec<String>,
}

pub(super) fn parse(filename: &str, text: &str) -> Result<Manifest, io::Error> {
    let manifest = match filename {
        "package.json" => parse_json(text)?,
        "Cargo.toml" | "pyproject.toml" => parse_toml(filename, text)?,
        _ => return Err(invalid("unsupported manifest")),
    };
    if manifest.members.len() + manifest.exclude.len() > MAX_MEMBERS {
        return Err(invalid("too many workspace paths"));
    }
    Ok(manifest)
}

fn parse_toml(filename: &str, text: &str) -> Result<Manifest, io::Error> {
    let document = Document::parse(text).map_err(invalid)?;
    let root = document.as_item();
    let (name, workspace) = if filename == "Cargo.toml" {
        (
            field(root, &["package", "name"])?,
            field(root, &["workspace"])?,
        )
    } else {
        let name = match field(root, &["project", "name"])? {
            Some(name) => Some(name),
            None => field(root, &["tool", "poetry", "name"])?,
        };
        (name, field(root, &["tool", "uv", "workspace"])?)
    };
    let name = name
        .map(|value| toml_name(value, text))
        .transpose()?
        .flatten();
    let (members, exclude) = match workspace {
        Some(workspace) => (
            toml_paths(field(workspace, &["members"])?)?,
            toml_paths(field(workspace, &["exclude"])?)?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(Manifest {
        name,
        members,
        exclude,
    })
}

fn field<'a>(mut item: &'a Item, path: &[&str]) -> Result<Option<&'a Item>, io::Error> {
    for key in path {
        let table = item
            .as_table_like()
            .ok_or_else(|| invalid("expected manifest table"))?;
        let Some(value) = table.get(key) else {
            return Ok(None);
        };
        item = value;
    }
    Ok(Some(item))
}

fn toml_name(value: &Item, text: &str) -> Result<Option<(String, Range<usize>)>, io::Error> {
    let name = value
        .as_str()
        .ok_or_else(|| invalid("expected string name"))?;
    let Some(span) = value.span() else {
        return Ok(None);
    };
    let raw = &text[span.clone()];
    let quotes = if raw.starts_with("\"\"\"") || raw.starts_with("'''") {
        3
    } else {
        1
    };
    let content = span.start + quotes..span.end - quotes;
    Ok((text.get(content.clone()) == Some(name)).then(|| (name.to_owned(), content)))
}

fn toml_paths(value: Option<&Item>) -> Result<Vec<String>, io::Error> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| invalid("expected workspace path array"))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("expected string workspace path"))
        })
        .collect()
}

fn parse_json(text: &str) -> Result<Manifest, io::Error> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(invalid)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("expected package object"))?;
    if object.get("name").is_some_and(|name| !name.is_string()) {
        return Err(invalid("expected string name"));
    }
    #[derive(Deserialize)]
    struct PackageName<'a> {
        #[serde(borrow)]
        name: Cow<'a, str>,
    }
    let name = if object.contains_key("name") {
        let package: PackageName<'_> = serde_json::from_str(text).map_err(invalid)?;
        match package.name {
            Cow::Borrowed(name) => {
                let start = name.as_ptr() as usize - text.as_ptr() as usize;
                Some((name.to_owned(), start..start + name.len()))
            }
            Cow::Owned(_) => None,
        }
    } else {
        None
    };
    let members = match object.get("workspaces") {
        None => Vec::new(),
        Some(value) => {
            let paths = if let Some(workspaces) = value.as_object() {
                workspaces
                    .get("packages")
                    .ok_or_else(|| invalid("missing workspace packages"))?
            } else {
                value
            };
            paths
                .as_array()
                .ok_or_else(|| invalid("expected workspace path array"))?
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("expected string workspace path"))
                })
                .collect::<Result<_, _>>()?
        }
    };
    Ok(Manifest {
        name,
        members,
        exclude: Vec::new(),
    })
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_members_come_only_from_supported_fields() {
        for (filename, text, expected, members, exclude) in [
            (
                "Cargo.toml",
                "[dependencies]\nname = 'decoy'\n[package]\nname = 'aria-kit'\n[workspace]\nmembers = ['crates/*']\nexclude = ['crates/demo']",
                "aria-kit",
                vec!["crates/*"],
                vec!["crates/demo"],
            ),
            (
                "package.json",
                r#"{"description":"aria-ui", "dependencies":{"name":"decoy"},"name":"aria-ui","workspaces":{"packages":["packages/*"]}}"#,
                "aria-ui",
                vec!["packages/*"],
                vec![],
            ),
            (
                "pyproject.toml",
                "[project]\nname = 'aria-tools'\n[tool.poetry]\nname = 'decoy'\n[tool.uv.workspace]\nmembers = ['packages/*']\nexclude = ['packages/demo']",
                "aria-tools",
                vec!["packages/*"],
                vec!["packages/demo"],
            ),
            (
                "pyproject.toml",
                "[tool.poetry]\nname = 'aria-poetry'",
                "aria-poetry",
                vec![],
                vec![],
            ),
        ] {
            let manifest = parse(filename, text).unwrap();
            let (name, span) = manifest.name.unwrap();
            assert_eq!(name, expected);
            assert_eq!(&text[span], expected);
            assert_eq!(manifest.members, members);
            assert_eq!(manifest.exclude, exclude);
        }
    }

    #[test]
    fn escaped_names_are_skipped_without_losing_workspace_members() {
        for (filename, text) in [
            (
                "Cargo.toml",
                "[package]\nname = \"aria\\u002dkit\"\n[workspace]\nmembers = ['packages/*']",
            ),
            (
                "package.json",
                r#"{"name":"aria\u002dkit","workspaces":["packages/*"]}"#,
            ),
        ] {
            let manifest = parse(filename, text).unwrap();
            assert!(manifest.name.is_none());
            assert_eq!(manifest.members, ["packages/*"]);
        }
    }

    #[test]
    fn malformed_fields_and_excessive_members_are_rejected() {
        for (filename, text) in [
            ("Cargo.toml", "package = 7"),
            ("Cargo.toml", "[package]\nname = 7"),
            ("Cargo.toml", "[workspace]\nmembers = [7]"),
            ("pyproject.toml", "[tool.uv]\nworkspace = 7"),
            ("package.json", r#"{"name":null}"#),
            ("package.json", r#"{"workspaces":{}}"#),
            ("package.json", r#"{"workspaces":[7]}"#),
        ] {
            assert!(parse(filename, text).is_err(), "{filename}: {text}");
        }
        let text =
            serde_json::json!({"workspaces": vec!["packages/*"; MAX_MEMBERS + 1]}).to_string();
        assert!(parse("package.json", &text).is_err());
    }
}
