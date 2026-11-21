use std::path::PathBuf;

use aria_platform::secret::Secret;

/// Every secret the daemon makes goes through here, so its own tests keep
/// them in files.
pub(super) fn secret(service: &'static str, account: &'static str, file: PathBuf) -> Secret {
    #[cfg(test)]
    aria_platform::secret::keep_in_files_for_tests();
    Secret::new(service, account, file)
}
