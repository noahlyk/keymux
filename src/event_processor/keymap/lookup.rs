//! A live lookup's capture: while one is open, every key still goes through the layers as it
//! always does (remaps, mod-taps, held layers), and what comes out types into the lookup
//! instead of reaching the application. Esc and Enter close it, even where a layer remaps them.

use super::KeymapProcessor;
use crate::config::Layer;
use crate::event_processor::actions::{HeldAction, ProcessResult};
use crate::keycode::KeyCode;
use crate::steno::lookup::{LookupInput, LookupKey};
use crate::steno::typing::TypingLayout;
use std::collections::HashSet;

pub(super) struct LookupCapture {
    /// Reads the keys that come out of the layers as input to the lookup
    input: LookupInput,
    /// Keys pressed during a lookup, whose releases must stay out of the application too
    keys: HashSet<KeyCode>,
}

impl LookupCapture {
    pub(super) fn new(layout: TypingLayout) -> Self {
        Self {
            input: LookupInput::new(layout),
            keys: HashSet::new(),
        }
    }

    /// What came out of the layers for a key, as input to the lookup.
    fn read(&mut self, result: &ProcessResult) -> Vec<LookupKey> {
        match result {
            ProcessResult::EmitKey(key, pressed) => {
                self.input.key(*key, *pressed).into_iter().collect()
            }
            ProcessResult::MultipleEvents(events) => events
                .iter()
                .filter_map(|&(key, pressed)| self.input.key(key, pressed))
                .collect(),
            // Text a layer types for you goes into the query like typed keys
            ProcessResult::TypeString(text) => text
                .chars()
                .filter(|c| !matches!(c, '\n' | '\t'))
                .map(LookupKey::Char)
                .collect(),
            ProcessResult::Retype { .. } | ProcessResult::Ops(_) | ProcessResult::None => {
                Vec::new()
            }
        }
    }
}

impl KeymapProcessor {
    /// Returns `None` when no lookup is open, or the key is not one for it (a stroke key let go
    /// of, say).
    pub(super) fn lookup_capture(&mut self, keycode: KeyCode, pressed: bool) -> Option<ProcessResult> {
        let open = self
            .steno_engines
            .iter()
            .find_map(|(layer, engine)| engine.lookup_active().then(|| layer.clone()));
        let Some(layer) = open else {
            // Let go of after the lookup closed: still not the application's business
            if !pressed && self.lookup.keys.remove(&keycode) {
                let result = self.process_key_release(keycode);
                self.read_lookup_keys(&result);
                return Some(ProcessResult::None);
            }
            return None;
        };
        if !self.layer_stack.layers().contains(&layer) {
            // The layer was left in the middle of it
            self.close_lookup(&layer);
            return None;
        }

        if !pressed {
            if !self.lookup.keys.remove(&keycode) {
                return None;
            }
            let result = self.process_key_release(keycode);
            self.read_lookup_keys(&result);
            return Some(ProcessResult::None);
        }

        if matches!(keycode, KeyCode::KC_ESC | KeyCode::KC_ENT) {
            self.close_lookup(&layer);
            // Swallowed on release too, so it can't leak a stray key event
            self.held_keys.insert(keycode, HeldAction::SwallowedBySteno);
            return Some(ProcessResult::None);
        }

        self.lookup.keys.insert(keycode);
        let result = self.process_key_press(keycode);
        self.read_lookup_keys(&result);
        // A key that left the steno layer (a remap to base, say) leaves the lookup behind
        if !self.layer_stack.layers().contains(&layer) {
            self.close_lookup(&layer);
        }
        Some(ProcessResult::None)
    }

    fn close_lookup(&mut self, layer: &Layer) {
        if let Some(engine) = self.steno_engines.get_mut(layer) {
            engine.close_lookup();
        }
        self.lookup.input.reset();
    }

    fn read_lookup_keys(&mut self, result: &ProcessResult) {
        let keys = self.lookup.read(result);
        if let Some(engine) = self
            .steno_engines
            .values_mut()
            .find(|engine| engine.lookup_active())
        {
            for key in keys {
                engine.lookup_input(key);
            }
        }
    }
}
