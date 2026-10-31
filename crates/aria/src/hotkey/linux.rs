//! No Linux hotkey listener exists. The config default keeps the hotkey
//! disabled on Linux, so this runs only when someone enables it by hand.

use super::{HotkeyKey, ListenerEvent, ListenerExit};

pub(crate) struct LinuxHotkeyListener;

impl LinuxHotkeyListener {
    pub(crate) fn unarmed_reason() -> String {
        "the dictation hotkey is not implemented on linux".to_owned()
    }

    pub(crate) fn listen(
        _key: HotkeyKey,
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
