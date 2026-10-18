//! The characters beside the caret in the focused field, read through
//! Accessibility so inserted text can be spaced against them.
//!
//! Only a few characters on each side are read, never the field's whole value,
//! and they are never logged. The same read records the field, so streaming
//! can check that typing continues there.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::super::Caret;
use super::element;

/// The whole read's budget. Past it, the text goes in without context.
const READ_BUDGET: Duration = Duration::from_millis(100);
/// UTF-16 units read before the caret: two characters, or one outside the BMP.
const UNITS_BEFORE: cf::Index = 2;
/// What web views report in place of an embedded object or image.
const OBJECT_REPLACEMENT: char = '\u{FFFC}';

pub(super) struct Focus {
    pub(super) application: arc::R<ax::UiElement>,
    pub(super) field: arc::R<ax::UiElement>,
    pub(super) selection: cf::Range,
}

/// The caret of `process`'s focused field, with unknown surroundings when the
/// field hides them, is secure, or answers too slowly. Also hands back the
/// field, when the read reached it.
pub(super) fn read(process: u32) -> (Caret, Option<Focus>) {
    let deadline = Instant::now() + READ_BUDGET;
    let focus = read_focus(process, deadline);
    let caret = focus
        .as_ref()
        .map_err(|reason| *reason)
        .and_then(|focus| context(focus, deadline))
        .unwrap_or_else(|reason| {
            tracing::debug!(reason, "caret context unknown");
            Caret::default()
        });
    (caret, focus.ok())
}

pub(super) fn read_focus(process: u32, deadline: Instant) -> Result<Focus, &'static str> {
    // `prepare` checked this before recording; it may have turned on since.
    if super::secure_input_enabled() {
        return Err("secure input");
    }
    let process = i32::try_from(process).map_err(|_| "process id out of range")?;
    let application = ax::UiElement::with_app_pid(process);
    let field = element::focused_field(&application, deadline)?;
    let selection = element::selection(&field, deadline)?;
    Ok(Focus {
        application,
        field,
        selection,
    })
}

fn context(focus: &Focus, deadline: Instant) -> Result<Caret, &'static str> {
    let Focus {
        field, selection, ..
    } = focus;
    if is_secure(field, deadline)? {
        return Err("secure field");
    }
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

fn is_secure(field: &ax::UiElement, deadline: Instant) -> Result<bool, &'static str> {
    element::limit(field, deadline)?;
    Ok(field
        .attr_value(ax::attr::subrole())
        .is_ok_and(|subrole| subrole.equal(ax::sub_role::secure_text_field())))
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
