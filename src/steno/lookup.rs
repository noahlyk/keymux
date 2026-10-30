//! Live lookup: "I'm mid-sentence and forgot the stroke for a word."
//!
//! A dictionary entry for `{PLOVER:LOOKUP}` opens a lookup. The steno layer stays on, but its
//! keys go back to what the layers below it make of them: every key goes through the usual
//! remaps, mod-taps and layers, and what comes out types into a query that is shown on the
//! overlay instead of reaching the application. The overlay lists the outlines that type the
//! word, best first, and lights the first stroke of the best one on its keyboard. Esc or Enter
//! closes it.

use super::commands::proper_outlines_in;
use super::layout::{parse_stroke, render_strokes};
use super::overlay::{LookupMode, LookupRow, LookupView};
use super::reverse::{self, Query};
use super::shortest::shortest_outline_labeled;
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

    /// What the overlay shows for the query.
    ///
    /// All-caps text with no lowercase letter that parses as steno notation (`KAT/-D`) resolves
    /// forward: one row, what it types. Anything else is a reverse lookup: a query with a space
    /// in it is a whole phrase, shown as one row, word by word, falling back to fingerspelling
    /// (marked in quotes) for a word with no entry — a single word keeps the fuller view this
    /// always had, its own outlines first then words that start with it, but now also falls back
    /// to fingerspelling when the dictionary has nothing at all, instead of showing nothing.
    ///
    /// `base` is a translator with nothing typed ([`Translator::fresh`]), over the dictionary
    /// the engine is using. See [`super::reverse`] for the engine this and the CLI's `keymux
    /// steno lookup` share.
    #[must_use]
    pub fn view(&self, base: &Translator) -> LookupView {
        let query = self.query.trim();
        if query.is_empty() {
            return LookupView {
                query: self.query.clone(),
                rows: Vec::new(),
                hint: 0,
                mode: LookupMode::Word,
            };
        }
        let (rows, mode) = match reverse::classify(query) {
            Query::Stroke(strokes) => (stroke_rows(base, &strokes), LookupMode::Stroke),
            Query::Text(text) if text.contains(char::is_whitespace) => {
                (vec![phrase_row(base, &text)], LookupMode::Phrase)
            }
            Query::Text(text) => (word_rows(base, &text), LookupMode::Word),
        };
        let hint = rows.first().map_or(0, |row: &LookupRow| {
            row.outline
                .split('/')
                .next()
                .and_then(parse_stroke)
                .unwrap_or(0)
        });
        LookupView {
            query: self.query.clone(),
            rows,
            hint,
            mode,
        }
    }
}

/// What a stroke sequence types, as a single row: outline is the notation itself, text is the
/// result in quotes (or a note that nothing matched).
fn stroke_row(base: &Translator, strokes: &[u32]) -> LookupRow {
    let mut probe = base.clone();
    for &bits in strokes {
        probe.stroke(bits);
    }
    let text = probe.text();
    LookupRow {
        text: if text.is_empty() {
            "(no translation)".to_string()
        } else {
            format!("\u{2192} {text:?}")
        },
        outline: render_strokes(strokes),
    }
}

/// Every row the raw-steno view shows: what the typed chord actually resolves to, first, then
/// the dictionary's other entries nearest to it - a chord a key or two off from a real entry
/// still shows what it was probably reaching for, instead of leaving it at "(no translation)".
fn stroke_rows(base: &Translator, strokes: &[u32]) -> Vec<LookupRow> {
    let mut rows = vec![stroke_row(base, strokes)];
    for outline in reverse::nearby_strokes(base.dictionary(), strokes, MAX_ROWS - 1) {
        rows.push(stroke_row(base, &outline));
    }
    rows
}

/// A whole phrase as a single row: the words (a fingerspelled one in quotes), and every stroke
/// that types them in order.
fn phrase_row(base: &Translator, phrase: &str) -> LookupRow {
    let segments = reverse::reverse_phrase(base, base.dictionary(), phrase);
    let (words, strokes) = reverse::display(&segments);
    LookupRow {
        text: words,
        outline: strokes,
    }
}

/// The existing single-word view: the word's own outlines first, then words that start with it,
/// each with its best outline — falling back to fingerspelling, marked in quotes, when the exact
/// query has no dictionary entry at all.
fn word_rows(base: &Translator, query: &str) -> Vec<LookupRow> {
    let mut rows = Vec::new();
    let exact = proper_outlines_in(base, query);
    if exact.is_empty() {
        if let Some(found) = shortest_outline_labeled(base.dictionary(), query) {
            rows.push(LookupRow {
                text: if found.fingerspelled {
                    format!("\"{query}\"")
                } else {
                    query.to_string()
                },
                outline: render_strokes(&found.strokes),
            });
        }
    } else {
        for outline in exact.iter().take(MAX_OUTLINES) {
            rows.push(LookupRow {
                text: query.to_string(),
                outline: render_strokes(outline),
            });
        }
    }
    if query.chars().count() >= MIN_PREFIX {
        // Ask for extra: a word with no outline that types it properly is skipped
        for word in base.dictionary().completions(query, MAX_ROWS * 2) {
            if rows.len() >= MAX_ROWS {
                break;
            }
            if let Some(outline) = proper_outlines_in(base, word).first() {
                rows.push(LookupRow {
                    text: word.to_string(),
                    outline: render_strokes(outline),
                });
            }
        }
    }
    rows.truncate(MAX_ROWS);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::dict::Dictionary;
    use crate::steno::layout::Layout;

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
            &Layout::plover(),
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
        assert_eq!(view.hint, Layout::plover().parse_stroke("KAPBD").unwrap());
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
        dict.merge_json(r#"{"-T": "the", "A": "a", "AB": "about"}"#, &Layout::plover())
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

    #[test]
    fn an_all_caps_query_resolves_the_stroke_forward() {
        let view = typed("KAPB").view(&translator());
        // The exact resolution always leads
        assert_eq!(view.rows[0].text, "\u{2192} \"can\"");
        assert_eq!(view.rows[0].outline, "KAPB");
        assert_eq!(view.hint, Layout::plover().parse_stroke("KAPB").unwrap());
        // Raw steno notation: the overlay labels this "Engine Resolver", not "Lookup"
        assert_eq!(view.mode, LookupMode::Stroke);
        // Every single-stroke entry in `translator()`'s dictionary besides "KAPB" itself and the
        // two-stroke "KAPB/TKEU" is close enough (they're all a handful of keys apart) to show
        // up as a "did you mean" row after it.
        let outlines: Vec<&str> = view.rows.iter().map(|r| r.outline.as_str()).collect();
        assert_eq!(outlines.len(), 7, "{outlines:?}");
        for expected in ["-T", "KAPBD", "KAPBL", "KAPBT", "KA", "SPH-PL"] {
            assert!(outlines.contains(&expected), "expected {expected} in {outlines:?}");
        }
    }

    #[test]
    fn a_query_with_a_space_looks_up_the_whole_phrase_as_one_row() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAPB": "can", "-T": "the", "KAPBD": "candy"}"#, &Layout::plover())
            .unwrap();
        let view = typed("can the candy").view(&Translator::new(dict));
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].text, "can the candy");
        assert_eq!(view.rows[0].outline, "KAPB / -T / KAPBD");
        // A whole phrase: the overlay labels this "Translator", not "Lookup"
        assert_eq!(view.mode, LookupMode::Phrase);
    }

    #[test]
    fn a_single_word_query_and_an_empty_one_are_the_plain_lookup_mode() {
        assert_eq!(typed("candy").view(&translator()).mode, LookupMode::Word);
        assert_eq!(typed("").view(&translator()).mode, LookupMode::Word);
    }

    #[test]
    fn a_word_with_no_dictionary_entry_falls_back_to_fingerspelling_in_quotes() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"TPH": "{&n}", "SR*": "{&v}", "SKW*": "{&i}", "PH*": "{&m}"}"#, &Layout::plover())
            .unwrap();
        let view = typed("nvim").view(&Translator::new(dict));
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].text, "\"nvim\"");
        assert_eq!(view.rows[0].outline, "TPH/SR*/SKW*/PH*");
    }
}
