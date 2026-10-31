//! No Linux hotkey listener exists, and the daemon keeps the hotkey disabled
//! on Linux.

use super::{AriaKey, ListenerEvent, ListenerExit};

pub(crate) struct LinuxHotkeyListener;

impl LinuxHotkeyListener {
    pub(crate) fn unarmed_reason() -> String {
        "the dictation hotkey is not implemented on linux".to_owned()
    }

    pub(crate) fn listen(
        _key: AriaKey,
        _events: impl Fn(ListenerEvent),
        _should_stop: impl Fn() -> bool,
    ) -> ListenerExit {
        tracing::warn!(
            trigger = "hotkey",
            "the dictation hotkey is not implemented on linux"
        );
        ListenerExit::Unsupported
    }
}
