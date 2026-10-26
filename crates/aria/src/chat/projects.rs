//! Which project an ask names. A project is a git repo directly under a
//! configured root, named by its folder. Matching is a closed vocabulary:
//! a name counts when it appears as whole words anywhere in the utterance.

use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use crate::config::{ChatConfig, ConfigError, Environment};
use crate::primitives::private_fs::{self, PRIVATE_FILE_MODE};

const FOCUS_DIRECTORY: &str = ".aria";
const FOCUS_FILE: &str = "project";
/// Longer than any path this writes.
const MAX_FOCUS_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

/// The repos an ask may name, and the one later asks fall back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projects {
    home: PathBuf,
    known: Vec<Project>,
    focused: Option<Project>,
}

impl Projects {
    pub(crate) fn load_with(
        environment: &Environment,
        config: &ChatConfig,
    ) -> Result<Self, ProjectsError> {
        let known = discover(&config.project_roots)?;
        let focused = load_focus(environment.home())
            .map_err(ProjectsError::Focus)?
            .and_then(|path| known.iter().find(|project| project.path == path))
            .cloned();
        Ok(Self {
            home: environment.home().to_path_buf(),
            known,
            focused,
        })
    }

    pub fn mentioned(&self, utterance: &str) -> Mention {
        match mentioned(&self.known, utterance) {
            Ok(None) => Mention::None,
            Ok(Some(project)) => Mention::One(project),
            Err(candidates) => Mention::Several(Ambiguous { candidates }),
        }
    }

    pub fn focused(&self) -> Option<&Project> {
        self.focused.as_ref()
    }

    /// The project to keep for later asks.
    pub fn focus(&self, project: Project) -> Focus {
        Focus::on(&self.home, project)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectsError {
    #[error("failed to read the chat configuration")]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Discover(#[from] DiscoverError),

    #[error("the project focus file is unusable; remove ~/.aria/project")]
    Focus(#[source] io::Error),
}

/// What an utterance says about projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    None,
    One(Project),
    Several(Ambiguous),
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("failed to list projects under {}", root.display())]
    Unreadable {
        root: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Two repos that speech cannot tell apart.
    #[error("projects {} and {} share the name {name}", first.display(), second.display())]
    SameName {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
}

/// Direct children of each root with a `.git` entry. A worktree keeps
/// `.git` as a file, so existence is the test, not the kind. Paths are
/// canonical so one repo reached through two roots counts once.
fn discover(roots: &[PathBuf]) -> Result<Vec<Project>, DiscoverError> {
    let mut projects: Vec<Project> = Vec::new();
    for root in roots {
        let with_root = |source| DiscoverError::Unreadable {
            root: root.clone(),
            source,
        };
        for entry in std::fs::read_dir(root).map_err(with_root)? {
            let path = entry.map_err(with_root)?.path();
            if !path.join(".git").exists() {
                continue;
            }
            let path = path.canonicalize().map_err(with_root)?;
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if projects.iter().any(|known| known.path == path) {
                continue;
            }
            if let Some(twin) = projects
                .iter()
                .find(|known| fold(&known.name) == fold(name))
            {
                return Err(DiscoverError::SameName {
                    name: name.to_owned(),
                    first: twin.path.clone(),
                    second: path,
                });
            }
            projects.push(Project {
                name: name.to_owned(),
                path,
            });
        }
    }
    Ok(projects)
}

/// The one project the utterance names. A shorter name inside a longer
/// hit does not count, so "writ delegation" is `Writ-delegation` alone.
fn mentioned(projects: &[Project], utterance: &str) -> Result<Option<Project>, Vec<Project>> {
    let words = words(utterance);
    let mut hits: Vec<(&Project, usize, usize)> = Vec::new();
    for project in projects {
        let folded = fold(&project.name);
        if folded.is_empty() {
            continue;
        }
        for start in 0..words.len() {
            let mut run = String::new();
            for (end, word) in words.iter().enumerate().skip(start) {
                run.push_str(word);
                if run.len() >= folded.len() {
                    if run == folded {
                        hits.push((project, start, end + 1));
                    }
                    break;
                }
            }
        }
    }
    // Earliest first, longest first among equals, so a hit is covered
    // when an earlier one already reached its end.
    hits.sort_by_key(|&(_, start, end)| (start, std::cmp::Reverse(end)));
    let mut reach = 0;
    let mut named: Vec<&Project> = Vec::new();
    for &(project, _, end) in &hits {
        if end > reach && !named.contains(&project) {
            named.push(project);
        }
        reach = reach.max(end);
    }
    match named.as_slice() {
        [] => Ok(None),
        [project] => Ok(Some((*project).clone())),
        _ => Err(named.into_iter().cloned().collect()),
    }
}

fn words(utterance: &str) -> Vec<String> {
    utterance
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn fold(name: &str) -> String {
    words(name).concat()
}

/// Two or more projects named at once, in spoken order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguous {
    candidates: Vec<Project>,
}

impl Ambiguous {
    pub fn names(&self) -> Vec<&str> {
        self.candidates
            .iter()
            .map(|project| project.name.as_str())
            .collect()
    }

    /// The candidate the answer names, if exactly one.
    pub fn pick(&self, answer: &str) -> Option<Project> {
        mentioned(&self.candidates, answer).ok().flatten()
    }
}

/// The project later asks fall back to, and where it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    directory: PathBuf,
    project: Project,
}

impl Focus {
    fn on(home: &Path, project: Project) -> Self {
        Self {
            directory: home.join(FOCUS_DIRECTORY),
            project,
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Replaces the file whole, private to this user, never through a link.
    pub fn store(&self) -> io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
        temporary.write_all(self.project.path.as_os_str().as_encoded_bytes())?;
        temporary
            .persist(self.directory.join(FOCUS_FILE))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

/// The path the focus file holds. Only a private regular file of this
/// user counts; a link, a pipe, or anything oversized is refused.
fn load_focus(home: &Path) -> io::Result<Option<PathBuf>> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let path = home.join(FOCUS_DIRECTORY).join(FOCUS_FILE);
    let file = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(descriptor) => File::from(descriptor),
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = private_fs::verify_file(&file).map_err(io::Error::other)?;
    if metadata.len() > MAX_FOCUS_BYTES {
        return Err(io::Error::other("the focus file is too large to be a path"));
    }
    let mut path = String::new();
    file.take(MAX_FOCUS_BYTES).read_to_string(&mut path)?;
    Ok(Some(PathBuf::from(path.trim_end())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> Project {
        Project {
            name: name.to_owned(),
            path: PathBuf::from("/repos").join(name),
        }
    }

    fn repo(root: &Path, name: &str) {
        std::fs::create_dir_all(root.join(name).join(".git")).unwrap();
    }

    fn registry() -> Vec<Project> {
        vec![
            project("Writ"),
            project("Writ-delegation"),
            project("Hevy"),
            project("bar"),
            project("Aria-Watch"),
        ]
    }

    #[test]
    fn a_name_anywhere_in_the_utterance_routes() {
        let found = mentioned(&registry(), "in hevy, why does the build fail?").unwrap();
        assert_eq!(found, Some(project("Hevy")));
    }

    #[test]
    fn hyphens_and_case_fold_both_ways() {
        let projects = registry();
        assert_eq!(
            mentioned(&projects, "run the aria watch tests").unwrap(),
            Some(project("Aria-Watch"))
        );
        assert_eq!(
            mentioned(&projects, "run the AriaWatch tests").unwrap(),
            Some(project("Aria-Watch"))
        );
    }

    #[test]
    fn a_name_inside_a_longer_word_is_a_miss() {
        assert_eq!(mentioned(&registry(), "paint the barn").unwrap(), None);
    }

    #[test]
    fn the_longest_name_wins() {
        let found = mentioned(&registry(), "in writ delegation, list the tests").unwrap();
        assert_eq!(found, Some(project("Writ-delegation")));
    }

    #[test]
    fn the_same_name_twice_is_one_hit() {
        let found = mentioned(&registry(), "hevy, is hevy building?").unwrap();
        assert_eq!(found, Some(project("Hevy")));
    }

    #[test]
    fn two_names_are_ambiguous_in_spoken_order() {
        let candidates = mentioned(&registry(), "compare the hevy and writ builds").unwrap_err();
        let ambiguous = Ambiguous { candidates };

        assert_eq!(ambiguous.names(), ["Hevy", "Writ"]);
        assert_eq!(ambiguous.pick("hevy please"), Some(project("Hevy")));
        assert_eq!(ambiguous.pick("bar"), None);
    }

    #[test]
    fn nothing_named_is_none() {
        let projects = registry();
        assert_eq!(
            mentioned(&projects, "why does the build fail?").unwrap(),
            None
        );
        assert_eq!(mentioned(&projects, "").unwrap(), None);
    }

    #[test]
    fn discovery_keeps_repos_and_worktrees_only() {
        let root = tempfile::tempdir().unwrap();
        repo(root.path(), "repo");
        std::fs::create_dir(root.path().join("worktree")).unwrap();
        std::fs::write(root.path().join("worktree/.git"), "gitdir: elsewhere").unwrap();
        std::fs::create_dir(root.path().join("plain")).unwrap();
        std::fs::write(root.path().join("file"), "").unwrap();
        let other = tempfile::tempdir().unwrap();
        repo(other.path(), "second");

        let mut names: Vec<String> =
            discover(&[root.path().to_path_buf(), other.path().to_path_buf()])
                .unwrap()
                .into_iter()
                .map(|project| project.name)
                .collect();
        names.sort();

        assert_eq!(names, ["repo", "second", "worktree"]);
    }

    #[test]
    fn one_repo_under_two_roots_counts_once() {
        let root = tempfile::tempdir().unwrap();
        repo(root.path(), "repo");
        let roots = [root.path().to_path_buf(), root.path().to_path_buf()];

        assert_eq!(discover(&roots).unwrap().len(), 1);
    }

    #[test]
    fn two_repos_with_one_spoken_name_are_refused() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        repo(first.path(), "aria-bar");
        repo(second.path(), "AriaBar");
        let roots = [first.path().to_path_buf(), second.path().to_path_buf()];

        let error = discover(&roots).unwrap_err();

        assert!(matches!(error, DiscoverError::SameName { name, .. } if name == "AriaBar"));
    }

    #[test]
    fn a_missing_root_names_itself() {
        let error = discover(&[PathBuf::from("/definitely/not/here")]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "failed to list projects under /definitely/not/here"
        );
    }

    #[test]
    fn the_focus_round_trips_as_a_private_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();

        assert_eq!(load_focus(home.path()).unwrap(), None);
        Focus::on(home.path(), project("Hevy")).store().unwrap();

        assert_eq!(
            load_focus(home.path()).unwrap(),
            Some(PathBuf::from("/repos/Hevy"))
        );
        let mode = std::fs::metadata(home.path().join(".aria/project"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_FILE_MODE);
    }

    #[test]
    fn a_linked_or_shared_focus_file_is_refused() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        let focus = home.path().join(".aria/project");

        std::os::unix::fs::symlink("/etc/hosts", &focus).unwrap();
        assert!(load_focus(home.path()).is_err());

        std::fs::remove_file(&focus).unwrap();
        std::fs::write(&focus, "/repos/Hevy").unwrap();
        std::fs::set_permissions(&focus, Permissions::from_mode(0o644)).unwrap();
        assert!(load_focus(home.path()).is_err());
    }
}
