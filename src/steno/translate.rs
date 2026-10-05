//! Turns finished strokes into text, the way Plover does.
//!
//! Every stroke is translated as soon as it completes. If the strokes so far
//! also form a longer dictionary entry, that entry replaces the earlier output
//! (Plover's retroactive correction), so there is no timing guess to get wrong.
//!
//! All output lives in one buffer. Each translation records the text it
//! replaced, so the bare `*` stroke can undo any step exactly, including
//! corrections that reached back into earlier words.

use super::dict::Dictionary;
use super::format::{capitalize_word, render, FormatState, Piece};
use super::numbers::number_text;
use super::orthography;
use std::sync::Arc;
use tracing::info;

/// The stroke that undoes the last translation: `*` on its own.
pub const UNDO_STROKE: u32 = 1 << 9;

/// How many translations stay undoable. Older output is never touched again.
const MAX_HISTORY: usize = 4096;

/// What the caller should do with the keyboard after a stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StenoOutput {
    /// Type this text
    Type(String),
    /// Delete `backspaces` characters, then type `text`
    Retype { backspaces: usize, text: String },
}

#[derive(Debug, Clone)]
struct Emitted {
    /// Every stroke this translation covers, including any it replaced
    strokes: Vec<u32>,
    /// Byte offset in the output buffer where this translation's text starts
    start: usize,
    /// The output text this translation replaced, restored on undo
    replaced: String,
    state_before: FormatState,
    /// Earlier translations this one replaced. Restored on undo, in order.
    consumed: Vec<Emitted>,
}

#[derive(Debug, Clone)]
pub struct Translator {
    /// Shared, so a copy of the translator (see `keys`) doesn't copy the dictionary
    dict: Arc<Dictionary>,
    /// Everything typed that can still be retyped, most recent last
    output: String,
    history: Vec<Emitted>,
    state: FormatState,
    unmatched: bool,
}

impl Translator {
    #[must_use]
    pub fn new(dict: Dictionary) -> Self {
        Self {
            dict: Arc::new(dict),
            output: String::new(),
            history: Vec::new(),
            state: FormatState::default(),
            unmatched: false,
        }
    }

    /// Forget what was typed, as when a steno layer is entered. Text typed before
    /// then is not retyped or undone, and the next word starts with no leading space.
    pub fn start_fresh(&mut self) {
        self.output.clear();
        self.history.clear();
        self.state = FormatState::default();
        self.unmatched = false;
    }

    /// Swap in dictionaries that finished loading in the background.
    pub fn set_dictionary(&mut self, dict: Dictionary) {
        self.dict = Arc::new(dict);
    }

    /// Everything typed so far that can still be retyped.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.output
    }

    /// The user pressed Backspace, which deleted the last character on screen.
    /// Keep the buffer in step, so a later `*` doesn't delete text already gone.
    pub fn backspace(&mut self) {
        self.output.pop();
        let len = self.output.len();
        while self.history.last().is_some_and(|entry| entry.start >= len) {
            self.history.pop();
        }
        self.state = FormatState {
            has_text: !self.output.is_empty(),
            ..FormatState::default()
        };
    }

    /// Whether any dictionary entries loaded. False means steno isn't set up yet.
    #[must_use]
    pub fn has_dictionary(&self) -> bool {
        !self.dict.is_empty()
    }

    /// Whether the last stroke matched nothing. The stroke is still recorded so
    /// the tape can show it.
    #[must_use]
    pub const fn was_unmatched(&self) -> bool {
        self.unmatched
    }

    /// A stroke is complete.
    pub fn stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        self.unmatched = false;
        if stroke == UNDO_STROKE {
            return self.undo();
        }
        match self.best_match(stroke) {
            Some((consume, pieces)) => self.apply(stroke, consume, &pieces),
            None => {
                self.unmatched = true;
                info!("steno stroke {stroke:#x} not in dictionary");
                // Recorded with no text, so strokes on either side can't combine across it
                let start = self.output.len();
                self.record(vec![stroke], start, String::new(), Vec::new(), self.state);
                None
            }
        }
    }

    /// The longest dictionary entry that ends with `stroke`, and how many earlier
    /// translations it replaces. Falls back to number strokes.
    fn best_match(&self, stroke: u32) -> Option<(usize, Vec<Piece>)> {
        let mut best = self.dict.get(&[stroke]).map(|pieces| (0, pieces.to_vec()));
        let mut joined = vec![stroke];
        for (k, entry) in self.history.iter().rev().enumerate() {
            joined.splice(0..0, entry.strokes.iter().copied());
            if joined.len() > self.dict.max_len() {
                break;
            }
            if let Some(pieces) = self.dict.get(&joined) {
                best = Some((k + 1, pieces.to_vec()));
            }
        }
        best.or_else(|| number_text(stroke).map(|digits| (0, vec![Piece::Text(digits)])))
    }

    /// Translate `stroke` with `pieces`, replacing the last `consume` translations
    /// they cover. Those are put back to their pre-translation text first, so
    /// the new entry always lands on a clean base.
    fn apply(&mut self, stroke: u32, consume: usize, pieces: &[Piece]) -> Option<StenoOutput> {
        let before = self.output.clone();
        let split = self.history.len() - consume;
        let consumed = self.history.split_off(split);
        let strokes: Vec<u32> = consumed
            .iter()
            .flat_map(|entry| entry.strokes.iter().copied())
            .chain([stroke])
            .collect();
        let state_before = consumed
            .first()
            .map_or(self.state, |entry| entry.state_before);

        if let Some(first) = consumed.first() {
            self.output.truncate(first.start);
            self.output.push_str(&first.replaced);
        }
        let base = self.output.len();
        let (start, edit, used) = edit_before(&self.output[..base], pieces);
        let mut state = state_before;
        let mut text = edit;
        if used > 0 {
            // The edit left a word just typed in front of the rest of the entry
            state.has_text = true;
            state.suppress_space = false;
            state.capitalize = false;
            state.last_fingerspell = false;
        }
        text.push_str(&render(&pieces[used..], &mut state));
        self.output.truncate(start);
        self.output.push_str(&text);
        self.state = state;

        // Undo must restore everything from here on, including any earlier word
        // the edit re-spelled and any translations this one replaced
        let undo_start = consumed
            .first()
            .map_or(start, |entry| entry.start)
            .min(start);
        let screen_old = before[undo_start..].to_string();
        let screen_new = self.output[undo_start..].to_string();
        self.record(
            strokes,
            undo_start,
            screen_old.clone(),
            consumed,
            state_before,
        );
        diff(&screen_old, &screen_new)
    }

    fn record(
        &mut self,
        strokes: Vec<u32>,
        start: usize,
        replaced: String,
        consumed: Vec<Emitted>,
        state_before: FormatState,
    ) {
        self.history.push(Emitted {
            strokes,
            start,
            replaced,
            state_before,
            consumed,
        });
        self.trim_history();
    }

    /// Forget the oldest translations so the buffer stays small. Those can no
    /// longer be undone.
    fn trim_history(&mut self) {
        if self.history.len() <= MAX_HISTORY {
            return;
        }
        let excess = self.history.len() - MAX_HISTORY;
        let cut = self.history[excess].start;
        self.history.drain(..excess);
        self.output.drain(..cut);
        for entry in &mut self.history {
            shift(entry, cut);
        }
    }

    fn undo(&mut self) -> Option<StenoOutput> {
        loop {
            let entry = self.history.pop()?;
            let removed = self.output[entry.start..].to_string();
            self.output.truncate(entry.start);
            self.output.push_str(&entry.replaced);
            self.state = entry.state_before;
            self.history.extend(entry.consumed);
            // Barriers for unmatched strokes have nothing to undo; skip past them
            if removed.is_empty() && entry.replaced.is_empty() {
                continue;
            }
            info!("steno undo ({} characters)", removed.chars().count());
            return Some(StenoOutput::Retype {
                backspaces: removed.chars().count(),
                text: entry.replaced,
            });
        }
    }
}

/// Where a dictionary entry's text starts, and what replaces the output from there.
///
/// Returns `(start, edit, used)`: `output[start..]` is replaced by `edit`, and the
/// first `used` pieces are already applied. Suffixes get orthography, and retro
/// commands edit the word or space before them.
fn edit_before(prefix: &str, pieces: &[Piece]) -> (usize, String, usize) {
    let base = prefix.len();
    match pieces.first() {
        Some(Piece::AttachText(suffix)) => {
            let word = trailing_letters(prefix);
            if word.is_empty() {
                return (base, String::new(), 0);
            }
            let start = base - word.len();
            (start, orthography::attach(word, suffix), 1)
        }
        Some(Piece::RetroCapitalize) => {
            let word = trailing_letters(prefix);
            if word.is_empty() {
                return (base, String::new(), 1);
            }
            (base - word.len(), capitalize_word(word), 1)
        }
        Some(Piece::RetroDeleteSpace) => {
            if prefix.ends_with(' ') {
                (base - 1, String::new(), 1)
            } else {
                (base, String::new(), 1)
            }
        }
        Some(Piece::RetroInsertSpace) => {
            if prefix.is_empty() || prefix.ends_with(' ') {
                (base, String::new(), 1)
            } else {
                (base, " ".to_string(), 1)
            }
        }
        _ => (base, String::new(), 0),
    }
}

/// The alphabetic characters at the end of `text`, if any.
fn trailing_letters(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphabetic())
        .last()
        .map_or(text.len(), |(index, _)| index);
    &text[start..]
}

/// The change from `old` to `new`, as backspaces plus new text. The part both
/// strings share stays on screen.
fn diff(old: &str, new: &str) -> Option<StenoOutput> {
    let shared = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    let backspaces = old[shared..].chars().count();
    let insert = &new[shared..];
    match (backspaces, insert.is_empty()) {
        (0, true) => None,
        (0, false) => Some(StenoOutput::Type(insert.to_string())),
        _ => Some(StenoOutput::Retype {
            backspaces,
            text: insert.to_string(),
        }),
    }
}

/// Move every offset in `entry` (and what it replaced) back by `cut` bytes.
fn shift(entry: &mut Emitted, cut: usize) {
    entry.start = entry.start.saturating_sub(cut);
    for earlier in &mut entry.consumed {
        shift(earlier, cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn stroke(text: &str) -> u32 {
        parse_stroke(text).unwrap()
    }

    fn translator(json: &str) -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(json).unwrap();
        Translator::new(dict)
    }

    #[test]
    fn single_strokes_type_words_with_spacing() {
        let mut t = translator(r#"{"KAT": "cat", "-PB": "and"}"#);
        assert_eq!(
            t.stroke(stroke("KAT")),
            Some(StenoOutput::Type("cat".into()))
        );
        assert_eq!(
            t.stroke(stroke("-PB")),
            Some(StenoOutput::Type(" and".into()))
        );
    }

    #[test]
    fn multi_stroke_entry_replaces_its_first_stroke() {
        let mut t = translator(r#"{"KPA/TKAOEU": "example"}"#);
        // KPA has no entry alone, so nothing types yet
        assert_eq!(t.stroke(stroke("KPA")), None);
        assert!(t.was_unmatched());
        assert_eq!(
            t.stroke(stroke("TKAOEU")),
            Some(StenoOutput::Type("example".into()))
        );
    }

    #[test]
    fn a_longer_match_corrects_text_already_typed() {
        let mut t = translator(r#"{"KPA": "ex", "KPA/TKAOEU": "example"}"#);
        assert_eq!(
            t.stroke(stroke("KPA")),
            Some(StenoOutput::Type("ex".into()))
        );
        // "example" extends "ex" by appending, so only the rest is typed
        assert_eq!(
            t.stroke(stroke("TKAOEU")),
            Some(StenoOutput::Type("ample".into()))
        );
    }

    #[test]
    fn a_longer_match_can_rewrite_earlier_text() {
        let mut t = translator(r#"{"KAR": "carry", "-S": "{^s}"}"#);
        assert_eq!(
            t.stroke(stroke("KAR")),
            Some(StenoOutput::Type("carry".into()))
        );
        // carry + s = carries: the y is replaced
        assert_eq!(
            t.stroke(stroke("-S")),
            Some(StenoOutput::Retype {
                backspaces: 1,
                text: "ies".into(),
            })
        );
    }

    #[test]
    fn plain_suffix_attaches_without_retyping() {
        let mut t = translator(r#"{"KAT": "cat", "-S": "{^s}"}"#);
        t.stroke(stroke("KAT"));
        assert_eq!(t.stroke(stroke("-S")), Some(StenoOutput::Type("s".into())));
    }

    #[test]
    fn a_stroke_that_does_not_extend_the_prefix_is_translated_alone() {
        let mut t = translator(r#"{"KPA/TKAOEU": "example", "KAT": "cat"}"#);
        assert_eq!(t.stroke(stroke("KPA")), None);
        assert_eq!(
            t.stroke(stroke("KAT")),
            Some(StenoOutput::Type("cat".into()))
        );
    }

    #[test]
    fn undo_removes_the_last_translation_and_restores_spacing() {
        let mut t = translator(r#"{"KAT": "cat", "-PB": "and"}"#);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("-PB"));
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 4,
                text: String::new(),
            })
        );
        assert_eq!(
            t.stroke(stroke("-PB")),
            Some(StenoOutput::Type(" and".into()))
        );
    }

    #[test]
    fn undo_restores_text_a_correction_replaced() {
        let mut t = translator(r#"{"KAR": "carry", "-S": "{^s}"}"#);
        t.stroke(stroke("KAR"));
        t.stroke(stroke("-S"));
        // Undo the correction: "carries" (7 characters) goes back to "carry"
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 7,
                text: "carry".into(),
            })
        );
    }

    #[test]
    fn undo_with_nothing_to_undo_does_nothing() {
        let mut t = translator("{}");
        assert_eq!(t.stroke(UNDO_STROKE), None);
    }

    #[test]
    fn number_strokes_type_digits_without_an_entry() {
        let mut t = translator("{}");
        assert_eq!(
            t.stroke(stroke("#STPH")),
            Some(StenoOutput::Type("1234".into()))
        );
    }

    /// Apply a translator output to the text it has typed so far.
    fn apply(typed: &mut String, output: Option<StenoOutput>) {
        match output {
            None => {}
            Some(StenoOutput::Type(text)) => typed.push_str(&text),
            Some(StenoOutput::Retype { backspaces, text }) => {
                for _ in 0..backspaces {
                    typed.pop();
                }
                typed.push_str(&text);
            }
        }
    }

    #[test]
    fn retro_correction_keeps_words_before_a_suffix_it_re_spelled() {
        let mut t = translator(
            r#"{"KAT": "cat", "SKP": "and", "SKWRUPL": "{^um}", "SKWRUPL/-PD": "jumped"}"#,
        );
        let mut typed = String::new();
        for stroke_text in ["KAT", "SKP", "SKWRUPL", "-PD"] {
            apply(&mut typed, t.stroke(stroke(stroke_text)));
        }
        assert_eq!(typed, "cat and jumped");
        // Undo puts back "cat andum": "and jumped" (10 characters) becomes "andum"
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 10,
                text: "andum".into(),
            })
        );
    }

    #[test]
    #[ignore = "needs the Plover dictionary from `keymux steno setup`; run with --ignored"]
    fn real_sentence_through_plover_dictionary() {
        let home = std::env::var("HOME").unwrap();
        let path = std::path::Path::new(&home).join(".config/keymux/steno/main.json");
        let mut t = Translator::new(Dictionary::load(&[path]));
        let mut typed = String::new();
        // "cat and jumped": SKWRUPL types "um" as a suffix, and -PD then rewrites it as "jumped"
        for stroke_text in ["KAT", "SKP", "SKWRUPL", "-PD"] {
            apply(&mut typed, t.stroke(stroke(stroke_text)));
        }
        assert_eq!(typed, "cat and jumped");
    }

    #[test]
    fn retro_commands_edit_the_previous_word() {
        let mut t = translator(r#"{"KAT": "cat", "-PB": "{*<}and"}"#);
        t.stroke(stroke("KAT"));
        // "cat" becomes "Cat" and " and" follows it
        assert_eq!(
            t.stroke(stroke("-PB")),
            Some(StenoOutput::Retype {
                backspaces: 3,
                text: "Cat and".into(),
            })
        );
    }
}
