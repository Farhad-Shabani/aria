//! The characters beside the caret in the focused field, read through
//! Accessibility so inserted text can be spaced against them.
//!
//! Only a few characters on each side are read, never the field's whole value,
//! and they are never logged.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::super::Caret;
use super::selection_range;

/// The whole read's budget. Past it, the text goes in without context.
const READ_BUDGET: Duration = Duration::from_millis(100);
/// UTF-16 units read before the caret: two characters, or one outside the BMP.
const UNITS_BEFORE: cf::Index = 2;
/// What web views report in place of an embedded object or image.
const OBJECT_REPLACEMENT: char = '\u{FFFC}';

/// The caret of `process`'s focused field, with unknown surroundings when the
/// field hides them, is secure, or answers too slowly.
pub(super) fn read(process: u32) -> Caret {
    let deadline = Instant::now() + READ_BUDGET;
    read_field(process, deadline).unwrap_or_else(|reason| {
        tracing::debug!(reason, "caret context unknown");
        Caret::default()
    })
}

fn read_field(process: u32, deadline: Instant) -> Result<Caret, &'static str> {
    // `prepare` checked this before recording; it may have turned on since.
    if super::secure_input_enabled() {
        return Err("secure input");
    }
    let process = i32::try_from(process).map_err(|_| "process id out of range")?;
    let application = ax::UiElement::with_app_pid(process);
    limit(&application, deadline)?;
    let field = application
        .focused_ui_element()
        .map_err(|_| "no focused element")?;
    if is_secure(&field, deadline)? {
        return Err("secure field");
    }
    if shows_placeholder(&field, deadline)? {
        return Err("placeholder shown");
    }
    limit(&field, deadline)?;
    let selection = field
        .attr_value(ax::attr::selected_text_range())
        .map_err(|_| "no selection")?;
    let selection = selection_range(&selection).ok_or("selection is not a range")?;
    if selection.loc < 0 || selection.len < 0 {
        return Err("selection out of range");
    }
    let selection_end = selection
        .loc
        .checked_add(selection.len)
        .ok_or("selection out of range")?;

    let before_start = selection.loc.saturating_sub(UNITS_BEFORE);
    let before = if before_start < selection.loc {
        limit(&field, deadline)?;
        string_for_range(&field, before_start, selection.loc - before_start)
            .ok_or("text before the caret unreadable")?
    } else {
        String::new()
    };
    // One unit decides the rule: half of a pair decodes to nothing, and the
    // emoji or ideograph it belongs to takes no space either way. A read past
    // the field's end fails, which means nothing follows.
    limit(&field, deadline)?;
    let after = string_for_range(&field, selection_end, 1).unwrap_or_default();

    if before.contains(OBJECT_REPLACEMENT) || after.contains(OBJECT_REPLACEMENT) {
        return Err("embedded object beside the caret");
    }
    Ok(Caret::between(&before, &after))
}

fn is_secure(field: &ax::UiElement, deadline: Instant) -> Result<bool, &'static str> {
    limit(field, deadline)?;
    Ok(field
        .attr_value(ax::attr::subrole())
        .is_ok_and(|subrole| subrole.equal(ax::sub_role::secure_text_field())))
}

/// Some web fields report their placeholder as the value of an empty field.
/// A character count equal to the placeholder's length is taken as that case.
fn shows_placeholder(field: &ax::UiElement, deadline: Instant) -> Result<bool, &'static str> {
    limit(field, deadline)?;
    let Ok(placeholder) = field.attr_value(ax::attr::placeholder_value()) else {
        return Ok(false);
    };
    let Some(placeholder) = placeholder.try_as_string().filter(|text| !text.is_empty()) else {
        return Ok(false);
    };
    limit(field, deadline)?;
    let count = field
        .attr_value(ax::attr::num_chars())
        .ok()
        .and_then(|count| count.try_as_number().and_then(cf::Number::to_i64));
    Ok(count == Some(placeholder.len() as i64))
}

/// Bounds the next Accessibility call by what is left of the budget.
fn limit(element: &ax::UiElement, deadline: Instant) -> Result<(), &'static str> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or("read budget spent")?;
    element
        .set_messaging_timeout_secs(remaining.as_secs_f32())
        .map_err(|_| "timeout refused")
}

#[allow(
    unsafe_code,
    reason = "cidre 0.28 wraps no parameterized attribute read; the \
              invariants live on the SAFETY comment below"
)]
fn string_for_range(field: &ax::UiElement, loc: cf::Index, len: cf::Index) -> Option<String> {
    let range = ax::Value::with_cf_range(&cf::Range { loc, len });
    let mut value: Option<arc::R<cf::Type>> = None;
    // SAFETY: every reference outlives the call. On success the function
    // stores one retained object in `value`, which `arc::R` then owns.
    let status = unsafe {
        AXUIElementCopyParameterizedAttributeValue(
            field,
            ax::param_attr::string_for_range(),
            &range,
            &mut value,
        )
    };
    if status.is_err() {
        return None;
    }
    let text = value?;
    let text = text.try_as_string()?;
    Some(decode(
        (0..text.len()).map(|index| text.character_at_index(index)),
    ))
}

/// Decodes UTF-16 and drops unpaired halves, which a read starting or ending
/// inside a surrogate pair returns.
fn decode(units: impl IntoIterator<Item = u16>) -> String {
    char::decode_utf16(units).filter_map(Result::ok).collect()
}

#[allow(
    unsafe_code,
    reason = "declares the Accessibility call cidre does not wrap"
)]
unsafe extern "C-unwind" {
    fn AXUIElementCopyParameterizedAttributeValue(
        element: &ax::UiElement,
        attribute: &ax::ParamAttr,
        parameter: &cf::Type,
        value: *mut Option<arc::R<cf::Type>>,
    ) -> ax::Error;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_inside_a_surrogate_pair_keeps_the_whole_characters() {
        let pizza: Vec<u16> = "🍕.".encode_utf16().collect();
        assert_eq!(decode(pizza[1..].iter().copied()), ".");
        assert_eq!(decode(pizza[..1].iter().copied()), "");
        assert_eq!(decode(pizza.iter().copied()), "🍕.");
    }
}
