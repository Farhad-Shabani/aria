//! Text into Ghostty as a paste, through its scripting interface.
//!
//! Ghostty handles `input text` as it handles Cmd-V, without the clipboard
//! and without its paste confirmation: a program that asked for bracketed
//! paste gets the text between paste markers. A modal editor or a vim-mode
//! prompt then reads it as text in any mode, where typed keys would run as
//! its commands.

use std::time::Duration;

use objc2_foundation::{NSAppleEventDescriptor, NSString};

use super::super::Undelivered;
use super::focus::{self, Lookup};
use super::key_window;
use super::scripting::{self, DIRECT_OBJECT, code};
use crate::delivery::DeliveryError;

pub(super) const BUNDLE_ID: &str = "com.mitchellh.ghostty";

// Ghostty.sdef.
const SUITE: u32 = code(b"Ghst");
const INPUT_TEXT: u32 = code(b"InTx");
const INPUT_TARGET: u32 = code(b"GItT");
const TERMINAL: u32 = code(b"Gtrm");
const FRONT_WINDOW: u32 = code(b"GFWn");
const SELECTED_TAB: u32 = code(b"GWsT");
const FOCUSED_TERMINAL: u32 = code(b"GTfT");
const UNIQUE_ID: u32 = code(b"ID  ");

/// The Ghostty terminal surface a press started in, by the stable ID its
/// scripting interface gives.
pub(in crate::delivery::macos) struct Terminal {
    id: String,
}

impl Terminal {
    pub(super) fn focused(process: u32) -> Result<Self, &'static str> {
        let target = scripting::target(process)?;
        scripting::permit(&target, process, (SUITE, INPUT_TEXT))?;
        confirmed_id(&target, process)
            .map(|id| Self { id })
            .map_err(|lookup| lookup.reason())
    }

    /// Refuses unless keyboard focus is still in this terminal. Another tab
    /// of the same window counts as elsewhere.
    pub(super) fn check_focus(&self, process: u32) -> Result<(), DeliveryError> {
        let current = scripting::target(process)
            .map_err(Lookup::Unanswered)
            .and_then(|target| confirmed_id(&target, process));
        focus::verdict(&self.id, current)
    }

    pub(super) fn paste(
        &self,
        process: u32,
        text: &str,
        timeout: Duration,
    ) -> Result<(), Undelivered> {
        let refused = || Undelivered::Refused(DeliveryError::TargetUnavailable);
        let target = scripting::target(process).map_err(|_| refused())?;
        let terminal = scripting::specifier(
            TERMINAL,
            UNIQUE_ID,
            &NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(&self.id)),
            &NSAppleEventDescriptor::nullDescriptor(),
        )
        .ok_or_else(refused)?;
        let event = scripting::event(SUITE, INPUT_TEXT, &target);
        event.setParamDescriptor_forKeyword(
            &NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(text)),
            DIRECT_OBJECT,
        );
        event.setParamDescriptor_forKeyword(&terminal, INPUT_TARGET);
        scripting::send(&event, timeout)
            .map(drop)
            .map_err(scripting::sent_error)
    }
}

fn confirmed_id(target: &NSAppleEventDescriptor, process: u32) -> Result<String, Lookup> {
    let app = NSAppleEventDescriptor::nullDescriptor();
    let terminal = scripting::property(FRONT_WINDOW, &app)
        .and_then(|window| scripting::property(SELECTED_TAB, &window))
        .and_then(|tab| scripting::property(FOCUSED_TERMINAL, &tab))
        .and_then(|terminal| scripting::property(UNIQUE_ID, &terminal))
        .ok_or(Lookup::Unanswered("focused terminal unaddressable"))?;
    let id = scripting::get_text(target, &terminal)?;
    key_window::confirm(process).map_err(Lookup::Elsewhere)?;
    Ok(id)
}
