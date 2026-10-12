use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use aria_kit::cancel::CancelToken;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

use super::extract::{Extractor, TextInput};
use super::{
    Candidate, Coverage, Discovery, MAX_CANDIDATES, MAX_SOURCE_BYTES, MAX_TERMS, MAX_TEXT_BYTES,
    Source, SourceKind, source_bytes,
};

const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 16;
const WORK_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error<E> {
    #[error("vocabulary discovery requires an absolute directory")]
    InvalidDirectory,
    #[error("vocabulary discovery exceeded a resource limit")]
    TooLarge,
    #[error("vocabulary discovery was cancelled")]
    Cancelled,
    #[error("project files changed during vocabulary discovery")]
    Unstable,
    #[error("vocabulary extractor returned invalid output")]
    InvalidOutput,
    #[error("vocabulary extraction failed")]
    Extractor(#[source] E),
    #[error("vocabulary filesystem operation failed")]
    Io(#[from] io::Error),
}

pub(crate) fn resolve(selected: &Path) -> io::Result<PathBuf> {
    if !selected.is_absolute() || selected.as_os_str().len() > 4096 {
        return Err(io::Error::other("expected an absolute directory"));
    }
    let root = selected.canonicalize()?;
    open_directory(&root)?;
    Ok(root)
}

pub(crate) fn scan_cached<E: Extractor>(
    selected: &Path,
    cancel: &CancelToken,
    previous: &[Source],
    extractor: &mut E,
) -> Result<Discovery, Error<E::Error>> {
    check_cancelled(cancel)?;
    if !selected.is_absolute() || selected.as_os_str().len() > 4096 {
        return Err(Error::InvalidDirectory);
    }
    let root = resolve(selected)?;
    check_cancelled(cancel)?;
    let directory = open_directory(&root)?;
    let mut scanner = Scanner {
        cancel,
        extractor,
        previous: HashMap::new(),
        coverage: Coverage::default(),
        term_count: 0,
        sources: Vec::new(),
        observed: Vec::new(),
        retained_bytes: 0,
    };
    for source in previous {
        check_cancelled(cancel)?;
        scanner
            .previous
            .insert((source.relative_path.as_path(), source.kind), source);
    }
    scanner.observe(Path::new("."), &directory.metadata()?)?;
    if let Some(name) = root.file_name().and_then(OsStr::to_str) {
        scanner.input(Path::new("."), SourceKind::DirectoryName, name)?;
    }
    scanner.walk(&directory, Path::new("."), 0)?;
    for (path, before) in &scanner.observed {
        check_cancelled(cancel)?;
        let current = open_beneath(&directory, path).map_err(|_| Error::Unstable)?;
        if stamp(&current.metadata()?) != *before {
            return Err(Error::Unstable);
        }
    }
    check_cancelled(cancel)?;
    let root_now = open_directory(&root)
        .map_err(|_| Error::Unstable)?
        .metadata()?;
    let root_opened = directory.metadata()?;
    if root_now.dev() != root_opened.dev() || root_now.ino() != root_opened.ino() {
        return Err(Error::Unstable);
    }
    check_cancelled(cancel)?;
    Ok(Discovery {
        selected: selected.to_path_buf(),
        root,
        sources: scanner.sources,
        coverage: scanner.coverage,
    })
}

struct Scanner<'a, E: Extractor> {
    cancel: &'a CancelToken,
    extractor: &'a mut E,
    previous: HashMap<(&'a Path, SourceKind), &'a Source>,
    coverage: Coverage,
    term_count: usize,
    sources: Vec<Source>,
    observed: Vec<(PathBuf, Stamp)>,
    retained_bytes: usize,
}

impl<E: Extractor> Scanner<'_, E> {
    fn walk(
        &mut self,
        directory: &File,
        scope: &Path,
        depth: usize,
    ) -> Result<(), Error<E::Error>> {
        check_cancelled(self.cancel)?;
        if depth > MAX_DEPTH {
            return Err(Error::TooLarge);
        }
        self.observe(scope, &directory.metadata()?)?;
        for entry in Dir::read_from(directory).map_err(io::Error::from)? {
            check_cancelled(self.cancel)?;
            let entry = entry.map_err(io::Error::from)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            self.coverage.entries += 1;
            if self.coverage.entries > MAX_ENTRIES {
                return Err(Error::TooLarge);
            }
            if name.as_bytes().starts_with(b".") {
                self.coverage.hidden += 1;
                continue;
            }
            let metadata = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            check_cancelled(self.cancel)?;
            let kind = FileType::from_raw_mode(metadata.st_mode);
            let path = join_relative(scope, name);
            match kind {
                FileType::Directory => {
                    let child = open_child_directory(directory, name)?;
                    if let Some(name) = name.to_str() {
                        self.input(&path, SourceKind::DirectoryName, name)?;
                    }
                    self.walk(&child, &path, depth + 1)?;
                }
                FileType::RegularFile => {
                    if let Some(name) = name.to_str() {
                        self.input(&path, SourceKind::FileName, name)?;
                    }
                    self.text(directory, name, &path)?;
                }
                FileType::Symlink => self.coverage.symlinks += 1,
                _ => self.coverage.unsupported += 1,
            }
        }
        check_cancelled(self.cancel)
    }

    fn text(&mut self, directory: &File, name: &OsStr, path: &Path) -> Result<(), Error<E::Error>> {
        check_cancelled(self.cancel)?;
        let descriptor = rustix::fs::openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let mut file = File::from(descriptor);
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(Error::Unstable);
        }
        self.observe(path, &before)?;
        if before.len() > MAX_TEXT_BYTES as u64 {
            self.coverage.oversized += 1;
            return Ok(());
        }
        let mut contents = Vec::new();
        let mut buffer = [0; WORK_BYTES];
        loop {
            check_cancelled(self.cancel)?;
            let count = file.read(&mut buffer)?;
            check_cancelled(self.cancel)?;
            self.coverage.bytes += count;
            if self.coverage.bytes > MAX_TOTAL_BYTES {
                return Err(Error::TooLarge);
            }
            if count == 0 {
                break;
            }
            if contents.len() + count > MAX_TEXT_BYTES {
                return Err(Error::Unstable);
            }
            contents.extend_from_slice(&buffer[..count]);
        }
        if stamp(&before) != stamp(&file.metadata()?) {
            return Err(Error::Unstable);
        }
        // Validate UTF-8 in bounded units, allowing a codepoint to cross a unit boundary.
        let mut offset = 0;
        let mut text = String::with_capacity(contents.len());
        while offset < contents.len() {
            check_cancelled(self.cancel)?;
            let end = (offset + WORK_BYTES).min(contents.len());
            match std::str::from_utf8(&contents[offset..end]) {
                Ok(chunk) => {
                    text.push_str(chunk);
                    offset = end;
                }
                Err(error) if error.error_len().is_none() && end < contents.len() => {
                    let valid_end = offset + error.valid_up_to();
                    let chunk = std::str::from_utf8(&contents[offset..valid_end])
                        .map_err(|_| Error::Unstable)?;
                    text.push_str(chunk);
                    offset = valid_end;
                }
                Err(_) => {
                    self.coverage.unsupported += 1;
                    return Ok(());
                }
            }
        }
        let mut checked = 0;
        for (offset, character) in text.char_indices() {
            if offset - checked >= WORK_BYTES - 4 {
                check_cancelled(self.cancel)?;
                checked = offset;
            }
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                self.coverage.unsupported += 1;
                return Ok(());
            }
        }
        self.input(path, SourceKind::Text, &text)
    }

    fn input(&mut self, path: &Path, kind: SourceKind, text: &str) -> Result<(), Error<E::Error>> {
        check_cancelled(self.cancel)?;
        let mut hasher = Sha256::new();
        for chunk in text.as_bytes().chunks(WORK_BYTES) {
            check_cancelled(self.cancel)?;
            hasher.update(chunk);
        }
        let fingerprint: [u8; 32] = hasher.finalize().into();
        if let Some(previous) = self.previous.get(&(path, kind))
            && previous.fingerprint == fingerprint
            && previous.input_bytes == text.len()
        {
            let previous = *previous;
            let mut valid = previous.candidates.len() <= MAX_CANDIDATES;
            let mut seen = HashSet::new();
            if valid {
                for candidate in &previous.candidates {
                    check_cancelled(self.cancel)?;
                    if !super::select::eligible(kind, &candidate.canonical)
                        || text.get(candidate.span.clone()) != Some(candidate.canonical.as_str())
                        || !seen.insert(candidate.canonical.as_str())
                    {
                        valid = false;
                        break;
                    }
                }
            }
            if valid {
                self.reserve(source_bytes(previous))?;
                self.term_count += previous.candidates.len();
                if self.term_count > MAX_TERMS {
                    return Err(Error::TooLarge);
                }
                let mut source = Source {
                    relative_path: path.to_path_buf(),
                    kind,
                    fingerprint,
                    input_bytes: text.len(),
                    candidates: Vec::new(),
                };
                for candidate in &previous.candidates {
                    check_cancelled(self.cancel)?;
                    source.candidates.push(candidate.clone());
                }
                self.sources.push(source);
                return Ok(());
            }
            tracing::warn!("invalid cached vocabulary evidence; extracting input again");
        }
        let result = self
            .extractor
            .extract(TextInput { kind, text }, MAX_CANDIDATES, self.cancel);
        check_cancelled(self.cancel)?;
        let spans = result.map_err(Error::Extractor)?;
        if spans.len() > MAX_CANDIDATES {
            return Err(Error::InvalidOutput);
        }
        let mut seen = HashSet::new();
        for span in &spans {
            check_cancelled(self.cancel)?;
            if span.start >= span.end || span.len() > 256 {
                return Err(Error::InvalidOutput);
            }
            let canonical = text.get(span.clone()).ok_or(Error::InvalidOutput)?;
            if !super::select::eligible(kind, canonical) || !seen.insert(canonical) {
                return Err(Error::InvalidOutput);
            }
        }
        self.term_count += spans.len();
        if self.term_count > MAX_TERMS {
            return Err(Error::TooLarge);
        }
        self.reserve(
            512 + 2 * path.as_os_str().len()
                + spans.iter().map(|span| 256 + 2 * span.len()).sum::<usize>(),
        )?;
        let mut candidates = Vec::with_capacity(spans.len());
        for span in spans {
            check_cancelled(self.cancel)?;
            candidates.push(Candidate {
                canonical: text[span.clone()].to_owned(),
                span,
            });
        }
        self.sources.push(Source {
            relative_path: path.to_path_buf(),
            kind,
            fingerprint,
            input_bytes: text.len(),
            candidates,
        });
        Ok(())
    }

    fn observe(&mut self, path: &Path, metadata: &Metadata) -> Result<(), Error<E::Error>> {
        self.reserve(256 + 2 * path.as_os_str().len())?;
        self.observed.push((path.to_path_buf(), stamp(metadata)));
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), Error<E::Error>> {
        self.retained_bytes += bytes;
        if self.retained_bytes > MAX_SOURCE_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(())
    }
}

fn check_cancelled<E: std::error::Error + 'static>(cancel: &CancelToken) -> Result<(), Error<E>> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
pub(super) fn open_directory(path: &Path) -> Result<File, io::Error> {
    if !path.is_absolute() {
        return Err(io::Error::other("expected an absolute directory"));
    }
    let root = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let directory = open_beneath(&File::from(root), path.strip_prefix("/").unwrap_or(path))?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::other("expected a directory"));
    }
    Ok(directory)
}

fn open_beneath(root: &File, path: &Path) -> Result<File, io::Error> {
    let mut current = root.try_clone()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let mut flags =
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
                if components.peek().is_some() {
                    flags |= OFlags::DIRECTORY;
                }
                current = File::from(rustix::fs::openat(&current, name, flags, Mode::empty())?);
            }
            _ => return Err(io::Error::other("invalid relative project path")),
        }
    }
    Ok(current)
}

fn open_child_directory(parent: &File, name: &OsStr) -> Result<File, io::Error> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(File::from(rustix::fs::openat(
        parent,
        name,
        flags,
        Mode::empty(),
    )?))
}

fn join_relative(scope: &Path, name: &OsStr) -> PathBuf {
    if scope == Path::new(".") {
        PathBuf::from(name)
    } else {
        scope.join(name)
    }
}

#[derive(PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn stamp(metadata: &Metadata) -> Stamp {
    Stamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::extract::LocalExtractor;
    use super::*;
    use std::fs;
    use std::ops::Range;
    use std::os::unix::fs::symlink;

    fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[derive(Default)]
    struct RecordingExtractor {
        inputs: Vec<(SourceKind, String)>,
    }

    impl Extractor for RecordingExtractor {
        type Error = io::Error;
        fn cache_key(&self) -> &str {
            "recording-test"
        }
        fn extract(
            &mut self,
            input: TextInput<'_>,
            _: usize,
            _: &CancelToken,
        ) -> Result<Vec<Range<usize>>, Self::Error> {
            self.inputs.push((input.kind, input.text.to_owned()));
            Ok(Vec::new())
        }
    }

    #[test]
    fn generic_folders_have_independent_inputs_and_exact_selected_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "JavaTree/Widget.java", "class WidgetFactory {}\n");
        write(
            root,
            "SwiftTree/View.swift",
            "let session = URLSession.shared\n",
        );
        write(root, "Documents/notes.txt", "Use vex16 and aria-kit.\n");
        write(root, "target/generated.txt", "generated-name");
        fs::create_dir(root.join("JavaTree/.git")).unwrap();
        let mut extractor = RecordingExtractor::default();
        let all = scan_cached(root, &CancelToken::never(), &[], &mut extractor).unwrap();
        assert_eq!(all.root, root.canonicalize().unwrap());
        for expected in ["Widget.java", "View.swift", "notes.txt", "generated.txt"] {
            assert!(
                extractor
                    .inputs
                    .iter()
                    .any(|(kind, text)| *kind == SourceKind::FileName && text == expected)
            );
        }
        assert!(
            extractor.inputs.iter().any(
                |(kind, text)| *kind == SourceKind::Text && text == "Use vex16 and aria-kit.\n"
            )
        );
        extractor.inputs.clear();
        let selected = root.join("JavaTree");
        let child = scan_cached(&selected, &CancelToken::never(), &[], &mut extractor).unwrap();
        assert_eq!(child.root, selected.canonicalize().unwrap());
        assert!(
            !extractor
                .inputs
                .iter()
                .any(|(_, text)| text.contains("URLSession"))
        );
        assert!(
            extractor
                .inputs
                .iter()
                .any(|(_, text)| text == "Widget.java")
        );
    }

    #[test]
    fn exclusions_retain_filenames_and_allow_explicit_hidden_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".selected");
        write(&root, ".hidden/secret.txt", "hidden-name");
        write(&root, "binary.dat", b"valid-prefix\0hidden-name");
        write(&root, "utf16.dat", [0xff, 0xfe, 0x61, 0x00]);
        write(&root, "oversized.txt", vec![b'a'; MAX_TEXT_BYTES + 1]);
        write(&root, "valid.txt", "vex16\tURLSession\n");
        symlink(root.join("valid.txt"), root.join("linked.txt")).unwrap();
        let mut extractor = RecordingExtractor::default();
        let result = scan_cached(&root, &CancelToken::never(), &[], &mut extractor).unwrap();
        assert_eq!(result.coverage.hidden, 1);
        assert_eq!(result.coverage.symlinks, 1);
        assert_eq!(result.coverage.unsupported, 2);
        assert_eq!(result.coverage.oversized, 1);
        assert!(
            extractor
                .inputs
                .contains(&(SourceKind::DirectoryName, ".selected".into()))
        );
        assert!(
            extractor
                .inputs
                .contains(&(SourceKind::FileName, "binary.dat".into()))
        );
        assert_eq!(
            extractor
                .inputs
                .iter()
                .filter(|(kind, _)| *kind == SourceKind::Text)
                .count(),
            1
        );
    }

    #[test]
    fn local_extraction_preserves_canonical_spelling_from_text() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "notes.txt",
            "Use URLSession with vex16 and aria-kit.",
        );
        let result =
            scan_cached(temp.path(), &CancelToken::never(), &[], &mut LocalExtractor).unwrap();
        let text = result
            .sources
            .iter()
            .find(|source| source.kind == SourceKind::Text)
            .unwrap();
        for expected in ["URLSession", "vex16", "aria-kit"] {
            assert!(
                text.candidates
                    .iter()
                    .any(|candidate| candidate.canonical == expected)
            );
        }
    }

    #[test]
    fn complete_unicode_inputs_cross_read_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!("{}é中\tURLSession\n", "a".repeat(WORK_BYTES - 1));
        write(temp.path(), "unicode.txt", &text);
        let mut extractor = RecordingExtractor::default();
        scan_cached(temp.path(), &CancelToken::never(), &[], &mut extractor).unwrap();
        assert!(extractor.inputs.contains(&(SourceKind::Text, text)));
    }

    #[test]
    fn cached_empty_results_are_reused_and_edits_and_deletions_are_observed() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "notes.txt", "first-name");
        write(temp.path(), "gone.txt", "deleted-name");
        let mut extractor = RecordingExtractor::default();
        let first = scan_cached(temp.path(), &CancelToken::never(), &[], &mut extractor).unwrap();
        extractor.inputs.clear();
        let same = scan_cached(
            temp.path(),
            &CancelToken::never(),
            &first.sources,
            &mut extractor,
        )
        .unwrap();
        assert!(extractor.inputs.is_empty());
        assert_eq!(same.sources, first.sources);
        write(temp.path(), "notes.txt", "other-name");
        fs::remove_file(temp.path().join("gone.txt")).unwrap();
        let changed = scan_cached(
            temp.path(),
            &CancelToken::never(),
            &first.sources,
            &mut extractor,
        )
        .unwrap();
        assert_eq!(extractor.inputs, [(SourceKind::Text, "other-name".into())]);
        assert!(
            !changed
                .sources
                .iter()
                .any(|source| source.relative_path == Path::new("gone.txt"))
        );
    }

    #[test]
    fn invalid_cached_candidates_are_replaced_with_exact_input_spans() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "notes.txt", "URLSession ordinary");
        let first =
            scan_cached(temp.path(), &CancelToken::never(), &[], &mut LocalExtractor).unwrap();
        let source_index = first
            .sources
            .iter()
            .position(|source| source.kind == SourceKind::Text)
            .unwrap();
        let valid = first.sources[source_index].candidates[0].clone();
        for candidates in [
            vec![Candidate {
                canonical: "URLForgery".into(),
                span: 0..10,
            }],
            vec![Candidate {
                canonical: "URLSession".into(),
                span: 100..110,
            }],
            vec![Candidate {
                canonical: "ordinary".into(),
                span: 11..19,
            }],
            vec![valid.clone(), valid.clone()],
            vec![valid; MAX_CANDIDATES + 1],
        ] {
            let mut previous = first.sources.clone();
            previous[source_index].candidates = candidates;
            let repaired = scan_cached(
                temp.path(),
                &CancelToken::never(),
                &previous,
                &mut LocalExtractor,
            )
            .unwrap();
            assert_eq!(repaired.sources, first.sources);
        }
    }

    struct InvalidExtractor(Vec<Range<usize>>);
    impl Extractor for InvalidExtractor {
        type Error = io::Error;
        fn cache_key(&self) -> &str {
            "invalid-test"
        }
        fn extract(
            &mut self,
            _: TextInput<'_>,
            _: usize,
            _: &CancelToken,
        ) -> Result<Vec<Range<usize>>, Self::Error> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn invalid_spans_counts_duplicates_and_ineligible_candidates_fail() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("éAlpha");
        fs::create_dir(&root).unwrap();
        for spans in [
            std::iter::once(0..0).collect(),
            std::iter::once(0..999).collect(),
            std::iter::once(1..2).collect(),
            vec![0..7, 0..7],
            vec![0..7; 129],
        ] {
            let result = scan_cached(
                &root,
                &CancelToken::never(),
                &[],
                &mut InvalidExtractor(spans),
            );
            assert!(matches!(result, Err(Error::InvalidOutput)));
        }
    }

    struct MutatingExtractor(PathBuf);
    impl Extractor for MutatingExtractor {
        type Error = io::Error;
        fn cache_key(&self) -> &str {
            "mutating-test"
        }
        fn extract(
            &mut self,
            input: TextInput<'_>,
            _: usize,
            _: &CancelToken,
        ) -> Result<Vec<Range<usize>>, Self::Error> {
            if input.kind == SourceKind::Text {
                fs::write(&self.0, "changed-content-during-extraction")?;
            }
            Ok(Vec::new())
        }
    }

    #[test]
    fn source_changes_during_extraction_reject_the_generation() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "notes.txt", "original-name");
        let result = scan_cached(
            temp.path(),
            &CancelToken::never(),
            &[],
            &mut MutatingExtractor(temp.path().join("notes.txt")),
        );
        assert!(matches!(result, Err(Error::Unstable)));
    }

    #[test]
    fn cancellation_depth_and_replaced_links_fail_without_partial_results() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            scan_cached(
                temp.path(),
                &cancel,
                &[],
                &mut RecordingExtractor::default()
            ),
            Err(Error::Cancelled)
        ));
        let mut nested = temp.path().to_path_buf();
        for _ in 0..=MAX_DEPTH {
            nested.push("nested");
        }
        fs::create_dir_all(&nested).unwrap();
        assert!(matches!(
            scan_cached(
                temp.path(),
                &CancelToken::never(),
                &[],
                &mut RecordingExtractor::default()
            ),
            Err(Error::TooLarge)
        ));
        assert!(
            scan_cached(
                &nested,
                &CancelToken::never(),
                &[],
                &mut RecordingExtractor::default()
            )
            .is_ok()
        );
        let directory = open_directory(&temp.path().canonicalize().unwrap()).unwrap();
        symlink(&nested, temp.path().join("link")).unwrap();
        assert!(open_child_directory(&directory, OsStr::new("link")).is_err());
        assert!(open_beneath(&directory, Path::new("../outside")).is_err());
    }
}
