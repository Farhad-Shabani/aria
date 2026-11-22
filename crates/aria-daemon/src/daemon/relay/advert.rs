//! Bonjour advertisement of the relay port, so a phone can find this Mac.
//!
//! On macOS the daemon registers through mDNSResponder's client API. Library
//! mDNS misbehaves on a Mac with many interfaces or restrictive multicast
//! routing. mDNSResponder drops a registration once its client's connection
//! closes, so the advert ends with the daemon, even a killed one.
//!
//! A `dns-sd` child outlives a killed daemon. After a re-mint, its stale
//! record keeps the old `id` and hides the next daemon's under the same name.
//! On Linux this API needs a library the daemon doesn't link, so a `dns-sd`
//! child holds the advert there, and a killed daemon can leave it behind.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux::Registration;
#[cfg(target_os = "macos")]
use macos::Registration;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("the relay advert supports only macOS and Linux");

use std::time::Duration;

use super::pairing::to_hex;

/// A type of its own, not the retired `_aria-ptt._tcp`. A phone still looking
/// for the old one must never reach a server that cannot speak its protocol.
pub const SERVICE_TYPE: &str = "_aria-grpc._tcp";

/// The default registration domains can include a wide-area one, which would
/// publish the fingerprint and port beyond the LAN.
const DOMAIN: &str = "local";

/// Which shape of the record the phone is reading, for when it grows a key.
const TXT_VERSION: &str = "version=1";

/// Registering waits for mDNSResponder's answer. A wedged mDNSResponder must
/// not hold up the daemon's start.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// A live registration. Dropping it unregisters.
pub(in crate::daemon) struct Advert {
    _registration: Registration,
}

impl Advert {
    /// `id` is the relay certificate's fingerprint: the one thing about this
    /// Mac that survives a rename, so the phone keys its pairing by it.
    pub(in crate::daemon) async fn register(port: u16, id: [u8; 32]) -> Result<Self, AdvertError> {
        let instance = instance_name();
        let registering = tokio::task::spawn_blocking({
            let instance = instance.clone();
            move || {
                Registration::new(
                    &instance,
                    SERVICE_TYPE,
                    DOMAIN,
                    port,
                    &[
                        TXT_VERSION,
                        &format!("name={}", hostname()),
                        &id_record(&id),
                    ],
                )
            }
        });
        let registration = tokio::time::timeout(REGISTER_TIMEOUT, registering)
            .await
            .map_err(|_elapsed| AdvertError::Unanswered)?
            .map_err(AdvertError::Panicked)??;
        tracing::info!(
            service = SERVICE_TYPE,
            instance = instance.as_str(),
            port,
            "relay advertised over Bonjour"
        );
        Ok(Self {
            _registration: registration,
        })
    }
}

/// The Bonjour instance name. A label for the user, not an identity: the
/// phone knows this Mac by the `id` TXT record.
pub(in crate::daemon) fn instance_name() -> String {
    format!("Aria ({})", hostname())
}

fn id_record(fingerprint: &[u8; 32]) -> String {
    format!("id={}", to_hex(fingerprint))
}

/// The machine's name, as the user sees it in Sharing settings. `hostname`
/// is a bare read of a system value, so a failure here is not worth a
/// registration: the advert just carries a duller name.
fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_str()
        .map(|name| name.trim_end_matches(".local").to_owned())
        .unwrap_or_else(|_error| "Mac".to_owned())
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum AdvertError {
    #[cfg(target_os = "macos")]
    #[error("the advert's name, type, or domain holds a NUL byte")]
    Nul(#[source] std::ffi::NulError),
    #[cfg(target_os = "macos")]
    #[error("the advert's TXT record is too long")]
    TxtRecord,
    #[cfg(target_os = "macos")]
    #[error("mDNSResponder refused the registration (error {0})")]
    Register(i32),
    #[cfg(target_os = "linux")]
    #[error("failed to start dns-sd to advertise the relay")]
    Spawn(#[source] std::io::Error),
    #[error("mDNSResponder didn't answer within {} s", REGISTER_TIMEOUT.as_secs())]
    Unanswered,
    #[error("registering the advert panicked")]
    Panicked(#[source] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone shows this to the user, after stripping the `Aria (…)`
    /// wrapper. An mDNS suffix left on would read as "example-mac.local", and
    /// an empty name as an orphan separator.
    #[test]
    fn the_advertised_hostname_is_bare_and_never_empty() {
        let host = hostname();

        assert!(!host.is_empty(), "a nameless machine still gets a name");
        assert!(!host.ends_with(".local"), "the mDNS suffix is trimmed");
    }

    /// The phone parses this with a fixed-width hex decoder; a stray `0x`,
    /// uppercase digit, or short string would read as "no id".
    #[test]
    fn the_id_record_is_lowercase_hex_of_the_fingerprint() {
        let mut fingerprint = [0u8; 32];
        fingerprint[0] = 0x00;
        fingerprint[1] = 0xab;
        fingerprint[31] = 0xff;

        let record = id_record(&fingerprint);

        assert_eq!(record, format!("id=00ab{}ff", "00".repeat(29)));
        assert_eq!(record.len(), "id=".len() + 64);
    }
}
