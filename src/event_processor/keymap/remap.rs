//! The plain path: a key no steno layer captures does what its layer says - a key, a mod-tap,
//! a tap-dance, a one-shot modifier, an SOCD pair, a layer switch, a command - and what its
//! press left held is let go of the same way when it comes up.

use super::KeymapProcessor;
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{
    handle_action_release, DtProcessor, EmitResult, HandleContext, HeldAction, MtProcessor,
    OsmProcessor, ProcessResult, SocdProcessor, TdResolution,
};
use crate::event_processor::adaptive::AdaptiveProcessor;
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use std::path::PathBuf;

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

    /// Everything an action runs with, borrowed together.
    fn context<'a>(&'a mut self, layer_stack: &'a mut LayerStack) -> HandleContext<'a> {
        HandleContext {
            mt_processor: &mut self.mt,
            dt_processor: &mut self.dt,
            osm_processor: &mut self.osm,
            socd_processor: &mut self.socd,
            layer_stack,
            config_dir: &self.config_dir,
            user_id: self.user_id,
        }
    }
}

impl KeymapProcessor {
    pub(super) fn process_key_press(&mut self, keycode: KeyCode) -> ProcessResult {
        self.remap.adaptive.record_key_press(keycode);
        // A tap-dance past its term, and one this key resolves (permissive hold), go out first
        let mut timeouts = self.remap.dt.handle_check_timeouts();
        timeouts.extend(self.remap.dt.on_other_key_press(keycode));

        let (result, held) = match self.lookup_action(keycode).cloned() {
            Some(KeyAction::DT(tap, double_tap)) => self.handle_dt_press(keycode, &tap, &double_tap),
            Some(action) => action.emit(keycode, &mut self.remap.context(&mut self.layer_stack)),
            None => KeyAction::Key(keycode)
                .emit(keycode, &mut self.remap.context(&mut self.layer_stack)),
        };
        if let Some(held) = held {
            self.held_keys.insert(keycode, held);
        }
        combine_with_timeouts(timeouts, result.to_process_result())
    }

    fn handle_dt_press(
        &mut self,
        keycode: KeyCode,
        tap: &KeyAction,
        double_tap: &KeyAction,
    ) -> (EmitResult, Option<HeldAction>) {
        match self.remap.dt.resolve_action(keycode, tap, double_tap, false) {
            TdResolution::EmitAction(action) => {
                action.emit(keycode, &mut self.remap.context(&mut self.layer_stack))
            }
            TdResolution::HoldFirst => {
                tap.emit(keycode, &mut self.remap.context(&mut self.layer_stack))
            }
            _ => (
                EmitResult::None,
                Some(HeldAction::DtManaged {
                    tap_action: tap.clone(),
                    double_tap_action: double_tap.clone(),
                }),
            ),
        }
    }

    pub(super) fn process_key_release(&mut self, keycode: KeyCode) -> ProcessResult {
        self.remap
            .adaptive
            .record_key_release(keycode, self.layer_stack.is_game_mode_active());
        let timeouts = self.remap.dt.handle_check_timeouts();
        let result = match self.held_keys.remove(&keycode) {
            Some(held) => {
                let ctx = self.remap.context(&mut self.layer_stack);
                match handle_action_release(held, keycode, ctx) {
                    ProcessResult::EmitKey(key, pressed) => ProcessResult::EmitKey(key, pressed),
                    ProcessResult::MultipleEvents(events) => ProcessResult::from_events(events),
                    // Letting go of a key only ever lets go of keys
                    _ => ProcessResult::None,
                }
            }
            None => ProcessResult::None,
        };
        combine_with_timeouts(timeouts, result)
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

/// Tap-dance events that resolved on this key go out before it, so what they typed is in order.
fn combine_with_timeouts(mut events: Vec<(KeyCode, bool)>, result: ProcessResult) -> ProcessResult {
    if events.is_empty() {
        return result;
    }
    match result {
        ProcessResult::None => ProcessResult::MultipleEvents(events),
        ProcessResult::EmitKey(key, pressed) => {
            events.push((key, pressed));
            ProcessResult::MultipleEvents(events)
        }
        ProcessResult::MultipleEvents(more) => {
            events.extend(more);
            ProcessResult::MultipleEvents(events)
        }
        other => other,
    }
}
