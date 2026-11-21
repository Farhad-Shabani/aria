//! Databases stay plaintext: Linux has no key store Aria uses yet.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::Error;

/// Loading the plaintext marker never fails.
#[derive(Debug, thiserror::Error)]
pub enum AtRestError {}

pub struct AtRest(());

impl AtRest {
    pub fn load(_databases: &[PathBuf]) -> Result<Self, AtRestError> {
        Ok(Self(()))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        Self(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn unlock_for_tests(&self, _connection: &Connection) {}

    pub(super) fn is_plaintext(&self, _path: &Path, _len: i64) -> Result<bool, Error> {
        Ok(false)
    }

    pub(super) fn seal(
        &self,
        _directory: &File,
        _directory_path: &Path,
        _name: &OsStr,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub(super) fn unlock(&self, _connection: &Connection) -> Result<(), Error> {
        Ok(())
    }
}
