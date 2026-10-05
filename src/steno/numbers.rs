//! Plover's number strokes.
//!
//! `#` plus the number keys: `S T P H A O` on the left and `-F -P -L -T` on the
//! right name digits, the same way on a real steno machine. A stroke with `#`
//! and no dictionary entry types its digits.

use super::layout::slot_by_name;

/// Bit of the `#` key in a stroke.
pub const NUMBER_BIT: u32 = 1 << 22;

/// Digits by slot name, in the order a stroke reads left to right.
const DIGITS: [(&str, char); 10] = [
    ("S-", '1'),
    ("T-", '2'),
    ("P-", '3'),
    ("H-", '4'),
    ("A-", '5'),
    ("O-", '0'),
    ("-F", '6'),
    ("-P", '7'),
    ("-L", '8'),
    ("-T", '9'),
];

/// The digits a number stroke types, or `None` if the stroke isn't a number.
///
/// A number stroke has `#` and only number keys. Any other key, or a bare `#`,
/// makes it a regular stroke.
#[must_use]
pub fn number_text(stroke: u32) -> Option<String> {
    if stroke & NUMBER_BIT == 0 {
        return None;
    }
    let digit_bits: u32 = DIGITS
        .iter()
        .filter_map(|(name, _)| slot_by_name(name))
        .fold(0, |acc, slot| acc | (1 << slot));
    let rest = stroke & !NUMBER_BIT;
    if rest & !digit_bits != 0 || rest == 0 {
        return None;
    }
    let text: String = DIGITS
        .iter()
        .filter(|(name, _)| slot_by_name(name).is_some_and(|slot| rest & (1 << slot) != 0))
        .map(|(_, digit)| *digit)
        .collect();
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    #[test]
    fn number_strokes_type_digits() {
        assert_eq!(
            number_text(parse_stroke("#STPH").unwrap()),
            Some("1234".to_string())
        );
        assert_eq!(
            number_text(parse_stroke("#O").unwrap()),
            Some("0".to_string())
        );
        assert_eq!(
            number_text(parse_stroke("#-FPLT").unwrap()),
            Some("6789".to_string())
        );
    }

    #[test]
    fn non_number_strokes_are_not_numbers() {
        assert_eq!(number_text(parse_stroke("KAT").unwrap()), None);
        assert_eq!(number_text(parse_stroke("#KAT").unwrap()), None);
        assert_eq!(number_text(NUMBER_BIT), None);
    }
}
