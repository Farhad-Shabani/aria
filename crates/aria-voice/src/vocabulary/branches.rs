//! Path names from a project's git branches: `origin/main` → `origin`.
//! Only git's ref files are read, never a `git` process, and no symlink
//! under the git folder is followed.

use std::collections::{BTreeSet, BinaryHeap};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use aria_kit::cancel::CancelToken;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

/// The most path names kept, the first in name order.
const MAX_PATH_NAMES: usize = 2048;
/// The most refs read from each of `heads`, `remotes` and `packed-refs`,
/// so one can't crowd out the others.
const MAX_REFS: usize = 20_000;
/// How many folders deep a branch name may go.
const MAX_DEPTH: usize = 8;
/// The largest `packed-refs` read. A larger one is skipped whole, so no
/// name is cut short.
const MAX_PACKED_BYTES: u64 = 1024 * 1024;
/// The largest `.git` file or `commondir` read. Each holds one path.
const MAX_POINTER_BYTES: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading branch names was cancelled")]
    Cancelled,
}

/// `origin/main` and `team/fix/typo` → `fix`, `origin`, `team`, sorted.
/// Empty when `root` isn't a readable repository.
pub fn path_names(root: &Path, cancel: &CancelToken) -> Result<Vec<String>, Error> {
    let Some(git) = common_git_dir(root) else {
        return Ok(Vec::new());
    };
    let mut names = BTreeSet::new();
    if let Some(refs) = open_child_dir(&git, "refs") {
        for kind in ["remotes", "heads"] {
            if let Some(folder) = open_child_dir(&refs, kind) {
                let mut left = MAX_REFS;
                read_ref_folder(&folder, "", 0, &mut left, &mut names, cancel)?;
            }
        }
    }
    push_packed_names(&git, &mut names, cancel)?;
    Ok(names.into_iter().take(MAX_PATH_NAMES).collect())
}

/// The git folder holding `root`'s refs. A linked worktree's `.git` file
/// and `commondir` lead there, followed as git follows them.
fn common_git_dir(root: &Path) -> Option<File> {
    let dot_git = root.join(".git");
    if let Some(folder) = open_dir(&dot_git, OFlags::NOFOLLOW) {
        return Some(folder);
    }
    let pointer = String::from_utf8(read_file(&dot_git, MAX_POINTER_BYTES)?).ok()?;
    let own = root.join(pointer.strip_prefix("gitdir:")?.trim());
    let common = match read_file(&own.join("commondir"), MAX_POINTER_BYTES) {
        Some(common) => own.join(String::from_utf8(common).ok()?.trim()),
        None => own,
    };
    open_dir(&common, OFlags::empty())
}

fn open_dir(path: &Path, flags: OFlags) -> Option<File> {
    let flags = flags | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    rustix::fs::open(path, flags, Mode::empty())
        .ok()
        .map(File::from)
}

fn open_child_dir(parent: &File, name: impl AsRef<OsStr>) -> Option<File> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(parent, name.as_ref(), flags, Mode::empty())
        .ok()
        .map(File::from)
}

/// The bytes of the regular file at `path`, when it holds at most `limit`.
/// Opening never follows a symlink or waits on a pipe.
fn read_file(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .ok()?;
    read_bounded(file, limit)
}

/// Like [`read_file`], for the file `name` in `folder`.
fn read_child_file(folder: &File, name: &str, limit: u64) -> Option<Vec<u8>> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = rustix::fs::openat(folder, name, flags, Mode::empty()).ok()?;
    read_bounded(File::from(file), limit)
}

fn read_bounded(file: File, limit: u64) -> Option<Vec<u8>> {
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    // One byte past the limit tells a file that grew past it since it was
    // opened.
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// Adds path names from the ref files under `folder`, reading at most
/// `left` refs. The first in sorted order win, however the folder lists.
fn read_ref_folder(
    folder: &File,
    prefix: &str,
    depth: usize,
    left: &mut usize,
    names: &mut BTreeSet<String>,
    cancel: &CancelToken,
) -> Result<(), Error> {
    let Ok(listing) = Dir::read_from(folder) else {
        return Ok(());
    };
    // Keeps the smallest names that still fit.
    let mut kept = BinaryHeap::new();
    for entry in listing {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Ok(entry) = entry else {
            break;
        };
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        kept.push(name.to_vec());
        if kept.len() > *left {
            kept.pop();
        }
    }
    for name in kept.into_sorted_vec() {
        if *left == 0 {
            break;
        }
        *left -= 1;
        let name = OsStr::from_bytes(&name);
        let Ok(stat) = rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        let Some(text) = name.to_str() else {
            continue;
        };
        let branch = format!("{prefix}{text}");
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory if depth < MAX_DEPTH => {
                if let Some(child) = open_child_dir(folder, name) {
                    read_ref_folder(
                        &child,
                        &format!("{branch}/"),
                        depth + 1,
                        left,
                        names,
                        cancel,
                    )?;
                }
            }
            FileType::RegularFile => add_path_names(&branch, names),
            _ => {}
        }
    }
    Ok(())
}

/// Adds path names from the branches `packed-refs` lists.
fn push_packed_names(
    git: &File,
    names: &mut BTreeSet<String>,
    cancel: &CancelToken,
) -> Result<(), Error> {
    let Some(contents) = read_child_file(git, "packed-refs", MAX_PACKED_BYTES) else {
        return Ok(());
    };
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let lines = contents.split(|byte| *byte == b'\n').take(MAX_REFS);
    for line in lines {
        let Some((_, name)) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.trim_end().split_once(' '))
        else {
            continue;
        };
        let branch = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/remotes/"));
        if let Some(branch) = branch {
            add_path_names(branch, names);
        }
    }
    Ok(())
}

/// `team/fix/typo` adds `team` and `fix`. Skips a branch that isn't plain
/// ASCII.
fn add_path_names(branch: &str, names: &mut BTreeSet<String>) {
    if !branch.chars().all(|c| c.is_ascii_graphic()) {
        return;
    }
    let Some((parents, _)) = branch.rsplit_once('/') else {
        return;
    };
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        names.insert(part.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn names(root: &Path) -> Vec<String> {
        path_names(root, &CancelToken::never()).unwrap()
    }

    #[test]
    fn loose_and_packed_refs_give_the_parts_before_a_slash() {
        let temp = tempfile::tempdir().unwrap();
        let git = temp.path().join(".git");
        write(&git.join("refs/heads/main"), "0\n");
        write(&git.join("refs/heads/feature/login"), "0\n");
        write(
            &git.join("refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/main\n",
        );
        write(
            &git.join("packed-refs"),
            "# pack-refs with: peeled\n\
             1 refs/remotes/upstream/team/main\n\
             ^2\n\
             3 refs/tags/release/v1\n",
        );
        assert_eq!(
            names(temp.path()),
            ["feature", "origin", "team", "upstream"]
        );
    }

    #[test]
    fn a_linked_worktree_reads_the_refs_of_its_common_folder() {
        let temp = tempfile::tempdir().unwrap();
        let common = temp.path().join("repo/.git");
        write(&common.join("refs/heads/fix/typo"), "0\n");
        write(&common.join("worktrees/tree/commondir"), "../..\n");
        let tree = temp.path().join("tree");
        write(
            &tree.join(".git"),
            &format!("gitdir: {}\n", common.join("worktrees/tree").display()),
        );
        assert_eq!(names(&tree), ["fix"]);
    }

    #[test]
    fn a_folder_without_a_repository_gives_none() {
        let temp = tempfile::tempdir().unwrap();
        assert!(names(temp.path()).is_empty());
    }

    #[test]
    fn no_ref_folder_is_followed_through_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join("elsewhere/team/secret"), "0\n");
        let heads = temp.path().join("inside/.git/refs/heads");
        fs::create_dir_all(&heads).unwrap();
        symlink(temp.path().join("elsewhere"), heads.join("linked")).unwrap();
        assert!(names(&temp.path().join("inside")).is_empty());
        let refs = temp.path().join("root/.git/refs");
        fs::create_dir_all(&refs).unwrap();
        symlink(temp.path().join("elsewhere"), refs.join("heads")).unwrap();
        assert!(names(&temp.path().join("root")).is_empty());
    }

    #[test]
    fn a_pipe_in_place_of_a_file_is_skipped_without_waiting() {
        let temp = tempfile::tempdir().unwrap();
        let git = temp.path().join(".git");
        write(&git.join("refs/heads/team/main"), "0\n");
        let mut mkfifo = std::process::Command::new("mkfifo");
        mkfifo.arg(git.join("packed-refs"));
        let outcome = aria_primitives::process::ChildProcess::spawn(&mut mkfifo)
            .unwrap()
            .wait_until(std::time::Instant::now() + Duration::from_secs(5), || false)
            .unwrap();
        assert!(matches!(
            outcome,
            aria_primitives::process::WaitOutcome::Exited(status) if status.success()
        ));
        let root = temp.path().to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(names(&root)).unwrap());
        let read = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("no wait");
        assert_eq!(read, ["team"]);
    }

    #[test]
    fn a_packed_list_past_its_limit_is_skipped_whole() {
        let temp = tempfile::tempdir().unwrap();
        let mut lines = String::from("0 refs/heads/team/main\n");
        while lines.len() as u64 <= MAX_PACKED_BYTES {
            lines.push_str("0 refs/heads/a-branch-name-that-fills-the-file\n");
        }
        write(&temp.path().join(".git/packed-refs"), &lines);
        assert!(names(temp.path()).is_empty());
    }

    #[test]
    fn a_line_that_isnt_text_leaves_the_others() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".git/packed-refs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"0 refs/heads/\xff/x\n0 refs/heads/team/main\r\n").unwrap();
        assert_eq!(names(temp.path()), ["team"]);
    }

    #[test]
    fn path_names_past_the_cap_drop_in_name_order() {
        let temp = tempfile::tempdir().unwrap();
        let lines: String = (0..MAX_PATH_NAMES + 10)
            .map(|index| format!("0 refs/heads/b{index:05}/x\n"))
            .collect();
        write(&temp.path().join(".git/packed-refs"), &lines);
        let kept = names(temp.path());
        assert_eq!(kept.len(), MAX_PATH_NAMES);
        assert_eq!(kept.last().map(String::as_str), Some("b02047"));
    }

    #[test]
    fn a_cancelled_read_says_so() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join(".git/refs/heads/team/main"), "0\n");
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            path_names(temp.path(), &cancel),
            Err(Error::Cancelled)
        ));
    }
}
