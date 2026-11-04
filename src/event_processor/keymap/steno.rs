//! A steno layer's capture: its stroke keys go to its engine and never reach the application,
//! keys the layer doesn't remap are swallowed, and a finished stroke - on a release, by its
//! timeout, or repeating while held - comes back as typing.

use super::KeymapProcessor;
use crate::config::Layer;
use crate::event_processor::actions::{HeldAction, ProcessResult};
use crate::keycode::KeyCode;
use crate::steno::translate::StenoOutput;
use crate::steno::{self, StenoEngine};
use std::time::Instant;

/// Keys that always reach the screen on a steno layer, without a remap.
const STENO_PASSTHROUGH: [KeyCode; 1] = [KeyCode::KC_DEL];

fn steno_result(output: Option<StenoOutput>) -> ProcessResult {
    match output {
        None => ProcessResult::None,
        Some(StenoOutput::Type(text)) => ProcessResult::TypeString(text),
        Some(StenoOutput::Retype { backspaces, text }) => {
            ProcessResult::Retype { backspaces, text }
        }
        Some(StenoOutput::Ops(ops)) => ProcessResult::Ops(ops),
    }
}

impl KeymapProcessor {
    /// Finish any steno stroke held past its timeout, or send the next repeat of a held one.
    /// Returns what to type, if anything.
    pub fn check_steno_timeouts(&mut self) -> ProcessResult {
        let now = Instant::now();
        let found = self
            .steno_engines
            .iter_mut()
            .find_map(|(layer, engine)| Some((layer.clone(), engine.tick(now)?)));
        let Some((layer, output)) = found else {
            return ProcessResult::None;
        };
        self.leave_if_asked(&layer);
        steno_result(Some(output))
    }

    /// Route a key to the top layer if that layer is steno. Returns `None` when the
    /// key should go through the normal remap path.
    ///
    /// On a steno layer, stroke keys are captured. Other keys use the layer's remaps
    /// if it has one, and are swallowed otherwise so they don't leak to layers below.
    pub(super) fn steno_capture(
        &mut self,
        keycode: KeyCode,
        pressed: bool,
        at: Instant,
    ) -> Option<ProcessResult> {
        if !pressed {
            // Releases belong to whoever pressed the key, even if its layer is gone now
            if !matches!(
                self.held_keys.get(&keycode)?,
                HeldAction::StenoManaged(_) | HeldAction::SwallowedBySteno
            ) {
                return None;
            }
            let result = match self.held_keys.remove(&keycode)? {
                HeldAction::StenoManaged(layer) => {
                    let output = self.steno_engines.get_mut(&layer)?.release_at(keycode, at);
                    self.leave_if_asked(&layer);
                    steno_result(output)
                }
                // A swallowed press must not produce a release on its own
                _ => ProcessResult::None,
            };
            return Some(result);
        }

        let top = self.layer_stack.layers().last();
        // Entering a steno layer carries on from what was typed, unless the layer asks to start over
        if self.steno_active.as_ref() != top {
            if let Some(engine) = top.and_then(|top| self.steno_engines.get_mut(top)) {
                engine.enter();
            }
            self.steno_active = top
                .filter(|top| self.steno_engines.contains_key(*top))
                .cloned();
        }
        let top = top?;
        let game_mode = self.layer_stack.is_game_mode_active();
        let engine = self.steno_engines.get_mut(top)?;
        if game_mode && engine.disabled_in_game_mode {
            return None;
        }

        if engine.owns(keycode) {
            engine.press(keycode, at);
            self.held_keys
                .insert(keycode, HeldAction::StenoManaged(top.clone()));
            return Some(ProcessResult::None);
        }

        let has_remap = self
            .layer_stack
            .layer_configs()
            .get(top)
            .is_some_and(|config| config.remaps.contains_key(&keycode));
        if STENO_PASSTHROUGH.contains(&keycode) || has_remap {
            // This key reaches the application, and may move the caret or change the text
            // behind the translator's back (arrows, Delete...). The typist sees it go by and
            // makes the history let go of what it remembers, or follows it if it types text.
            return None;
        }

        // Any other key does nothing on a steno layer: Space and Backspace too, so an
        // accidental press can't wipe text (`*` is the only way to take text back). Its
        // release is swallowed with its press, so it can't leak a stray key event.
        self.held_keys.insert(keycode, HeldAction::SwallowedBySteno);
        Some(ProcessResult::None)
    }

    /// A stroke that wrote `{plover:toggle}` or `{plover:suspend}` leaves the steno layer, so the
    /// keys type normally again.
    pub(super) fn leave_if_asked(&mut self, layer: &Layer) {
        if self
            .steno_engines
            .get_mut(layer)
            .is_some_and(StenoEngine::take_leave_request)
        {
            self.layer_stack.deactivate_layer(layer);
            self.publish_overlay_layer();
        }
    }

    /// On a steno layer with no dictionary at all, point the user at how to get one.
    /// Rate-limited so holding a key doesn't spam notifications.
    pub(super) fn steno_setup_hint(&mut self) {
        let needs_setup = self
            .layer_stack
            .layers()
            .last()
            .and_then(|top| self.steno_engines.get(top))
            .is_some_and(StenoEngine::needs_setup);
        if needs_setup && self.setup_hint.ready(Instant::now()) {
            steno::setup::notify_user(
                self.remap.user_id,
                "Steno has no dictionary",
                "Strokes type as they are. Run: keymux steno theory lapwing (or plover), \
                 or drop dictionaries into ~/.config/keymux/steno",
            );
        }
    }
}
