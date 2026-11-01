//! Reads of another app's Accessibility elements. `limit` bounds each call
//! by what is left of a deadline.

use std::time::Instant;

use cidre::{arc, ax, cf};

pub(super) fn focused_field(
    application: &ax::UiElement,
    deadline: Instant,
) -> Result<arc::R<ax::UiElement>, &'static str> {
    limit(application, deadline)?;
    application
        .focused_ui_element()
        .map_err(|_| "no focused element")
}

pub(super) fn selection(
    field: &ax::UiElement,
    deadline: Instant,
) -> Result<cf::Range, &'static str> {
    limit(field, deadline)?;
    let selection = field
        .attr_value(ax::attr::selected_text_range())
        .map_err(|_| "no selection")?;
    let selection = selection_range(&selection).ok_or("selection is not a range")?;
    if selection.loc < 0 || selection.len < 0 {
        return Err("selection out of range");
    }
    Ok(selection)
}

/// The field's length in UTF-16 units.
pub(super) fn field_length(field: &ax::UiElement, deadline: Instant) -> Result<i64, &'static str> {
    limit(field, deadline)?;
    field
        .attr_value(ax::attr::num_chars())
        .ok()
        .and_then(|count| count.try_as_number().and_then(cf::Number::to_i64))
        .ok_or("field length unreadable")
}

/// The range an Accessibility selection attribute holds, or `None` when the
/// application answered with another type.
pub(super) fn selection_range(value: &cf::Type) -> Option<cf::Range> {
    ax_value(value)?.cf_range()
}

#[allow(
    unsafe_code,
    reason = "AXValue is a transparent CF type; the runtime type is checked before casting"
)]
pub(super) fn ax_value(value: &cf::Type) -> Option<&ax::Value> {
    if value.get_type_id() != ax::Value::type_id() {
        return None;
    }
    // SAFETY: the type ID identifies an AXValue, and the borrow retains its owner.
    Some(unsafe { &*std::ptr::from_ref(value).cast::<ax::Value>() })
}

/// Bounds the next Accessibility call by what is left of the budget.
pub(super) fn limit(element: &ax::UiElement, deadline: Instant) -> Result<(), &'static str> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or("read budget spent")?;
    element
        .set_messaging_timeout_secs(remaining.as_secs_f32())
        .map_err(|_| "timeout refused")
}

pub(super) fn string_for_range(
    field: &ax::UiElement,
    loc: cf::Index,
    len: cf::Index,
) -> Option<String> {
    let range = ax::Value::with_cf_range(&cf::Range { loc, len });
    let text = parameterized(field, ax::param_attr::string_for_range(), &range)?;
    let text = text.try_as_string()?;
    Some(decode(
        (0..text.len()).map(|index| text.character_at_index(index)),
    ))
}

#[allow(
    unsafe_code,
    reason = "cidre 0.28 wraps no parameterized attribute read; the \
              invariants live on the SAFETY comment below"
)]
pub(super) fn parameterized(
    element: &ax::UiElement,
    attribute: &ax::ParamAttr,
    parameter: &cf::Type,
) -> Option<arc::R<cf::Type>> {
    let mut value: Option<arc::R<cf::Type>> = None;
    // SAFETY: every reference outlives the call. On success the function
    // stores one retained object in `value`, which `arc::R` then owns.
    let status = unsafe {
        AXUIElementCopyParameterizedAttributeValue(element, attribute, parameter, &mut value)
    };
    if status.is_err() {
        return None;
    }
    value
}

/// Decodes UTF-16 and drops unpaired halves, which a read starting or ending
/// inside a surrogate pair returns.
pub(super) fn decode(units: impl IntoIterator<Item = u16>) -> String {
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
    fn range_readback_checks_the_external_type() {
        let range = cf::Range { loc: 2, len: 3 };
        let value = ax::Value::with_cf_range(&range);
        let actual = selection_range(&value).expect("AX range");
        assert_eq!(actual.loc, range.loc);
        assert_eq!(actual.len, range.len);
        assert!(selection_range(&cf::String::from_str("wrong type")).is_none());
    }

    #[test]
    fn a_read_inside_a_surrogate_pair_keeps_the_whole_characters() {
        let pizza: Vec<u16> = "🍕.".encode_utf16().collect();
        assert_eq!(decode(pizza[1..].iter().copied()), ".");
        assert_eq!(decode(pizza[..1].iter().copied()), "");
        assert_eq!(decode(pizza.iter().copied()), "🍕.");
    }
}
