//! Bonjour advertisement of the relay port, so a phone can find this Mac.
//!
//! Registration shells out to `dns-sd`, which macOS ships, rather than to a
//! library: library mDNS misbehaves on a Mac with many interfaces or
//! restrictive multicast routing, where Apple's own resolver does not.
//! `dns-sd -R` holds the registration for as long as it runs, so the child
//! *is* the advert: [`ChildProcess`] kills its group on drop, which covers a
//! clean shutdown and a panic in the server task alike.

use std::process::{Command, Stdio};

use aria_kit::primitives::process::ChildProcess;

use super::pairing::to_hex;

/// A type of its own, not the retired `_aria-ptt._tcp`. A phone still looking
/// for the old one must never reach a server that cannot speak its protocol.
pub(crate) const SERVICE_TYPE: &str = "_aria-grpc._tcp";

const DOMAIN: &str = "local";
/// Which shape of the record the phone is reading, for when it grows a key.
const TXT_VERSION: &str = "version=1";

/// A live registration. Dropping it unregisters.
pub(in crate::daemon) struct Advert {
    _registration: ChildProcess,
}

impl Advert {
    /// `id` is the relay certificate's fingerprint: the one thing about this
    /// Mac that survives a rename, so the phone keys its pairing by it.
    pub(in crate::daemon) fn register(port: u16, id: &[u8; 32]) -> Result<Self, AdvertError> {
        let host = hostname();
        let instance = instance_name();
        let mut command = Command::new("dns-sd");
        command
            .args([
                "-R",
                &instance,
                SERVICE_TYPE,
                DOMAIN,
                &port.to_string(),
                TXT_VERSION,
                &format!("name={host}"),
                &id_record(id),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let registration = ChildProcess::spawn(&mut command).map_err(AdvertError::Spawn)?;
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
    #[error("failed to start dns-sd to advertise the relay")]
    Spawn(#[source] std::io::Error),
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
