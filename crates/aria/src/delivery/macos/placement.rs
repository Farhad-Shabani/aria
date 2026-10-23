//! Where the caret sits on screen, so the Mac app can draw the words not yet
//! typed beside it. Reads geometry and the character before the caret, and
//! logs neither.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::element;
use crate::delivery::{Font, Placement, Rect};

/// Past it, no ghost shows until the next preview.
pub(super) const READ_BUDGET: Duration = Duration::from_millis(50);
/// How far a zero-width caret may stray from the previous character's edge.
/// Some apps report one far from where the caret blinks.
const CARET_DRIFT: f64 = 24.0;
/// How far outside the field a caret may sit before it counts as bogus.
const FIELD_MARGIN: f64 = 20.0;
/// Caret heights at or past these come from apps that report placeholders.
const MIN_CARET_HEIGHT: f64 = 2.0;
const MAX_CARET_HEIGHT: f64 = 200.0;

pub(super) fn read(
    field: &ax::UiElement,
    caret: cf::Index,
    deadline: Instant,
) -> Option<Placement> {
    let frame = frame(field, deadline);
    let at_caret = bounds(field, caret, 0, deadline);
    let before = (caret > 0)
        .then(|| attributed(field, caret - 1, deadline))
        .flatten();
    let before = before.as_deref().and_then(attributes::attr_string);
    let previous = (caret > 0)
        .then(|| bounds(field, caret - 1, 1, deadline))
        .flatten()
        .map(|rect| {
            if before.is_some_and(breaks_line) {
                Previous::LineBreak(rect)
            } else {
                Previous::Character(rect)
            }
        });
    let caret_rect = choose_caret(at_caret, previous, frame)?;
    let font = before.and_then(font_of);
    Some(Placement {
        caret: caret_rect,
        field: frame,
        font,
    })
}

/// The character before the caret, as the app measures it.
#[derive(Clone, Copy)]
enum Previous {
    Character(Rect),
    /// Its rectangle can run to the end of its line, so its right edge is no
    /// caret.
    LineBreak(Rect),
}

/// Picks the caret rectangle from the zero-width one and the previous
/// character, or `None` when neither is believable or the pick falls outside
/// the field.
fn choose_caret(
    at_caret: Option<Rect>,
    previous: Option<Previous>,
    field: Option<Rect>,
) -> Option<Rect> {
    let at_caret = at_caret.filter(plausible);
    let chosen = match previous {
        Some(Previous::LineBreak(line_break)) if plausible(&line_break) => {
            next_line(at_caret?, line_break)
        }
        Some(Previous::Character(character)) if plausible(&character) => {
            let edge = Rect {
                x: character.x + character.width,
                width: 0.0,
                ..character
            };
            match at_caret {
                Some(at_caret) if near(at_caret, edge) || below(at_caret, edge) => at_caret,
                _ => edge,
            }
        }
        _ => at_caret?,
    };
    let Some(field) = field else {
        return Some(chosen);
    };
    inside(chosen, field).then(|| below_top(chosen, field))
}

fn plausible(rect: &Rect) -> bool {
    rect.height > MIN_CARET_HEIGHT && rect.height < MAX_CARET_HEIGHT && rect.width >= 0.0
}

fn near(at_caret: Rect, edge: Rect) -> bool {
    (at_caret.x - edge.x).abs() <= CARET_DRIFT && (at_caret.y - edge.y).abs() < edge.height / 2.0
}

/// Some apps, Notes among them, report a caret after a line break on the
/// break's own line. It belongs at the start of the next one.
fn next_line(at_caret: Rect, line_break: Rect) -> Rect {
    if below(at_caret, line_break) {
        return at_caret;
    }
    Rect {
        y: line_break.y + line_break.height,
        ..at_caret
    }
}

/// A caret on a later line than the previous character follows a line break.
fn below(at_caret: Rect, edge: Rect) -> bool {
    at_caret.y >= edge.y + edge.height / 2.0
}

/// AppKit text views, TextEdit among them, report a caret with no character
/// before it one line above where it blinks. No caret sits above its field.
fn below_top(caret: Rect, field: Rect) -> Rect {
    Rect {
        y: caret.y.max(field.y),
        ..caret
    }
}

fn inside(rect: Rect, field: Rect) -> bool {
    rect.x >= field.x - FIELD_MARGIN
        && rect.x <= field.x + field.width + FIELD_MARGIN
        && rect.y >= field.y - FIELD_MARGIN
        && rect.y + rect.height <= field.y + field.height + FIELD_MARGIN
}

fn frame(field: &ax::UiElement, deadline: Instant) -> Option<Rect> {
    element::limit(field, deadline).ok()?;
    field.frame().ok()?.cg_rect().map(rect)
}

fn bounds(
    field: &ax::UiElement,
    loc: cf::Index,
    len: cf::Index,
    deadline: Instant,
) -> Option<Rect> {
    element::limit(field, deadline).ok()?;
    let range = ax::Value::with_cf_range(&cf::Range { loc, len });
    let value = element::parameterized(field, ax::param_attr::bounds_for_range(), &range)?;
    element::ax_value(&value)?.cg_rect().map(rect)
}

/// The character at `loc`, with its attributes.
fn attributed(
    field: &ax::UiElement,
    loc: cf::Index,
    deadline: Instant,
) -> Option<arc::R<cf::Type>> {
    element::limit(field, deadline).ok()?;
    let range = ax::Value::with_cf_range(&cf::Range { loc, len: 1 });
    element::parameterized(field, ax::param_attr::attr_string_for_range(), &range)
}

fn breaks_line(character: &cf::AttrString) -> bool {
    let string = character.string();
    !string.is_empty() && matches!(string.character_at_index(0), 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// Another process's font arrives as an `AXFont` dictionary, never a font
/// object.
fn font_of(string: &cf::AttrString) -> Option<Font> {
    let font = attributes::font(string)?;
    let name = font
        .value(cf::str!(c"AXFontName"))?
        .try_as_string()?
        .to_string();
    let size = font
        .value(cf::str!(c"AXFontSize"))?
        .try_as_number()?
        .to_f64()?;
    Some(Font { name, size })
}

fn rect(rect: cidre::cg::Rect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

/// Reads the `AXFont` attribute from an attributed string, which cidre 0.28
/// does not wrap.
#[allow(
    unsafe_code,
    reason = "checked CF casts and one CoreFoundation getter; the invariants \
              live on the SAFETY comments"
)]
mod attributes {
    use cidre::cf;

    pub(super) fn attr_string(value: &cf::Type) -> Option<&cf::AttrString> {
        if value.get_type_id() != cf::AttrString::type_id() {
            return None;
        }
        // SAFETY: the type ID identifies a CFAttributedString, and the borrow
        // retains its owner.
        Some(unsafe { &*std::ptr::from_ref(value).cast::<cf::AttrString>() })
    }

    pub(super) fn font(string: &cf::AttrString) -> Option<&cf::Dictionary> {
        if string.is_empty() {
            return None;
        }
        // SAFETY: index 0 is inside a nonempty string, the name is a live
        // CFString, and a null range pointer asks for no range. The result
        // follows the Get rule, so it borrows from `string`.
        let font = unsafe {
            CFAttributedStringGetAttribute(string, 0, cf::str!(c"AXFont"), std::ptr::null_mut())
        }?;
        if font.get_type_id() != cf::Dictionary::type_id() {
            return None;
        }
        // SAFETY: the type ID identifies a CFDictionary owned by `string`.
        Some(unsafe { &*std::ptr::from_ref(font).cast::<cf::Dictionary>() })
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C-unwind" {
        fn CFAttributedStringGetAttribute<'a>(
            string: &'a cf::AttrString,
            loc: cf::Index,
            name: &cf::String,
            effective_range: *mut cf::Range,
        ) -> Option<&'a cf::Type>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn the_font_comes_from_the_ax_font_attribute() {
        let name = cf::String::from_str("Helvetica");
        let size = cf::Number::from_f64(13.0);
        let font = cf::Dictionary::with_keys_values(
            &[cf::str!(c"AXFontName"), cf::str!(c"AXFontSize")],
            &[&name, &size],
        )
        .unwrap();
        let attributes =
            cf::Dictionary::with_keys_values(&[cf::str!(c"AXFont")], &[&font]).unwrap();
        let string = cf::AttrString::with_attrs(&cf::String::from_str("a"), &attributes);

        assert_eq!(
            font_of(&string),
            Some(Font {
                name: "Helvetica".to_owned(),
                size: 13.0
            })
        );
        assert_eq!(
            font_of(&cf::AttrString::new(&cf::String::from_str("a"))),
            None
        );
    }

    #[test]
    fn line_breaks_are_told_apart_from_text() {
        for line_break in ["\n", "\r", "\u{2028}", "\u{2029}"] {
            assert!(breaks_line(&cf::AttrString::new(&cf::String::from_str(
                line_break
            ))));
        }
        assert!(!breaks_line(&cf::AttrString::new(&cf::String::from_str(
            "a"
        ))));
    }

    #[test]
    fn a_zero_width_caret_near_the_previous_character_wins() {
        let at_caret = rect(110.0, 50.0, 0.0, 16.0);
        let previous = rect(100.0, 50.0, 8.0, 16.0);
        assert_eq!(
            choose_caret(Some(at_caret), Some(Previous::Character(previous)), None),
            Some(at_caret)
        );
    }

    #[test]
    fn a_stray_zero_width_caret_falls_back_to_the_previous_edge() {
        let stray = rect(400.0, 50.0, 0.0, 16.0);
        let previous = rect(100.0, 50.0, 8.0, 16.0);
        assert_eq!(
            choose_caret(Some(stray), Some(Previous::Character(previous)), None),
            Some(rect(108.0, 50.0, 0.0, 16.0))
        );
    }

    #[test]
    fn a_caret_after_a_line_break_keeps_its_own_line() {
        let next_line = rect(20.0, 70.0, 0.0, 16.0);
        let previous = rect(300.0, 50.0, 8.0, 16.0);
        assert_eq!(
            choose_caret(Some(next_line), Some(Previous::Character(previous)), None),
            Some(next_line)
        );
    }

    #[test]
    fn a_caret_after_a_line_break_starts_the_next_line() {
        // Notes' own numbers: the caret comes back on the break's line.
        let line_break = Previous::LineBreak(rect(1006.0, 185.0, 330.0, 16.0));
        let reported = rect(680.0, 186.0, 0.0, 16.0);
        assert_eq!(
            choose_caret(Some(reported), Some(line_break), None),
            Some(rect(680.0, 201.0, 0.0, 16.0))
        );
        let already_below = rect(680.0, 201.0, 0.0, 16.0);
        assert_eq!(
            choose_caret(Some(already_below), Some(line_break), None),
            Some(already_below)
        );
        assert_eq!(choose_caret(None, Some(line_break), None), None);
    }

    #[test]
    fn empty_or_giant_rects_are_not_carets() {
        let empty = rect(0.0, 900.0, 0.0, 0.0);
        let giant = rect(10.0, 10.0, 0.0, 400.0);
        assert_eq!(
            choose_caret(Some(rect(10.0, 10.0, 0.0, 2.0)), None, None),
            None
        );
        assert_eq!(choose_caret(Some(empty), None, None), None);
        assert_eq!(
            choose_caret(Some(giant), Some(Previous::Character(empty)), None),
            None
        );
    }

    #[test]
    fn a_caret_above_its_field_drops_to_the_first_line() {
        // TextEdit's own numbers for an empty document.
        let field = rect(181.0, 133.0, 656.0, 384.0);
        let reported = rect(191.0, 120.0, 0.0, 13.0);
        assert_eq!(
            choose_caret(Some(reported), None, Some(field)),
            Some(rect(191.0, 133.0, 0.0, 13.0))
        );
    }

    #[test]
    fn a_caret_outside_its_field_is_rejected() {
        let field = rect(0.0, 0.0, 200.0, 100.0);
        assert_eq!(
            choose_caret(Some(rect(500.0, 10.0, 0.0, 16.0)), None, Some(field)),
            None
        );
        let inside_field = rect(150.0, 10.0, 0.0, 16.0);
        assert_eq!(
            choose_caret(Some(inside_field), None, Some(field)),
            Some(inside_field)
        );
    }
}
