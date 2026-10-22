use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use aria_kit::cancel::CancelToken;
use rustix::fs::{AtFlags, Dir, FileType};
use sha2::{Digest, Sha256};

use super::collection::{Collector, Error};
use super::discover::{Stamp, open_beneath, open_directory, resolve, stamp};
use super::{
    Candidate, Coverage, Directory, Discovery, MAX_SOURCE_BYTES, MAX_TEXT_BYTES, SELECTED_TERMS,
    Source, SourceKind, manifests, source_bytes,
};

const MAX_MEMBERS: usize = 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MANIFESTS: [&str; 3] = ["Cargo.toml", "package.json", "pyproject.toml"];

pub(crate) struct ProjectCollector;

impl Collector for ProjectCollector {
    type Error = io::Error;

    fn pipeline_key(&self) -> Option<String> {
        Some(format!(
            "manifests1:select1:text{MAX_TEXT_BYTES}:members{MAX_MEMBERS}:terms{SELECTED_TERMS}"
        ))
    }

    fn collect(
        &mut self,
        selected: &Directory,
        _previous: &[Source],
        cancel: &CancelToken,
    ) -> Result<Discovery, Error<Self::Error>> {
        check(cancel)?;
        let root = resolve(selected.as_path())?;
        let directory = open_directory(&root)?;
        let mut collection = Collection {
            root: &directory,
            cancel,
            coverage: Coverage::default(),
            observed: HashMap::new(),
            sources: Vec::new(),
            retained_bytes: 0,
        };
        collection.open(Path::new("."))?;
        if let Some(name) = root.file_name().and_then(OsStr::to_str)
            && super::select::eligible(SourceKind::DirectoryName, name)
        {
            collection.source(
                Path::new("."),
                SourceKind::DirectoryName,
                name,
                Some((name.to_owned(), 0..name.len())),
            )?;
        }
        let mut inputs = BTreeSet::new();
        for filename in MANIFESTS {
            let Some(manifest) = collection.manifest(Path::new(filename))? else {
                continue;
            };
            let exclusions = manifest
                .exclude
                .iter()
                .map(|value| member_pattern(value))
                .collect::<io::Result<Vec<_>>>()?;
            for member in manifest.members {
                check(cancel)?;
                let pattern = member_pattern(&member)?;
                for path in collection.expand(&pattern)? {
                    if exclusions.iter().any(|excluded| excluded.matches(&path)) {
                        continue;
                    }
                    let input = if path == Path::new(".") {
                        PathBuf::from(filename)
                    } else {
                        path.join(filename)
                    };
                    if !inputs.contains(&input) {
                        collection.reserve(128 + 2 * input.as_os_str().len())?;
                        inputs.insert(input);
                    }
                    if inputs.len() > MAX_MEMBERS {
                        return Err(Error::TooLarge);
                    }
                }
            }
        }
        for path in inputs {
            check(cancel)?;
            if path.components().count() > 1 {
                collection.manifest(&path)?;
            }
        }
        for (path, expected) in &collection.observed {
            check(cancel)?;
            let actual = match open_beneath(&directory, path) {
                Ok(file) => Some(stamp(&file.metadata()?)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(_) => return Err(Error::Unstable),
            };
            if actual != *expected {
                return Err(Error::Unstable);
            }
        }
        if stamp(&open_directory(&root)?.metadata()?) != stamp(&directory.metadata()?) {
            return Err(Error::Unstable);
        }
        check(cancel)?;
        Ok(Discovery {
            selected: selected.as_path().to_owned(),
            root,
            sources: collection.sources,
            coverage: collection.coverage,
        })
    }
}

struct Collection<'a> {
    root: &'a File,
    cancel: &'a CancelToken,
    coverage: Coverage,
    observed: HashMap<PathBuf, Option<Stamp>>,
    sources: Vec<Source>,
    retained_bytes: usize,
}

impl Collection<'_> {
    fn open(&mut self, path: &Path) -> Result<Option<File>, Error<io::Error>> {
        check(self.cancel)?;
        self.coverage.entries += 1;
        if self.coverage.entries > MAX_ENTRIES {
            return Err(Error::TooLarge);
        }
        if !self.observed.contains_key(path) {
            self.reserve(256 + 2 * path.as_os_str().len())?;
        }
        let file = match open_beneath(self.root, path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.observed.get(path).is_some_and(Option::is_some) {
                    return Err(Error::Unstable);
                }
                self.observed.insert(path.to_owned(), None);
                return Ok(None);
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let current = Some(stamp(&file.metadata()?));
        if let Some(previous) = self.observed.get(path)
            && *previous != current
        {
            return Err(Error::Unstable);
        }
        self.observed.insert(path.to_owned(), current);
        Ok(Some(file))
    }

    fn manifest(&mut self, path: &Path) -> Result<Option<manifests::Manifest>, Error<io::Error>> {
        let mut ancestor = PathBuf::new();
        for component in path.parent().unwrap_or(Path::new("")).components() {
            ancestor.push(component);
            if self.open(&ancestor)?.is_none() {
                return Ok(None);
            }
        }
        let Some(mut file) = self.open(path)? else {
            return Ok(None);
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Io(io::Error::other("expected a manifest file")));
        }
        if metadata.len() > MAX_TEXT_BYTES as u64 {
            return Err(Error::TooLarge);
        }
        let mut contents = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            check(self.cancel)?;
            let count = file.read(&mut buffer)?;
            self.coverage.bytes += count;
            if self.coverage.bytes > MAX_BYTES || contents.len() + count > MAX_TEXT_BYTES {
                return Err(Error::TooLarge);
            }
            if count == 0 {
                break;
            }
            contents.extend_from_slice(&buffer[..count]);
        }
        if stamp(&metadata) != stamp(&file.metadata()?) {
            return Err(Error::Unstable);
        }
        let text = std::str::from_utf8(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| io::Error::other("invalid manifest path"))?;
        let manifest = manifests::parse(filename, text).map_err(Error::Extractor)?;
        check(self.cancel)?;
        self.source(path, SourceKind::Text, text, manifest.name.clone())?;
        Ok(Some(manifest))
    }

    fn source(
        &mut self,
        path: &Path,
        kind: SourceKind,
        text: &str,
        name: Option<(String, std::ops::Range<usize>)>,
    ) -> Result<(), Error<io::Error>> {
        let candidates = name
            .into_iter()
            .filter(|(name, span)| {
                super::select::eligible(kind, name) && text.get(span.clone()) == Some(name.as_str())
            })
            .map(|(canonical, span)| Candidate { canonical, span })
            .collect();
        let source = Source {
            relative_path: path.to_owned(),
            kind,
            fingerprint: Sha256::digest(text.as_bytes()).into(),
            input_bytes: text.len(),
            candidates,
        };
        self.reserve(source_bytes(&source))?;
        self.sources.push(source);
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), Error<io::Error>> {
        self.retained_bytes += bytes;
        if self.retained_bytes > MAX_SOURCE_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(())
    }

    fn expand(&mut self, pattern: &MemberPattern) -> Result<Vec<PathBuf>, Error<io::Error>> {
        if pattern
            .path
            .components()
            .any(|component| excluded_component(component.as_os_str()))
        {
            return Ok(Vec::new());
        }
        if !pattern.children {
            return Ok(vec![pattern.path.clone()]);
        }
        let Some(directory) = self.open(&pattern.path)? else {
            return Ok(Vec::new());
        };
        if !directory.metadata()?.is_dir() {
            return Err(Error::Io(io::Error::other(
                "expected a membership directory",
            )));
        }
        let mut paths = Vec::new();
        for entry in Dir::read_from(&directory).map_err(io::Error::from)? {
            check(self.cancel)?;
            let entry = entry.map_err(io::Error::from)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            self.coverage.entries += 1;
            if self.coverage.entries > MAX_ENTRIES {
                return Err(Error::TooLarge);
            }
            if name.as_bytes().starts_with(b".") || excluded_component(name) {
                continue;
            }
            // A matching symlink fails when opened; it cannot redirect collection outside the root.
            let metadata = rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            let kind = FileType::from_raw_mode(metadata.st_mode);
            if matches!(kind, FileType::Directory | FileType::Symlink) {
                paths.push(if pattern.path == Path::new(".") {
                    PathBuf::from(name)
                } else {
                    pattern.path.join(name)
                });
                if paths.len() > MAX_MEMBERS {
                    return Err(Error::TooLarge);
                }
            }
        }
        Ok(paths)
    }
}

struct MemberPattern {
    path: PathBuf,
    children: bool,
}

impl MemberPattern {
    fn matches(&self, path: &Path) -> bool {
        if self.children {
            path.parent().map(|parent| {
                if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                }
            }) == Some(self.path.as_path())
        } else {
            path == self.path
        }
    }
}

fn member_pattern(value: &str) -> io::Result<MemberPattern> {
    let (base, children) = if value == "*" {
        (".", true)
    } else if let Some(base) = value.strip_suffix("/*") {
        (base, true)
    } else {
        (value, false)
    };
    if base.is_empty()
        || base.len() > 4096
        || base.contains(['*', '?', '[', ']', '{', '}', '!', '\\'])
    {
        return Err(io::Error::other("unsupported workspace member pattern"));
    }
    let mut path = PathBuf::new();
    let mut depth = 0;
    for component in Path::new(base).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                depth += 1;
                path.push(name);
            }
            _ => {
                return Err(io::Error::other(
                    "workspace member is outside the supported scope",
                ));
            }
        }
    }
    if depth > 16 {
        return Err(io::Error::other(
            "workspace member path exceeds depth limit",
        ));
    }
    if path.as_os_str().is_empty() {
        path.push(".");
    }
    Ok(MemberPattern { path, children })
}

fn excluded_component(name: &OsStr) -> bool {
    [
        "target",
        "node_modules",
        "build",
        "dist",
        "vendor",
        ".git",
        ".venv",
    ]
    .iter()
    .any(|excluded| name == *excluded)
}

fn check(cancel: &CancelToken) -> Result<(), Error<io::Error>> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn collect(root: &Path) -> Result<Discovery, Error<io::Error>> {
        ProjectCollector.collect(
            &Directory::new(root.to_owned()).unwrap(),
            &[],
            &CancelToken::never(),
        )
    }

    #[test]
    fn declared_members_are_collected_across_ecosystems_without_walking_source_trees() {
        for (filename, root_manifest, member_manifest) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = ['.', './packages/*', 'packages/member']\nexclude = ['packages/excluded']",
                "[package]\nname = 'aria-kit'",
            ),
            (
                "package.json",
                r#"{"workspaces":["packages/*"]}"#,
                r#"{"name":"aria-kit","dependencies":{"unrelated-name":"1"}}"#,
            ),
            (
                "pyproject.toml",
                "[tool.uv.workspace]\nmembers = ['packages/*']\nexclude = ['packages/excluded']",
                "[project]\nname = 'aria-kit'",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("project");
            write(&root, filename, root_manifest);
            write(
                &root,
                &format!("packages/member/{filename}"),
                member_manifest,
            );
            write(
                &root,
                "packages/node_modules/package.json",
                "invalid manifest",
            );
            if filename != "package.json" {
                write(
                    &root,
                    &format!("packages/excluded/{filename}"),
                    "invalid manifest",
                );
            }
            let before = collect(&root).unwrap();
            write(&root, "packages/member/src/secret.txt", "source-decoy");
            write(&root, "target/deep/deeper/Cargo.toml", "invalid manifest");
            write(&root, "unrelated/package.json", "invalid manifest");
            let discovery = collect(&root).unwrap();
            let names: Vec<_> = discovery
                .sources
                .iter()
                .flat_map(|source| &source.candidates)
                .map(|candidate| candidate.canonical.as_str())
                .collect();
            assert_eq!(names, ["project", "aria-kit"]);
            assert_eq!(discovery.sources.len(), 3);
            assert_eq!(discovery.coverage.entries, before.coverage.entries);
            assert_eq!(discovery.coverage.bytes, before.coverage.bytes);
            assert!(
                discovery.sources.iter().any(|source| source.relative_path
                    == Path::new(&format!("packages/member/{filename}")))
            );
        }
    }

    #[test]
    fn unsupported_patterns_symlinks_and_invalid_manifests_fail_the_whole_collection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        for member in [
            "../outside",
            "/outside",
            "packages/**",
            "packages/?",
            "packages/{a,b}",
        ] {
            write(
                &root,
                "package.json",
                serde_json::json!({"name":"aria-kit", "workspaces":[member]}).to_string(),
            );
            assert!(collect(&root).is_err(), "{member}");
        }
        write(&root, "package.json", r#"{"workspaces":["packages/*"]}"#);
        write(&root, "packages/member/package.json", "invalid");
        assert!(collect(&root).is_err());
        fs::remove_file(root.join("packages/member/package.json")).unwrap();
        write(
            temp.path(),
            "outside/package.json",
            r#"{"name":"outside-name"}"#,
        );
        symlink(
            temp.path().join("outside/package.json"),
            root.join("packages/member/package.json"),
        )
        .unwrap();
        assert!(collect(&root).is_err());
        fs::remove_file(root.join("packages/member/package.json")).unwrap();
        symlink(temp.path().join("outside"), root.join("packages/link")).unwrap();
        assert!(collect(&root).is_err());
    }

    #[test]
    fn no_manifest_is_folder_only_and_limits_never_return_partial_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("aria-kit");
        write(&root, "nested/Cargo.toml", "invalid");
        let discovery = collect(&root).unwrap();
        assert_eq!(discovery.sources.len(), 1);
        assert_eq!(discovery.sources[0].candidates[0].canonical, "aria-kit");
        assert_eq!(discovery.coverage.entries, 4);
        write(&root, "Cargo.toml", vec![b' '; MAX_TEXT_BYTES + 1]);
        assert!(matches!(collect(&root), Err(Error::TooLarge)));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            ProjectCollector.collect(&Directory::new(root).unwrap(), &[], &cancel),
            Err(Error::Cancelled)
        ));
    }
}
