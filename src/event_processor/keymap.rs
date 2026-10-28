use super::adaptive::AdaptiveProcessor;
use crate::config::{Config, KeyAction, Layer, LayerKind};
use crate::event_processor::actions::{
    handle_action_release, EmitResult, HandleContext, HeldAction, ProcessResult, TdResolution,
};
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use crate::steno::journal::{self, SharedHistory};
use crate::steno::lookup::{LookupInput, LookupKey};
use crate::steno::overlay::OverlayHub;
use crate::steno::setup::Cooldown;
use crate::steno::translate::StenoOutput;
use crate::steno::typing::{TypingLayout, Typist};
use crate::steno::{self, StenoEngine};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::error;

/// Keys that always reach the screen on a steno layer, without a remap.
const STENO_PASSTHROUGH: [KeyCode; 1] = [KeyCode::KC_DEL];

fn steno_result(output: Option<StenoOutput>) -> ProcessResult {
    match output {
        None => ProcessResult::None,
        Some(StenoOutput::Type(text)) => ProcessResult::TypeString(text, false),
        Some(StenoOutput::Retype { backspaces, text }) => {
            ProcessResult::Retype { backspaces, text }
        }
        Some(StenoOutput::Ops(ops)) => ProcessResult::Ops(ops),
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

    pub(crate) fn shared_history(&self) -> SharedHistory {
        std::sync::Arc::clone(&self.history)
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
    /// Where this keyboard's steno state is published for the overlay. One hub is shared by
    /// every keyboard: the overlay follows whichever was touched last.
    overlay_hub: &'static OverlayHub,
    /// Reads the keys that come out of the layers during a live lookup
    lookup_input: LookupInput,
    /// Keys pressed during a lookup, whose releases must stay out of the application too
    lookup_keys: HashSet<KeyCode>,
    /// Limits the "steno isn't set up" notification
    setup_hint: Cooldown,
    config_dir: PathBuf,
    user_id: u32,
    /// What is on screen, shared by every keyboard of this user and every steno layer
    history: SharedHistory,
    /// Follows the keys that reach the application from plain layers, into the history. `None`
    /// when there is no steno layer to use it, or it is turned off.
    typist: Option<Typist>,
    /// Where finished words typed by hand are noted, next to the strokes
    tape: Option<PathBuf>,
}

impl KeymapProcessor {
    #[must_use]
    pub fn new(config: &Config, config_path: PathBuf, user_id: u32) -> Self {
        // Tests keep to their own history; the daemon shares one among a user's keyboards
        let history = if cfg!(test) {
            journal::new_shared(steno::formatter::Formatter::default())
        } else {
            journal::for_user(user_id)
        };
        Self::with_history(config, config_path, user_id, history)
    }

    /// A keyboard that writes to the history `history`, which other keyboards share.
    #[must_use]
    pub fn with_history(
        config: &Config,
        config_path: PathBuf,
        user_id: u32,
        history: SharedHistory,
    ) -> Self {
        let config_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let home = crate::get_user_home_dir(user_id).ok();
        // A steno layer needs a place to drop dictionaries, so make it before anything looks
        if config
            .layers
            .values()
            .any(|layer| matches!(layer.kind, LayerKind::Steno(_)))
        {
            steno::setup::prepare_dir(&config_dir);
        }
        let mut steno_engines = HashMap::new();
        let mut typist = None;
        let mut layout = None;
        // Layers in name order, so which one sets the typing layout doesn't change from run to run
        let mut layers: Vec<_> = config.layers.iter().collect();
        layers.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        for (layer, layer_config) in layers {
            let LayerKind::Steno(steno) = &layer_config.kind else {
                continue;
            };
            if layout.is_none() {
                layout = Some(
                    TypingLayout::parse(&steno.typing_layout).unwrap_or_else(|| {
                        error!(
                            "steno typing_layout \"{}\" is not known, so us is used",
                            steno.typing_layout
                        );
                        TypingLayout::Us
                    }),
                );
            }
            if typist.is_none() && !steno.disable_typing_history {
                typist = layout.map(Typist::new);
            }
            match StenoEngine::from_config(
                steno,
                layer_config.disabled_in_game_mode,
                &config_dir,
                home.as_deref(),
                &history,
                &layer.0,
            ) {
                Ok(engine) => {
                    steno_engines.insert(layer.clone(), engine);
                }
                // Left out of steno_engines, so the layer behaves as a plain remap layer
                Err(e) => error!("steno layer \"{}\" disabled: {e:#}", layer.0),
            }
        }

        let layout = layout.unwrap_or_default();
        let processor = Self {
            held_keys: HashMap::new(),
            layer_stack: LayerStack::new(config),
            mt_processor: crate::event_processor::actions::MtProcessor::new(config),
            dt_processor: crate::event_processor::actions::DtProcessor::new(config),
            osm_processor: crate::event_processor::actions::OsmProcessor::new(config),
            socd_processor: crate::event_processor::actions::SocdProcessor::from_config(config),
            adaptive_processor: AdaptiveProcessor::new(),
            steno_engines,
            steno_active: None,
            overlay_hub: OverlayHub::global(),
            lookup_input: LookupInput::new(layout),
            lookup_keys: HashSet::new(),
            setup_hint: Cooldown::new(Duration::from_secs(3)),
            config_dir,
            user_id,
            history,
            typist,
            tape: home.as_deref().map(steno::tape::default_path),
        };
        // A keyboard that starts on a steno layer (`default_layer`) shows the overlay now,
        // not on its first key press. One that starts on base publishes nothing yet, so it
        // can't hide the overlay for a steno keyboard that started first; it takes over
        // when its first key is touched.
        if processor.overlay_layer_name().is_some() {
            processor.publish_overlay_layer();
        }
        processor
    }

    /// Publish to `hub` instead of the daemon's, from here on. Tests use this so they
    /// don't share state through the global hub.
    #[cfg(test)]
    pub(crate) fn set_overlay_hub(&mut self, hub: &'static OverlayHub) {
        self.overlay_hub = hub;
        for engine in self.steno_engines.values_mut() {
            engine.set_hub(hub);
        }
        if self.overlay_layer_name().is_some() {
            self.publish_overlay_layer();
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
        if let Some(result) = self.lookup_capture(keycode, pressed) {
            self.publish_overlay_layer();
            return result;
        }
        let result = self.steno_capture(keycode, pressed).unwrap_or_else(|| {
            let result = if pressed {
                self.process_key_press(keycode)
            } else {
                self.process_key_release(keycode)
            };
            // What reached the application from a plain layer, steno never typed
            self.watch_typing(&result);
            result
        });
        if pressed {
            self.steno_setup_hint();
        }
        self.publish_overlay_layer();
        result
    }

    /// Follow what a plain layer sent to the application into the history, so steno carries on
    /// from it and `*` can take it back.
    fn watch_typing(&mut self, result: &ProcessResult) {
        if self.layer_stack.is_game_mode_active() {
            return;
        }
        let Some(typist) = self.typist.as_mut() else {
            return;
        };
        let mut history = journal::lock(&self.history);
        match result {
            ProcessResult::EmitKey(key, pressed) => typist.key(&mut history, *key, *pressed),
            ProcessResult::TapKeyPressRelease(key) => {
                typist.key(&mut history, *key, true);
                typist.key(&mut history, *key, false);
            }
            ProcessResult::MultipleEvents(events) => {
                for (key, pressed) in events {
                    typist.key(&mut history, *key, *pressed);
                }
            }
            // Text a layer types for you is on screen like any other
            ProcessResult::TypeString(text, _) => {
                for ch in text.chars() {
                    history.typed(ch);
                }
            }
            ProcessResult::Retype { .. } | ProcessResult::Ops(_) | ProcessResult::None => {}
        }
        let words = std::mem::take(&mut history.finished_words);
        drop(history);
        let Some(path) = self.tape.clone() else {
            return;
        };
        // The first layer's dictionary, by name, once it has loaded. Looking a word up in it takes
        // a moment, so that happens on a thread of its own and never holds up a key.
        let dict = self
            .steno_engines
            .iter()
            .min_by(|a, b| a.0 .0.cmp(&b.0 .0))
            .and_then(|(_, engine)| engine.dictionary_handle());
        for word in words {
            let path = path.clone();
            let dict = dict.clone();
            std::thread::spawn(move || {
                let line = dict.map_or_else(
                    || format!("{word:?}"),
                    |dict| steno::tape::describe_typed(&dict, &word),
                );
                steno::tape::append_note(&path, "typed", &line);
            });
        }
    }

    /// The steno layer that is on top for this keyboard, if any: what the overlay should show.
    fn overlay_layer_name(&self) -> Option<&str> {
        // An open lookup keeps the overlay on its layer while a layer is held above it
        let layers = self.layer_stack.layers();
        if let Some((layer, _)) = self
            .steno_engines
            .iter()
            .find(|(layer, engine)| engine.lookup_active() && layers.contains(layer))
        {
            return Some(layer.0.as_str());
        }
        let game_mode = self.layer_stack.is_game_mode_active();
        self.layer_stack
            .layers()
            .last()
            .filter(|layer| {
                self.steno_engines
                    .get(*layer)
                    .is_some_and(|engine| !(game_mode && engine.disabled_in_game_mode))
            })
            .map(|layer| layer.0.as_str())
    }

    /// Tell the overlay about this keyboard's steno state. It runs on every key event, so
    /// whichever keyboard was touched last decides what the overlay shows: a steno keyboard
    /// shows it, and touching one that isn't in steno hides it. This also shows it on the
    /// press that switches into steno, not on the next stroke. The hub ignores a repeat of
    /// what it already has, so the common case is one uncontended lock and a comparison.
    fn publish_overlay_layer(&self) {
        self.overlay_hub.set_layer(self.overlay_layer_name());
    }

    /// A stroke that wrote `{plover:toggle}` or `{plover:suspend}` leaves the steno layer, so the
    /// keys type normally again.
    fn leave_if_asked(&mut self, layer: &Layer) {
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
    fn steno_setup_hint(&mut self) {
        let top = self.layer_stack.current_layer();
        let needs_setup = self
            .steno_engines
            .get(&top)
            .is_some_and(|engine| engine.needs_setup());
        if needs_setup && self.setup_hint.ready(Instant::now()) {
            steno::setup::notify_user(
                self.user_id,
                "Steno has no dictionary",
                "Strokes type as they are. Run: keymux steno theory lapwing (or plover), \
                 or drop dictionaries into ~/.config/keymux/steno",
            );
        }
    }

    /// While a live lookup is open, every key goes through the layers as it always does (remaps,
    /// mod-taps, held layers), and what comes out types into the lookup instead of reaching the
    /// application. Esc and Enter close it, even where a layer remaps them. Returns `None` when
    /// no lookup is open, or the key is not one for it (a stroke key let go of, say).
    fn lookup_capture(&mut self, keycode: KeyCode, pressed: bool) -> Option<ProcessResult> {
        let open = self
            .steno_engines
            .iter()
            .find_map(|(layer, engine)| engine.lookup_active().then(|| layer.clone()));
        let Some(layer) = open else {
            // Let go of after the lookup closed: still not the application's business
            if !pressed && self.lookup_keys.remove(&keycode) {
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
            if !self.lookup_keys.remove(&keycode) {
                return None;
            }
            let result = self.process_key_release(keycode);
            self.read_lookup_keys(&result);
            return Some(ProcessResult::None);
        }

        if matches!(keycode, KeyCode::KC_ESC | KeyCode::KC_ENT) {
            self.close_lookup(&layer);
            // Swallowed on release too, so it can't leak a stray key event
            self.held_keys
                .insert(keycode, vec![HeldAction::SwallowedBySteno]);
            return Some(ProcessResult::None);
        }

        self.lookup_keys.insert(keycode);
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
        self.lookup_input.reset();
    }

    /// What came out of the layers for a key during a lookup, as input to it.
    fn read_lookup_keys(&mut self, result: &ProcessResult) {
        let mut keys = Vec::new();
        match result {
            ProcessResult::EmitKey(key, pressed) => {
                keys.push(self.lookup_input.key(*key, *pressed))
            }
            ProcessResult::TapKeyPressRelease(key) => {
                keys.push(self.lookup_input.key(*key, true));
                keys.push(self.lookup_input.key(*key, false));
            }
            ProcessResult::MultipleEvents(events) => {
                for (key, pressed) in events {
                    keys.push(self.lookup_input.key(*key, *pressed));
                }
            }
            // Text a layer types for you goes into the query like typed keys
            ProcessResult::TypeString(text, _) => {
                keys.extend(
                    text.chars()
                        .filter(|c| !matches!(c, '\n' | '\t'))
                        .map(|c| Some(LookupKey::Char(c))),
                );
            }
            ProcessResult::Retype { .. } | ProcessResult::Ops(_) | ProcessResult::None => {}
        }
        let Some(engine) = self
            .steno_engines
            .values_mut()
            .find(|engine| engine.lookup_active())
        else {
            return;
        };
        for key in keys.into_iter().flatten() {
            engine.lookup_input(key);
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
                    self.leave_if_asked(&layer);
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
        // Entering a steno layer carries on from what was typed, unless the layer asks to start over
        if self.steno_active.as_ref() != Some(&top) {
            if let Some(engine) = self.steno_engines.get_mut(&top) {
                engine.enter();
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

        let has_remap = self
            .layer_stack
            .layer_configs()
            .get(&top)
            .is_some_and(|config| config.remaps.contains_key(&keycode));
        if STENO_PASSTHROUGH.contains(&keycode) || has_remap {
            // This key reaches the application, and may move the caret or change the text
            // behind the translator's back (arrows, Delete...). The typist sees it go by and
            // makes the history let go of what it remembers, or follows it if it types text.
            return None;
        }

        // Space and Backspace don't type on a steno layer, even with nothing on the tape, so an
        // accidental press can't wipe text. `*` is the only way to take text back.
        if matches!(keycode, KeyCode::KC_SPC | KeyCode::KC_BSPC) {
            self.held_keys
                .insert(keycode, vec![HeldAction::SwallowedBySteno]);
            return Some(ProcessResult::None);
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

        // Game mode only replaces the base layer; active layers still take priority
        if self.layer_stack.is_game_mode_active() {
            if let Some(action) = self.layer_stack.game_mode_remaps().get(&keycode) {
                return Some(action);
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
