//! Registers through mDNSResponder's client API, which libSystem exports.

use std::ffi::{CString, c_char, c_void};
use std::ptr;

use super::AdvertError;

/// mDNSResponder's `DNSServiceRef`, a handle to one client connection.
type ServiceRef = *mut c_void;

#[allow(unsafe_code, reason = "mDNSResponder's client API has no safe binding")]
unsafe extern "C" {
    fn DNSServiceRegister(
        service: *mut ServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_length: u16,
        txt_record: *const c_void,
        callback: *const c_void,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefDeallocate(service: ServiceRef);
}

pub(super) struct Registration {
    service: ServiceRef,
}

// SAFETY: mDNSResponder forbids only concurrent use of a reference. Only
// `Drop` uses this one after registering, and `&mut self` makes that exclusive.
#[allow(
    unsafe_code,
    reason = "a raw mDNSResponder handle is not Send on its own"
)]
unsafe impl Send for Registration {}

impl Registration {
    pub(super) fn new(
        instance: &str,
        service_type: &str,
        domain: &str,
        port: u16,
        txt: &[&str],
    ) -> Result<Self, AdvertError> {
        let instance = CString::new(instance).map_err(AdvertError::Nul)?;
        let service_type = CString::new(service_type).map_err(AdvertError::Nul)?;
        let domain = CString::new(domain).map_err(AdvertError::Nul)?;
        let txt = txt_record(txt)?;
        let txt_length = u16::try_from(txt.len()).map_err(|_| AdvertError::TxtRecord)?;
        let mut service: ServiceRef = ptr::null_mut();
        // SAFETY: the strings are NUL-terminated, and the TXT bytes span
        // `txt_length`. mDNSResponder copies all of them before returning. A
        // NULL host means this Mac. A NULL callback is allowed. mDNSResponder
        // then reports nothing back, so only `Drop` touches `service`.
        #[allow(unsafe_code, reason = "mDNSResponder's client API has no safe binding")]
        let code = unsafe {
            DNSServiceRegister(
                &mut service,
                0,
                0,
                instance.as_ptr(),
                service_type.as_ptr(),
                domain.as_ptr(),
                ptr::null(),
                port.to_be(),
                txt_length,
                txt.as_ptr().cast(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if code != 0 {
            return Err(AdvertError::Register(code));
        }
        Ok(Self { service })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: `service` came from a registration that succeeded, and only
        // this drop releases it.
        #[allow(unsafe_code, reason = "mDNSResponder's client API has no safe binding")]
        unsafe {
            DNSServiceRefDeallocate(self.service);
        }
    }
}

/// The DNS wire form: each entry is one length byte, then the entry.
fn txt_record(entries: &[&str]) -> Result<Vec<u8>, AdvertError> {
    let mut record = Vec::new();
    for entry in entries {
        let length = u8::try_from(entry.len()).map_err(|_| AdvertError::TxtRecord)?;
        record.push(length);
        record.extend_from_slice(entry.as_bytes());
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use aria_kit::primitives::process::ChildProcess;

    use super::*;

    const SERVICE_TYPE: &str = "_aria-test._tcp";
    const INSTANCE_ENV: &str = "ARIA_TEST_ADVERT_INSTANCE";
    const TXT_ID: &str = "id=test";

    #[test]
    #[ignore = "a helper that another test runs in its own process"]
    fn helper_registers_and_waits() {
        let Ok(instance) = std::env::var(INSTANCE_ENV) else {
            return;
        };
        let _registration =
            Registration::new(&instance, SERVICE_TYPE, "local", 9, &[TXT_ID]).unwrap();
        println!("registered");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn a_registration_ends_with_its_process() {
        let instance = format!("Aria test {}", std::process::id());
        let (_crate, module) = module_path!().split_once("::").unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                &format!("{module}::helper_registers_and_waits"),
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(INSTANCE_ENV, &instance)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut helper = ChildProcess::spawn(&mut command).unwrap();
        let output = lines_of(helper.take_stdout().unwrap());
        assert!(
            wait_for(&output, "registered", Duration::from_secs(10)),
            "the helper registered"
        );
        assert!(
            resolves(&instance, Duration::from_secs(5)),
            "the record shows while its process lives"
        );

        helper.terminate().unwrap();

        // mDNSResponder drops the record only once it reads the closed
        // connection. A lookup started right away can still see it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while resolves(&instance, Duration::from_secs(1)) {
            assert!(
                Instant::now() < deadline,
                "the record ends with its process"
            );
        }
    }

    fn resolves(instance: &str, wait: Duration) -> bool {
        let mut command = Command::new("dns-sd");
        command
            .args(["-L", instance, SERVICE_TYPE, "local"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut lookup = ChildProcess::spawn(&mut command).unwrap();
        let output = lines_of(lookup.take_stdout().unwrap());
        let found = wait_for(&output, TXT_ID, wait);
        lookup.terminate().unwrap();
        found
    }

    fn lines_of(output: impl Read + Send + 'static) -> Receiver<String> {
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        lines
    }

    fn wait_for(lines: &Receiver<String>, needle: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}
