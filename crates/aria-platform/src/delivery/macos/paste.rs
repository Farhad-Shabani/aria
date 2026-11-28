//! Pasting into a terminal through its own paste path, so a program that
//! asked for bracketed paste reads the text as text.

use std::time::Duration;

use super::Undelivered;
use crate::delivery::DeliveryError;

mod focus;
mod ghostty;
mod key_window;
mod scripting;
mod tmux;

pub(super) enum Paste {
    Ghostty(ghostty::Terminal),
    Tmux(tmux::Pane),
}

impl Paste {
    /// `None` means typed keys with no warning: no paste path, or no trusted
    /// tmux client in front. An error carries the reason to warn about.
    pub(super) fn focused(process: u32, bundle_id: &str) -> Result<Option<Self>, &'static str> {
        if bundle_id == ghostty::BUNDLE_ID {
            return ghostty::Terminal::focused(process)
                .map(|terminal| Some(Self::Ghostty(terminal)));
        }
        let Some(host) = tmux::Host::of(bundle_id) else {
            return Ok(None);
        };
        match tmux::Pane::focused(host, process) {
            Ok(pane) => Ok(Some(Self::Tmux(pane))),
            Err(tmux::Unpinned::NoTmux) => Ok(None),
            Err(tmux::Unpinned::Refused(reason)) => Err(reason),
        }
    }

    /// Whether the paste finds its exact place again, so text may wait even
    /// when the field can't be read. A Ghostty terminal can't: a multiplexer
    /// inside it may show another pane by then.
    pub(super) fn pins_target(&self) -> bool {
        matches!(self, Self::Tmux(_))
    }

    pub(super) fn check_pin(&self, process: u32) -> Result<(), DeliveryError> {
        match self {
            Self::Ghostty(terminal) => terminal.check_pin(process),
            Self::Tmux(pane) => pane.check_pin(process),
        }
    }

    pub(super) fn send(
        &self,
        process: u32,
        text: &str,
        timeout: Duration,
    ) -> Result<(), Undelivered> {
        match self {
            Self::Ghostty(terminal) => terminal.paste(process, text, timeout),
            Self::Tmux(pane) => pane.paste(text, timeout),
        }
    }
}
