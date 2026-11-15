use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use aria_kit::cancel::{CancelToken, Deadline};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::collection::{Collector, Error};
use super::extract::{Extractor, TextInput};
use super::{
    Candidate, Completeness, Coverage, Discovery, MAX_CANDIDATES, MAX_SOURCE_BYTES, MAX_TERMS,
    Source, SourceKind, cache_key, source_bytes,
};

const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_IGNORE_BYTES: usize = 64 * 1024;
const MAX_ENTRIES: usize = 20_000;
const DIRECTORY_ENTRIES: usize = 4096;
const MAX_DEPTH: usize = 16;
const WORK_BYTES: usize = 4096;

pub struct LocalCollector<E> {
    extractor: E,
}

impl<E: Extractor> LocalCollector<E> {
    pub fn new(extractor: E) -> Self {
        Self { extractor }
    }
}

impl<E: Extractor> Collector for LocalCollector<E> {
    type Error = E::Error;

    fn cache_key(&self) -> Option<String> {
        cache_key(self.extractor.cache_key())
    }

    fn collect(
        &mut self,
        selected: &aria_settings::Directory,
        previous: &[Source],
        deadline: Deadline,
        cancel: &CancelToken,
    ) -> Result<Discovery, Error<Self::Error>> {
        scan_cached(
            selected.as_path(),
            previous,
            &mut self.extractor,
            deadline,
            cancel,
        )
    }
}

pub fn resolve(selected: &Path) -> io::Result<PathBuf> {
    if !selected.is_absolute() || selected.as_os_str().len() > 4096 {
        return Err(io::Error::other("expected an absolute directory"));
    }
    let root = selected.canonicalize()?;
    open_directory(&root)?;
    Ok(root)
}

/// Lists file and folder names breadth-first until `deadline`. A limit, the
/// deadline, or an unreadable folder shortens the result and marks it
/// partial. Cancellation, a failing extractor, or a missing or replaced
/// selected folder fails it.
pub fn scan_cached<E: Extractor>(
    selected: &Path,
    previous: &[Source],
    extractor: &mut E,
    deadline: Deadline,
    cancel: &CancelToken,
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
        deadline,
        coverage: Coverage::default(),
        term_count: 0,
        sources: Vec::new(),
        retained_bytes: 0,
        stopped: false,
    };
    for source in previous {
        check_cancelled(cancel)?;
        scanner
            .previous
            .insert((source.relative_path.as_path(), source.kind), source);
    }
    if let Some(name) = root.file_name().and_then(OsStr::to_str) {
        scanner.input(Path::new("."), SourceKind::DirectoryName, &composed(name))?;
    }
    scanner.walk(&directory)?;
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
    deadline: Deadline,
    coverage: Coverage,
    term_count: usize,
    sources: Vec<Source>,
    retained_bytes: usize,
    /// A walk-wide limit was reached; what was found so far stays.
    stopped: bool,
}

/// A folder waiting to be listed, with the `.gitignore` rules above it.
struct Pending {
    path: PathBuf,
    depth: usize,
    ignores: Vec<Rc<Gitignore>>,
}

/// A folder's entry names in byte order.
struct Listing {
    names: Vec<OsString>,
    /// The walk's entry budget ran out within this folder.
    budget_spent: bool,
}

enum Rules {
    Absent,
    Present(Gitignore),
    Unreadable,
    /// Reading them spent a walk-wide budget.
    Stopped,
}

impl<E: Extractor> Scanner<'_, E> {
    fn walk(&mut self, root: &File) -> Result<(), Error<E::Error>> {
        let mut queue = VecDeque::from([Pending {
            path: PathBuf::from("."),
            depth: 0,
            ignores: Vec::new(),
        }]);
        let mut opened = Opened::default();
        while let Some(pending) = queue.pop_front() {
            check_cancelled(self.cancel)?;
            self.stop_if_late();
            if self.stopped {
                break;
            }
            let Ok(directory) = opened.open(root, &pending.path) else {
                self.skip_unreadable();
                continue;
            };
            self.list(&directory, pending, &mut queue)?;
        }
        check_cancelled(self.cancel)
    }

    fn list(
        &mut self,
        directory: &File,
        pending: Pending,
        queue: &mut VecDeque<Pending>,
    ) -> Result<(), Error<E::Error>> {
        let mut ignores = pending.ignores;
        match self.ignore_rules(directory, &pending.path)? {
            Rules::Absent => {}
            Rules::Present(rules) => ignores.push(Rc::new(rules)),
            Rules::Unreadable => {
                self.skip_unreadable();
                return Ok(());
            }
            Rules::Stopped => return Ok(()),
        }
        let Some(Listing {
            names,
            budget_spent,
        }) = self.entries(directory)?
        else {
            self.skip_unreadable();
            return Ok(());
        };
        for name in names {
            check_cancelled(self.cancel)?;
            self.stop_if_late();
            if self.stopped {
                return Ok(());
            }
            let name = name.as_os_str();
            if name.as_bytes().starts_with(b".") {
                self.coverage.hidden += 1;
                continue;
            }
            // An entry removed since the listing is skipped; the next refresh drops it.
            let Ok(metadata) = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            else {
                continue;
            };
            check_cancelled(self.cancel)?;
            let kind = FileType::from_raw_mode(metadata.st_mode);
            let path = join_relative(&pending.path, name);
            if ignores
                .iter()
                .rev()
                .map(|rules| rules.matched(&path, kind == FileType::Directory))
                .find(|matched| !matched.is_none())
                .is_some_and(|matched| matched.is_ignore())
            {
                continue;
            }
            match kind {
                FileType::Directory => {
                    if is_linked(directory, name) {
                        self.coverage.linked += 1;
                        continue;
                    }
                    if let Some(text) = name.to_str() {
                        self.input(&path, SourceKind::DirectoryName, &composed(text))?;
                    }
                    if pending.depth == MAX_DEPTH {
                        self.mark(Completeness::Partial);
                        continue;
                    }
                    queue.push_back(Pending {
                        path,
                        depth: pending.depth + 1,
                        ignores: ignores.clone(),
                    });
                }
                FileType::RegularFile | FileType::Symlink => {
                    if kind == FileType::Symlink {
                        self.coverage.symlinks += 1;
                    }
                    if let Some(text) = name.to_str() {
                        self.input(&path, SourceKind::FileName, &composed(text))?;
                    }
                }
                _ => self.coverage.unsupported += 1,
            }
        }
        // Stops only after this folder, so names counted against the entry
        // budget are kept.
        if budget_spent {
            self.stop();
        }
        Ok(())
    }

    /// The folder's listing, or `None` when it can't be read.
    fn entries(&mut self, directory: &File) -> Result<Option<Listing>, Error<E::Error>> {
        let Ok(reader) = Dir::read_from(directory) else {
            return Ok(None);
        };
        let mut names = Vec::new();
        let mut budget_spent = false;
        for entry in reader {
            check_cancelled(self.cancel)?;
            let Ok(entry) = entry else {
                self.coverage.unreadable += 1;
                self.mark(Completeness::Partial);
                break;
            };
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if names.len() == DIRECTORY_ENTRIES {
                self.mark(Completeness::Partial);
                break;
            }
            if self.coverage.entries == MAX_ENTRIES {
                budget_spent = true;
                break;
            }
            self.coverage.entries += 1;
            names.push(OsString::from_vec(name.to_vec()));
        }
        names.sort_unstable();
        Ok(Some(Listing {
            names,
            budget_spent,
        }))
    }

    fn ignore_rules(&mut self, directory: &File, scope: &Path) -> Result<Rules, Error<E::Error>> {
        check_cancelled(self.cancel)?;
        let descriptor = match rustix::fs::openat(
            directory,
            ".gitignore",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            // Git doesn't follow a symlinked `.gitignore` either.
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(Rules::Absent),
            Err(_) => return Ok(Rules::Unreadable),
        };
        let mut file = File::from(descriptor);
        match file.metadata() {
            Ok(metadata) if !metadata.is_file() => return Ok(Rules::Absent),
            Ok(metadata) if metadata.len() <= MAX_IGNORE_BYTES as u64 => {}
            _ => return Ok(Rules::Unreadable),
        }
        let mut contents = Vec::new();
        let mut buffer = [0; WORK_BYTES];
        loop {
            check_cancelled(self.cancel)?;
            let Ok(count) = file.read(&mut buffer) else {
                return Ok(Rules::Unreadable);
            };
            self.coverage.ignore_bytes += count;
            if self.coverage.ignore_bytes > MAX_TOTAL_BYTES {
                self.stop();
                return Ok(Rules::Stopped);
            }
            if contents.len() + count > MAX_IGNORE_BYTES {
                return Ok(Rules::Unreadable);
            }
            if count == 0 {
                break;
            }
            contents.extend_from_slice(&buffer[..count]);
        }
        let text = String::from_utf8_lossy(&contents);
        let path = join_relative(scope, OsStr::new(".gitignore"));
        let mut builder = GitignoreBuilder::new(scope);
        for line in text.trim_start_matches('\u{feff}').lines() {
            check_cancelled(self.cancel)?;
            if !self.reserve(512 + 2 * line.len()) {
                return Ok(Rules::Stopped);
            }
            // Git skips a pattern it can't parse and applies the rest.
            let _ = builder.add_line(Some(path.clone()), line);
        }
        let Ok(rules) = builder.build() else {
            return Ok(Rules::Unreadable);
        };
        check_cancelled(self.cancel)?;
        Ok(Rules::Present(rules))
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
            let mut valid =
                !previous.candidates.is_empty() && previous.candidates.len() <= MAX_CANDIDATES;
            let mut seen = HashSet::new();
            if valid {
                for candidate in &previous.candidates {
                    check_cancelled(self.cancel)?;
                    if !super::select::eligible(&candidate.canonical)
                        || text.get(candidate.span.clone()) != Some(candidate.canonical.as_str())
                        || !seen.insert(candidate.canonical.as_str())
                    {
                        valid = false;
                        break;
                    }
                }
            }
            if valid {
                if !self.retain(source_bytes(previous), previous.candidates.len()) {
                    return Ok(());
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
            if !super::select::eligible(canonical) || !seen.insert(canonical) {
                return Err(Error::InvalidOutput);
            }
        }
        // The catalog keeps only names it can type, not every path walked.
        if spans.is_empty() {
            return Ok(());
        }
        let bytes = 512
            + 2 * path.as_os_str().len()
            + spans.iter().map(|span| 256 + 2 * span.len()).sum::<usize>();
        if !self.retain(bytes, spans.len()) {
            return Ok(());
        }
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

    /// Whether a source of `bytes` holding `terms` names still fits.
    fn retain(&mut self, bytes: usize, terms: usize) -> bool {
        if self.term_count + terms > MAX_TERMS || !self.reserve(bytes) {
            self.stop();
            return false;
        }
        self.term_count += terms;
        true
    }

    fn reserve(&mut self, bytes: usize) -> bool {
        if self.retained_bytes + bytes > MAX_SOURCE_BYTES {
            self.stop();
            return false;
        }
        self.retained_bytes += bytes;
        true
    }

    fn stop_if_late(&mut self) {
        if !self.stopped && self.deadline.expired() {
            self.stop();
            self.mark(Completeness::OutOfTime);
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.mark(Completeness::Partial);
    }

    fn mark(&mut self, completeness: Completeness) {
        self.coverage.completeness = self.coverage.completeness.max(completeness);
    }

    fn skip_unreadable(&mut self) {
        self.coverage.unreadable += 1;
        self.mark(Completeness::Partial);
    }
}

/// A name in NFC, the form dictation writes. A file system may store it
/// decomposed, as `e` followed by a combining accent.
fn composed(name: &str) -> String {
    name.nfc().collect()
}

/// A folder whose `.git` is a file is a linked worktree or a submodule: a
/// copy of a repository, or another project's files.
fn is_linked(parent: &File, name: &OsStr) -> bool {
    rustix::fs::statat(
        parent,
        Path::new(name).join(".git"),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .is_ok_and(|metadata| FileType::from_raw_mode(metadata.st_mode) == FileType::RegularFile)
}

/// The folders open along the last path listed. The walk visits siblings
/// one after another, so the next folder usually opens from its parent here
/// instead of from the root, and at most `MAX_DEPTH` handles stay open.
#[derive(Default)]
struct Opened {
    chain: Vec<(OsString, File)>,
}

impl Opened {
    fn open(&mut self, root: &File, path: &Path) -> io::Result<File> {
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => names.push(name),
                _ => return Err(io::Error::other("invalid relative project path")),
            }
        }
        let shared = self
            .chain
            .iter()
            .zip(&names)
            .take_while(|((open, _), name)| open.as_os_str() == **name)
            .count();
        self.chain.truncate(shared);
        for name in &names[shared..] {
            let parent = self.chain.last().map_or(root, |(_, directory)| directory);
            let child = open_child_directory(parent, name)?;
            self.chain.push((name.to_os_string(), child));
        }
        self.chain
            .last()
            .map_or(root, |(_, directory)| directory)
            .try_clone()
    }
}

fn check_cancelled<E: std::error::Error + 'static>(cancel: &CancelToken) -> Result<(), Error<E>> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn open_directory(path: &Path) -> Result<File, io::Error> {
    if !path.is_absolute() {
        return Err(io::Error::other("expected an absolute directory"));
    }
    let root = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    open_beneath(&File::from(root), path.strip_prefix("/").unwrap_or(path))
}

/// Opens the directory at `path` under `root`, refusing a symlink at every step.
fn open_beneath(root: &File, path: &Path) -> Result<File, io::Error> {
    let mut current = root.try_clone()?;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => current = open_child_directory(&current, name)?,
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

#[cfg(test)]
mod tests {
    use super::super::extract::LocalExtractor;
    use super::*;
    use std::fs;
    use std::ops::Range;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn scan(root: &Path) -> Discovery {
        scan_cached(
            root,
            &[],
            &mut LocalExtractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap()
    }

    fn paths(discovery: &Discovery) -> HashSet<&Path> {
        discovery
            .sources
            .iter()
            .map(|source| source.relative_path.as_path())
            .collect()
    }

    fn names(discovery: &Discovery) -> HashSet<&str> {
        discovery
            .sources
            .iter()
            .flat_map(|source| &source.candidates)
            .map(|candidate| candidate.canonical.as_str())
            .collect()
    }

    /// A sayable name per index: `item-aaaa.rs`, `item-aaab.rs`, …
    fn item(mut index: usize) -> String {
        let mut letters = [b'a'; 4];
        for letter in letters.iter_mut().rev() {
            *letter += (index % 26) as u8;
            index /= 26;
        }
        format!("item-{}.rs", std::str::from_utf8(&letters).unwrap())
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
            Ok(LocalExtractor
                .extract(input, MAX_CANDIDATES, &CancelToken::never())
                .unwrap())
        }
    }

    #[test]
    fn file_contents_never_yield_names() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "notes.txt", "Use ledger-api and URLSession.\n");
        let result = scan(temp.path());
        assert!(names(&result).contains("notes.txt"));
        assert!(!names(&result).contains("ledger-api"));
        assert_eq!(result.coverage.completeness, Completeness::Whole);
    }

    #[test]
    fn gitignore_prunes_subtrees_and_updates_cached_sources() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".gitignore", "*\n");
        let selected = temp.path().join("selected");
        let root = selected.as_path();
        for path in [
            "target/generated.txt",
            "keep.txt",
            "drop.txt",
            "root-only.rs",
            "src/root-only.rs",
            "src/inside.txt",
            "src/blocked/escape.txt",
            "other/inside.txt",
        ] {
            write(root, path, "");
        }
        let first = scan(root);
        assert!(paths(&first).contains(Path::new("target/generated.txt")));

        write(
            root,
            ".gitignore",
            "target/\n*.txt\n!keep.txt\n/root-only.rs\n",
        );
        write(
            root,
            "src/.gitignore",
            "!inside.txt\nblocked/\n!blocked/escape.txt\n",
        );
        let mut deep = root.join("target");
        for _ in 0..=MAX_DEPTH {
            deep.push("nested");
        }
        fs::create_dir_all(deep).unwrap();
        let refreshed = scan_cached(
            root,
            &first.sources,
            &mut LocalExtractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(refreshed.coverage.completeness, Completeness::Whole);
        let listed = paths(&refreshed);
        for included in ["keep.txt", "src/inside.txt", "src/root-only.rs"] {
            assert!(listed.contains(Path::new(included)), "missing {included}");
        }
        for excluded in [
            "target",
            "target/generated.txt",
            "drop.txt",
            "root-only.rs",
            "src/blocked",
            "src/blocked/escape.txt",
            "other/inside.txt",
        ] {
            assert!(!listed.contains(Path::new(excluded)), "included {excluded}");
        }

        write(root, "src/.gitignore", "inside.txt\nblocked/\n");
        let changed = scan_cached(
            root,
            &refreshed.sources,
            &mut LocalExtractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert!(!paths(&changed).contains(Path::new("src/inside.txt")));
    }

    #[test]
    fn unreadable_folders_and_ignore_files_skip_only_their_folder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "open-name.rs", "");
        write(root, "locked/locked-name.rs", "");
        write(
            root,
            "oversized/.gitignore",
            vec![b'a'; MAX_IGNORE_BYTES + 1],
        );
        write(root, "oversized/oversized-name.rs", "");
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let result = scan_cached(
            root,
            &[],
            &mut LocalExtractor,
            Deadline::never(),
            &CancelToken::never(),
        );
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
        let result = result.unwrap();
        let found = names(&result);
        assert!(found.contains("open-name.rs"));
        assert!(!found.contains("locked-name.rs"));
        assert!(!found.contains("oversized-name.rs"));
        assert_eq!(result.coverage.unreadable, 2);
        assert_eq!(result.coverage.completeness, Completeness::Partial);
    }

    #[test]
    fn a_gitignore_is_read_as_git_reads_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, ".gitignore", b"a[\n\xff\n*.log\n");
        write(root, "kept-name.rs", "");
        write(root, "dropped-name.log", "");
        write(root, "rules/ignore-all.txt", "*\n");
        write(root, "linked/linked-name.rs", "");
        symlink(
            root.join("rules/ignore-all.txt"),
            root.join("linked/.gitignore"),
        )
        .unwrap();
        let result = scan(root);
        let found = names(&result);
        assert!(found.contains("kept-name.rs"));
        assert!(!found.contains("dropped-name.log"));
        assert!(found.contains("linked-name.rs"));
        assert_eq!(result.coverage.completeness, Completeness::Whole);
    }

    #[test]
    fn only_names_it_can_type_are_kept_and_decomposed_ones_are_composed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "Taxes 2025/Settlement letter.pdf", "");
        write(root, "cafe\u{301}-menu/README", "");
        let result = scan(root);
        assert_eq!(
            paths(&result),
            HashSet::from([Path::new("cafe\u{301}-menu")])
        );
        assert_eq!(names(&result), HashSet::from(["caf\u{e9}-menu"]));
    }

    #[test]
    fn nested_folders_with_a_git_file_are_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "linked-copy/.git",
            "gitdir: /elsewhere/.git/worktrees/copy\n",
        );
        write(root, "linked-copy/copied-name.rs", "");
        write(root, "nested-repo/.git/HEAD", "ref: refs/heads/main\n");
        write(root, "nested-repo/own-name.rs", "");
        let result = scan(root);
        let found = names(&result);
        assert!(!found.contains("linked-copy"));
        assert!(!found.contains("copied-name.rs"));
        assert!(found.contains("nested-repo"));
        assert!(found.contains("own-name.rs"));
        assert_eq!(result.coverage.linked, 1);
        assert_eq!(result.coverage.completeness, Completeness::Whole);
    }

    #[test]
    fn symlink_names_are_taken_without_following_them() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "elsewhere/inner-name.rs", "");
        let selected = temp.path().join("selected");
        fs::create_dir(&selected).unwrap();
        symlink(temp.path().join("elsewhere"), selected.join("linked-tree")).unwrap();
        symlink(
            temp.path().join("elsewhere/inner-name.rs"),
            selected.join("alias-name.rs"),
        )
        .unwrap();
        let result = scan(&selected);
        let found = names(&result);
        assert!(found.contains("linked-tree"));
        assert!(found.contains("alias-name.rs"));
        assert!(!found.contains("inner-name.rs"));
        assert_eq!(result.coverage.symlinks, 2);
    }

    #[test]
    fn hidden_entries_are_skipped_but_a_hidden_selected_folder_is_named() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".selected");
        write(&root, ".hidden/secret-name.txt", "");
        write(&root, "binary.dat", b"\0\xff");
        let mut extractor = RecordingExtractor::default();
        let result = scan_cached(
            &root,
            &[],
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(result.coverage.hidden, 1);
        assert_eq!(
            extractor.inputs,
            [
                (SourceKind::DirectoryName, ".selected".into()),
                (SourceKind::FileName, "binary.dat".into()),
            ]
        );
    }

    #[test]
    fn a_scan_past_its_deadline_keeps_what_it_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("my-project");
        write(&root, "later-name.rs", "");
        let result = scan_cached(
            &root,
            &[],
            &mut LocalExtractor,
            Deadline::after(std::time::Duration::ZERO),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(names(&result), HashSet::from(["my-project"]));
        assert_eq!(result.coverage.completeness, Completeness::OutOfTime);
    }

    #[test]
    fn a_deep_tree_keeps_its_shallow_names() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "top-name.rs", "");
        let mut deep = temp.path().to_path_buf();
        for _ in 0..=MAX_DEPTH {
            deep.push("nested");
        }
        write(&deep, "deep-name.rs", "");
        let result = scan(temp.path());
        let found = names(&result);
        assert!(found.contains("top-name.rs"));
        assert!(!found.contains("deep-name.rs"));
        assert_eq!(result.coverage.completeness, Completeness::Partial);
    }

    #[test]
    fn a_wide_folder_ends_its_own_listing_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("wide")).unwrap();
        for index in 0..=DIRECTORY_ENTRIES {
            fs::write(root.join("wide").join(item(index)), "").unwrap();
        }
        write(root, "zeta/after-name.rs", "");
        let result = scan(root);
        let paths = paths(&result);
        assert!(paths.contains(Path::new("zeta/after-name.rs")));
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.parent() == Some(Path::new("wide")))
                .count(),
            DIRECTORY_ENTRIES
        );
        assert_eq!(result.coverage.completeness, Completeness::Partial);
    }

    #[test]
    fn the_entry_budget_keeps_shallow_names() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "top-name.rs", "");
        write(root, "a/b/deep-name.rs", "");
        let folders = MAX_ENTRIES / DIRECTORY_ENTRIES + 1;
        for folder in 0..folders {
            let folder = root.join(format!("wide{folder}"));
            fs::create_dir(&folder).unwrap();
            for index in 0..DIRECTORY_ENTRIES {
                fs::write(folder.join(item(index)), "").unwrap();
            }
        }
        let result = scan(root);
        let found = names(&result);
        assert!(found.contains("top-name.rs"));
        assert!(!found.contains("deep-name.rs"));
        assert_eq!(result.coverage.entries, MAX_ENTRIES);
        assert_eq!(result.coverage.completeness, Completeness::Partial);
        let last = PathBuf::from(format!("wide{}", folders - 1)).join(item(0));
        assert!(paths(&result).contains(last.as_path()));
    }

    #[test]
    fn generic_folders_have_independent_inputs_and_exact_selected_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "JavaTree/Widget.java", "class WidgetFactory {}\n");
        write(root, "SwiftTree/View.swift", "");
        write(root, "Documents/notes.txt", "");
        fs::create_dir(root.join("JavaTree/.git")).unwrap();
        let mut extractor = RecordingExtractor::default();
        let all = scan_cached(
            root,
            &[],
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(all.root, root.canonicalize().unwrap());
        for expected in ["Widget.java", "View.swift", "notes.txt"] {
            assert!(
                extractor
                    .inputs
                    .contains(&(SourceKind::FileName, expected.into()))
            );
        }
        extractor.inputs.clear();
        let selected = root.join("JavaTree");
        let child = scan_cached(
            &selected,
            &[],
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(child.root, selected.canonicalize().unwrap());
        assert!(
            !extractor
                .inputs
                .iter()
                .any(|(_, text)| text == "View.swift")
        );
        assert!(
            extractor
                .inputs
                .iter()
                .any(|(_, text)| text == "Widget.java")
        );
    }

    #[test]
    fn cached_names_are_reused_and_renames_and_deletions_are_observed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("my-project");
        write(&root, "first-name.rs", "");
        write(&root, "gone-name.rs", "");
        let mut extractor = RecordingExtractor::default();
        let first = scan_cached(
            &root,
            &[],
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        extractor.inputs.clear();
        let same = scan_cached(
            &root,
            &first.sources,
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert!(extractor.inputs.is_empty());
        assert_eq!(same.sources, first.sources);
        fs::rename(root.join("first-name.rs"), root.join("other-name.rs")).unwrap();
        fs::remove_file(root.join("gone-name.rs")).unwrap();
        let changed = scan_cached(
            &root,
            &first.sources,
            &mut extractor,
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(
            extractor.inputs,
            [(SourceKind::FileName, "other-name.rs".into())]
        );
        assert!(!paths(&changed).contains(Path::new("gone-name.rs")));
    }

    #[test]
    fn invalid_cached_candidates_are_replaced_with_exact_input_spans() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "aria-kit.rs", "");
        let first = scan(temp.path());
        let source_index = first
            .sources
            .iter()
            .position(|source| source.relative_path == Path::new("aria-kit.rs"))
            .unwrap();
        let valid = first.sources[source_index].candidates[0].clone();
        for candidates in [
            vec![Candidate {
                canonical: "aria-xyz.rs".into(),
                span: 0..11,
            }],
            vec![Candidate {
                canonical: "aria-kit.rs".into(),
                span: 100..111,
            }],
            vec![Candidate {
                canonical: "kit".into(),
                span: 5..8,
            }],
            vec![valid.clone(), valid.clone()],
            vec![valid; MAX_CANDIDATES + 1],
        ] {
            let mut previous = first.sources.clone();
            previous[source_index].candidates = candidates;
            let repaired = scan_cached(
                temp.path(),
                &previous,
                &mut LocalExtractor,
                Deadline::never(),
                &CancelToken::never(),
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
        let root = temp.path().join("é-Alpha");
        fs::create_dir(&root).unwrap();
        for spans in [
            std::iter::once(0..0).collect(),
            std::iter::once(0..999).collect(),
            std::iter::once(1..2).collect(),
            std::iter::once(3..8).collect(),
            vec![0..8, 0..8],
            vec![0..8; MAX_CANDIDATES + 1],
        ] {
            let result = scan_cached(
                &root,
                &[],
                &mut InvalidExtractor(spans),
                Deadline::never(),
                &CancelToken::never(),
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
            limit: usize,
            cancel: &CancelToken,
        ) -> Result<Vec<Range<usize>>, Self::Error> {
            if self.0.exists() {
                fs::remove_file(&self.0)?;
            }
            Ok(LocalExtractor.extract(input, limit, cancel).unwrap())
        }
    }

    #[test]
    fn a_change_during_the_walk_does_not_stop_it() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "a-first.rs", "");
        write(temp.path(), "z-gone.rs", "");
        let result = scan_cached(
            temp.path(),
            &[],
            &mut MutatingExtractor(temp.path().join("z-gone.rs")),
            Deadline::never(),
            &CancelToken::never(),
        )
        .unwrap();
        let paths = paths(&result);
        assert!(paths.contains(Path::new("a-first.rs")));
        assert!(!paths.contains(Path::new("z-gone.rs")));
    }

    #[test]
    fn cancellation_fails_and_links_are_never_opened() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            scan_cached(
                temp.path(),
                &[],
                &mut RecordingExtractor::default(),
                Deadline::never(),
                &cancel
            ),
            Err(Error::Cancelled)
        ));
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let directory = open_directory(&temp.path().canonicalize().unwrap()).unwrap();
        symlink(&nested, temp.path().join("link")).unwrap();
        assert!(open_child_directory(&directory, OsStr::new("link")).is_err());
        assert!(open_beneath(&directory, Path::new("link")).is_err());
        assert!(open_beneath(&directory, Path::new("../outside")).is_err());
    }
}
