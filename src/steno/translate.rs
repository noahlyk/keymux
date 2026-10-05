//! Turns finished strokes into text.
//!
//! Strokes are held while they could still be the start of a longer dictionary
//! entry, and released when the next stroke rules that out or the idle timeout
//! passes. Each translation is recorded so the bare `*` stroke can undo it.

use super::dict::Dictionary;
use super::format::{render, FormatState};
use std::time::{Duration, Instant};
use tracing::info;

/// The stroke that undoes the last translation: `*` on its own.
const UNDO_STROKE: u32 = 1 << 9;

/// What the caller should do with the keyboard after a stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StenoOutput {
    /// Type this text
    Type(String),
    /// Delete `backspaces` characters, then type `text`
    Retype { backspaces: usize, text: String },
}

#[derive(Debug)]
struct Emitted {
    text: String,
    state_before: FormatState,
}

#[derive(Debug)]
pub struct Translator {
    dict: Dictionary,
    /// Strokes waiting to be translated
    pending: Vec<u32>,
    /// What each translation typed, most recent last, for undo
    history: Vec<Emitted>,
    state: FormatState,
    last_stroke: Option<Instant>,
    idle_timeout: Duration,
}

impl Translator {
    #[must_use]
    pub const fn new(dict: Dictionary, idle_timeout: Duration) -> Self {
        Self {
            dict,
            pending: Vec::new(),
            history: Vec::new(),
            state: FormatState {
                has_text: false,
                suppress_space: false,
                capitalize: false,
            },
            last_stroke: None,
            idle_timeout,
        }
    }

    /// A stroke is complete.
    pub fn stroke(&mut self, stroke: u32, now: Instant) -> Option<StenoOutput> {
        self.last_stroke = Some(now);
        if stroke == UNDO_STROKE {
            return self.undo();
        }
        self.pending.push(stroke);
        self.translate(false)
    }

    /// Release held strokes once nothing has arrived for `idle_timeout`.
    pub fn idle_flush(&mut self, now: Instant) -> Option<StenoOutput> {
        let idle = self
            .last_stroke
            .is_some_and(|last| now.duration_since(last) >= self.idle_timeout);
        if idle && !self.pending.is_empty() {
            self.translate(true)
        } else {
            None
        }
    }

    fn undo(&mut self) -> Option<StenoOutput> {
        if !self.pending.is_empty() {
            self.pending.clear();
            return None;
        }
        let emitted = self.history.pop()?;
        self.state = emitted.state_before;
        info!("steno undo ({} characters)", emitted.text.chars().count());
        Some(StenoOutput::Retype {
            backspaces: emitted.text.chars().count(),
            text: String::new(),
        })
    }

    /// Translate as much of `pending` as possible from the front. With `flush`
    /// set, hold nothing back even if the strokes could still extend an entry.
    fn translate(&mut self, flush: bool) -> Option<StenoOutput> {
        let mut typed = String::new();
        while !self.pending.is_empty() {
            if !flush && self.dict.is_prefix(&self.pending) {
                break;
            }
            let longest = (1..=self.pending.len().min(self.dict.max_len()))
                .rev()
                .find(|&len| self.dict.get(&self.pending[..len]).is_some());
            match longest {
                Some(len) => {
                    let before = self.state;
                    let pieces = self.dict.get(&self.pending[..len]).unwrap_or_default();
                    let text = render(pieces, &mut self.state);
                    self.pending.drain(..len);
                    typed.push_str(&text);
                    self.history.push(Emitted {
                        text,
                        state_before: before,
                    });
                }
                None => {
                    // Not in the dictionary: drop the stroke and keep going
                    let stroke = self.pending.remove(0);
                    info!("steno stroke {stroke:#x} not in dictionary");
                }
            }
        }
        (!typed.is_empty()).then_some(StenoOutput::Type(typed))
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
        Translator::new(dict, Duration::from_millis(400))
    }

    #[test]
    fn single_strokes_type_words_with_spacing() {
        let mut t = translator(r#"{"KAT": "cat", "-PB": "and"}"#);
        let now = Instant::now();
        assert_eq!(
            t.stroke(stroke("KAT"), now),
            Some(StenoOutput::Type("cat".into()))
        );
        assert_eq!(
            t.stroke(stroke("-PB"), now),
            Some(StenoOutput::Type(" and".into()))
        );
    }

    #[test]
    fn multi_stroke_entry_waits_for_the_rest() {
        let mut t = translator(r#"{"KPA/TKAOEU": "example"}"#);
        let now = Instant::now();
        assert_eq!(t.stroke(stroke("KPA"), now), None);
        assert_eq!(
            t.stroke(stroke("TKAOEU"), now),
            Some(StenoOutput::Type("example".into()))
        );
    }

    #[test]
    fn a_stroke_that_does_not_extend_the_prefix_is_translated_alone() {
        let mut t = translator(r#"{"KPA/TKAOEU": "example", "KAT": "cat"}"#);
        let now = Instant::now();
        // KPA could start "example", so it waits. KAT doesn't continue it, so KPA is
        // dropped (it's not an entry on its own) and KAT translates.
        assert_eq!(t.stroke(stroke("KPA"), now), None);
        assert_eq!(
            t.stroke(stroke("KAT"), now),
            Some(StenoOutput::Type("cat".into()))
        );
    }

    #[test]
    fn idle_timeout_releases_a_held_prefix() {
        let mut t = translator(r#"{"KPA/TKAOEU": "example"}"#);
        let t0 = Instant::now();
        assert_eq!(t.stroke(stroke("KPA"), t0), None);
        assert_eq!(t.idle_flush(t0 + Duration::from_millis(100)), None);
        // Nothing in the dictionary matches KPA alone, so it's dropped
        assert_eq!(t.idle_flush(t0 + Duration::from_millis(500)), None);
    }

    #[test]
    fn undo_removes_the_last_translation_and_restores_spacing() {
        let mut t = translator(r#"{"KAT": "cat", "-PB": "and"}"#);
        let now = Instant::now();
        t.stroke(stroke("KAT"), now);
        t.stroke(stroke("-PB"), now);
        assert_eq!(
            t.stroke(UNDO_STROKE, now),
            Some(StenoOutput::Retype {
                backspaces: 4,
                text: String::new(),
            })
        );
        // " and" was removed, so the next word is the first word again in terms of spacing
        assert_eq!(
            t.stroke(stroke("-PB"), now),
            Some(StenoOutput::Type(" and".into()))
        );
    }

    #[test]
    fn undo_with_nothing_to_undo_does_nothing() {
        let mut t = translator("{}");
        assert_eq!(t.stroke(UNDO_STROKE, Instant::now()), None);
    }
}
