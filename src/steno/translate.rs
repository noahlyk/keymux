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
use super::format::{capitalize_word, lowercase_word, render, FormatState, KeyPress, Piece};
use super::numbers::{number_text, NUMBER_BIT};
use super::orthography;
use crate::keycode::KeyCode;
use std::sync::Arc;
use tracing::info;

/// The stroke that undoes the last translation: `*` on its own.
pub const UNDO_STROKE: u32 = 1 << 9;

/// How many translations stay undoable. Older output is never touched again.
const MAX_HISTORY: usize = 4096;

/// Typed for a stroke that is incomplete, so it shows the stroke registered. The next
/// stroke replaces it or takes it back. ASCII only, since keyboard output can't type more.
const PENDING_MARK: &str = "_";

/// What the caller should do with the keyboard after a stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StenoOutput {
    /// Type this text
    Type(String),
    /// Delete `backspaces` characters, then type `text`
    Retype { backspaces: usize, text: String },
    /// Press these keys in order, such as cursor movement
    Keys(Vec<KeyPress>),
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
    /// Spacing and capitalization after this translation, so restoring it restores them
    state_after: FormatState,
    /// Earlier translations this one replaced. Restored on undo, in order.
    consumed: Vec<Emitted>,
    /// A key press rather than text. Undo consumes it without retyping anything.
    presses: bool,
    /// The placeholder for an incomplete stroke, see [`PENDING_MARK`]
    pending: bool,
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

    /// A translator over the same dictionary with nothing typed. Cheap: the dictionary is
    /// shared, not copied. Lookups use this to try strokes without touching real typing.
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self {
            dict: Arc::clone(&self.dict),
            output: String::new(),
            history: Vec::new(),
            state: FormatState::default(),
            unmatched: false,
        }
    }

    /// The dictionary this translator reads.
    #[must_use]
    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// Everything typed so far that can still be retyped.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.output
    }

    /// Nothing typed on this layer is left to undo.
    #[must_use]
    pub fn tape_is_empty(&self) -> bool {
        self.history.is_empty()
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

    /// The user typed a space on the steno layer. The space goes into the buffer as
    /// its own step, so the next word doesn't add another one. `*` takes it back.
    pub fn space(&mut self) -> StenoOutput {
        // The stroke this placeholder stood for never finished, so it is taken back first
        let removed = self.drop_pending();
        let start = self.output.len();
        self.output.push(' ');
        let state_before = self.state;
        self.state = FormatState::default();
        self.record(
            Vec::new(),
            start,
            String::new(),
            Vec::new(),
            state_before,
            self.state,
        );
        if removed == 0 {
            StenoOutput::Type(" ".to_string())
        } else {
            StenoOutput::Retype {
                backspaces: removed,
                text: " ".to_string(),
            }
        }
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
        // `#` on its own repeats the last stroke, as in Lapwing's `#TPH*R/#/#/#`. It is also
        // the start of proper-noun outlines such as `#/KAL/TKPWAR`, so it repeats only when
        // nothing in the dictionary starts with it.
        if stroke == NUMBER_BIT && self.best_match(stroke).is_none() {
            return self.repeat_last();
        }
        // What is on screen before this stroke, placeholder included
        let screen = self.output.clone();
        // A placeholder is replaced only by a stroke that joins it. Any other stroke takes it back first.
        let pending = self.history.last().is_some_and(|entry| entry.pending);
        let joins = pending && matches!(self.best_match(stroke), Some((consume, _)) if consume > 0);
        let removed = if joins { 0 } else { self.drop_pending() };
        let output = self.place(stroke);
        if removed == 0 {
            return output;
        }
        match output {
            Some(StenoOutput::Keys(presses)) => {
                // The placeholder is on screen, so delete it before the keys are pressed
                let backspaces = (0..removed).map(|_| KeyPress {
                    modifiers: Vec::new(),
                    key: KeyCode::KC_BSPC,
                });
                Some(StenoOutput::Keys(backspaces.chain(presses).collect()))
            }
            _ => diff(&screen, &self.output),
        }
    }

    /// Translate a stroke that is neither undo nor a repeat. See [`Translator::stroke`].
    fn place(&mut self, stroke: u32) -> Option<StenoOutput> {
        // A retro space command acts between two strokes, which a joined entry hides, so
        // split that entry first. The change is reported against the screen before the split.
        let screen = self.output.clone();
        let split = matches!(
            self.best_match(stroke),
            Some((0, ref pieces)) if is_retro_space(pieces)
        ) && self.split_last();
        match self.best_match(stroke) {
            Some((consume, pieces)) if split => {
                self.apply(stroke, consume, &pieces);
                diff(&screen, &self.output)
            }
            Some((consume, pieces)) => self.apply(stroke, consume, &pieces),
            None if self.dict.opens_longer_entry(stroke) => self.hold(stroke),
            None => {
                self.unmatched = true;
                info!("steno stroke {stroke:#x} not in dictionary");
                // Recorded with no text, so strokes on either side can't combine across it
                let start = self.output.len();
                self.record(
                    vec![stroke],
                    start,
                    String::new(),
                    Vec::new(),
                    self.state,
                    self.state,
                );
                None
            }
        }
    }

    /// A stroke that matches nothing alone, but starts a longer entry, so it is incomplete.
    /// Types a placeholder in its place, which the next stroke replaces or takes back.
    fn hold(&mut self, stroke: u32) -> Option<StenoOutput> {
        let start = self.output.len();
        let state_before = self.state;
        let mut state = self.state;
        // Spaced like a word, so it doesn't run into the word before it
        let text = render(&[Piece::Text(PENDING_MARK.to_string())], &mut state);
        self.output.push_str(&text);
        self.state = state;
        self.record(
            vec![stroke],
            start,
            String::new(),
            Vec::new(),
            state_before,
            self.state,
        );
        if let Some(entry) = self.history.last_mut() {
            entry.pending = true;
        }
        Some(StenoOutput::Type(text))
    }

    /// Take back the placeholder for an incomplete stroke, if it is the last thing typed.
    /// Returns how many characters were taken back, so the caller can delete them.
    fn drop_pending(&mut self) -> usize {
        let Some(entry) = self.history.last().filter(|entry| entry.pending) else {
            return 0;
        };
        let (start, state) = (entry.start, entry.state_before);
        let removed = self.output[start..].chars().count();
        self.history.pop();
        self.output.truncate(start);
        self.state = state;
        removed
    }

    /// The longest dictionary entry that ends with `stroke`, and how many earlier
    /// translations it replaces. Falls back to number strokes.
    fn best_match(&self, stroke: u32) -> Option<(usize, Vec<Piece>)> {
        let mut best = self.dict.get(&[stroke]).map(|pieces| (0, pieces.to_vec()));
        let mut joined = vec![stroke];
        for (k, entry) in self.history.iter().rev().enumerate() {
            // A typed space is a step of its own. No entry can span it
            if entry.strokes.is_empty() {
                break;
            }
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
        // The spacing and capitalization to start from. Entries that only rewrite text
        // below don't change where this word starts, so only `consumed` decides it.
        let state_before = consumed
            .first()
            .map_or(self.state, |entry| earliest_translation(entry).state_before);
        // Restore to before the earliest translation covered, which may sit inside an
        // earlier join: `replaced` there is what that join replaced, not what came before it
        if let Some(first) = consumed.first().map(earliest_translation) {
            self.output.truncate(first.start);
            self.output.push_str(&first.replaced);
        }
        let base = self.output.len();
        let (start, edit, used) = edit_before(&self.output[..base], pieces);
        // Earlier translations whose text this edit rewrites are covered by this one now.
        // Keeping them (rather than dropping them) means their strokes can still join and
        // undo still restores them.
        let mut covered = Vec::new();
        // Where the text of the entry being looked at ends: the end of the text, then the start
        // of the entry after it
        let mut end = self.output.len();
        while let Some(entry) = self.history.last() {
            if entry.start < start {
                break;
            }
            // An entry that typed nothing and sits exactly where the edit begins, such as a
            // stroke that matched nothing, has no text for the edit to rewrite. Covering it
            // would fold its stroke into this translation's strokes, so a later stroke could
            // never combine with this one (`HRAOEUP`, `PER`, `TPEBGT` would give `per effect`,
            // not `perfect`): the lookup would be for HRAOEUP/PER/TPEBGT.
            if entry.start == start && entry.start == end {
                break;
            }
            end = entry.start;
            covered.extend(self.history.pop());
        }
        covered.reverse();
        covered.extend(consumed);

        let earliest = covered.first().map(earliest_translation);
        let undo_start = earliest.map_or(start, |entry| entry.start).min(start);
        let strokes: Vec<u32> = covered
            .iter()
            .flat_map(|entry| entry.strokes.iter().copied())
            .chain([stroke])
            .collect();

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
        let screen_old = before[undo_start..].to_string();
        let screen_new = self.output[undo_start..].to_string();
        let text_change = diff(&screen_old, &screen_new);
        self.record(
            strokes,
            undo_start,
            screen_old,
            covered,
            state_before,
            self.state,
        );
        let keys = keys_of(pieces);
        if keys.is_empty() {
            return text_change;
        }
        if let Some(entry) = self.history.last_mut() {
            entry.presses = true;
        }
        if text_change.is_some() {
            // Keys and text in one outline have no single output, so the text wins
            info!("steno outline mixes key presses and text; the key presses are dropped");
            return text_change;
        }
        Some(StenoOutput::Keys(keys))
    }

    fn record(
        &mut self,
        strokes: Vec<u32>,
        start: usize,
        replaced: String,
        consumed: Vec<Emitted>,
        state_before: FormatState,
        state_after: FormatState,
    ) {
        self.history.push(Emitted {
            strokes,
            start,
            replaced,
            state_before,
            state_after,
            consumed,
            presses: false,
            pending: false,
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
            let (removed, replaced, presses) = self.pop_translation()?;
            // A key press has no text to take back, so undo ends here without touching the screen
            if presses {
                return None;
            }
            // Barriers for unmatched strokes have nothing to undo; skip past them
            if removed.is_empty() && replaced.is_empty() {
                continue;
            }
            info!("steno undo ({} characters)", removed.chars().count());
            return Some(StenoOutput::Retype {
                backspaces: removed.chars().count(),
                text: replaced,
            });
        }
    }

    /// Remove the most recent translation from the buffer and history, restoring what
    /// it replaced and what it consumed. Returns the text it removed and the text it
    /// restored.
    fn pop_translation(&mut self) -> Option<(String, String, bool)> {
        let entry = self.history.pop()?;
        let removed = self.output[entry.start..].to_string();
        self.output.truncate(entry.start);
        self.output.push_str(&entry.replaced);
        // The text is back to what the last restored translation left, so its spacing is too
        self.state = entry
            .consumed
            .last()
            .map_or(entry.state_before, |earlier| earlier.state_after);
        self.history.extend(entry.consumed);
        Some((removed, entry.replaced, entry.presses))
    }

    /// Repeat the last stroke that wasn't itself a repeat. The repeat is recorded as a `#`
    /// stroke, so a later stroke can still join onto it, as in `#/KAL/TKPWAR`.
    fn repeat_last(&mut self) -> Option<StenoOutput> {
        let last = self
            .history
            .iter()
            .rev()
            .flat_map(|entry| entry.strokes.iter().rev())
            .copied()
            .find(|&s| s != NUMBER_BIT);
        let output = match last {
            Some(last) => self.stroke(last),
            None => {
                let start = self.output.len();
                self.record(
                    vec![NUMBER_BIT],
                    start,
                    String::new(),
                    Vec::new(),
                    self.state,
                    self.state,
                );
                None
            }
        };
        // The repeated stroke becomes `#`. Strokes that were merged into this entry stay.
        if let Some(entry) = self.history.last_mut() {
            if let Some(last) = entry.strokes.last_mut() {
                *last = NUMBER_BIT;
            }
        }
        output
    }

    /// Re-translate the last multi-stroke entry one stroke at a time, so a retro space
    /// command can act at the boundary inside it. Returns false if there was nothing to
    /// split.
    fn split_last(&mut self) -> bool {
        let Some(entry) = self.history.last() else {
            return false;
        };
        if entry.strokes.len() < 2 {
            return false;
        }
        let stroke = *entry.strokes.last().unwrap_or(&0);
        self.pop_translation();
        // The stroke alone: no joining with what came before, so the boundary stays
        match self.dict.get(&[stroke]) {
            Some(pieces) => {
                let pieces = pieces.to_vec();
                self.apply(stroke, 0, &pieces);
            }
            None => {
                let start = self.output.len();
                self.record(
                    vec![stroke],
                    start,
                    String::new(),
                    Vec::new(),
                    self.state,
                    self.state,
                );
            }
        }
        true
    }
}

/// The first translation an entry covers, looking through earlier joins.
fn earliest_translation(entry: &Emitted) -> &Emitted {
    entry.consumed.first().map_or(entry, earliest_translation)
}

/// The key presses in an outline, in order.
fn keys_of(pieces: &[Piece]) -> Vec<KeyPress> {
    pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Key(press) => Some(press.clone()),
            _ => None,
        })
        .collect()
}

/// Whether a translation is one of the retro space commands (`{*?}` and `{*!}`).
fn is_retro_space(pieces: &[Piece]) -> bool {
    matches!(
        pieces.first(),
        Some(Piece::RetroInsertSpace | Piece::RetroDeleteSpace)
    )
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
        Some(Piece::RetroLowercase) => {
            let word = trailing_letters(prefix);
            if word.is_empty() {
                return (base, String::new(), 1);
            }
            (base - word.len(), lowercase_word(word), 1)
        }
        Some(Piece::RetroDeleteSpace) => {
            // A trailing space is the one just typed (`space()`), so it goes
            if prefix.ends_with(' ') {
                return (base - 1, String::new(), 1);
            }
            // Otherwise drop the space between the last two words
            let word = trailing_letters(prefix);
            let word_start = base - word.len();
            if !word.is_empty() && prefix[..word_start].ends_with(' ') {
                (word_start - 1, word.to_string(), 1)
            } else {
                (base, String::new(), 1)
            }
        }
        Some(Piece::RetroInsertSpace) => {
            // Put a space between the last word and the one before it, if there is no
            // space there already and a word before it exists
            let word = trailing_letters(prefix);
            let word_start = base - word.len();
            if word.is_empty() || word_start == 0 || prefix[..word_start].ends_with(' ') {
                (base, String::new(), 1)
            } else {
                (word_start, format!(" {word}"), 1)
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
    use crate::steno::format::parse_pieces;
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
    fn fingerspelled_letters_type_a_word() {
        let mut t = translator(r#"{"TPH": "{&n}", "SRA": "{&v}", "EU": "{&i}", "PL": "{&m}"}"#);
        let mut typed = String::new();
        for chord in ["TPH", "SRA", "EU", "PL"] {
            apply(&mut typed, t.stroke(stroke(chord)));
        }
        assert_eq!(typed, "nvim");
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
        // KPA has no entry alone, but starts one, so a placeholder shows it registered
        assert_eq!(t.stroke(stroke("KPA")), Some(StenoOutput::Type("_".into())));
        assert!(!t.was_unmatched());
        // The placeholder is the first thing on screen, so the entry replaces it
        assert_eq!(
            t.stroke(stroke("TKAOEU")),
            Some(StenoOutput::Retype {
                backspaces: 1,
                text: "example".into(),
            })
        );
    }

    #[test]
    fn an_incomplete_stroke_shows_a_placeholder_until_the_next_one_joins_it() {
        let json = r#"{"KPA/TKAOEU": "example"}"#;
        assert_eq!(type_strokes(json, &["KPA"]), "_");
        assert_eq!(type_strokes(json, &["KPA", "TKAOEU"]), "example");
    }

    #[test]
    fn a_stroke_that_stands_alone_takes_back_the_placeholder() {
        let json = r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#;
        assert_eq!(type_strokes(json, &["KPA", "KAT"]), "cat");
    }

    #[test]
    fn a_placeholder_after_a_word_is_spaced_and_taken_back_cleanly() {
        let json = r#"{"KAT": "cat", "KPA/TKAOEU": "example", "SKP": "and"}"#;
        assert_eq!(type_strokes(json, &["KAT", "KPA"]), "cat _");
        assert_eq!(type_strokes(json, &["KAT", "KPA", "SKP"]), "cat and");
    }

    #[test]
    fn a_space_takes_back_an_incomplete_stroke() {
        let mut t = translator(r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#);
        let mut typed = String::new();
        for chord in ["KAT", "KPA"] {
            apply(&mut typed, t.stroke(stroke(chord)));
        }
        assert_eq!(typed, "cat _");
        let space = t.space();
        assert_eq!(
            space,
            StenoOutput::Retype {
                backspaces: 2,
                text: " ".into(),
            }
        );
        apply(&mut typed, Some(space));
        assert_eq!(typed, "cat ");
    }

    #[test]
    fn undo_takes_back_an_incomplete_stroke() {
        let mut t = translator(r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("KPA"));
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 2,
                text: String::new(),
            })
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
        assert_eq!(t.stroke(stroke("KPA")), Some(StenoOutput::Type("_".into())));
        // KAT takes the placeholder's place rather than joining KPA
        assert_eq!(
            t.stroke(stroke("KAT")),
            Some(StenoOutput::Retype {
                backspaces: 1,
                text: "cat".into(),
            })
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
            None | Some(StenoOutput::Keys(_)) => {}
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

    /// PER and TPEBGT each have an entry, and together are "perfect". PUR is another word.
    const PERFECT: &str = r#"{"PER": "per", "TPEBGT": "effect", "PER/TPEBGT": "perfect", "PUR": "purr"}"#;

    #[test]
    fn a_stroke_that_matched_nothing_does_not_stop_the_next_entry_combining() {
        let mut t = translator(PERFECT);
        // HRAOEUP has no entry. It types nothing, so the text is still empty and the next
        // word starts in the same place: that must not fold HRAOEUP into the word's strokes
        assert_eq!(t.stroke(stroke("HRAOEUP")), None);
        assert!(t.was_unmatched());
        assert_eq!(t.stroke(stroke("PER")), Some(StenoOutput::Type("per".into())));
        // PER/TPEBGT is "perfect", so this adds "fect", not " effect"
        assert_eq!(
            t.stroke(stroke("TPEBGT")),
            Some(StenoOutput::Type("fect".into()))
        );
    }

    #[test]
    fn an_unmatched_stroke_then_an_undo_that_empties_the_text_still_combines_after() {
        // The sequence from a real tape: an unmatched stroke, a word taken back with `*`, and
        // then a two-stroke word, with nothing on screen in between
        let mut t = translator(PERFECT);
        let mut typed = String::new();
        for text in ["HRAOEUP", "PUR", "*", "PER", "TPEBGT"] {
            apply(&mut typed, t.stroke(stroke(text)));
        }
        assert_eq!(typed, "perfect");
    }

    #[test]
    fn an_unmatched_stroke_before_a_word_does_not_change_what_undo_takes_back() {
        let mut t = translator(PERFECT);
        let mut typed = String::new();
        for text in ["HRAOEUP", "PER", "TPEBGT"] {
            apply(&mut typed, t.stroke(stroke(text)));
        }
        assert_eq!(typed, "perfect");
        // `*` takes "perfect" back to "per", then "per" away, as it does with no stroke before
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 7,
                text: "per".into(),
            })
        );
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Retype {
                backspaces: 3,
                text: String::new(),
            })
        );
        // Nothing but the unmatched stroke is left, and it has nothing to take back
        assert_eq!(t.stroke(UNDO_STROKE), None);
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

    /// Type `strokes` one by one, as the keyboard would, and return the text on screen.
    fn type_strokes(json: &str, strokes: &[&str]) -> String {
        let mut t = translator(json);
        let mut typed = String::new();
        for chord in strokes {
            apply(&mut typed, t.stroke(stroke(chord)));
        }
        typed
    }

    #[test]
    fn first_word_has_no_space_in_front() {
        assert_eq!(
            type_strokes(r#"{"KAT": "cat", "-PB": "and"}"#, &["KAT", "-PB"]),
            "cat and"
        );
    }

    #[test]
    fn word_after_a_fingerspelled_letter_is_spaced() {
        // Plover: a fingerspelled letter leaves a trailing space, so the next word is not glued on
        assert_eq!(
            type_strokes(r#"{"TPH": "{>}{&n}", "KAT": "cat"}"#, &["TPH", "KAT"]),
            "n cat"
        );
    }

    #[test]
    fn fingerspelled_letter_after_a_word_is_spaced() {
        assert_eq!(
            type_strokes(r#"{"KAT": "cat", "TPH": "{>}{&n}"}"#, &["KAT", "TPH"]),
            "cat n"
        );
    }

    #[test]
    fn letter_then_word_is_spaced_without_attach() {
        // Live check: TPH*/SREUPL types "n vim", not "nvim"
        assert_eq!(
            type_strokes(r#"{"TPH": "{>}{&n}", "SREUPL": "vim"}"#, &["TPH", "SREUPL"]),
            "n vim"
        );
    }

    #[test]
    fn attach_after_a_letter_glues_the_next_word() {
        // {^} after a fingerspelled letter makes the next word attach with no space
        assert_eq!(
            type_strokes(
                r#"{"TPH": "{>}{&n}{^}", "SREUPL": "vim"}"#,
                &["TPH", "SREUPL"]
            ),
            "nvim"
        );
    }

    #[test]
    fn sentence_end_attaches_and_capitalizes_the_next_word() {
        assert_eq!(
            type_strokes(
                r#"{"KAT": "cat", "-PB": "and", "-PBG": "{.}"}"#,
                &["KAT", "-PBG", "-PB"]
            ),
            "cat. And"
        );
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

    #[test]
    fn book_retro_insert_space_splits_a_joined_word() {
        // Lapwing ch20: KOR/TPHET is "cornet", and adding S-PD gives "core net"
        assert_eq!(
            type_strokes(
                r#"{"KOR": "core", "TPHET": "net", "KOR/TPHET": "cornet", "S-PD": "{*?}"}"#,
                &["KOR", "TPHET", "S-PD"]
            ),
            "core net"
        );
    }

    #[test]
    fn book_retro_delete_space_joins_two_words() {
        // Lapwing ch20: PHOR/AUFR is "more over", and SP-LS gives "moreover"
        assert_eq!(
            type_strokes(
                r#"{"PHOR": "more", "AUFR": "over", "SP-LS": "{*!}"}"#,
                &["PHOR", "AUFR", "SP-LS"]
            ),
            "moreover"
        );
    }

    #[test]
    fn retro_insert_space_on_a_single_word_changes_nothing() {
        assert_eq!(
            type_strokes(r#"{"KAT": "cat", "S-PD": "{*?}"}"#, &["KAT", "S-PD"]),
            "cat"
        );
    }

    #[test]
    fn retro_lowercase_lowers_the_last_word() {
        assert_eq!(
            type_strokes(
                r#"{"KAT": "Cat", "HRO*ERD": "{:retro_case:lower_first_char}"}"#,
                &["KAT", "HRO*ERD"]
            ),
            "cat"
        );
    }

    #[test]
    fn glue_and_raw_text_commands_parse() {
        assert_eq!(
            parse_pieces("{^ ^}"),
            Some(vec![Piece::Glue(" ".to_string())])
        );
        assert_eq!(parse_pieces("{^^}"), Some(vec![Piece::Attach]));
        assert_eq!(
            parse_pieces("{~|\"^}"),
            Some(vec![Piece::TextAttach("\"".to_string())])
        );
    }

    #[test]
    fn number_key_with_nothing_before_it_types_nothing() {
        let mut t = translator(r#"{"KAT": "cat"}"#);
        assert_eq!(t.stroke(NUMBER_BIT), None);
        // The `#` stays available to start a proper-noun outline after it
        assert_eq!(
            t.stroke(stroke("KAT")),
            Some(StenoOutput::Type("cat".into()))
        );
    }

    #[test]
    fn number_key_repeats_a_word_typed_before_it() {
        let mut t = translator(r#"{"KAT": "cat"}"#);
        t.stroke(stroke("KAT"));
        // Repeating "cat" from a fresh sentence puts a space in front of it
        assert_eq!(t.stroke(NUMBER_BIT), Some(StenoOutput::Type(" cat".into())));
        assert_eq!(t.text(), "cat cat");
    }

    #[test]
    fn newline_text_passes_through_to_the_screen() {
        let mut t = translator(r#"{"KAT": "cat", "R-R": "{^~|\n^}"}"#);
        t.stroke(stroke("KAT"));
        assert_eq!(
            t.stroke(stroke("R-R")),
            Some(StenoOutput::Type("\n".into()))
        );
        assert_eq!(t.text(), "cat\n");
    }

    #[test]
    fn unknown_commands_are_not_typed_as_text() {
        assert_eq!(parse_pieces("{:stitch:Z}"), None);
        assert_eq!(parse_pieces("{PLOVER:FOCUS}"), None);
        assert_eq!(parse_pieces("{#NotAKey}"), None);
        assert_eq!(parse_pieces("{#Bogus(Left)}"), None);
    }
}
