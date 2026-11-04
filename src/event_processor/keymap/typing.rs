//! Hand-typed text, followed into the steno history so a stroke carries on from it and `*`
//! can take it back, with each finished word noted on the tape next to the strokes.

use super::KeymapProcessor;
use crate::event_processor::actions::ProcessResult;
use crate::steno::journal::{self, SharedHistory};
use crate::steno::tape;
use crate::steno::typing::Typist;
use std::path::PathBuf;

pub(super) struct TypingWatch {
    /// `None` when there is no steno layer to follow for, or following is turned off
    typist: Option<Typist>,
    /// What is on screen, shared by every keyboard of this user and every steno layer
    history: SharedHistory,
    /// Where finished words are noted, next to the strokes
    tape: Option<PathBuf>,
}

impl TypingWatch {
    pub(super) const fn new(
        typist: Option<Typist>,
        history: SharedHistory,
        tape: Option<PathBuf>,
    ) -> Self {
        Self {
            typist,
            history,
            tape,
        }
    }

    #[cfg(test)]
    pub(super) fn shared_history(&self) -> SharedHistory {
        std::sync::Arc::clone(&self.history)
    }

    /// Follow what reached the application into the history. The words that finished.
    fn follow(&mut self, result: &ProcessResult) -> Vec<String> {
        let Some(typist) = self.typist.as_mut() else {
            return Vec::new();
        };
        let mut history = journal::lock(&self.history);
        match result {
            ProcessResult::EmitKey(key, pressed) => typist.key(&mut history, *key, *pressed),
            ProcessResult::MultipleEvents(events) => {
                for (key, pressed) in events {
                    typist.key(&mut history, *key, *pressed);
                }
            }
            // Text a layer types for you is on screen like any other
            ProcessResult::TypeString(text) => {
                for ch in text.chars() {
                    history.typed(ch);
                }
            }
            ProcessResult::Retype { .. } | ProcessResult::Ops(_) | ProcessResult::None => {}
        }
        std::mem::take(&mut history.finished_words)
    }
}

impl KeymapProcessor {
    /// Follow what a plain layer sent to the application into the history, so steno carries on
    /// from it and `*` can take it back.
    pub(super) fn watch_typing(&mut self, result: &ProcessResult) {
        if self.layer_stack.is_game_mode_active() {
            return;
        }
        let words = self.typing.follow(result);
        let Some(path) = self.typing.tape.as_deref().filter(|_| !words.is_empty()) else {
            return;
        };
        // The first layer's dictionary, by name, once it has loaded
        let dict = self
            .steno_engines
            .iter()
            .min_by(|a, b| a.0 .0.cmp(&b.0 .0))
            .and_then(|(_, engine)| engine.dictionary_handle());
        for word in words {
            tape::note_typed(path, dict.clone(), word);
        }
    }
}
