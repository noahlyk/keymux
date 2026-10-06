use super::adaptive::AdaptiveProcessor;
use crate::config::{Config, KeyAction, Layer, LayerKind};
use crate::event_processor::actions::{
    handle_action_release, EmitResult, HandleContext, HeldAction, ProcessResult, TdResolution,
};
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use crate::steno::translate::StenoOutput;
use crate::steno::setup::Cooldown;
use crate::steno::{self, StenoEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::error;

/// Keys that always reach the screen on a steno layer, without a remap. Backspace
/// and Delete are the ones a steno user needs to correct text.
const STENO_PASSTHROUGH: [KeyCode; 2] = [KeyCode::KC_BSPC, KeyCode::KC_DEL];

fn steno_result(output: Option<StenoOutput>) -> ProcessResult {
    match output {
        None => ProcessResult::None,
        Some(StenoOutput::Type(text)) => ProcessResult::TypeString(text, false),
        Some(StenoOutput::Retype { backspaces, text }) => {
            ProcessResult::Retype { backspaces, text }
        }
    }
}

#[cfg(test)]
impl KeymapProcessor {
    /// Block until every steno layer's dictionaries have loaded.
    pub(crate) fn wait_for_steno_dictionaries(&mut self) {
        for engine in self.steno_engines.values_mut() {
            engine.wait_until_loaded();
        }
    }

    pub(crate) fn current_layer_name(&self) -> String {
        self.layer_stack.current_layer().0
    }

    pub(crate) fn deactivate_layer_for_test(&mut self, layer: &Layer) {
        self.layer_stack.deactivate_layer(layer);
    }
}

pub struct KeymapProcessor {
    held_keys: HashMap<KeyCode, Vec<HeldAction>>,
    layer_stack: LayerStack,
    mt_processor: crate::event_processor::actions::MtProcessor,
    dt_processor: crate::event_processor::actions::DtProcessor,
    osm_processor: crate::event_processor::actions::OsmProcessor,
    socd_processor: crate::event_processor::actions::SocdProcessor,
    adaptive_processor: AdaptiveProcessor,
    /// Chord state for each steno layer, keyed by layer name
    steno_engines: HashMap<Layer, StenoEngine>,
    /// The steno layer that was on top at the last key press, if any
    steno_active: Option<Layer>,
    /// Limits the "steno isn't set up" notification
    setup_hint: Cooldown,
    config_dir: PathBuf,
    user_id: u32,
}

impl KeymapProcessor {
    #[must_use]
    pub fn new(config: &Config, config_path: PathBuf, user_id: u32) -> Self {
        let config_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let home = crate::get_user_home_dir(user_id).ok();
        let mut steno_engines = HashMap::new();
        for (layer, layer_config) in &config.layers {
            let LayerKind::Steno(steno) = &layer_config.kind else {
                continue;
            };
            match StenoEngine::from_config(
                steno,
                layer_config.disabled_in_game_mode,
                &config_dir,
                home.as_deref(),
            ) {
                Ok(engine) => {
                    steno_engines.insert(layer.clone(), engine);
                }
                // Left out of steno_engines, so the layer behaves as a plain remap layer
                Err(e) => error!("steno layer \"{}\" disabled: {e:#}", layer.0),
            }
        }

        Self {
            held_keys: HashMap::new(),
            layer_stack: LayerStack::new(config),
            mt_processor: crate::event_processor::actions::MtProcessor::new(config),
            dt_processor: crate::event_processor::actions::DtProcessor::new(config),
            osm_processor: crate::event_processor::actions::OsmProcessor::new(config),
            socd_processor: crate::event_processor::actions::SocdProcessor::from_config(config),
            adaptive_processor: AdaptiveProcessor::new(),
            steno_engines,
            steno_active: None,
            setup_hint: Cooldown::new(Duration::from_secs(3)),
            config_dir,
            user_id,
        }
    }

    pub fn set_game_mode(&mut self, active: bool) {
        self.layer_stack.set_game_mode(active);
        self.mt_processor.set_game_mode(active);
    }

    pub fn check_dt_timeouts(&mut self) -> ProcessResult {
        let events = self.dt_processor.handle_check_timeouts();
        if events.is_empty() {
            ProcessResult::None
        } else {
            ProcessResult::MultipleEvents(events)
        }
    }

    /// Finish any steno stroke that has been held past its timeout, or translate
    /// held strokes that have gone idle. Returns what to type, if anything.
    pub fn check_steno_timeouts(&mut self) -> ProcessResult {
        let now = Instant::now();
        let output = self
            .steno_engines
            .values_mut()
            .find_map(|engine| engine.tick(now));
        steno_result(output)
    }

    pub fn get_held_keys(&self) -> Vec<KeyCode> {
        self.held_keys.keys().copied().collect()
    }

    pub fn save_adaptive_stats(&self, user_id: u32) -> Result<(), std::io::Error> {
        self.adaptive_processor.save_adaptive_stats(user_id)
    }

    pub fn load_adaptive_stats(&mut self, user_id: u32) -> Result<(), std::io::Error> {
        self.adaptive_processor.load_adaptive_stats(user_id)
    }

    pub fn get_all_key_stats(
        &self,
    ) -> HashMap<KeyCode, crate::event_processor::actions::RollingStats> {
        self.adaptive_processor.get_all_key_stats()
    }

    pub fn process_key(&mut self, keycode: KeyCode, pressed: bool) -> ProcessResult {
        let result = match self.steno_capture(keycode, pressed) {
            Some(result) => result,
            None if pressed => self.process_key_press(keycode),
            None => self.process_key_release(keycode),
        };
        if pressed {
            self.steno_setup_hint();
        }
        result
    }

    /// On a steno layer that has no dictionary yet, point the user at setup.
    /// Rate-limited so holding a key doesn't spam notifications.
    fn steno_setup_hint(&mut self) {
        let top = self.layer_stack.current_layer();
        let needs_setup = self
            .steno_engines
            .get(&top)
            .is_some_and(|engine| engine.needs_setup());
        if needs_setup && self.setup_hint.ready(Instant::now()) {
            steno::setup::notify_user(
                self.user_id,
                "Steno isn't set up",
                "Run: keymux steno setup",
            );
        }
    }

    /// Route a key to the top layer if that layer is steno. Returns `None` when the
    /// key should go through the normal remap path.
    ///
    /// On a steno layer, stroke keys are captured. Other keys use the layer's remaps
    /// if it has one, and are swallowed otherwise so they don't leak to layers below.
    fn steno_capture(&mut self, keycode: KeyCode, pressed: bool) -> Option<ProcessResult> {
        if !pressed {
            // Releases belong to whoever pressed the key, even if its layer is gone now
            match self.held_keys.get(&keycode)?.first()? {
                HeldAction::StenoManaged(layer) => {
                    let layer = layer.clone();
                    self.held_keys.remove(&keycode);
                    let output = self.steno_engines.get_mut(&layer)?.release(keycode);
                    return Some(steno_result(output));
                }
                // A swallowed press must not produce a release on its own
                HeldAction::SwallowedBySteno => {
                    self.held_keys.remove(&keycode);
                    return Some(ProcessResult::None);
                }
                _ => return None,
            }
        }

        let top = self.layer_stack.current_layer();
        // Entering a steno layer starts a fresh sentence, so its first word has no leading space
        if self.steno_active.as_ref() != Some(&top) {
            if let Some(engine) = self.steno_engines.get_mut(&top) {
                engine.start_fresh();
            }
            self.steno_active = self.steno_engines.contains_key(&top).then(|| top.clone());
        }
        let game_mode = self.layer_stack.is_game_mode_active();
        let engine = self.steno_engines.get_mut(&top)?;
        if game_mode && engine.disabled_in_game_mode {
            return None;
        }

        if engine.owns(keycode) {
            engine.press(keycode, Instant::now());
            self.held_keys
                .insert(keycode, vec![HeldAction::StenoManaged(top)]);
            return Some(ProcessResult::None);
        }

        // Editing keys reach the screen as usual. Backspace also tells the translator,
        // so `*` never deletes text that Backspace already removed.
        if keycode == KeyCode::KC_BSPC {
            engine.backspace();
        }
        if STENO_PASSTHROUGH.contains(&keycode) {
            return None;
        }

        let has_remap = self
            .layer_stack
            .layer_configs()
            .get(&top)
            .is_some_and(|config| config.remaps.contains_key(&keycode));
        if has_remap {
            return None;
        }

        // Space types a space and the translator records it, so the next word doesn't
        // add a second one. The press is typed here, so its release has nothing to do.
        if keycode == KeyCode::KC_SPC {
            self.held_keys
                .insert(keycode, vec![HeldAction::SwallowedBySteno]);
            return Some(steno_result(Some(engine.space())));
        }

        // On a steno layer, any other key does nothing unless the layer remaps it. Its
        // press and release are both swallowed, so it can't leak a stray key event.
        self.held_keys
            .insert(keycode, vec![HeldAction::SwallowedBySteno]);
        Some(ProcessResult::None)
    }

    fn process_key_press(&mut self, keycode: KeyCode) -> ProcessResult {
        self.adaptive_processor.record_key_press(keycode);

        let dt_timeout_events = self.dt_processor.handle_check_timeouts();

        // Notify DT of other key press for permissive hold
        let dt_permissive_events = self.dt_processor.on_other_key_press(keycode);

        let action = self.lookup_action(keycode).cloned();

        let (result, key_action) = match action {
            Some(KeyAction::DT(tap_action, double_tap_action)) => {
                self.handle_dt_press(keycode, &tap_action, &double_tap_action)
            }
            Some(action) => {
                let mut ctx = self.make_context();
                action.emit(keycode, &mut ctx)
            }
            None => {
                let mut ctx = self.make_context();
                KeyAction::Key(keycode).emit(keycode, &mut ctx)
            }
        };

        if let Some(ka) = key_action {
            self.held_keys.insert(keycode, vec![ka]);
        }

        // Combine timeout events and permissive hold events
        let mut all_dt_events = dt_timeout_events;
        all_dt_events.extend(dt_permissive_events);

        self.combine_with_timeouts(all_dt_events, result.to_process_result())
    }

    fn handle_dt_press(
        &mut self,
        keycode: KeyCode,
        tap_action: &KeyAction,
        double_tap_action: &KeyAction,
    ) -> (EmitResult, Option<HeldAction>) {
        let resolution =
            self.dt_processor
                .resolve_action(keycode, tap_action, double_tap_action, false);

        match resolution {
            TdResolution::EmitAction(action) => {
                let mut ctx = self.make_context();
                let (emit_result, held) = action.emit(keycode, &mut ctx);
                (emit_result, held)
            }
            TdResolution::HoldFirst => {
                let mut ctx = self.make_context();
                let (emit_result, held) = tap_action.emit(keycode, &mut ctx);
                (emit_result, held)
            }
            _ => (
                EmitResult::None,
                Some(HeldAction::DtManaged {
                    tap_action: (*tap_action).clone(),
                    double_tap_action: (*double_tap_action).clone(),
                }),
            ),
        }
    }

    fn process_key_release(&mut self, keycode: KeyCode) -> ProcessResult {
        self.adaptive_processor
            .record_key_release(keycode, self.layer_stack.is_game_mode_active());

        let dt_timeout_events = self.dt_processor.handle_check_timeouts();

        if let Some(actions) = self.held_keys.remove(&keycode) {
            let mut events = Vec::new();

            for action in actions {
                let ctx = self.make_context();
                let result = handle_action_release(action, keycode, ctx);

                match result {
                    ProcessResult::EmitKey(key, pressed) => events.push((key, pressed)),
                    ProcessResult::MultipleEvents(mut evts) => events.append(&mut evts),
                    ProcessResult::TapKeyPressRelease(key) => {
                        events.push((key, true));
                        events.push((key, false));
                        return self.combine_with_timeouts(
                            dt_timeout_events,
                            ProcessResult::MultipleEvents(events),
                        );
                    }
                    ProcessResult::None => {}
                    _ => {}
                }
            }

            if events.is_empty() {
                ProcessResult::None
            } else if events.len() == 1 {
                ProcessResult::EmitKey(events[0].0, events[0].1)
            } else {
                ProcessResult::MultipleEvents(events)
            }
        } else {
            ProcessResult::None
        }
    }

    fn make_context(&mut self) -> HandleContext<'_> {
        HandleContext {
            mt_processor: &mut self.mt_processor,
            dt_processor: &mut self.dt_processor,
            osm_processor: &mut self.osm_processor,
            socd_processor: &mut self.socd_processor,
            layer_stack: &mut self.layer_stack,
            config_dir: self.config_dir.clone(),
            user_id: self.user_id,
        }
    }

    fn lookup_action(&self, keycode: KeyCode) -> Option<&KeyAction> {
        if self.layer_stack.is_game_mode_active() {
            if let Some(action) = self.layer_stack.game_mode_remaps().get(&keycode) {
                return Some(action);
            }
        }

        for layer in self.layer_stack.layers().iter().rev() {
            if let Some(config) = self.layer_stack.layer_configs().get(layer) {
                if let Some(action) = config.remaps.get(&keycode) {
                    if action.is_transparent() {
                        continue;
                    }
                    return Some(action);
                }
            }
        }

        self.layer_stack.base_remaps().get(&keycode)
    }

    fn combine_with_timeouts(
        &self,
        timeout_events: Vec<(KeyCode, bool)>,
        result: ProcessResult,
    ) -> ProcessResult {
        if timeout_events.is_empty() {
            return result;
        }

        match result {
            ProcessResult::None => ProcessResult::MultipleEvents(timeout_events),
            ProcessResult::EmitKey(key, pressed) => {
                let mut all_events = timeout_events;
                all_events.push((key, pressed));
                ProcessResult::MultipleEvents(all_events)
            }
            ProcessResult::TapKeyPressRelease(key) => {
                let mut all_events = timeout_events;
                all_events.push((key, true));
                all_events.push((key, false));
                ProcessResult::MultipleEvents(all_events)
            }
            ProcessResult::MultipleEvents(mut events) => {
                let mut all_events = timeout_events;
                all_events.append(&mut events);
                ProcessResult::MultipleEvents(all_events)
            }
            other => other,
        }
    }
}
