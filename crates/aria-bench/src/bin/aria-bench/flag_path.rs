//! Path flags as the shell leaves them.

use std::path::PathBuf;

use aria::primitives::user_home;

/// `~/` becomes the home directory. The shell does not expand a `~` that
/// follows `=` inside an argument, and environment values are never expanded.
pub(crate) fn expand(path: &str) -> anyhow::Result<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Ok(user_home::resolve()?.join(rest)),
        None => Ok(PathBuf::from(path)),
    }
}
