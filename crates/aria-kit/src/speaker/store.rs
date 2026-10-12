//! The speakers directory: enrolled voices read back, stored, and forgotten.
//!
//! One document per person, named for them. The engine that measured a
//! profile is recorded with it, because two engines put a voice in different
//! places and comparing across them is meaningless.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::primitives::private_document::{DocumentError, PrivateDocument};

use super::{Profile, Role, SpeakerName, SpeakerNameError};

/// A profile is a name, a role, and a few hundred floats. Anything larger is
/// not one.
const MAX_PROFILE_BYTES: u64 = 64 * 1024;

/// The directory of enrolled voices.
pub struct ProfileStore {
    directory: PathBuf,
}

impl ProfileStore {
    pub fn open(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Every readable profile, sorted by name.
    ///
    /// A profile that will not load is skipped with a warning: one unreadable
    /// file must not cost a session every other enrolled voice. A directory
    /// that does not exist means nobody has enrolled, which is ordinary.
    pub fn read_all(&self) -> Vec<Profile> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut profiles = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                // Loud, because a profile this build cannot read is a person
                // it will not recognize, and silence looks like nobody
                // enrolled. The path stays out: the stem is their name.
                tracing::warn!(
                    "a file in the speakers directory is not a profile this build reads"
                );
                continue;
            }
            match self.read(&path) {
                Ok(Some(profile)) => profiles.push(profile),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "skipping an unreadable speaker profile"),
            }
        }
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        profiles
    }

    /// The profiles `engine` measured, at the width it produces. Everything
    /// else was made by a different model and cannot be compared.
    pub fn read_for(&self, engine: &str, dimension: usize) -> Vec<Profile> {
        self.read_all()
            .into_iter()
            .filter(|profile| profile.engine == engine && profile.vector.len() == dimension)
            .collect()
    }

    /// Stores a profile. `Replace::Never` refuses when a profile is already
    /// there, judged by the filesystem rather than by comparing names: a name
    /// that folds onto an existing one would otherwise overwrite it.
    ///
    /// [`ProfileError::DurabilityUnconfirmed`] means the profile is stored
    /// and live; only whether it survives a power cut is unknown.
    pub fn save(&self, profile: &Profile, replace: Replace) -> Result<(), ProfileError> {
        let document = self.document(&profile.name);
        if replace == Replace::Never && !document.claim().map_err(ProfileError::Write)? {
            return Err(ProfileError::AlreadyStored);
        }
        document
            .prepare(&StoredProfile::from(profile))
            .map_err(ProfileError::Write)?
            .commit()
            .map_err(ProfileError::Write)?
            .confirm_durability()
            .map_err(ProfileError::DurabilityUnconfirmed)
    }

    /// `false` when nobody by that name was enrolled.
    pub fn remove(&self, name: &SpeakerName) -> Result<bool, ProfileError> {
        self.document(name).remove().map_err(ProfileError::Write)
    }

    fn document(&self, name: &SpeakerName) -> PrivateDocument {
        PrivateDocument::new(
            self.directory.join(format!("{name}.json")),
            MAX_PROFILE_BYTES,
        )
    }

    fn read(&self, path: &Path) -> Result<Option<Profile>, ProfileError> {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or(ProfileError::UnnamedFile)?;
        let name = SpeakerName::new(name).map_err(ProfileError::Name)?;

        let document = PrivateDocument::new(path, MAX_PROFILE_BYTES);
        let Some(stored) = document
            .read::<StoredProfile>()
            .map_err(ProfileError::Read)?
        else {
            return Ok(None);
        };

        let vector = super::normalized(stored.vector).ok_or(ProfileError::NoDirection)?;
        Ok(Some(Profile {
            name,
            role: stored.role,
            engine: stored.engine,
            // A stored moment far enough in the future to overflow is a
            // tampered file, not a profile: it joins the skipped ones.
            enrolled_at: UNIX_EPOCH
                .checked_add(std::time::Duration::from_secs(stored.enrolled_at))
                .ok_or(ProfileError::UnreadableMoment)?,
            vector,
        }))
    }
}

/// The on-disk shape. Unknown fields are refused: a profile written by a
/// newer Aria is not one this build can judge a voice with.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProfile {
    role: Role,
    engine: String,
    /// Seconds since the Unix epoch.
    enrolled_at: u64,
    vector: Vec<f32>,
}

impl From<&Profile> for StoredProfile {
    fn from(profile: &Profile) -> Self {
        Self {
            role: profile.role,
            engine: profile.engine.clone(),
            enrolled_at: profile
                .enrolled_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            vector: profile.vector.clone(),
        }
    }
}

/// Whether storing a profile may overwrite one already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    Never,
    Allowed,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("the profile could not be read")]
    Read(#[source] DocumentError),

    #[error("the profile could not be written")]
    Write(#[source] DocumentError),

    #[error("the profile is stored, but its durability could not be confirmed")]
    DurabilityUnconfirmed(#[source] DocumentError),

    #[error("a profile of that name is already stored")]
    AlreadyStored,

    #[error("the profile records a moment outside any calendar")]
    UnreadableMoment,

    #[error("the profile filename is not a usable speaker name")]
    Name(#[source] SpeakerNameError),

    #[error("the profile filename has no name in it")]
    UnnamedFile,

    #[error("the profile embedding has no direction")]
    NoDirection,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A store in a directory that lives as long as the test holds the pair.
    fn store() -> (tempfile::TempDir, ProfileStore) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let store = ProfileStore::open(home.path());
        (home, store)
    }

    fn profile(name: &str, role: Role, engine: &str, vector: Vec<f32>) -> Profile {
        Profile {
            name: SpeakerName::new(name).expect("the name is usable"),
            role,
            engine: engine.to_owned(),
            enrolled_at: UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            vector,
        }
    }

    #[test]
    fn a_saved_profile_reads_back() {
        let (_home, store) = store();
        let alex = profile("alex", Role::Owner, "wespeaker", vec![1.0, 0.0]);
        store
            .save(&alex, Replace::Never)
            .expect("the profile is written");

        assert_eq!(store.read_all(), vec![alex]);
    }

    #[test]
    fn profiles_come_back_sorted_by_name() {
        let (_home, store) = store();
        for name in ["zoe", "ada"] {
            store
                .save(
                    &profile(name, Role::Guest, "wespeaker", vec![0.0, 1.0]),
                    Replace::Never,
                )
                .expect("the profile is written");
        }

        let names: Vec<String> = store
            .read_all()
            .iter()
            .map(|profile| profile.name.to_string())
            .collect();
        assert_eq!(names, ["ada", "zoe"]);
    }

    #[test]
    fn a_profile_from_another_engine_is_not_offered_to_this_one() {
        let (_home, store) = store();
        store
            .save(
                &profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");
        store
            .save(
                &profile("zoe", Role::Owner, "other", vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");

        let usable = store.read_for("wespeaker", 2);
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].name.as_str(), "ada");
    }

    #[test]
    fn a_profile_of_another_width_is_not_offered_either() {
        let (_home, store) = store();
        store
            .save(
                &profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");

        assert!(store.read_for("wespeaker", 2).is_empty());
    }

    #[test]
    fn one_unreadable_profile_does_not_cost_the_others() {
        let (home, store) = store();
        store
            .save(
                &profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");
        std::fs::write(home.path().join("broken.json"), b"{").expect("the file is written");

        let names: Vec<String> = store
            .read_all()
            .iter()
            .map(|profile| profile.name.to_string())
            .collect();
        assert_eq!(names, ["ada"]);
    }

    #[test]
    fn an_empty_speakers_directory_means_nobody_enrolled() {
        let home = tempfile::tempdir().expect("a temporary directory");
        assert!(
            ProfileStore::open(home.path().join("absent"))
                .read_all()
                .is_empty()
        );
    }

    #[test]
    fn removing_says_whether_anyone_was_enrolled() {
        let (_home, store) = store();
        let ada = SpeakerName::new("ada").expect("the name is usable");
        store
            .save(
                &profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");

        assert!(store.remove(&ada).expect("the delete succeeds"));
        assert!(!store.remove(&ada).expect("the delete succeeds"));
        assert!(store.read_all().is_empty());
    }

    #[test]
    fn a_second_profile_of_the_same_name_is_refused_unless_asked() {
        let (_home, store) = store();
        let first = profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0]);
        store
            .save(&first, Replace::Never)
            .expect("the profile is written");

        let second = profile("ada", Role::Guest, "wespeaker", vec![0.0, 1.0]);
        assert!(matches!(
            store.save(&second, Replace::Never),
            Err(ProfileError::AlreadyStored)
        ));
        assert_eq!(store.read_all(), vec![first]);

        store
            .save(&second, Replace::Allowed)
            .expect("the profile is replaced");
        assert_eq!(store.read_all(), vec![second]);
    }

    #[test]
    fn a_profile_recording_an_impossible_moment_is_skipped() {
        let (home, store) = store();
        store
            .save(
                &profile("ada", Role::Owner, "wespeaker", vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");
        let tampered = home.path().join("zoe.json");
        std::fs::write(
            &tampered,
            br#"{"role":"guest","engine":"wespeaker","enrolled_at":18446744073709551615,"vector":[1.0,0.0]}"#,
        )
        .expect("the file is written");
        std::fs::set_permissions(&tampered, std::fs::Permissions::from_mode(0o600))
            .expect("the mode is set");

        let names: Vec<String> = store
            .read_all()
            .iter()
            .map(|profile| profile.name.to_string())
            .collect();
        assert_eq!(names, ["ada"]);
    }

    #[test]
    fn a_name_that_could_escape_the_directory_is_refused() {
        for name in ["", " ada", "ada ", "../ada", "ada/zoe", ".hidden", "a\nb"] {
            assert!(
                SpeakerName::new(name).is_err(),
                "{name:?} must not become a filename"
            );
        }
    }

    #[test]
    fn an_ordinary_name_is_accepted() {
        assert_eq!(
            SpeakerName::new("Alex Morgan")
                .expect("the name is usable")
                .as_str(),
            "Alex Morgan"
        );
    }
}
