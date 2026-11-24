//! The record of what is on screen, shared by every keyboard and every set of dictionaries.
//!
//! Plover keeps one history per translator. keymux has several: a steno layer on each keyboard,
//! and not always the same dictionaries. They all type into the same window, so what `*` takes
//! back, and what spacing and capitals follow from, has to be one record. This is it.
//!
//! Each entry says where it came from ([`Origin`]) and which *machine* made it, that is, which
//! set of dictionaries. A stroke only joins strokes of its own machine into a longer outline,
//! but anything can undo, or build on, anything: undoing uses the formatting stored in the
//! entry, not a dictionary.
//!
//! Text typed on a plain layer is recorded too, as words, so going back into steno carries on
//! from it instead of from a blank page, and `*` can take it back.

use super::action::{rightmost_word, Action, Case};
use super::formatter::Formatter;
use super::strokes::StrokeLog;
use super::translate::Translation;
use std::sync::{Arc, Mutex};

/// Which set of dictionaries made an entry: a hash of where they are loaded from. Zero is for
/// anything made without one (tests, a lone translator).
pub type MachineId = u64;

/// Where an entry in the history came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Origin {
    /// Strokes translated with a dictionary
    #[default]
    Stroke,
    /// Text typed on a plain layer: a word, with the space that ended it
    Typed(String),
    /// Text a key removed: Backspace, or `{:back}`. Undoing it types the text again.
    Erased(String),
}

impl Origin {
    /// Made by translating strokes, so it has outlines a dictionary can join with.
    #[must_use]
    pub const fn is_stroke(&self) -> bool {
        matches!(self, Self::Stroke)
    }
}

/// How many more translations than usual the history keeps to keep a held run whole.
const RUN_KEPT_WHOLE: usize = 1000;

/// The translations still undoable, and the one before them for context.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub translations: Vec<Translation>,
    /// The oldest translation still kept, no longer undoable
    pub tail: Option<Translation>,
}

impl State {
    /// Reduce the history to about `n` strokes.
    pub fn restrict_size(&mut self, n: usize) {
        let mut strokes = 0;
        let mut count = 0;
        for translation in self.translations.iter().rev() {
            strokes += translation.strokes.len().max(1);
            count += 1;
            if strokes >= n {
                break;
            }
        }
        let mut index = self.translations.len() - count;
        // A held run is undone as one, so it is kept whole (up to a bound, for a key held down
        // for minutes): cut before it, not through it
        let run = self.translations.get(index).and_then(|t| t.run);
        let mut kept = 0;
        while index > 0
            && run.is_some()
            && self.translations[index - 1].run == run
            && kept < RUN_KEPT_WHOLE
        {
            index -= 1;
            kept += 1;
        }
        if index > 0 {
            self.tail = Some(self.translations[index - 1].clone());
        }
        self.translations.drain(..index);
    }
}

/// Everything the translators remember between strokes. A translator works on this while it
/// translates; between strokes it lives in the [`SharedHistory`].
#[derive(Debug, Clone, Default)]
pub struct History {
    pub state: State,
    pub to_undo: Vec<Translation>,
    pub to_do: usize,
    pub formatter: Formatter,
    /// What is on screen, as far as it is known
    pub screen: String,
    /// Groups of translations that undo took back, last group on top. Redo puts one back. Any
    /// new input empties it.
    pub redo: Vec<Vec<Translation>>,
    /// Words typed by hand that were finished (by a space) and not yet put on the tape
    pub finished_words: Vec<String>,
    /// The strokes of the translations, in order, for plugins to read back (a wall is where
    /// text came from elsewhere than strokes); see [`super::strokes`]
    pub strokes: StrokeLog,
}

/// The history every keyboard of a user shares.
pub type SharedHistory = Arc<Mutex<History>>;

/// The history of each user, so every keyboard of theirs writes to the same one.
fn registry() -> &'static Mutex<std::collections::HashMap<u32, SharedHistory>> {
    static REGISTRY: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, SharedHistory>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// The history shared by all of `user`'s keyboards.
#[must_use]
pub fn for_user(user: u32) -> SharedHistory {
    let mut users = crate::lock(registry());
    Arc::clone(
        users
            .entry(user)
            .or_insert_with(|| new_shared(Formatter::default())),
    )
}

/// Forget what is on screen for everyone: the focus moved to another window.
pub fn fence_all() {
    let users = crate::lock(registry());
    for history in users.values() {
        lock(history).fence();
    }
}

/// A history of its own, for a translator that shares nothing.
#[must_use]
pub fn new_shared(formatter: Formatter) -> SharedHistory {
    Arc::new(Mutex::new(History {
        formatter,
        ..History::default()
    }))
}

/// Lock the history, even if a thread died holding it: the record is still the best we have.
pub fn lock(shared: &SharedHistory) -> std::sync::MutexGuard<'_, History> {
    crate::lock(shared)
}

impl History {
    /// Forget everything: what is at the cursor is unknown now (another window, a moved caret),
    /// so nothing is undoable and the next word starts a fresh sentence.
    pub fn fence(&mut self) {
        let formatter = self.formatter.clone();
        *self = Self {
            formatter,
            ..Self::default()
        };
    }

    /// The text that undoing would take back, if anything is recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.state.translations.is_empty()
    }

    /// Text was typed on a plain layer (one character at a time). It extends the word being
    /// typed, or starts a new one.
    pub fn typed(&mut self, ch: char) {
        self.redo.clear();
        self.screen.push(ch);
        let extends = matches!(
            self.state.translations.last(),
            Some(Translation { origin: Origin::Typed(text), .. })
                if !text.ends_with(char::is_whitespace)
        );
        if extends {
            let Some(last) = self.state.translations.pop() else {
                return;
            };
            let Origin::Typed(mut text) = last.origin else {
                return;
            };
            text.push(ch);
            if ch.is_whitespace() {
                self.finished_words.push(text.clone());
            }
            self.push_recorded(Origin::Typed(text), last.machine);
        } else {
            self.push_recorded(Origin::Typed(ch.to_string()), 0);
        }
    }

    /// A Backspace reached the screen. It shortens what was typed by hand, or else takes a
    /// character out of a stroke's text, which is remembered so `*` can put it back. Returns
    /// whether anything was recorded.
    pub fn backspace(&mut self) -> bool {
        self.redo.clear();
        let Some(last) = self.state.translations.pop() else {
            return false;
        };
        let machine = last.machine;
        match last.origin.clone() {
            Origin::Typed(mut text) => {
                self.screen.pop();
                text.pop();
                // Backspacing over the word's end leaves the rest as a word that is still open
                if !text.is_empty() {
                    self.push_recorded(Origin::Typed(text), machine);
                }
            }
            Origin::Erased(mut erased) => {
                // The character this Backspace removed comes before the ones already erased
                let Some(ch) = self.screen.pop() else {
                    self.state.translations.push(last);
                    return false;
                };
                erased.insert(0, ch);
                self.push_recorded(Origin::Erased(erased), machine);
            }
            Origin::Stroke => {
                self.state.translations.push(last);
                let Some(ch) = self.screen.pop() else {
                    return false;
                };
                self.push_recorded(Origin::Erased(ch.to_string()), 0);
            }
        }
        true
    }

    /// Record an entry that is already on screen, with its formatting made for it.
    fn push_recorded(&mut self, origin: Origin, machine: MachineId) {
        self.strokes.wall();
        let before: &[Translation] = match (&self.state.translations[..], &self.state.tail) {
            ([], Some(tail)) => std::slice::from_ref(tail),
            (translations, _) => translations,
        };
        let last = self.formatter.last_action(before);
        let mut translation = Translation::new(Vec::new(), None);
        translation.machine = machine;
        translation.formatting = vec![recorded_action(&origin, &last)];
        translation.origin = origin;
        self.state.translations.push(translation);
        self.state.restrict_size(super::translate::UNDO_LENGTH);
    }
}

/// The action for an entry made of text that is already on screen.
#[must_use]
pub fn recorded_action(origin: &Origin, last: &Action) -> Action {
    let mut action = last.new_state();
    match origin {
        Origin::Typed(text) => {
            // The text brings its own spaces, so nothing is put in front of it
            action.prev_attach = true;
            action.text = Some(text.clone());
            let ends_in_space = text.ends_with(char::is_whitespace);
            action.word = Some(rightmost_word(text)).filter(|word| !word.is_empty());
            action.word_is_finished = ends_in_space;
            action.orthography = false;
            // After a space the next word attaches to it; after a full stop it is a capital
            action.next_attach = ends_in_space;
            if ends_in_space && text.trim_end().ends_with(['.', '!', '?']) {
                action.next_case = Some(Case::CapFirstWord);
            }
        }
        Origin::Erased(text) => {
            // What was removed is "replaced" by nothing, so undoing it types it again
            let state = last.copy_state();
            action = Action {
                prev_attach: true,
                prev_replace: text.clone(),
                text: Some(String::new()),
                word: None,
                ..state
            };
        }
        Origin::Stroke => {}
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> History {
        History {
            formatter: Formatter::new(super::super::formatter::Options {
                start_attached: true,
                ..super::super::formatter::Options::default()
            }),
            ..History::default()
        }
    }

    fn type_text(history: &mut History, text: &str) {
        for ch in text.chars() {
            history.typed(ch);
        }
    }

    #[test]
    fn typed_text_is_recorded_a_word_at_a_time() {
        let mut h = history();
        type_text(&mut h, "hello world");
        let words: Vec<_> = h
            .state
            .translations
            .iter()
            .map(|t| t.origin.clone())
            .collect();
        assert_eq!(
            words,
            vec![
                Origin::Typed("hello ".into()),
                Origin::Typed("world".into())
            ]
        );
        assert_eq!(h.screen, "hello world");
    }

    #[test]
    fn backspace_shortens_the_word_typed() {
        let mut h = history();
        type_text(&mut h, "hello wor");
        h.backspace();
        assert_eq!(h.screen, "hello wo");
        assert_eq!(
            h.state.translations.last().unwrap().origin,
            Origin::Typed("wo".into())
        );
        // Backspacing a word away leaves the one before it
        h.backspace();
        h.backspace();
        assert_eq!(
            h.state.translations.last().unwrap().origin,
            Origin::Typed("hello ".into())
        );
    }

    #[test]
    fn a_fence_forgets_everything() {
        let mut h = history();
        type_text(&mut h, "hello world");
        h.fence();
        assert!(h.is_empty());
        assert_eq!(h.screen, "");
    }

    #[test]
    fn a_held_run_is_kept_whole_when_the_history_is_cut() {
        let one = |run| Translation {
            run,
            ..Translation::new(vec![1], Some("cat".into()))
        };
        let mut state = State {
            translations: (0..10).map(|_| one(None)).collect(),
            tail: None,
        };
        state.translations.extend((0..150).map(|_| one(Some(7))));
        state.restrict_size(100);
        // Cut before the run, never through it
        assert_eq!(state.translations.len(), 150);
        assert!(state.translations.iter().all(|t| t.run == Some(7)));
        assert!(state.tail.as_ref().is_some_and(|t| t.run.is_none()));
        // A run past the bound is cut at the bound
        state.translations = (0..2000).map(|_| one(Some(8))).collect();
        state.restrict_size(100);
        assert_eq!(state.translations.len(), 100 + RUN_KEPT_WHOLE);
    }
}
