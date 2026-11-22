//! `dns-sd -R` holds the registration for as long as it runs.
//! [`ChildProcess`] kills its group on drop, which ends the advert on a clean
//! shutdown or an unwinding panic.

use std::process::{Command, Stdio};

use aria_kit::primitives::process::ChildProcess;

use super::AdvertError;

pub(super) struct Registration {
    _child: ChildProcess,
}

impl Registration {
    pub(super) fn new(
        instance: &str,
        service_type: &str,
        domain: &str,
        port: u16,
        txt: &[&str],
    ) -> Result<Self, AdvertError> {
        let mut command = Command::new("dns-sd");
        command
            .args(["-R", instance, service_type, domain, &port.to_string()])
            .args(txt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = ChildProcess::spawn(&mut command).map_err(AdvertError::Spawn)?;
        Ok(Self { _child: child })
    }
}
