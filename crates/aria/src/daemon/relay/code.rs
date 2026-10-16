//! The one-time code the Mac app draws as a QR. A phone trades it for the
//! bearer token, so the QR never carries the token itself.

use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use super::pairing::{PairingError, random_hex};

const CODE_BYTES: usize = 16;
const CODE_LIFETIME: Duration = Duration::from_secs(60);

/// 16 random bytes as lowercase hex. Its `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::daemon) struct PairingCode(String);

impl PairingCode {
    pub(in crate::daemon) fn raw(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(redacted)")
    }
}

#[derive(Debug)]
struct Live {
    code: PairingCode,
    expires_at: Instant,
}

/// Holds at most one live code. Issuing again while it is live returns the
/// same code, so a client that polls never invalidates what it just drew.
#[derive(Debug, Default)]
pub(in crate::daemon) struct PairingCodes {
    current: Mutex<Option<Live>>,
}

impl PairingCodes {
    /// The live code and how long it has left, minting one if none is live.
    pub(in crate::daemon) fn issue(
        &self,
        now: Instant,
    ) -> Result<(PairingCode, Duration), PairingError> {
        let mut current = self.lock();
        if let Some(live) = current.as_ref().filter(|live| now < live.expires_at) {
            return Ok((live.code.clone(), live.expires_at - now));
        }
        let code = PairingCode(random_hex(CODE_BYTES)?);
        *current = Some(Live {
            code: code.clone(),
            expires_at: now + CODE_LIFETIME,
        });
        Ok((code, CODE_LIFETIME))
    }

    /// Whether `presented` is the live code. A match uses it up.
    pub(in crate::daemon) fn redeem(&self, presented: &str, now: Instant) -> bool {
        let mut current = self.lock();
        let matches = current.as_ref().is_some_and(|live| {
            now < live.expires_at && bool::from(presented.as_bytes().ct_eq(live.code.0.as_bytes()))
        });
        if matches {
            *current = None;
        }
        matches
    }

    // The guarded value is a plain `Option` that no panic can leave half
    // written, so a poisoned lock still holds a usable state.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuing_again_returns_the_live_code() {
        let codes = PairingCodes::default();
        let start = Instant::now();

        let (first, remaining) = codes.issue(start).unwrap();
        let (again, left) = codes.issue(start + Duration::from_secs(20)).unwrap();

        assert_eq!(first.raw().len(), CODE_BYTES * 2);
        assert!(
            first
                .raw()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(remaining, CODE_LIFETIME);
        assert_eq!(again, first);
        assert_eq!(left, Duration::from_secs(40));
    }

    #[test]
    fn a_code_redeems_once() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let (code, _) = codes.issue(now).unwrap();

        assert!(codes.redeem(code.raw(), now));
        assert!(!codes.redeem(code.raw(), now));
    }

    #[test]
    fn a_wrong_code_does_not_redeem_or_burn_the_live_one() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let (code, _) = codes.issue(now).unwrap();

        assert!(!codes.redeem(&"0".repeat(CODE_BYTES * 2), now));
        assert!(!codes.redeem("", now));
        assert!(codes.redeem(code.raw(), now));
    }

    #[test]
    fn nothing_redeems_before_a_code_is_issued() {
        assert!(!PairingCodes::default().redeem("", Instant::now()));
    }

    #[test]
    fn an_expired_code_does_not_redeem() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let (code, _) = codes.issue(now).unwrap();

        assert!(!codes.redeem(code.raw(), now + CODE_LIFETIME));
    }

    #[test]
    fn an_expired_or_used_code_is_replaced_on_the_next_issue() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let (expired, _) = codes.issue(now).unwrap();

        let later = now + CODE_LIFETIME;
        let (fresh, remaining) = codes.issue(later).unwrap();
        assert_ne!(fresh, expired);
        assert_eq!(remaining, CODE_LIFETIME);

        assert!(codes.redeem(fresh.raw(), later));
        let (after_use, _) = codes.issue(later).unwrap();
        assert_ne!(after_use, fresh);
    }

    #[test]
    fn a_code_never_prints_itself() {
        let codes = PairingCodes::default();
        let (code, _) = codes.issue(Instant::now()).unwrap();

        assert_eq!(format!("{code:?}"), "PairingCode(redacted)");
        assert!(!format!("{codes:?}").contains(code.raw()));
    }
}
