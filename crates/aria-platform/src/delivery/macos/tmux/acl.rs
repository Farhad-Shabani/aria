//! Extended access control lists, which can grant access the mode bits don't
//! show.

#![allow(
    unsafe_code,
    reason = "libc has no binding for the ACL calls; the invariants are on the SAFETY comments"
)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// <sys/acl.h>.
const ACL_TYPE_EXTENDED: c_int = 256;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: c_int = 1;

unsafe extern "C" {
    fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, which: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

/// Whether `path` itself, not a symlink's target, has an entry that allows
/// access. Deny entries, such as macOS puts on home folders, only take away.
/// An ACL that can't be read counts as allowing.
pub(super) fn allows_access(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT);
    }
    let mut allows = false;
    let mut which = ACL_FIRST_ENTRY;
    let mut entry = std::ptr::null_mut();
    // SAFETY: `acl` is the live list from above, and `entry` receives a
    // pointer into it.
    while unsafe { acl_get_entry(acl, which, &raw mut entry) } == 0 {
        which = ACL_NEXT_ENTRY;
        let mut tag = 0;
        // SAFETY: `entry` points into `acl`, which is still live.
        let read = unsafe { acl_get_tag_type(entry, &raw mut tag) };
        if read != 0 || tag == ACL_EXTENDED_ALLOW {
            allows = true;
            break;
        }
    }
    // SAFETY: `acl` came from `acl_get_link_np` and is freed once.
    unsafe { acl_free(acl) };
    allows
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use aria_primitives::cancel::CancelToken;
    use aria_primitives::process::{self, Bounds, Capture};

    use super::*;

    fn add_entry(path: &Path, entry: &str) {
        let mut chmod = Command::new("/bin/chmod");
        chmod.arg("+a").arg(entry).arg(path);
        let bounds = Bounds {
            timeout: std::time::Duration::from_secs(5),
            stdout: Capture::KeepHead(1024),
            stderr: Capture::KeepHead(1024),
        };
        let output =
            process::run(&mut chmod, &[], bounds, &CancelToken::new()).expect("chmod runs");
        assert!(output.status.success(), "chmod +a {entry}");
    }

    #[test]
    fn only_an_allow_entry_grants_access_beside_the_mode() {
        let plain = tempfile::tempdir().expect("a folder");
        let denying = tempfile::tempdir().expect("a folder");
        add_entry(denying.path(), "everyone deny delete");
        let allowing = tempfile::tempdir().expect("a folder");
        add_entry(allowing.path(), "everyone allow add_file,delete_child");

        assert!(!allows_access(plain.path()));
        assert!(!allows_access(denying.path()));
        assert!(allows_access(allowing.path()));
    }
}
