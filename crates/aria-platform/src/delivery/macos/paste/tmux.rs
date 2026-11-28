//! Text into a tmux pane as a paste, for terminals that can't paste from a
//! script.
//!
//! The terminal names its focused tab's tty, and the tmux client in front on
//! that tty names the pane. `paste-buffer -p` adds paste markers only if the
//! pane asked for them.

mod acl;
mod proc_info;

use std::ffi::OsStr;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use aria_primitives::cancel::CancelToken;
use aria_primitives::file_inspection::InspectedFile;
use aria_primitives::process::{self, Bounds, Capture, RunError};
use objc2_foundation::NSAppleEventDescriptor;

use super::super::Undelivered;
use super::focus::{self, LOOKUP_TIMEOUT, Lookup};
use super::key_window;
use super::scripting::{self, CORE_SUITE, GET_DATA, code};
use crate::delivery::DeliveryError;

/// The most a tmux command may print before it counts as broken.
const MAX_OUTPUT: usize = 64 * 1024;

const CLIENTS_FORMAT: &str = "#{client_pid} #{client_readonly} #{client_control_mode} \
     #{client_flags} #{pane_id} #{pane_in_mode} #{pane_input_off}";

/// A terminal that can name its focused tab's tty but can't paste from a
/// script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Host {
    Terminal,
    ITerm,
}

impl Host {
    pub(super) fn of(bundle_id: &str) -> Option<Self> {
        match bundle_id {
            "com.apple.Terminal" => Some(Self::Terminal),
            "com.googlecode.iterm2" => Some(Self::ITerm),
            _ => None,
        }
    }

    /// Terminal.sdef: `tty of selected tab of window 1`.
    /// iTerm2.sdef: `tty of current session of current window`.
    fn tty(self) -> Option<objc2::rc::Retained<NSAppleEventDescriptor>> {
        let app = NSAppleEventDescriptor::nullDescriptor();
        match self {
            Self::Terminal => scripting::element_at(code(b"cwin"), 1, &app)
                .and_then(|window| scripting::property(code(b"tcnt"), &window))
                .and_then(|tab| scripting::property(code(b"ttty"), &tab)),
            Self::ITerm => scripting::property(code(b"Crwn"), &app)
                .and_then(|window| scripting::property(code(b"Wcsn"), &window))
                .and_then(|session| scripting::property(code(b"Stty"), &session)),
        }
    }
}

/// Why a press can't paste through tmux.
pub(super) enum Unpinned {
    /// No trusted tmux client is in front on the focused tab.
    NoTmux,
    Refused(&'static str),
}

/// The tmux pane a press started in, through the client in front on the
/// focused tab.
pub(in crate::delivery::macos) struct Pane {
    host: Host,
    client: Client,
    socket: PathBuf,
    server: i32,
    place: Place,
}

/// What must stay the same for text to keep going to the pane.
#[derive(Debug, PartialEq, Eq)]
struct Place {
    tty: String,
    pane: String,
}

impl Pane {
    pub(super) fn focused(host: Host, process: u32) -> Result<Self, Unpinned> {
        let target = scripting::target(process).map_err(Unpinned::Refused)?;
        scripting::permit(&target, process, (CORE_SUITE, GET_DATA)).map_err(Unpinned::Refused)?;
        let tty = confirmed_tty(host, &target, process)
            .map_err(|lookup| Unpinned::Refused(lookup.reason()))?;
        let client = Client::in_front(&tty).ok_or(Unpinned::NoTmux)?;
        let (socket, server, shown) = client.server().map_err(Unpinned::Refused)?;
        Ok(Self {
            host,
            client,
            socket,
            server,
            place: Place {
                tty,
                pane: shown.pane,
            },
        })
    }

    /// Refuses unless the same tab has focus, its client is still in front,
    /// and that client shows the same pane, ready for input.
    pub(super) fn check_pin(&self, process: u32) -> Result<(), DeliveryError> {
        let current = scripting::target(process)
            .map_err(Lookup::Unanswered)
            .and_then(|target| confirmed_tty(self.host, &target, process))
            .and_then(|tty| {
                if !self.client.is_in_front(&tty) {
                    return Err(Lookup::Elsewhere(
                        "the tmux client isn't in front on the focused tab",
                    ));
                }
                if !is_private_socket(&self.socket, rustix::process::getuid().as_raw()) {
                    return Err(Lookup::Unanswered("the tmux socket is no longer private"));
                }
                let listing = self
                    .client
                    .look(&self.socket)
                    .ok_or(Lookup::Unanswered("tmux did not answer"))?;
                let shown = pane_of(&listing, self.client.pid)
                    .ok_or(Lookup::Elsewhere("the tmux client detached"))?
                    .map_err(Lookup::Elsewhere)?;
                // tmux skips a paste into a pane with input off, and still
                // succeeds.
                if shown.input_off {
                    return Err(Lookup::Unanswered("the tmux pane takes no input"));
                }
                // Keys go to the mode, so the person isn't typing into the
                // pane's program.
                if shown.in_mode {
                    return Err(Lookup::Elsewhere(
                        "the tmux pane is in a mode, such as copy mode",
                    ));
                }
                let child_terminals = proc_info::children(self.server)
                    .into_iter()
                    .filter_map(proc_info::terminal);
                if has_popup(&listing, child_terminals) {
                    return Err(Lookup::Elsewhere("a tmux popup is open"));
                }
                Ok(Place {
                    tty,
                    pane: shown.pane,
                })
            });
        focus::verdict(&self.place, current)
    }

    pub(super) fn paste(&self, text: &str, timeout: Duration) -> Result<(), Undelivered> {
        let buffer = buffer_name();
        let pasted = run_tmux(
            &self.client.tmux,
            &self.socket,
            &[
                "load-buffer",
                "-b",
                &buffer,
                "-",
                ";",
                "paste-buffer",
                "-p",
                "-d",
                "-b",
                &buffer,
                "-t",
                &self.place.pane,
            ],
            text.as_bytes(),
            timeout,
        );
        let outcome = paste_outcome(pasted);
        if outcome.is_err() {
            self.delete_buffer(&buffer);
        }
        outcome
    }

    /// A paste that failed after `load-buffer` leaves the text in the buffer.
    fn delete_buffer(&self, buffer: &str) {
        let deleted = run_tmux(
            &self.client.tmux,
            &self.socket,
            &["delete-buffer", "-b", buffer],
            &[],
            LOOKUP_TIMEOUT,
        );
        if !deleted.is_ok_and(|output| output.status.success()) {
            tracing::debug!("tmux kept no paste buffer to delete, or didn't answer");
        }
    }
}

/// A random name, so a paste neither replaces nor deletes one of the person's
/// buffers.
fn buffer_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("aria-{random:016x}")
}

/// tmux stops a command list at its first error, so an exit with an error
/// means `paste-buffer` wrote nothing. Once tmux has started, any other
/// failure, a signal among them, may come after the paste landed.
fn paste_outcome(pasted: Result<Output, RunError>) -> Result<(), Undelivered> {
    match pasted {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) if output.status.code().is_some() => {
            tracing::warn!(status = %output.status, "tmux refused the text");
            Err(Undelivered::Refused(DeliveryError::TargetUnavailable))
        }
        Err(RunError::Spawn(error)) => {
            tracing::warn!(%error, "tmux couldn't start");
            Err(Undelivered::Refused(DeliveryError::TargetUnavailable))
        }
        Ok(output) => {
            tracing::warn!(status = %output.status, "tmux didn't confirm the paste");
            Err(Undelivered::Uncertain(DeliveryError::PasteUnconfirmed))
        }
        Err(error) => {
            tracing::warn!(%error, "tmux didn't confirm the paste");
            Err(Undelivered::Uncertain(DeliveryError::PasteUnconfirmed))
        }
    }
}

/// A tmux client process, and the executable it runs. Commands go through
/// that executable, so they match its server.
struct Client {
    pid: i32,
    tmux: PathBuf,
}

impl Client {
    /// The tmux client in the foreground of `tty`, where the tab's keys go.
    /// Only the user, root or `admin` may change the folders above its
    /// executable, or the file could be swapped after the check.
    fn in_front(tty: &str) -> Option<Self> {
        let device = terminal_device(tty)?;
        let (pid, tmux) = proc_info::on_terminal(device)
            .into_iter()
            .filter(|&pid| proc_info::is_foreground(pid, device))
            .filter_map(|pid| Some((pid, proc_info::executable(pid)?)))
            .find(|(_, path)| is_tmux(path))?;
        let tmux = InspectedFile::inspect(tmux)
            .and_then(InspectedFile::require_regular_file)
            .and_then(InspectedFile::require_executable)
            .and_then(InspectedFile::require_trusted)
            .inspect_err(|error| tracing::warn!(%error, "the tmux executable isn't trusted"))
            .ok()?;
        let uid = rustix::process::getuid().as_raw();
        if acl::allows_access(&tmux) || !is_guarded(&tmux, uid, Writers::Admins) {
            tracing::warn!("another account may change the tmux executable or a folder above it");
            return None;
        }
        Some(Self { pid, tmux })
    }

    fn is_in_front(&self, tty: &str) -> bool {
        terminal_device(tty).is_some_and(|device| proc_info::is_foreground(self.pid, device))
    }

    /// The server this client is attached to, and its active pane. `-L`, `-S`
    /// and `TMUX_TMPDIR` can put the socket anywhere, so every private socket
    /// the person's tmux processes listen on gets asked.
    fn server(&self) -> Result<(PathBuf, i32, Shown), &'static str> {
        let uid = rustix::process::getuid().as_raw();
        let mut listening: Vec<(PathBuf, i32)> = proc_info::of_user(uid)
            .into_iter()
            .filter(|&pid| proc_info::executable(pid).is_some_and(|path| is_tmux(&path)))
            .flat_map(|pid| {
                proc_info::bound_sockets(pid)
                    .into_iter()
                    .filter_map(move |path| Some((private_socket(&path, uid)?, pid)))
            })
            .collect();
        listening.sort();
        listening.dedup();
        for (socket, server) in listening {
            let Some(listing) = self.look(&socket) else {
                continue;
            };
            if let Some(shown) = pane_of(&listing, self.pid) {
                return shown.map(|shown| (socket, server, shown));
            }
        }
        Err("no tmux server lists the client")
    }

    /// The server's clients in `CLIENTS_FORMAT`, then a `tty <path>` line for
    /// each of its panes.
    fn look(&self, socket: &Path) -> Option<String> {
        let output = run_tmux(
            &self.tmux,
            socket,
            &[
                "list-clients",
                "-F",
                CLIENTS_FORMAT,
                ";",
                "list-panes",
                "-a",
                "-F",
                "tty #{pane_tty}",
            ],
            &[],
            LOOKUP_TIMEOUT,
        )
        .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn confirmed_tty(
    host: Host,
    target: &NSAppleEventDescriptor,
    process: u32,
) -> Result<String, Lookup> {
    let tty = host
        .tty()
        .ok_or(Lookup::Unanswered("focused tab unaddressable"))?;
    let tty = scripting::get_text(target, &tty)?;
    key_window::confirm(process).map_err(Lookup::Elsewhere)?;
    Ok(tty)
}

fn terminal_device(tty: &str) -> Option<u32> {
    u32::try_from(std::fs::metadata(tty).ok()?.rdev()).ok()
}

fn is_tmux(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("tmux"))
}

/// The pane a tmux client shows, and whether it can take a paste now.
#[derive(Debug, PartialEq, Eq)]
struct Shown {
    pane: String,
    in_mode: bool,
    input_off: bool,
}

/// What `client` shows, from `list-clients` output in `CLIENTS_FORMAT`, if
/// the client is listed. tmux drops a read-only client's keys but not a paste
/// from another command, so a read-only client is refused. A control-mode
/// client, as iTerm2's tmux integration runs, and an `active-pane` client
/// may show a different pane than the one tmux names. An older tmux leaves
/// the fields it doesn't know empty, which refuses too.
fn pane_of(listing: &str, client: i32) -> Option<Result<Shown, &'static str>> {
    let client = client.to_string();
    listing.lines().find_map(|line| {
        let mut fields = line.split(' ');
        if fields.next()? != client {
            return None;
        }
        let [readonly, control, flags, pane, in_mode, input_off] =
            std::array::from_fn(|_| fields.next().unwrap_or_default());
        let own_pane = flags.split(',').any(|flag| flag == "active-pane");
        Some(match (readonly, control) {
            ("1", _) => Err("the tmux client is read-only"),
            (_, "1") => Err("the tmux client is in control mode"),
            _ if own_pane => Err("the tmux client picks its own active pane"),
            ("0", "0") if pane.starts_with('%') && !in_mode.is_empty() && !input_off.is_empty() => {
                Ok(Shown {
                    pane: pane.to_owned(),
                    in_mode: in_mode != "0",
                    input_off: input_off == "1",
                })
            }
            _ => Err("tmux can't say which pane its client shows"),
        })
    })
}

/// Whether a child of the tmux server runs on a terminal no pane has, as a
/// popup's program does. tmux keeps typed keys from the pane under a popup,
/// but not a paste. tmux shows no format for a popup, a menu or its command
/// prompt, nor which client a popup belongs to. So a popup whose program
/// exited goes unseen, and another client's popup counts too.
fn has_popup(listing: &str, child_terminals: impl IntoIterator<Item = u32>) -> bool {
    let panes: Vec<u32> = listing
        .lines()
        .filter_map(|line| line.strip_prefix("tty "))
        .filter_map(terminal_device)
        .collect();
    child_terminals
        .into_iter()
        .any(|device| !panes.contains(&device))
}

/// `path` by its canonical path, if it's a socket private to `uid`.
fn private_socket(path: &Path, uid: u32) -> Option<PathBuf> {
    let socket = path.parent()?.canonicalize().ok()?.join(path.file_name()?);
    is_private_socket(&socket, uid).then_some(socket)
}

/// Whether `socket` is a socket `uid` owns, in folders no other account can
/// change. An account that could replace the socket, or rename a folder on
/// its way, could put in a server that receives the text.
fn is_private_socket(socket: &Path, uid: u32) -> bool {
    let owned = std::fs::symlink_metadata(socket)
        .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.uid() == uid);
    owned && is_guarded(socket, uid, Writers::Nobody)
}

/// Who besides the user and root may write a folder that still counts as
/// safe.
#[derive(Debug, Clone, Copy)]
enum Writers {
    Nobody,
    /// Members of `admin` can become root anyway, and some package managers
    /// let them write their folders.
    Admins,
}

/// Whether only `uid`, root and `writers` can change the folders above
/// `path`, by their mode bits and by any access control list.
fn is_guarded(path: &Path, uid: u32, writers: Writers) -> bool {
    path.ancestors().skip(1).all(|folder| {
        std::fs::symlink_metadata(folder).is_ok_and(|metadata| {
            metadata.is_dir()
                && is_held(
                    (metadata.uid(), metadata.gid(), metadata.mode()),
                    uid,
                    writers,
                )
                && !acl::allows_access(folder)
        })
    })
}

/// Whether a folder with this owner, group and mode is safe. A sticky
/// folder, such as `/tmp`, lets no one move what they don't own.
fn is_held((owner, group, mode): (u32, u32, u32), uid: u32, writers: Writers) -> bool {
    const ADMIN: u32 = 80;
    let sticky = mode & 0o1000 != 0;
    let admins_allowed = matches!(writers, Writers::Admins) && group == ADMIN;
    let group_writes = mode & 0o020 != 0 && !admins_allowed;
    let others_write = mode & 0o002 != 0;
    (owner == uid || owner == 0) && (sticky || !(group_writes || others_write))
}

fn run_tmux(
    tmux: &Path,
    socket: &Path,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
) -> Result<Output, RunError> {
    let mut command = Command::new(tmux);
    command.env_clear().arg("-S").arg(socket).args(args);
    let bounds = Bounds {
        timeout,
        stdout: Capture::FailAbove(MAX_OUTPUT),
        stderr: Capture::KeepHead(MAX_OUTPUT),
    };
    process::run(&mut command, input, bounds, &CancelToken::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pane_comes_from_the_listed_client_that_can_take_text() {
        let shown = |in_mode, input_off| {
            Some(Ok(Shown {
                pane: "%17".to_owned(),
                in_mode,
                input_off,
            }))
        };
        let cases = [
            (
                "41 0 0 attached %3 0 0\n92 0 0 attached,focused %17 0 0\n",
                shown(false, false),
            ),
            ("92 0 0 attached %17 1 0\n", shown(true, false)),
            ("92 0 0 attached %17 0 1\n", shown(false, true)),
            ("41 0 0 attached %3 0 0\ntty /dev/ttys004\n", None),
            ("", None),
            (
                "92 1 0 attached,read-only %17 0 0\n",
                Some(Err("the tmux client is read-only")),
            ),
            (
                "92 0 1 attached,control-mode %17 0 0\n",
                Some(Err("the tmux client is in control mode")),
            ),
            (
                "92 0 0 attached,active-pane %17 0 0\n",
                Some(Err("the tmux client picks its own active pane")),
            ),
            (
                "92    %17  \n",
                Some(Err("tmux can't say which pane its client shows")),
            ),
        ];
        for (listing, expected) in cases {
            assert_eq!(pane_of(listing, 92), expected, "listing {listing:?}");
        }
    }

    #[test]
    fn only_the_executable_may_sit_in_folders_admins_can_write() {
        const UID: u32 = 501;
        const ADMIN: u32 = 80;
        const STAFF: u32 = 20;
        let homebrew = (UID, ADMIN, 0o40775);
        assert!(is_held(homebrew, UID, Writers::Admins));
        assert!(!is_held(homebrew, UID, Writers::Nobody));
        let staff = (UID, STAFF, 0o40775);
        assert!(!is_held(staff, UID, Writers::Admins));
        let tmp = (0, 0, 0o41777);
        assert!(is_held(tmp, UID, Writers::Nobody));
        let open = (UID, STAFF, 0o40777);
        assert!(!is_held(open, UID, Writers::Admins));
        let foreign = (UID + 1, STAFF, 0o40755);
        assert!(!is_held(foreign, UID, Writers::Nobody));
    }

    #[test]
    fn a_server_child_on_a_terminal_no_pane_has_is_a_popup() {
        let pane = terminal_device("/dev/null").expect("a device");
        let listing = "92 0 0 attached %17 0 0\ntty /dev/null\n";

        assert!(!has_popup(listing, [pane]));
        assert!(has_popup(listing, [pane, pane + 1]));
        assert!(!has_popup(listing, []));
    }

    #[test]
    fn only_sockets_private_to_the_user_are_asked() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let uid = rustix::process::getuid().as_raw();
        let root = tempfile::tempdir().expect("a temporary directory");
        let root = root.path().canonicalize().expect("a canonical path");
        let folder = |name: &str, mode: u32| {
            let folder = root.join(name);
            std::fs::create_dir(&folder).expect("a folder");
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode))
                .expect("the folder takes the mode");
            folder
        };
        let private = folder("private", 0o700).join("default");
        let sticky = folder("sticky", 0o1777).join("default");
        let open = folder("open", 0o777).join("default");
        let servers: Vec<_> = [&private, &sticky, &open]
            .into_iter()
            .map(|socket| UnixListener::bind(socket).expect("a socket"))
            .collect();
        let link = root.join("private/link");
        std::os::unix::fs::symlink(&private, &link).expect("a symlink");
        let file = root.join("private/file");
        std::fs::write(&file, "").expect("a file");
        let paths = [private.clone(), sticky.clone(), open, link, file];

        let private_to = |uid| {
            paths
                .iter()
                .filter_map(|path| private_socket(path, uid))
                .collect::<Vec<_>>()
        };
        assert_eq!(private_to(uid), vec![private, sticky]);
        assert!(private_to(uid + 1).is_empty());
        drop(servers);
    }

    #[test]
    fn only_a_tmux_that_never_started_or_answered_with_an_error_took_no_text() {
        use std::os::unix::process::ExitStatusExt;

        let exited = |code: i32| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let failure = || std::io::Error::other("broken pipe");

        assert!(paste_outcome(Ok(exited(0))).is_ok());
        let killed = Output {
            status: std::process::ExitStatus::from_raw(9),
            ..exited(0)
        };
        assert!(matches!(
            paste_outcome(Ok(killed)),
            Err(Undelivered::Uncertain(DeliveryError::PasteUnconfirmed))
        ));
        assert!(matches!(
            paste_outcome(Ok(exited(1))),
            Err(Undelivered::Refused(_))
        ));
        assert!(matches!(
            paste_outcome(Err(RunError::Spawn(failure()))),
            Err(Undelivered::Refused(_))
        ));
        for error in [
            RunError::Write(failure()),
            RunError::Read(failure()),
            RunError::TimedOut(Duration::from_secs(1)),
        ] {
            assert!(matches!(
                paste_outcome(Err(error)),
                Err(Undelivered::Uncertain(DeliveryError::PasteUnconfirmed))
            ));
        }
    }

    #[test]
    fn only_the_two_terminals_without_a_paste_command_go_through_tmux() {
        assert_eq!(Host::of("com.apple.Terminal"), Some(Host::Terminal));
        assert_eq!(Host::of("com.googlecode.iterm2"), Some(Host::ITerm));
        assert_eq!(Host::of("com.mitchellh.ghostty"), None);
    }
}
