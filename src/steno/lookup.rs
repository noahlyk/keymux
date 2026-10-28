//! Live lookup: "I'm mid-sentence and forgot the stroke for a word."
//!
//! A dictionary entry for `{PLOVER:LOOKUP}` opens a lookup. The steno layer stays on, but its
//! keys go back to plain QWERTY: they type into a query that is shown on the overlay and
//! never reaches the application. The overlay lists the outlines that type the word, best
//! first, and lights the first stroke of the best one on its keyboard. Esc or Enter closes it.

use super::commands::proper_outlines_in;
use super::layout::render_stroke;
use super::overlay::{LookupRow, LookupView};
use super::translate::Translator;
use crate::keycode::KeyCode;

/// Rows the overlay is sent; it shows as many as fit.
const MAX_ROWS: usize = 12;
/// Outlines shown for the word that was typed exactly, since it may have several.
const MAX_OUTLINES: usize = 4;
/// Words that merely start with the query are only suggested from this length on: a scan
/// of the whole dictionary per key press isn't worth it for a single letter.
const MIN_PREFIX: usize = 2;

/// What a key press means inside a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKey {
    Char(char),
    Backspace,
    /// Esc or Enter: close the lookup
    Close,
    /// Any other key does nothing, and is still swallowed
    Ignore,
}

/// The meaning of `key` in a lookup, on a US QWERTY layout (the keys are physical positions,
/// so this is what the keycaps say). `shift` capitalizes letters: lookups are case-sensitive.
#[must_use]
pub fn classify(key: KeyCode, shift: bool) -> LookupKey {
    use KeyCode::*;
    let letter = |c: char| LookupKey::Char(if shift { c.to_ascii_uppercase() } else { c });
    match key {
        KC_ESC | KC_ENT => LookupKey::Close,
        KC_BSPC => LookupKey::Backspace,
        KC_SPC => LookupKey::Char(' '),
        KC_A => letter('a'),
        KC_B => letter('b'),
        KC_C => letter('c'),
        KC_D => letter('d'),
        KC_E => letter('e'),
        KC_F => letter('f'),
        KC_G => letter('g'),
        KC_H => letter('h'),
        KC_I => letter('i'),
        KC_J => letter('j'),
        KC_K => letter('k'),
        KC_L => letter('l'),
        KC_M => letter('m'),
        KC_N => letter('n'),
        KC_O => letter('o'),
        KC_P => letter('p'),
        KC_Q => letter('q'),
        KC_R => letter('r'),
        KC_S => letter('s'),
        KC_T => letter('t'),
        KC_U => letter('u'),
        KC_V => letter('v'),
        KC_W => letter('w'),
        KC_X => letter('x'),
        KC_Y => letter('y'),
        KC_Z => letter('z'),
        KC_1 | KC_2 | KC_3 | KC_4 | KC_5 | KC_6 | KC_7 | KC_8 | KC_9 | KC_0 if !shift => {
            LookupKey::Char(match key {
                KC_1 => '1',
                KC_2 => '2',
                KC_3 => '3',
                KC_4 => '4',
                KC_5 => '5',
                KC_6 => '6',
                KC_7 => '7',
                KC_8 => '8',
                KC_9 => '9',
                _ => '0',
            })
        }
        KC_MINS if !shift => LookupKey::Char('-'),
        KC_QUOT if !shift => LookupKey::Char('\''),
        KC_DOT if !shift => LookupKey::Char('.'),
        KC_COMM if !shift => LookupKey::Char(','),
        _ => LookupKey::Ignore,
    }
}

/// An open lookup: the text typed so far.
#[derive(Debug, Default)]
pub struct Lookup {
    query: String,
}

impl Lookup {
    /// Apply a key. Returns `false` when the lookup should close.
    pub fn apply(&mut self, key: LookupKey) -> bool {
        match key {
            LookupKey::Char(c) => self.query.push(c),
            LookupKey::Backspace => {
                self.query.pop();
            }
            LookupKey::Close => return false,
            LookupKey::Ignore => {}
        }
        true
    }

    /// What the overlay shows for the query: the word's own outlines first, then words that
    /// start with it, each with its best outline.
    ///
    /// `base` is a translator with nothing typed ([`Translator::fresh`]), over the dictionary
    /// the engine is using.
    #[must_use]
    pub fn view(&self, base: &Translator) -> LookupView {
        let query = self.query.trim();
        let mut rows = Vec::new();
        let mut hint = 0;
        let mut add = |text: &str, outline: &[u32], rows: &mut Vec<LookupRow>| {
            if rows.is_empty() {
                hint = outline.first().copied().unwrap_or(0);
            }
            rows.push(LookupRow {
                text: text.to_owned(),
                outline: outline
                    .iter()
                    .map(|&bits| render_stroke(bits))
                    .collect::<Vec<_>>()
                    .join("/"),
            });
        };

        if !query.is_empty() {
            for outline in proper_outlines_in(base, query).iter().take(MAX_OUTLINES) {
                add(query, outline, &mut rows);
            }
            if query.chars().count() >= MIN_PREFIX {
                // Ask for extra: a word with no outline that types it properly is skipped
                for word in base.dictionary().completions(query, MAX_ROWS * 2) {
                    if rows.len() >= MAX_ROWS {
                        break;
                    }
                    if let Some(outline) = proper_outlines_in(base, word).first() {
                        add(word, outline, &mut rows);
                    }
                }
            }
        }
        rows.truncate(MAX_ROWS);
        LookupView {
            query: self.query.clone(),
            rows,
            hint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::dict::Dictionary;
    use crate::steno::layout::parse_stroke;

    fn translator() -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{
                "-T": "the",
                "KAPB": "can",
                "KAPBD": "candy",
                "KAPBL": "candle",
                "KAPBT": "cant",
                "KAPB/TKEU": "candy",
                "KA": "ca",
                "SPH-PL": "{^ing}"
            }"#,
        )
        .unwrap();
        Translator::new(dict)
    }

    fn typed(query: &str) -> Lookup {
        let mut lookup = Lookup::default();
        for c in query.chars() {
            assert!(lookup.apply(LookupKey::Char(c)));
        }
        lookup
    }

    #[test]
    fn an_exact_word_lists_its_outlines_fewest_strokes_first_and_lights_the_best() {
        let view = typed("candy").view(&translator());
        assert_eq!(view.query, "candy");
        let outlines: Vec<_> = view.rows.iter().map(|r| r.outline.as_str()).collect();
        assert_eq!(outlines, ["KAPBD", "KAPB/TKEU"]);
        assert!(view.rows.iter().all(|row| row.text == "candy"));
        // The keyboard lights the first stroke of the best outline
        assert_eq!(view.hint, parse_stroke("KAPBD").unwrap());
    }

    #[test]
    fn words_that_start_with_the_query_follow_the_exact_match_shortest_first() {
        let view = typed("can").view(&translator());
        let rows: Vec<_> = view
            .rows
            .iter()
            .map(|r| (r.text.as_str(), r.outline.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("can", "KAPB"),
                ("cant", "KAPBT"),
                ("candy", "KAPBD"),
                ("candle", "KAPBL"),
            ]
        );
    }

    #[test]
    fn an_unknown_or_empty_query_has_no_rows_and_no_hint() {
        for query in ["", "qzx", "   "] {
            let view = typed(query).view(&translator());
            assert!(view.rows.is_empty(), "{query:?}");
            assert_eq!(view.hint, 0);
        }
    }

    #[test]
    fn one_letter_gives_no_suggestions_but_an_exact_word_still_does() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"-T": "the", "A": "a", "AB": "about"}"#).unwrap();
        let base = Translator::new(dict);
        let rows = typed("a").view(&base).rows;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outline, "A");
    }

    #[test]
    fn lookups_are_case_sensitive_and_shift_capitalizes() {
        assert_eq!(classify(KeyCode::KC_C, false), LookupKey::Char('c'));
        assert_eq!(classify(KeyCode::KC_C, true), LookupKey::Char('C'));
        let view = typed("Can").view(&translator());
        assert!(view.rows.is_empty(), "the dictionary has `can`, not `Can`");
    }

    #[test]
    fn keys_mean_what_their_keycaps_say() {
        assert_eq!(classify(KeyCode::KC_SPC, false), LookupKey::Char(' '));
        assert_eq!(classify(KeyCode::KC_5, false), LookupKey::Char('5'));
        assert_eq!(classify(KeyCode::KC_MINS, false), LookupKey::Char('-'));
        assert_eq!(classify(KeyCode::KC_QUOT, false), LookupKey::Char('\''));
        // Shifted digits and punctuation aren't words, and F-keys and arrows do nothing
        assert_eq!(classify(KeyCode::KC_5, true), LookupKey::Ignore);
        assert_eq!(classify(KeyCode::KC_F1, false), LookupKey::Ignore);
        assert_eq!(classify(KeyCode::KC_ESC, false), LookupKey::Close);
        assert_eq!(classify(KeyCode::KC_ENT, false), LookupKey::Close);
        assert_eq!(classify(KeyCode::KC_BSPC, false), LookupKey::Backspace);
    }

    #[test]
    fn backspace_edits_the_query_and_close_ends_it() {
        let mut lookup = typed("cat");
        assert!(lookup.apply(LookupKey::Backspace));
        assert_eq!(lookup.view(&translator()).query, "ca");
        // Backspacing past the start is harmless
        for _ in 0..5 {
            assert!(lookup.apply(LookupKey::Backspace));
        }
        assert_eq!(lookup.view(&translator()).query, "");
        assert!(!lookup.apply(LookupKey::Close));
    }
}
