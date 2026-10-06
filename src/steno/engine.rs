//! One steno layer's runtime state: which physical keys are stroke keys, the
//! chord in progress, and the translator that turns strokes into text.

use super::chord::ChordState;
use super::dict::Dictionary;
use super::layout::{build_key_map, preset};
use super::setup::default_dictionaries;
use super::tape;
use super::translate::{StenoOutput, Translator};
use crate::config::{KeyAction, StenoConfig};
use crate::keycode::KeyCode;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct StenoEngine {
    keys: HashMap<KeyCode, u32>,
    /// Stroke keys physically down, so a release can tell which of their bits are still pressed
    held_keys: HashSet<KeyCode>,
    chord: ChordState,
    translator: Translator,
    /// Dictionaries still loading on a background thread
    loading: Option<Receiver<Dictionary>>,
    /// Strokes finished before the dictionaries arrived. Translated once they do.
    backlog: Vec<u32>,
    /// Where each stroke is recorded. `None` when the home directory is unknown.
    tape: Option<PathBuf>,
    pub disabled_in_game_mode: bool,
}

impl StenoEngine {
    /// Build an engine from a layer's steno config.
    ///
    /// Dictionaries load on a background thread, so the daemon doesn't wait on
    /// a large file. Until they arrive, strokes wait in the chord.
    ///
    /// `home` is the session user's home directory, used to expand `~/` in
    /// dictionary paths and to place the tape. Relative dictionary paths are
    /// resolved against `config_dir`.
    pub fn from_config(
        config: &StenoConfig,
        disabled_in_game_mode: bool,
        config_dir: &Path,
        home: Option<&Path>,
    ) -> Result<Self> {
        let preset = preset(&config.layout).map_err(|e| anyhow!(e))?;
        let overrides: HashMap<String, KeyAction> = config.layout_overrides.clone();
        let keys =
            build_key_map(&preset, &overrides).map_err(|errors| anyhow!(errors.join("; ")))?;

        // Without an explicit list, use what `keymux steno setup` installed
        let paths: Vec<PathBuf> = if config.dictionaries.is_empty() {
            default_dictionaries(config_dir)
        } else {
            config
                .dictionaries
                .iter()
                .map(|raw| resolve_dictionary_path(raw, config_dir, home))
                .collect()
        };
        let (sender, loading) = mpsc::channel();
        std::thread::spawn(move || {
            // The engine may be dropped before this finishes; nothing to do then
            let _ = sender.send(Dictionary::load(&paths));
        });

        Ok(Self {
            keys,
            held_keys: HashSet::new(),
            chord: ChordState::new(Duration::from_millis(u64::from(config.stroke_timeout_ms))),
            translator: Translator::new(Dictionary::default()),
            loading: Some(loading),
            backlog: Vec::new(),
            tape: home.map(tape::default_path),
            disabled_in_game_mode,
        })
    }

    /// Whether dictionaries are loaded. False means `keymux steno setup` hasn't run.
    /// Still loading counts as set up, so the hint doesn't fire during startup.
    #[must_use]
    pub fn needs_setup(&self) -> bool {
        self.loading.is_none() && !self.translator.has_dictionary()
    }

    /// Whether this physical key is a stroke key on this layer.
    #[must_use]
    pub fn owns(&self, key: KeyCode) -> bool {
        self.keys.contains_key(&key)
    }

    /// Start a fresh sentence, as when the layer is entered. See [`Translator::start_fresh`].
    pub fn start_fresh(&mut self) {
        self.translator.start_fresh();
    }

    /// A stroke key went down.
    pub fn press(&mut self, key: KeyCode, now: Instant) {
        if let Some(&bit) = self.keys.get(&key) {
            self.held_keys.insert(key);
            self.chord.press(bit, now);
        }
    }

    /// A stroke key came up. Returns output if this completed a stroke.
    pub fn release(&mut self, key: KeyCode) -> Option<StenoOutput> {
        let bit = *self.keys.get(&key)?;
        self.held_keys.remove(&key);
        // A combined key (V = A- and O-) shares bits with the keys still down, so keep those
        let still_held: u32 = self
            .held_keys
            .iter()
            .filter_map(|other| self.keys.get(other))
            .fold(0, |bits, &other| bits | other);
        let stroke = self.chord.release(bit & !still_held)?;
        self.on_stroke(stroke)
    }

    /// Backspace was pressed on this layer. It still reaches the screen, and the
    /// translator drops the character it removed.
    pub fn backspace(&mut self) {
        self.translator.backspace();
    }

    /// A space was typed on the steno layer. Returns what to type.
    pub fn space(&mut self) -> StenoOutput {
        self.translator.space()
    }

    /// Check the stuck-key timeout and pick up finished dictionary loads.
    /// Returns output if a stuck stroke was flushed, or if strokes queued during
    /// loading were just translated.
    pub fn tick(&mut self, now: Instant) -> Option<StenoOutput> {
        if let Some(output) = self.poll_dictionary() {
            return Some(output);
        }
        let stroke = self.chord.tick(now)?;
        self.on_stroke(stroke)
    }

    /// Install the dictionaries once they arrive, and translate what was queued.
    fn poll_dictionary(&mut self) -> Option<StenoOutput> {
        let loading = self.loading.as_ref()?;
        let dict = match loading.try_recv() {
            Ok(dict) => dict,
            Err(TryRecvError::Empty) => return None,
            // The loader thread died. Steno stays unset up instead of stuck loading.
            Err(TryRecvError::Disconnected) => Dictionary::default(),
        };
        self.loading = None;
        self.translator.set_dictionary(dict);
        let mut merged = Merged::default();
        for stroke in std::mem::take(&mut self.backlog) {
            if let Some(output) = self.translate(stroke) {
                merged.push(output);
            }
        }
        merged.finish()
    }

    fn on_stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        if self.loading.is_some() {
            self.backlog.push(stroke);
            return None;
        }
        self.translate(stroke)
    }

    fn translate(&mut self, stroke: u32) -> Option<StenoOutput> {
        let output = self.translator.stroke(stroke);
        if let Some(path) = &self.tape {
            tape::append(
                path,
                stroke,
                &tape::describe(output.as_ref(), self.translator.was_unmatched()),
            );
        }
        output
    }

    /// Block until the dictionaries are loaded. Tests use this so they don't race
    /// the loader thread.
    #[cfg(test)]
    pub fn wait_until_loaded(&mut self) {
        if let Some(loading) = self.loading.take() {
            if let Ok(dict) = loading.recv() {
                self.translator.set_dictionary(dict);
            }
        }
    }
}

/// Several outputs combined into one, so a backlog replays as a single edit.
#[derive(Default)]
struct Merged {
    backspaces: usize,
    text: String,
}

impl Merged {
    fn push(&mut self, output: StenoOutput) {
        match output {
            StenoOutput::Type(text) => self.text.push_str(&text),
            StenoOutput::Retype { backspaces, text } => {
                let mut remaining = backspaces;
                while remaining > 0 && self.text.pop().is_some() {
                    remaining -= 1;
                }
                // Whatever is left was already on screen
                self.backspaces += remaining;
                self.text.push_str(&text);
            }
        }
    }

    fn finish(self) -> Option<StenoOutput> {
        match (self.backspaces, self.text.is_empty()) {
            (0, true) => None,
            (0, false) => Some(StenoOutput::Type(self.text)),
            (backspaces, _) => Some(StenoOutput::Retype {
                backspaces,
                text: self.text,
            }),
        }
    }
}

/// Resolve a dictionary path from config: `~/` goes to `home`, relative paths to `config_dir`.
#[must_use]
pub fn resolve_dictionary_path(raw: &str, config_dir: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (raw.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_dict(json: &str) -> StenoEngine {
        let mut dict = Dictionary::default();
        dict.merge_json(json).unwrap();
        StenoEngine {
            keys: build_key_map(&preset("qwerty").unwrap(), &HashMap::new()).unwrap(),
            held_keys: HashSet::new(),
            chord: ChordState::new(Duration::from_secs(1)),
            translator: Translator::new(dict),
            loading: None,
            backlog: Vec::new(),
            tape: None,
            disabled_in_game_mode: true,
        }
    }

    fn key_for(engine: &StenoEngine, bit: u32) -> KeyCode {
        *engine.keys.iter().find(|(_, b)| **b == bit).unwrap().0
    }

    #[test]
    fn combined_key_keeps_bits_that_another_held_key_still_presses() {
        let mut engine = engine_with_dict(r#"{"AO": "ao"}"#);
        let now = Instant::now();
        let a_bit = 1 << crate::steno::layout::slot_by_name("A-").unwrap();
        let o_bit = 1 << crate::steno::layout::slot_by_name("O-").unwrap();
        let x = key_for(&engine, a_bit);
        let v = key_for(&engine, a_bit | o_bit);

        // Letting go of X must not lift A-, since V still presses it
        engine.press(x, now);
        engine.press(v, now);
        assert!(engine.release(x).is_none());
        // V was the last key, so the stroke A- O- completes
        let out = engine.release(v);
        assert!(matches!(out, Some(StenoOutput::Type(ref text)) if text.trim() == "ao"));
    }

    #[test]
    fn chord_types_dictionary_text() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let k = key_for(&engine, 1 << 2);
        let a = key_for(&engine, 1 << 7);
        let t = key_for(&engine, 1 << 18);
        engine.press(k, now);
        engine.press(a, now);
        engine.press(t, now);
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(k), None);
        assert_eq!(
            engine.release(t),
            Some(StenoOutput::Type("cat".to_string()))
        );
    }

    #[test]
    fn unknown_stroke_types_nothing() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let k = key_for(&engine, 1 << 2);
        engine.press(k, now);
        assert_eq!(engine.release(k), None);
    }

    #[test]
    fn strokes_made_before_the_dictionary_arrives_type_once_it_does() {
        let mut engine = engine_with_dict("{}");
        let (sender, loading) = mpsc::channel();
        engine.loading = Some(loading);
        engine.translator = Translator::new(Dictionary::default());

        let now = Instant::now();
        let keys = [1 << 2, 1 << 7, 1 << 18].map(|bit| key_for(&engine, bit));
        for key in keys {
            engine.press(key, now);
        }
        for key in keys {
            // Still loading, so the stroke waits in the backlog
            assert_eq!(engine.release(key), None);
        }

        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat"}"#).unwrap();
        sender.send(dict).unwrap();
        assert_eq!(engine.tick(now), Some(StenoOutput::Type("cat".to_string())));
    }

    #[test]
    fn owns_only_stroke_keys() {
        let engine = engine_with_dict("{}");
        let k = key_for(&engine, 1 << 2);
        assert!(engine.owns(k));
        assert!(!engine.owns(KeyCode::KC_ESC));
    }

    #[test]
    fn dictionary_paths_resolve() {
        let config_dir = Path::new("/etc/keymux");
        let home = Path::new("/home/alice");
        assert_eq!(
            resolve_dictionary_path("~/steno/main.json", config_dir, Some(home)),
            PathBuf::from("/home/alice/steno/main.json")
        );
        assert_eq!(
            resolve_dictionary_path("main.json", config_dir, Some(home)),
            PathBuf::from("/etc/keymux/main.json")
        );
        assert_eq!(
            resolve_dictionary_path("/usr/share/x.json", config_dir, Some(home)),
            PathBuf::from("/usr/share/x.json")
        );
    }

    #[test]
    fn from_config_rejects_unknown_preset() {
        let config = StenoConfig {
            layout: "dvorak".to_string(),
            layout_overrides: HashMap::new(),
            dictionaries: Vec::new(),
            stroke_timeout_ms: 5000,
            translation_timeout_ms: None,
        };
        let err = StenoEngine::from_config(&config, true, Path::new("."), None).unwrap_err();
        assert!(err.to_string().contains("unknown steno layout preset"));
    }

    #[test]
    fn stroke_names_render_for_the_tape() {
        use crate::steno::layout::render_stroke;
        assert_eq!(render_stroke(1 << 2 | 1 << 7 | 1 << 18), "KAT");
    }
}
