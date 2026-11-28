//! The characters beside the caret in the focused field, read through
//! Accessibility so inserted text can be spaced against them.
//!
//! Only a few characters on each side are read, never the field's whole value,
//! and they are never logged. The same read records the field, so streaming
//! can check that typing continues there.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::super::{Caret, DeliveryError};
use super::element;

/// The whole read's budget. Past it, the text goes in without context.
const READ_BUDGET: Duration = Duration::from_millis(100);
/// UTF-16 units read before the caret: two characters, or one outside the BMP.
const UNITS_BEFORE: cf::Index = 2;
/// What web views report in place of an embedded object or image.
const OBJECT_REPLACEMENT: char = '\u{FFFC}';

/// Where typed keys would land in an app: its focused element, when that
/// element takes text.
pub(super) struct Focus {
    pub(super) application: arc::R<ax::UiElement>,
    pub(super) element: arc::R<ax::UiElement>,
    /// Absent where the element shows no caret, as a native document that
    /// hides its text does. Only the element's identity can be checked then.
    pub(super) selection: Option<cf::Range>,
}

/// The caret of `process`'s focused field, with unknown surroundings when the
/// field hides them, is secure, or answers too slowly.
pub(super) fn read(process: u32) -> (Caret, Result<Focus, Refusal>) {
    let deadline = Instant::now() + READ_BUDGET;
    let focus = read_focus(process, deadline);
    let caret = focus
        .as_ref()
        .map_err(|refusal| refusal.reason())
        .and_then(|focus| context(focus, deadline))
        .unwrap_or_else(|reason| {
            tracing::debug!(reason, "caret context unknown");
            Caret::default()
        });
    (caret, focus)
}

/// Refuses a focus that takes no text: a password field, a control, or
/// anything in a web page or a list short of a text field.
pub(super) fn read_focus(process: u32, deadline: Instant) -> Result<Focus, Refusal> {
    use Refusal::{NoText, Unreadable};
    // `prepare` checked this before recording; it may have turned on since.
    if super::secure_input_enabled() {
        return Err(NoText("secure input"));
    }
    let process = i32::try_from(process).map_err(|_| Unreadable("process id out of range"))?;
    let application = ax::UiElement::with_app_pid(process);
    let element = element::focus_of(&application, deadline)
        .map_err(Unreadable)?
        .ok_or(NoText("nothing has focus"))?;
    let role = element::role(&element, deadline).map_err(Unreadable)?;
    let subrole = element::subrole(&element, deadline).map_err(Unreadable)?;
    let selection = match kind(&role, subrole.as_deref()) {
        Kind::Never => return Err(NoText("focus takes no text")),
        Kind::Text => element::selection(&element, deadline).ok(),
        Kind::Other => None,
    };
    if selection.is_none()
        && element::has_ancestor(&element, &application, takes_keys_only_in_text, deadline)
            .map_err(Unreadable)?
    {
        return Err(NoText("a page or a list takes keys only in a text field"));
    }
    // An element that lost focus still answers for itself, so focus is read
    // again once its answers are in.
    let now = element::focused_element(&application, deadline).map_err(Unreadable)?;
    if !now.equal(&element) {
        return Err(Unreadable("focus moved during the read"));
    }
    Ok(Focus {
        application,
        element,
        selection,
    })
}

/// Some apps, browsers among them, build their accessibility tree only once a
/// client reads a role; a focus read alone finds no field. Reading the app's role
/// wakes the narrowest mode, which writes nothing to the app.
pub(super) fn wake(process: u32) {
    let Ok(process) = i32::try_from(process) else {
        return;
    };
    let application = ax::UiElement::with_app_pid(process);
    if let Err(reason) = element::role(&application, Instant::now() + READ_BUDGET) {
        tracing::debug!(reason, "the app's accessibility role is unreadable");
    }
}

/// Why keys can't post to the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Nothing that takes text has focus.
    NoText(&'static str),
    /// The app didn't answer in time, or its answers disagreed.
    Unreadable(&'static str),
}

impl Refusal {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::NoText(reason) | Self::Unreadable(reason) => reason,
        }
    }

    pub(super) fn into_error(self) -> DeliveryError {
        match self {
            Self::NoText(_) => DeliveryError::NoTextField,
            Self::Unreadable(_) => DeliveryError::FocusUnreadable,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Edits text. A selection alone isn't enough: web pages and file lists
    /// report one too.
    Text,
    /// Never takes text, so its keys can only run shortcuts.
    Never,
    /// Anything else. Native documents that hide their text report these.
    Other,
}

fn kind(role: &str, subrole: Option<&str>) -> Kind {
    if subrole == Some("AXSecureTextField") || takes_keys_only_in_text(role) {
        return Kind::Never;
    }
    match role {
        "AXTextField" | "AXTextArea" | "AXComboBox" => Kind::Text,
        "AXButton"
        | "AXCheckBox"
        | "AXRadioButton"
        | "AXPopUpButton"
        | "AXMenuButton"
        | "AXMenuItem"
        | "AXMenu"
        | "AXMenuBar"
        | "AXMenuBarItem"
        | "AXStaticText"
        | "AXImage"
        | "AXLink"
        | "AXDisclosureTriangle"
        | "AXScrollBar"
        | "AXSlider"
        | "AXIncrementor"
        | "AXTabGroup"
        | "AXToolbar"
        | "AXSplitter"
        | "AXDockItem"
        | "AXRadioGroup"
        | "AXColorWell"
        | "AXRow"
        | "AXCell"
        | "AXColumn" => Kind::Never,
        _ => Kind::Other,
    }
}

/// Web content reports its editors as text roles, so any other focus there is
/// the page. The items of a list, outline, table, grid or column browser read
/// typed keys as selection and shortcuts.
fn takes_keys_only_in_text(container: &str) -> bool {
    matches!(
        container,
        "AXWebArea" | "AXList" | "AXOutline" | "AXTable" | "AXGrid" | "AXBrowser"
    )
}

fn context(focus: &Focus, deadline: Instant) -> Result<Caret, &'static str> {
    let field = &focus.element;
    let selection = focus.selection.as_ref().ok_or("no caret shown")?;
    if shows_placeholder(field, deadline)? {
        return Err("placeholder shown");
    }
    let selection_end = selection
        .loc
        .checked_add(selection.len)
        .ok_or("selection out of range")?;

    let before_start = selection.loc.saturating_sub(UNITS_BEFORE);
    let before = if before_start < selection.loc {
        element::limit(field, deadline)?;
        element::string_for_range(field, before_start, selection.loc - before_start)
            .ok_or("text before the caret unreadable")?
    } else {
        String::new()
    };
    // One unit decides the rule: half of a pair decodes to nothing, and the
    // emoji or ideograph it belongs to takes no space either way. A read past
    // the field's end fails, which means nothing follows.
    element::limit(field, deadline)?;
    let after = element::string_for_range(field, selection_end, 1).unwrap_or_default();

    if before.contains(OBJECT_REPLACEMENT) || after.contains(OBJECT_REPLACEMENT) {
        return Err("embedded object beside the caret");
    }
    Ok(Caret::between(&before, &after))
}

/// Some web fields report their placeholder as the value of an empty field.
/// A character count equal to the placeholder's length is taken as that case.
fn shows_placeholder(field: &ax::UiElement, deadline: Instant) -> Result<bool, &'static str> {
    element::limit(field, deadline)?;
    let Ok(placeholder) = field.attr_value(ax::attr::placeholder_value()) else {
        return Ok(false);
    };
    let Some(placeholder) = placeholder.try_as_string().filter(|text| !text.is_empty()) else {
        return Ok(false);
    };
    Ok(element::field_length(field, deadline).ok() == Some(placeholder.len() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_role_gets_its_kind() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox"] {
            assert_eq!(kind(role, None), Kind::Text, "{role}");
        }
        for role in [
            "AXWebArea",
            "AXList",
            "AXTable",
            "AXRow",
            "AXLink",
            "AXButton",
        ] {
            assert_eq!(kind(role, None), Kind::Never, "{role}");
        }
        for role in [
            "AXSplitGroup",
            "AXScrollArea",
            "AXGroup",
            "AXLayoutArea",
            "AXWindow",
        ] {
            assert_eq!(kind(role, None), Kind::Other, "{role}");
        }
    }

    #[test]
    fn only_a_page_or_a_list_refuses_what_it_holds() {
        for container in [
            "AXWebArea",
            "AXList",
            "AXOutline",
            "AXTable",
            "AXGrid",
            "AXBrowser",
        ] {
            assert!(takes_keys_only_in_text(container), "{container}");
        }
        // Native documents and spreadsheets sit in these.
        for container in ["AXSplitGroup", "AXScrollArea", "AXWindow"] {
            assert!(!takes_keys_only_in_text(container), "{container}");
        }
    }

    #[test]
    fn a_password_field_takes_no_typed_keys() {
        assert_eq!(kind("AXTextField", Some("AXSecureTextField")), Kind::Never);
        assert_eq!(kind("AXTextField", Some("AXSearchField")), Kind::Text);
    }
}
