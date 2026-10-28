//! Live lookup: "I'm mid-sentence and forgot the stroke for a word."
//!
//! A dictionary entry for `{PLOVER:LOOKUP}` opens a lookup. The steno layer stays on, but its
//! keys go back to what the layers below it make of them: every key goes through the usual
//! remaps, mod-taps and layers, and what comes out types into a query that is shown on the
//! overlay instead of reaching the application. The overlay lists the outlines that type the
//! word, best first, and lights the first stroke of the best one on its keyboard. Esc or Enter
//! closes it.

use super::commands::proper_outlines_in;
use super::layout::render_stroke;
use super::overlay::{LookupRow, LookupView};
use super::translate::Translator;
use super::typing::TypingLayout;
use crate::keycode::KeyCode;
use std::collections::HashSet;

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

/// Turns the keys that come out of the layers into what a lookup does with them.
///
/// These are the keys the application would have got (after remaps, mod-taps and layers), so a
/// symbol layer or a Shift on a thumb key work as they do everywhere else. Shift capitalizes:
/// lookups are case-sensitive.
#[derive(Debug, Clone, Default)]
pub struct LookupInput {
    layout: TypingLayout,
    shift: HashSet<KeyCode>,
    /// Ctrl, Alt or Super held: the next key is a shortcut, not text
    shortcut: HashSet<KeyCode>,
    caps: bool,
}

impl LookupInput {
    #[must_use]
    pub fn new(layout: TypingLayout) -> Self {
        Self {
            layout,
            ..Self::default()
        }
    }

    /// A key came out of the layers (`pressed` is false for its release). Returns what the
    /// lookup should do about it, if anything.
    pub fn key(&mut self, key: KeyCode, pressed: bool) -> Option<LookupKey> {
        if key.is_modifier() {
            let held = if matches!(key, KeyCode::KC_LSFT | KeyCode::KC_RSFT) {
                &mut self.shift
            } else {
                &mut self.shortcut
            };
            if pressed {
                held.insert(key);
            } else {
                held.remove(&key);
            }
            return None;
        }
        if !pressed {
            return None;
        }
        match key {
            KeyCode::KC_ESC | KeyCode::KC_ENT => Some(LookupKey::Close),
            KeyCode::KC_BSPC => Some(LookupKey::Backspace),
            KeyCode::KC_CAPS => {
                self.caps = !self.caps;
                None
            }
            _ if !self.shortcut.is_empty() => Some(LookupKey::Ignore),
            _ => Some(
                match self
                    .layout
                    .character(key, !self.shift.is_empty(), self.caps)
                {
                    Some('\t' | '\n') | None => LookupKey::Ignore,
                    Some(c) => LookupKey::Char(c),
                },
            ),
        }
    }

    /// Forget which modifiers are held, as a lookup opens.
    pub fn reset(&mut self) {
        self.shift.clear();
        self.shortcut.clear();
        self.caps = false;
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
        dict.merge_json(r#"{"-T": "the", "A": "a", "AB": "about"}"#)
            .unwrap();
        let base = Translator::new(dict);
        let rows = typed("a").view(&base).rows;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outline, "A");
    }

    fn press(input: &mut LookupInput, key: KeyCode) -> Option<LookupKey> {
        let result = input.key(key, true);
        input.key(key, false);
        result
    }

    #[test]
    fn lookups_are_case_sensitive_and_shift_capitalizes() {
        let mut input = LookupInput::default();
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('c')));
        input.key(KeyCode::KC_LSFT, true);
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('C')));
        input.key(KeyCode::KC_LSFT, false);
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('c')));
        let view = typed("Can").view(&translator());
        assert!(view.rows.is_empty(), "the dictionary has `can`, not `Can`");
    }

    #[test]
    fn keys_mean_what_the_layers_made_of_them() {
        let mut input = LookupInput::default();
        assert_eq!(
            press(&mut input, KeyCode::KC_SPC),
            Some(LookupKey::Char(' '))
        );
        assert_eq!(press(&mut input, KeyCode::KC_5), Some(LookupKey::Char('5')));
        assert_eq!(
            press(&mut input, KeyCode::KC_MINS),
            Some(LookupKey::Char('-'))
        );
        // Symbols come with Shift, as from a symbol layer, and so does Caps Lock for letters
        input.key(KeyCode::KC_RSFT, true);
        assert_eq!(press(&mut input, KeyCode::KC_5), Some(LookupKey::Char('%')));
        assert_eq!(
            press(&mut input, KeyCode::KC_QUOT),
            Some(LookupKey::Char('"'))
        );
        input.key(KeyCode::KC_RSFT, false);
        input.key(KeyCode::KC_CAPS, true);
        assert_eq!(press(&mut input, KeyCode::KC_A), Some(LookupKey::Char('A')));
        // F-keys, arrows and Tab do nothing; a shortcut isn't text; Esc and Enter close
        assert_eq!(press(&mut input, KeyCode::KC_F1), Some(LookupKey::Ignore));
        assert_eq!(press(&mut input, KeyCode::KC_TAB), Some(LookupKey::Ignore));
        input.key(KeyCode::KC_LCTL, true);
        assert_eq!(press(&mut input, KeyCode::KC_A), Some(LookupKey::Ignore));
        input.key(KeyCode::KC_LCTL, false);
        assert_eq!(press(&mut input, KeyCode::KC_ESC), Some(LookupKey::Close));
        assert_eq!(press(&mut input, KeyCode::KC_ENT), Some(LookupKey::Close));
        assert_eq!(
            press(&mut input, KeyCode::KC_BSPC),
            Some(LookupKey::Backspace)
        );
    }

    #[test]
    fn the_typing_layout_decides_the_letters() {
        let mut input = LookupInput::new(TypingLayout::Dvorak);
        assert_eq!(press(&mut input, KeyCode::KC_S), Some(LookupKey::Char('o')));
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
