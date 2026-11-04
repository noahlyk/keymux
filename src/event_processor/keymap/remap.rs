//! The plain path: a key no steno layer captures does what its layer says - a key, a mod-tap,
//! a tap-dance, a one-shot modifier, an SOCD pair, a layer switch, a command - and what its
//! press left held is let go of the same way when it comes up.

use super::KeymapProcessor;
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{
    dt::Due, handle_action_release, DtProcessor, HandleContext, HeldAction, MtProcessor,
    OsmProcessor, ProcessResult, SocdProcessor,
};
use crate::event_processor::adaptive::AdaptiveProcessor;
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use std::path::PathBuf;
use std::time::Instant;

/// The processors behind the plain actions, and what a command runs with.
pub(super) struct Remap {
    pub(super) mt: MtProcessor,
    pub(super) dt: DtProcessor,
    osm: OsmProcessor,
    socd: SocdProcessor,
    pub(super) adaptive: AdaptiveProcessor,
    config_dir: PathBuf,
    pub(super) user_id: u32,
}

impl Remap {
    pub(super) fn new(config: &Config, config_dir: PathBuf, user_id: u32) -> Self {
        Self {
            mt: MtProcessor::new(config),
            dt: DtProcessor::new(config),
            osm: OsmProcessor::new(config),
            socd: SocdProcessor::from_config(config),
            adaptive: AdaptiveProcessor::new(),
            config_dir,
            user_id,
        }
    }

    /// Everything an action runs with, borrowed together, for an event that happened at `at`.
    fn context<'a>(
        &'a mut self,
        layer_stack: &'a mut LayerStack,
        at: Instant,
    ) -> HandleContext<'a> {
        HandleContext {
            mt_processor: &mut self.mt,
            dt_processor: &mut self.dt,
            osm_processor: &mut self.osm,
            socd_processor: &mut self.socd,
            layer_stack,
            config_dir: &self.config_dir,
            user_id: self.user_id,
            at,
        }
    }
}

impl KeymapProcessor {
    pub(super) fn process_key_press(&mut self, keycode: KeyCode, at: Instant) -> ProcessResult {
        self.remap.adaptive.record_key_press(keycode, at);
        // What time and this key decide for the tap-dances goes out before the key does
        let mut events = self.settle_tap_dances(at);
        let interrupted = self.remap.dt.interrupted_by(keycode);
        events.extend(self.perform_due(interrupted, at));
        let action = self
            .lookup_action(keycode)
            .cloned()
            .unwrap_or(KeyAction::Key(keycode));
        let (result, held) =
            action.emit(keycode, &mut self.remap.context(&mut self.layer_stack, at));
        if let Some(held) = held {
            self.held_keys.insert(keycode, held);
        }
        events.extend(result.into_events());
        ProcessResult::from_events(events)
    }

    pub(super) fn process_key_release(&mut self, keycode: KeyCode, at: Instant) -> ProcessResult {
        self.remap
            .adaptive
            .record_key_release(keycode, at, self.layer_stack.is_game_mode_active());
        let mut events = self.settle_tap_dances(at);
        if let Some(held) = self.held_keys.remove(&keycode) {
            let ctx = self.remap.context(&mut self.layer_stack, at);
            match handle_action_release(held, keycode, ctx) {
                ProcessResult::EmitKey(key, pressed) => events.push((key, pressed)),
                ProcessResult::MultipleEvents(more) => events.extend(more),
                // Letting go of a key only ever lets go of keys
                _ => {}
            }
        }
        ProcessResult::from_events(events)
    }

    /// Perform what time has decided for the tap-dances as of `at`: a first press down past
    /// the term is pressed and held, a tap whose window passed is tapped.
    pub(super) fn settle_tap_dances(&mut self, at: Instant) -> Vec<(KeyCode, bool)> {
        let due = self.remap.dt.due(at);
        self.perform_due(due, at)
    }

    fn perform_due(&mut self, due: Vec<Due>, at: Instant) -> Vec<(KeyCode, bool)> {
        let mut events = Vec::new();
        for item in due {
            let mut ctx = self.remap.context(&mut self.layer_stack, at);
            match item {
                Due::Hold(key, action) => {
                    let (result, held) = action.emit(key, &mut ctx);
                    events.extend(result.into_events());
                    match self.held_keys.get_mut(&key) {
                        // The key is down, so its release lets go of what was just pressed
                        Some(HeldAction::DtManaged(pressed)) => {
                            *pressed = held.map(|held| Box::new((action, held)));
                        }
                        // Not held as a tap-dance of its own (inside a MULTI): nothing would
                        // release it, so it is tapped instead of left down
                        _ => {
                            if let Some(held) = held {
                                events.extend(action.unemit(held, key, &mut ctx).into_events());
                            }
                        }
                    }
                }
                Due::Tap(key, action) => {
                    let (result, held) = action.emit(key, &mut ctx);
                    events.extend(result.into_events());
                    if let Some(held) = held {
                        events.extend(action.unemit(held, key, &mut ctx).into_events());
                    }
                }
            }
        }
        events
    }

    fn lookup_action(&self, keycode: KeyCode) -> Option<&KeyAction> {
        for layer in self.layer_stack.layers().iter().rev() {
            if let Some(action) = self
                .layer_stack
                .layer_configs()
                .get(layer)
                .and_then(|config| config.remaps.get(&keycode))
            {
                if !action.is_transparent() {
                    return Some(action);
                }
            }
        }
        // Game mode only replaces the base layer; active layers still take priority
        if self.layer_stack.is_game_mode_active() {
            if let Some(action) = self.layer_stack.game_mode_remaps().get(&keycode) {
                return Some(action);
            }
        }
        self.layer_stack.base_remaps().get(&keycode)
    }
}
