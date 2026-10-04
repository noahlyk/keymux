//! One steno layer's runtime state: which physical keys are stroke keys, the
//! chord in progress, and the dictionary used to translate finished strokes.

use super::chord::ChordState;
use super::dict::Dictionary;
use super::layout::{build_key_map, preset, render_stroke};
use crate::config::{KeyAction, StenoConfig};
use crate::keycode::KeyCode;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug)]
pub struct StenoEngine {
    keys: HashMap<KeyCode, u32>,
    chord: ChordState,
    dict: Dictionary,
    pub disabled_in_game_mode: bool,
}

impl StenoEngine {
    /// Build an engine from a layer's steno config.
    ///
    /// `home` is the session user's home directory, used to expand `~/` in
    /// dictionary paths. Relative dictionary paths are resolved against `config_dir`.
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

        let paths: Vec<PathBuf> = config
            .dictionaries
            .iter()
            .map(|raw| resolve_dictionary_path(raw, config_dir, home))
            .collect();
        let dict = Dictionary::load(&paths);

        Ok(Self {
            keys,
            chord: ChordState::new(Duration::from_millis(u64::from(config.stroke_timeout_ms))),
            dict,
            disabled_in_game_mode,
        })
    }

    /// Whether this physical key is a stroke key on this layer.
    #[must_use]
    pub fn owns(&self, key: KeyCode) -> bool {
        self.keys.contains_key(&key)
    }

    /// A stroke key went down.
    pub fn press(&mut self, key: KeyCode, now: Instant) {
        if let Some(&bit) = self.keys.get(&key) {
            self.chord.press(bit, now);
        }
    }

    /// A stroke key came up. Returns text to type if this completed a stroke.
    pub fn release(&mut self, key: KeyCode) -> Option<String> {
        let bit = *self.keys.get(&key)?;
        let stroke = self.chord.release(bit)?;
        self.resolve(stroke)
    }

    /// Check the stroke timeout. Returns text to type if a stuck stroke was flushed.
    pub fn tick(&mut self, now: Instant) -> Option<String> {
        let stroke = self.chord.tick(now)?;
        self.resolve(stroke)
    }

    fn resolve(&self, stroke: u32) -> Option<String> {
        let rendered = render_stroke(stroke);
        self.dict.lookup(stroke).map_or_else(
            || {
                info!("steno {rendered} -> (not in dictionary)");
                None
            },
            |text| {
                info!("steno {rendered} -> {text:?}");
                // Plover-style: each word is followed by a space
                Some(format!("{text} "))
            },
        )
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
            chord: ChordState::new(Duration::from_secs(1)),
            dict,
            disabled_in_game_mode: true,
        }
    }

    fn key_for(engine: &StenoEngine, bit: u32) -> KeyCode {
        *engine.keys.iter().find(|(_, b)| **b == bit).unwrap().0
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
        assert_eq!(engine.release(t), Some("cat ".to_string()));
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
            stroke_timeout_ms: 1000,
        };
        let err = StenoEngine::from_config(&config, true, Path::new("."), None).unwrap_err();
        assert!(err.to_string().contains("unknown steno layout preset"));
    }
}
