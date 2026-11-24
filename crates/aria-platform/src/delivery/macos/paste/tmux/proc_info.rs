//! What the kernel says about processes, through libproc.

#![allow(
    unsafe_code,
    reason = "libproc fills caller-owned buffers; the invariants are on the SAFETY comments"
)]

use std::ffi::{OsStr, c_int};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

// <sys/proc_info.h> values libc doesn't carry.
const PROC_TTY_ONLY: u32 = 3;
const PROC_UID_ONLY: u32 = 4;
const PROC_PPID_ONLY: u32 = 6;
const PROC_PIDFDSOCKETINFO: c_int = 3;
const SOCKINFO_UN: i32 = 3;

/// Processes whose controlling terminal is `device`.
pub(super) fn on_terminal(device: u32) -> Vec<i32> {
    list(PROC_TTY_ONLY, device)
}

pub(super) fn of_user(uid: u32) -> Vec<i32> {
    list(PROC_UID_ONLY, uid)
}

pub(super) fn children(pid: i32) -> Vec<i32> {
    u32::try_from(pid).map_or_else(|_| Vec::new(), |pid| list(PROC_PPID_ONLY, pid))
}

fn list(kind: u32, info: u32) -> Vec<i32> {
    // SAFETY: a null buffer asks only for the size the list needs.
    let needed = unsafe { libc::proc_listpids(kind, info, std::ptr::null_mut(), 0) };
    let Ok(needed) = usize::try_from(needed) else {
        return Vec::new();
    };
    // Room for processes started between the two calls.
    let mut pids = vec![0i32; needed / size_of::<i32>() + 32];
    let size = c_int::try_from(size_of_val(pids.as_slice())).unwrap_or(c_int::MAX);
    // SAFETY: `pids` is a live buffer of `size` bytes, which the call fills
    // and reports the filled length of.
    let filled = unsafe { libc::proc_listpids(kind, info, pids.as_mut_ptr().cast(), size) };
    let count = usize::try_from(filled).unwrap_or(0) / size_of::<i32>();
    pids.truncate(count.min(pids.len()));
    pids.retain(|&pid| pid > 0);
    pids
}

/// Whether `pid` is in the foreground process group of terminal `device`, so
/// the keys typed there reach it.
pub(super) fn is_foreground(pid: i32, device: u32) -> bool {
    bsd_info(pid).is_some_and(|info| info.e_tdev == device && info.pbi_pgid == info.e_tpgid)
}

/// The controlling terminal of `pid`, if it has one.
pub(super) fn terminal(pid: i32) -> Option<u32> {
    const NO_DEVICE: u32 = u32::MAX;
    bsd_info(pid)
        .map(|info| info.e_tdev)
        .filter(|&device| device != NO_DEVICE)
}

fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: `proc_bsdinfo` is plain integers, for which zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = c_int::try_from(size_of_val(&info)).ok()?;
    // SAFETY: `info` is a live `proc_bsdinfo` of `size` bytes, the flavor's
    // layout.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(info)
}

pub(super) fn executable(pid: i32) -> Option<PathBuf> {
    const MAX_PATH: usize = libc::PROC_PIDPATHINFO_MAXSIZE as usize;
    let mut path = vec![0u8; MAX_PATH];
    // SAFETY: `path` is a live buffer of `MAX_PATH` bytes; the call writes at
    // most that many and returns the length written.
    let written = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), MAX_PATH as u32) };
    let length = usize::try_from(written).ok().filter(|&length| length > 0)?;
    path.truncate(length);
    Some(PathBuf::from(OsStr::from_bytes(&path)))
}

/// The paths of the unix sockets `pid` has bound, such as a server's
/// listening socket.
pub(super) fn bound_sockets(pid: i32) -> Vec<PathBuf> {
    descriptors(pid)
        .into_iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
        .filter_map(|fd| socket_path(pid, fd.proc_fd))
        .collect()
}

fn descriptors(pid: i32) -> Vec<libc::proc_fdinfo> {
    // SAFETY: a null buffer asks only for the size the list needs.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let Ok(needed) = usize::try_from(needed) else {
        return Vec::new();
    };
    let empty = libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    };
    // Room for descriptors opened between the two calls.
    let mut fds = vec![empty; needed / size_of::<libc::proc_fdinfo>() + 8];
    let size = c_int::try_from(size_of_val(fds.as_slice())).unwrap_or(c_int::MAX);
    // SAFETY: `fds` is a live buffer of `size` bytes, which the call fills
    // and reports the filled length of.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
    let count = usize::try_from(filled).unwrap_or(0) / size_of::<libc::proc_fdinfo>();
    fds.truncate(count.min(fds.len()));
    fds
}

/// `struct socket_fdinfo` from `<sys/proc_info.h>`, named only as far as a
/// unix socket's own address.
#[repr(C, align(8))]
struct SocketFdInfo {
    _head: [u8; 256],
    /// `soi_kind`, which says how to read the protocol union after it.
    kind: i32,
    _connection: [u8; 20],
    _address_header: [u8; 2],
    path: [u8; 104],
    _tail: [u8; 406],
}

const _: () = assert!(size_of::<SocketFdInfo>() == 792);

fn socket_path(pid: i32, fd: i32) -> Option<PathBuf> {
    let mut info = SocketFdInfo {
        _head: [0; 256],
        kind: 0,
        _connection: [0; 20],
        _address_header: [0; 2],
        path: [0; 104],
        _tail: [0; 406],
    };
    let size = c_int::try_from(size_of_val(&info)).ok()?;
    // SAFETY: `info` is a live buffer of `size` bytes laid out as
    // `struct socket_fdinfo`.
    let written = unsafe {
        libc::proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, (&raw mut info).cast(), size)
    };
    if written != size || info.kind != SOCKINFO_UN {
        return None;
    }
    let end = info
        .path
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(info.path.len());
    let path = PathBuf::from(OsStr::from_bytes(&info.path[..end]));
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listening_socket_shows_the_path_it_was_bound_to() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("server");
        let _server = std::os::unix::net::UnixListener::bind(&socket).expect("a socket");
        let pid = i32::try_from(std::process::id()).expect("a pid");

        assert!(bound_sockets(pid).contains(&socket));
    }
}
