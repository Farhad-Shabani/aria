//! The paste keystroke on X11, through the XTest extension.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode, Keysym,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::desktop::{Refusal, SendFailure};

const XK_SHIFT_L: Keysym = 0xffe1;
const XK_INSERT: Keysym = 0xff63;
/// The oldest XTest with `FakeInput`.
const XTEST_VERSION: (u8, u16) = (2, 1);

pub(super) struct X11Keyboard {
    connection: RustConnection,
    shift: Keycode,
    insert: Keycode,
}

impl X11Keyboard {
    pub(super) fn connect() -> Result<Self, Refusal> {
        let (connection, _screen) = x11rb::connect(None).map_err(|error| {
            tracing::warn!(%error, "failed to reach the X server");
            Refusal::NoDisplay
        })?;
        let (major, minor) = XTEST_VERSION;
        connection
            .xtest_get_version(major, minor)
            .map_err(|_| Refusal::Failed)?
            .reply()
            .map_err(|error| {
                tracing::warn!(%error, "the X server lacks XTest");
                Refusal::Unsupported
            })?;
        let shift = keycode(&connection, XK_SHIFT_L)?;
        let insert = keycode(&connection, XK_INSERT)?;
        Ok(Self {
            connection,
            shift,
            insert,
        })
    }

    /// Presses Shift+Insert, then waits until the server has taken it.
    pub(super) fn paste(&self) -> Result<(), SendFailure> {
        let mut chord_sent = false;
        for (kind, key) in [
            (KEY_PRESS_EVENT, self.shift),
            (KEY_PRESS_EVENT, self.insert),
            (KEY_RELEASE_EVENT, self.insert),
            (KEY_RELEASE_EVENT, self.shift),
        ] {
            self.connection
                .xtest_fake_input(kind, key, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
                .map_err(|error| {
                    tracing::warn!(%error, "XTest refused a key");
                    if chord_sent {
                        SendFailure::PartlySent
                    } else {
                        SendFailure::NothingSent
                    }
                })?;
            chord_sent |= key == self.insert;
        }
        self.connection.sync().map_err(|error| {
            tracing::warn!(%error, "the X server did not confirm the paste");
            SendFailure::PartlySent
        })
    }
}

fn keycode(connection: &RustConnection, keysym: Keysym) -> Result<Keycode, Refusal> {
    let setup = connection.setup();
    let (first, last) = (setup.min_keycode, setup.max_keycode);
    let mapping = connection
        .get_keyboard_mapping(first, last - first + 1)
        .map_err(|_| Refusal::Failed)?
        .reply()
        .map_err(|_| Refusal::Failed)?;
    find_keycode(first, mapping.keysyms_per_keycode, &mapping.keysyms, keysym).ok_or_else(|| {
        tracing::warn!(keysym, "no key on this keyboard produces the paste chord");
        Refusal::Unsupported
    })
}

fn find_keycode(
    first: Keycode,
    per_keycode: u8,
    keysyms: &[Keysym],
    keysym: Keysym,
) -> Option<Keycode> {
    let index = keysyms
        .chunks(usize::from(per_keycode).max(1))
        .position(|row| row.contains(&keysym))?;
    first.checked_add(u8::try_from(index).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keysym_maps_to_the_first_keycode_whose_row_holds_it() {
        let rows = [0, 0, XK_SHIFT_L, 0, XK_INSERT, XK_INSERT];
        assert_eq!(find_keycode(8, 2, &rows, XK_SHIFT_L), Some(9));
        assert_eq!(find_keycode(8, 2, &rows, XK_INSERT), Some(10));
        assert_eq!(find_keycode(8, 2, &rows, 0x0076), None);
    }
}
