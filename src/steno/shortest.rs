//! The fewest-stroke outline that types a word exactly, for words the dictionary
//! has no entry for. `keymux steno keys` shows it in place of "no stroke".
//!
//! Two kinds of segment can make up an outline:
//! - a dictionary entry whose value is plain text, such as `KAT` for `cat`
//! - one fingerspelled letter per letter, from the dictionary's `{&x}` entries
//!
//! Segments only join where the translator would put no space between them. Its
//! spacing rules (see `format::render`) add a space before every word and before
//! every fingerspelled letter, except that consecutive fingerspelled letters join.
//! So under the current rules:
//! - letters join only with letters, and a word can't be glued after a letter
//!   (that would type `t cat`, not `tcat`), so a plain word is never a segment
//!   after a letter
//! - a plain word can only be the first segment, and nothing can follow it, so it
//!   must cover the whole target
//!
//! Gluing a word after a letter would need a dictionary change (an entry that
//! attaches its text, such as `{^...}`, or a letter that doesn't add a space).

use super::dict::Dictionary;
use super::format::Piece;
use std::collections::HashMap;

/// The fewest strokes that type `word` exactly, or `None` if neither a plain
/// dictionary word nor fingerspelling can type it. Ties go to the dictionary word.
#[must_use]
pub fn shortest_outline(dict: &Dictionary, word: &str) -> Option<Vec<u32>> {
    let target: Vec<char> = word.chars().collect();
    if target.is_empty() {
        return None;
    }
    let whole = whole_word_outline(dict, &target);
    let spelled = spelled_outline(&letter_strokes(dict), &target);
    match (whole, spelled) {
        (Some(word_outline), Some(letters)) => {
            Some(if letters.len() < word_outline.len() { letters } else { word_outline })
        }
        (word_outline, letters) => word_outline.or(letters),
    }
}

/// Fewest-stroke outline spelling `target` one letter at a time. `best[i]` is the
/// cheapest outline for `target[..i]`, filled left to right.
fn spelled_outline(letters: &HashMap<char, u32>, target: &[char]) -> Option<Vec<u32>> {
    let mut best: Vec<Option<Vec<u32>>> = vec![None; target.len() + 1];
    best[0] = Some(Vec::new());
    for (i, &c) in target.iter().enumerate() {
        let (Some(prefix), Some(&stroke)) = (best[i].clone(), letters.get(&c)) else {
            continue;
        };
        let mut outline = prefix;
        outline.push(stroke);
        relax(&mut best[i + 1], outline);
    }
    best[target.len()].take()
}

/// The fewest-stroke dictionary entry whose plain text is exactly `target`.
fn whole_word_outline(dict: &Dictionary, target: &[char]) -> Option<Vec<u32>> {
    dict.iter()
        .filter(|(_, pieces)| {
            matches!(pieces.as_slice(), [Piece::Text(text)] if text.chars().eq(target.iter().copied()))
        })
        .map(|(strokes, _)| strokes.clone())
        .min_by_key(|strokes| {
            (strokes.len(), strokes.iter().map(|s| s.count_ones()).sum::<u32>(), strokes.clone())
        })
}

/// Keep `candidate` if the slot is empty or it uses fewer strokes.
fn relax(slot: &mut Option<Vec<u32>>, candidate: Vec<u32>) {
    if slot.as_ref().is_none_or(|best| candidate.len() < best.len()) {
        *slot = Some(candidate);
    }
}

/// The stroke that fingerspells each letter, from the dictionary's `{&x}` entries. Where a
/// letter has more than one, the lowest stroke is used so the answer doesn't depend on order.
fn letter_strokes(dict: &Dictionary) -> HashMap<char, u32> {
    let mut map: HashMap<char, u32> = HashMap::new();
    for (strokes, pieces) in dict.iter() {
        // `{>}` only keeps the letter lowercase, which it already is
        let pieces: Vec<&Piece> = pieces.iter().filter(|piece| !matches!(piece, Piece::LowercaseNext)).collect();
        let ([stroke], [Piece::Fingerspell(letters)]) = (strokes.as_slice(), pieces.as_slice()) else {
            continue;
        };
        let mut chars = letters.chars();
        if let (Some(letter), None) = (chars.next(), chars.next()) {
            map.entry(letter).and_modify(|s| *s = (*s).min(*stroke)).or_insert(*stroke);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn dict(json: &str) -> Dictionary {
        let mut dict = Dictionary::default();
        dict.merge_json(json).unwrap();
        dict
    }

    #[test]
    fn nvim_is_four_letter_strokes() {
        let dict = dict(r#"{"TPH": "{&n}", "SR*": "{&v}", "SKW*": "{&i}", "PH*": "{&m}"}"#);
        let outline = shortest_outline(&dict, "nvim").unwrap();
        let expected: Vec<u32> = ["TPH", "SR*", "SKW*", "PH*"].iter().map(|s| parse_stroke(s).unwrap()).collect();
        assert_eq!(outline, expected);
    }

    #[test]
    fn a_word_with_a_direct_entry_uses_it() {
        let dict = dict(r#"{"KAT": "cat", "TPH": "{&n}", "SRA": "{&v}"}"#);
        assert_eq!(shortest_outline(&dict, "cat"), Some(vec![parse_stroke("KAT").unwrap()]));
    }

    #[test]
    fn a_word_with_a_letter_that_has_no_stroke_gets_none() {
        let dict = dict(r#"{"TPH": "{&n}"}"#);
        assert_eq!(shortest_outline(&dict, "n1"), None);
        assert_eq!(shortest_outline(&dict, ""), None);
    }

    #[test]
    fn a_dictionary_word_is_not_glued_after_a_letter() {
        // "ncat" can't be "n" + "cat": the cat stroke would type " cat" after the letter,
        // and there are no letter strokes for c, a or t
        let dict = dict(r#"{"TPH": "{&n}", "KAT": "cat"}"#);
        assert_eq!(shortest_outline(&dict, "ncat"), None);
    }
}
