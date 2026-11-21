use security_framework::passwords::{self, PasswordOptions};
use security_framework_sys::base::errSecItemNotFound;

use super::SecretError;

pub(super) struct Item {
    service: &'static str,
    account: &'static str,
}

impl Item {
    pub(super) fn new(service: &'static str, account: &'static str) -> Self {
        Self { service, account }
    }

    pub(super) fn read(&self) -> Result<Option<String>, SecretError> {
        match passwords::generic_password(PasswordOptions::new_generic_password(
            self.service,
            self.account,
        )) {
            Ok(stored) => String::from_utf8(stored)
                .map(Some)
                .map_err(|_| SecretError::Malformed),
            Err(error) if error.code() == errSecItemNotFound => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    pub(super) fn write(&self, value: &str) -> Result<(), SecretError> {
        passwords::set_generic_password(self.service, self.account, value.as_bytes())
            .map_err(keychain_error)
    }

    pub(super) fn remove(&self) -> Result<bool, SecretError> {
        match passwords::delete_generic_password(self.service, self.account) {
            Ok(()) => Ok(true),
            Err(error) if error.code() == errSecItemNotFound => Ok(false),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

fn keychain_error(error: security_framework::base::Error) -> SecretError {
    SecretError::Keychain {
        status: error.code(),
    }
}
