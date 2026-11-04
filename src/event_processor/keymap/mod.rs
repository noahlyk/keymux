//! One keyboard's keymap: every key event comes through [`KeymapProcessor`].
//!
//! The plain path - a key's action on its layer: mod-taps, tap-dances, one-shot modifiers,
//! SOCD, layer switches, commands - is `remap`. A steno layer captures its stroke keys in
//! `steno`; a live lookup captures what the layers emit in `lookup`; `overlay` tells the
//! on-screen keyboard which steno layer is up; `typing` follows hand-typed text into the
//! steno history.

mod lookup;
mod overlay;
mod remap;
mod steno;
mod typing;

use self::lookup::LookupCapture;
use self::remap::Remap;
use self::typing::TypingWatch;
use crate::config::{Config, Layer, LayerKind};
use crate::event_processor::actions::{HeldAction, ProcessResult, RollingStats};
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use crate::steno::formatter::Formatter;
use crate::steno::journal::{self, SharedHistory};
use crate::steno::overlay::OverlayHub;
use crate::steno::setup::{self, Cooldown};
use crate::steno::typing::{TypingLayout, Typist};
use crate::steno::{tape, StenoEngine};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::error;

pub struct KeymapProcessor {
    /// What each key that is down will do when it comes up
    held_keys: HashMap<KeyCode, HeldAction>,
    layer_stack: LayerStack,
    remap: Remap,
    /// The engine of each steno layer, by layer name
    steno_engines: HashMap<Layer, StenoEngine>,
    /// The steno layer that was on top at the last key press, if any
    steno_active: Option<Layer>,
    /// Limits the "steno isn't set up" notification
    setup_hint: Cooldown,
    lookup: LookupCapture,
    /// Where this keyboard's steno state is published for the overlay. One hub is shared by
    /// every keyboard: the overlay follows whichever was touched last.
    overlay_hub: &'static OverlayHub,
    typing: TypingWatch,
}

impl KeymapProcessor {
    #[must_use]
    pub fn new(config: &Config, config_path: PathBuf, user_id: u32) -> Self {
        // Tests keep to their own history; the daemon shares one among a user's keyboards
        let history = if cfg!(test) {
            journal::new_shared(Formatter::default())
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
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

        let home = crate::get_user_home_dir(user_id).ok();
        // A steno layer needs a place to drop dictionaries, so make it before anything looks
        if config
            .layers
            .values()
            .any(|layer| matches!(layer.kind, LayerKind::Steno(_)))
        {
            setup::prepare_dir(&config_dir);
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

        let processor = Self {
            held_keys: HashMap::new(),
            layer_stack: LayerStack::new(config),
            remap: Remap::new(config, config_dir, user_id),
            steno_engines,
            steno_active: None,
            setup_hint: Cooldown::new(Duration::from_secs(3)),
            lookup: LookupCapture::new(layout.unwrap_or_default()),
            overlay_hub: OverlayHub::global(),
            typing: TypingWatch::new(
                typist,
                history,
                home.as_deref().map(tape::default_path),
            ),
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

    pub fn set_game_mode(&mut self, active: bool) {
        self.layer_stack.set_game_mode(active);
        self.remap.mt.set_game_mode(active);
    }

    pub fn check_dt_timeouts(&mut self) -> ProcessResult {
        let events = self.remap.dt.handle_check_timeouts();
        if events.is_empty() {
            ProcessResult::None
        } else {
            ProcessResult::MultipleEvents(events)
        }
    }

    /// When a timeout next needs checking with no key to prompt it: a tap-dance or a steno
    /// stroke that resolves by itself. `None` while nothing is waiting on one.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let steno = self
            .steno_engines
            .values()
            .filter_map(StenoEngine::next_deadline);
        self.remap
            .dt
            .next_deadline()
            .into_iter()
            .chain(steno)
            .min()
    }

    #[must_use]
    pub fn get_held_keys(&self) -> Vec<KeyCode> {
        self.held_keys.keys().copied().collect()
    }

    pub fn save_adaptive_stats(&self, user_id: u32) -> std::io::Result<()> {
        self.remap.adaptive.save_adaptive_stats(user_id)
    }

    pub fn load_adaptive_stats(&mut self, user_id: u32) -> std::io::Result<()> {
        self.remap.adaptive.load_adaptive_stats(user_id)
    }

    #[must_use]
    pub fn get_all_key_stats(&self) -> HashMap<KeyCode, RollingStats> {
        self.remap.adaptive.get_all_key_stats()
    }

    pub fn process_key(&mut self, keycode: KeyCode, pressed: bool) -> ProcessResult {
        self.process_key_at(keycode, pressed, Instant::now())
    }

    /// [`Self::process_key`] for an event that happened at `at`: when the keyboard reported
    /// it, which is earlier than now for events that waited to be read.
    pub fn process_key_at(&mut self, keycode: KeyCode, pressed: bool, at: Instant) -> ProcessResult {
        if let Some(result) = self.lookup_capture(keycode, pressed) {
            self.publish_overlay_layer();
            return result;
        }
        let result = self.steno_capture(keycode, pressed, at).unwrap_or_else(|| {
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
        self.typing.shared_history()
    }

    pub(crate) fn current_layer_name(&self) -> String {
        self.layer_stack.current_layer().0
    }

    pub(crate) fn deactivate_layer_for_test(&mut self, layer: &Layer) {
        self.layer_stack.deactivate_layer(layer);
    }
}
