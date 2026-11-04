//! The plain path: a key no steno layer captures does what its layer says - a key, a mod-tap,
//! a tap-dance, a one-shot modifier, an SOCD pair, a layer switch, a command - and what its
//! press left held is let go of the same way when it comes up.

use super::KeymapProcessor;
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{
    handle_action_release, DtProcessor, HandleContext, MtProcessor, OsmProcessor, ProcessResult,
    SocdProcessor,
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
    fn context<'a>(&'a mut self, layer_stack: &'a mut LayerStack, at: Instant) -> HandleContext<'a> {
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
        self.remap.dt.expire(at);
        let action = self
            .lookup_action(keycode)
            .cloned()
            .unwrap_or(KeyAction::Key(keycode));
        let (result, held) = action.emit(keycode, &mut self.remap.context(&mut self.layer_stack, at));
        if let Some(held) = held {
            self.held_keys.insert(keycode, held);
        }
        result.to_process_result()
    }

    pub(super) fn process_key_release(&mut self, keycode: KeyCode, at: Instant) -> ProcessResult {
        self.remap
            .adaptive
            .record_key_release(keycode, at, self.layer_stack.is_game_mode_active());
        self.remap.dt.expire(at);
        let Some(held) = self.held_keys.remove(&keycode) else {
            return ProcessResult::None;
        };
        let ctx = self.remap.context(&mut self.layer_stack, at);
        match handle_action_release(held, keycode, ctx) {
            ProcessResult::EmitKey(key, pressed) => ProcessResult::EmitKey(key, pressed),
            ProcessResult::MultipleEvents(events) => ProcessResult::from_events(events),
            // Letting go of a key only ever lets go of keys
            _ => ProcessResult::None,
        }
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
