//! Whether a terminal's key window is where text may go.
//!
//! A terminal's scripting interface names its front window's focused tab.
//! That leaves out panels, such as Ghostty's quick terminal, and windows that
//! aren't terminals. So Accessibility must agree that window has keyboard
//! focus before text goes to what the terminal named.

use std::time::Instant;

use cidre::ax;

use super::super::element;
use super::focus::LOOKUP_TIMEOUT;

pub(super) fn confirm(process: u32) -> Result<(), &'static str> {
    is_front_terminal(&key_window(process)?)
}

struct KeyWindow {
    standard: bool,
    frontmost: bool,
    terminal_focused: bool,
}

/// Titles can't identify a window: a quick terminal can share its front
/// window's title.
fn is_front_terminal(key: &KeyWindow) -> Result<(), &'static str> {
    if !key.standard {
        return Err("the key window is a panel, such as Ghostty's quick terminal");
    }
    if !key.frontmost {
        return Err("another window is in front of the key window");
    }
    if !key.terminal_focused {
        return Err("the key window is not a terminal");
    }
    Ok(())
}

fn key_window(process: u32) -> Result<KeyWindow, &'static str> {
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    let pid = i32::try_from(process).map_err(|_| "process id out of range")?;
    let app = ax::UiElement::with_app_pid(pid);
    element::limit(&app, deadline)?;
    let window = app
        .attr_value(ax::attr::focused_window())
        .map_err(|_| "no key window")?;
    let frontmost = element::is_first_window(&app, &window, deadline)?;
    let window = element::ui_element(&window).ok_or("key window is not an element")?;
    element::limit(window, deadline)?;
    let standard = window
        .attr_value(ax::attr::subrole())
        .is_ok_and(|subrole| subrole.equal(ax::sub_role::standard_window()));
    let field = element::focused_field(&app, deadline)?;
    element::limit(&field, deadline)?;
    let terminal_focused = field
        .role()
        .is_ok_and(|role| role.equal(ax::role::text_area()));
    Ok(KeyWindow {
        standard,
        frontmost,
        terminal_focused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRONT_TERMINAL: KeyWindow = KeyWindow {
        standard: true,
        frontmost: true,
        terminal_focused: true,
    };

    #[test]
    fn text_goes_to_a_terminal_only_when_its_key_window_is_the_front_terminal_window() {
        assert!(is_front_terminal(&FRONT_TERMINAL).is_ok());
        let quick_terminal = KeyWindow {
            standard: false,
            ..FRONT_TERMINAL
        };
        let behind_a_floating_window = KeyWindow {
            frontmost: false,
            ..FRONT_TERMINAL
        };
        let about_window = KeyWindow {
            terminal_focused: false,
            ..FRONT_TERMINAL
        };
        for key in [quick_terminal, behind_a_floating_window, about_window] {
            assert!(is_front_terminal(&key).is_err());
        }
    }
}
