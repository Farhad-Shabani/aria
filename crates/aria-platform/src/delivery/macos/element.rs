//! Reads of another app's Accessibility elements. `limit` bounds each call
//! by what is left of a deadline.

use std::time::Instant;

use cidre::{arc, ax, cf, os};

/// Far deeper than any app nests its focus, so a loop in the tree ends.
const MAX_DEPTH: usize = 64;

pub(super) fn focused_element(
    application: &ax::UiElement,
    deadline: Instant,
) -> Result<arc::R<ax::UiElement>, &'static str> {
    focus_of(application, deadline)?.ok_or("no focused element")
}

/// `None` when the app reports nothing in focus, as against no answer.
pub(super) fn focus_of(
    application: &ax::UiElement,
    deadline: Instant,
) -> Result<Option<arc::R<ax::UiElement>>, &'static str> {
    limit(application, deadline)?;
    match application.focused_ui_element() {
        Ok(element) => Ok(Some(element)),
        Err(error) if is_absent(error) => Ok(None),
        Err(_) => Err("focus unreadable"),
    }
}

pub(super) fn role(element: &ax::UiElement, deadline: Instant) -> Result<String, &'static str> {
    limit(element, deadline)?;
    element
        .attr_value(ax::attr::role())
        .ok()
        .and_then(|role| role.try_as_string().map(|role| role.to_string()))
        .ok_or("role unreadable")
}

/// `None` when the element has no subrole, as most don't.
pub(super) fn subrole(
    element: &ax::UiElement,
    deadline: Instant,
) -> Result<Option<String>, &'static str> {
    limit(element, deadline)?;
    match element.attr_value(ax::attr::subrole()) {
        Ok(subrole) => subrole
            .try_as_string()
            .map(|subrole| Some(subrole.to_string()))
            .ok_or("subrole is not a string"),
        Err(error) if is_absent(error) => Ok(None),
        Err(_) => Err("subrole unreadable"),
    }
}

pub(super) fn has_ancestor(
    element: &ax::UiElement,
    application: &ax::UiElement,
    matches: impl Fn(&str) -> bool,
    deadline: Instant,
) -> Result<bool, &'static str> {
    walk_up(
        element.retained(),
        |element| parent_of(element, deadline),
        |ancestor| ancestor.equal(application),
        |ancestor| Ok(matches(&role(ancestor, deadline)?)),
    )
}

/// Every element but the application has a parent, so a walk that ends
/// short of the application proves nothing.
fn walk_up<E>(
    start: E,
    parent: impl Fn(&E) -> Result<Option<E>, &'static str>,
    is_application: impl Fn(&E) -> bool,
    matches: impl Fn(&E) -> Result<bool, &'static str>,
) -> Result<bool, &'static str> {
    let mut next = parent(&start)?;
    for _ in 0..MAX_DEPTH {
        let ancestor = next.ok_or("the tree ends short of the app")?;
        if is_application(&ancestor) {
            return Ok(false);
        }
        if matches(&ancestor)? {
            return Ok(true);
        }
        next = parent(&ancestor)?;
    }
    Err("accessibility tree too deep")
}

fn parent_of(
    element: &ax::UiElement,
    deadline: Instant,
) -> Result<Option<arc::R<ax::UiElement>>, &'static str> {
    limit(element, deadline)?;
    match element.attr_value(ax::attr::parent()) {
        Ok(parent) => ui_element(&parent)
            .map(|parent| Some(parent.retained()))
            .ok_or("parent is not an element"),
        Err(error) if is_absent(error) => Ok(None),
        Err(_) => Err("parent unreadable"),
    }
}

/// The element lacks the attribute. Any other error means the app gave no
/// answer.
fn is_absent(error: os::Error) -> bool {
    error == ax::err::ATTR_UNSUPPORTED || error == ax::err::NO_VALUE
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

pub(super) fn is_selection_settable(
    field: &ax::UiElement,
    deadline: Instant,
) -> Result<bool, &'static str> {
    limit(field, deadline)?;
    field
        .is_settable(ax::attr::selected_text_range())
        .map_err(|_| "selection settability unreadable")
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
fn selection_range(value: &cf::Type) -> Option<cf::Range> {
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

#[allow(
    unsafe_code,
    reason = "AXUIElement is a transparent CF type; the runtime type is checked before casting"
)]
pub(super) fn ui_element(value: &cf::Type) -> Option<&ax::UiElement> {
    if value.get_type_id() != ax::UiElement::type_id() {
        return None;
    }
    // SAFETY: the type ID identifies an AXUIElement, and the borrow retains its owner.
    Some(unsafe { &*std::ptr::from_ref(value).cast::<ax::UiElement>() })
}

/// Accessibility lists windows front to back.
#[allow(
    unsafe_code,
    reason = "cidre 0.28 has no checked cast to CFArray; the runtime type is \
              checked before casting, and the invariants live on the SAFETY comments"
)]
pub(super) fn is_first_window(
    application: &ax::UiElement,
    window: &cf::Type,
    deadline: Instant,
) -> Result<bool, &'static str> {
    limit(application, deadline)?;
    let windows = application
        .attr_value(ax::attr::windows())
        .map_err(|_| "window list unreadable")?;
    if windows.get_type_id() != cf::Array::type_id() {
        return Err("window list is not a list");
    }
    // SAFETY: the type ID identifies a CFArray, and the borrow retains its owner.
    let windows = unsafe { &*std::ptr::from_ref::<cf::Type>(&windows).cast::<cf::Array>() };
    if windows.is_empty() {
        return Err("no windows");
    }
    // SAFETY: index 0 exists, and Accessibility lists only CF objects, which
    // `windows` keeps alive for this borrow.
    let first = unsafe { &*windows.get(0).cast::<cf::Type>() };
    Ok(first.equal(window))
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

    /// Walks a chain of roles, `None` marking a missing parent link.
    fn walk(chain: &[Option<&str>]) -> Result<bool, &'static str> {
        walk_up(
            0,
            |&at| match chain.get(at + 1) {
                Some(Some(_)) => Ok(Some(at + 1)),
                Some(None) | None => Ok(None),
            },
            |&at| chain[at] == Some("AXApplication"),
            |&at| Ok(chain[at] == Some("AXList")),
        )
    }

    #[test]
    fn a_walk_must_reach_the_app_to_clear_an_element() {
        let native = [Some("AXGroup"), Some("AXWindow"), Some("AXApplication")];
        let listed = [Some("AXGroup"), Some("AXList"), Some("AXApplication")];
        let cut = [Some("AXGroup"), Some("AXGroup"), None, Some("AXList")];

        assert_eq!(walk(&native), Ok(false));
        assert_eq!(walk(&listed), Ok(true));
        assert!(walk(&cut).is_err());
    }

    #[test]
    fn only_a_missing_attribute_reads_as_absent() {
        assert!(is_absent(ax::err::ATTR_UNSUPPORTED));
        assert!(is_absent(ax::err::NO_VALUE));
        assert!(!is_absent(ax::err::CANNOT_COMPLETE));
        assert!(!is_absent(ax::err::API_DISABLED));
    }

    #[test]
    fn a_read_inside_a_surrogate_pair_keeps_the_whole_characters() {
        let pizza: Vec<u16> = "🍕.".encode_utf16().collect();
        assert_eq!(decode(pizza[1..].iter().copied()), ".");
        assert_eq!(decode(pizza[..1].iter().copied()), "");
        assert_eq!(decode(pizza.iter().copied()), "🍕.");
    }
}
