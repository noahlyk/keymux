//! Steno capture through the real `KeymapProcessor`: strokes are typed, unmapped
//! keys are swallowed, and the layer's own remaps still work.

use crate::config::{Config, Layer};
use crate::event_processor::{KeymapProcessor, ProcessResult};
use crate::keycode::KeyCode;
use std::path::{Path, PathBuf};

const CONFIG: &str = r#"(
    remaps: {
        KC_TAB: TO("chords"),
        KC_CAPS: Key(KC_ESC),
    },
    layers: {
        "chords": (
            kind: Steno((
                dictionaries: ["DICT"],
            )),
            remaps: {
                KC_ESC: Key(KC_ESC),
            },
            disabled_in_game_mode: true,
        ),
    },
    game_mode: (
        remaps: {},
    ),
)"#;

/// Writes a config and dictionary into a unique temp directory and builds a processor.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("keymux-steno-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn processor(&self) -> KeymapProcessor {
        let dict = self.dir.join("main.json");
        std::fs::write(&dict, r#"{"KAT": "cat", "-PB": "and"}"#).unwrap();
        let config_path = self.dir.join("config.ron");
        std::fs::write(&config_path, CONFIG.replace("DICT", &path_str(&dict))).unwrap();
        let config = Config::load(&config_path).unwrap();
        KeymapProcessor::new(&config, config_path, 0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Right Ctrl toggles the steno layer on and off, like the config this is meant for.
const TOGGLE_CONFIG: &str = r#"(
    remaps: {
        KC_RCTL: TG("steno"),
    },
    layers: {
        "steno": (
            kind: Steno((
                dictionaries: ["DICT"],
            )),
            remaps: {
                KC_RCTL: TG("steno"),
            },
            disabled_in_game_mode: true,
        ),
    },
    game_mode: (
        remaps: {},
    ),
)"#;

#[test]
fn tg_latches_after_release_and_toggles_off() {
    let fixture = Fixture::new("toggle");
    let dict = fixture.dir.join("main.json");
    std::fs::write(&dict, r#"{"KAT": "cat"}"#).unwrap();
    let config_path = fixture.dir.join("toggle.ron");
    std::fs::write(
        &config_path,
        TOGGLE_CONFIG.replace("DICT", &path_str(&dict)),
    )
    .unwrap();
    let config = Config::load(&config_path).unwrap();
    let mut p = KeymapProcessor::new(&config, config_path, 0);

    // Press and release: the layer stays on
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(p.current_layer_name(), "steno");

    // Strokes work while latched
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]),
        ProcessResult::TypeString("cat".to_string(), false)
    );

    // Pressing it again on the steno layer exits it (via the layer's own remap)
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(p.current_layer_name(), "base");
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

/// Switch to the steno layer. `TO` is momentary here, so the activation key stays held.
fn enter_steno(p: &mut KeymapProcessor) {
    p.process_key(KeyCode::KC_TAB, true);
    assert_eq!(p.current_layer_name(), "chords");
}

/// Press and release a set of keys in order, returning the result of the last release.
fn chord(p: &mut KeymapProcessor, keys: &[KeyCode]) -> ProcessResult {
    for key in keys {
        assert_eq!(p.process_key(*key, true), ProcessResult::None);
    }
    let mut last = ProcessResult::None;
    for key in keys {
        last = p.process_key(*key, false);
    }
    last
}

#[test]
fn chord_on_steno_layer_types_dictionary_text() {
    let fixture = Fixture::new("chord");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // K- A- -T is "cat"
    let result = chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert_eq!(result, ProcessResult::TypeString("cat".to_string(), false));
}

#[test]
fn unmapped_keys_are_swallowed_not_leaked_to_base() {
    let fixture = Fixture::new("swallow");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // KC_CAPS remaps to Escape on the base layer, but the steno layer must not leak it
    assert_eq!(p.process_key(KeyCode::KC_CAPS, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_CAPS, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_Y, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_Y, false), ProcessResult::None);
}

#[test]
fn layer_remaps_still_apply_to_non_stroke_keys() {
    let fixture = Fixture::new("remap");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    let press = p.process_key(KeyCode::KC_ESC, true);
    assert_ne!(press, ProcessResult::None);
}

#[test]
fn game_mode_hands_keys_back_to_normal_remapping() {
    let fixture = Fixture::new("game");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    p.set_game_mode(true);
    // KC_D is a stroke key, but this layer is disabled in game mode, so it's an ordinary key
    assert_eq!(
        p.process_key(KeyCode::KC_D, true),
        ProcessResult::EmitKey(KeyCode::KC_D, true)
    );
    assert_eq!(
        p.process_key(KeyCode::KC_D, false),
        ProcessResult::EmitKey(KeyCode::KC_D, false)
    );
}

#[test]
fn stroke_released_after_layer_exit_still_types() {
    let fixture = Fixture::new("exit");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    assert_eq!(p.process_key(KeyCode::KC_S, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_P, true), ProcessResult::None);
    p.deactivate_layer_for_test(&Layer("chords".to_string()));
    assert_eq!(p.process_key(KeyCode::KC_S, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, false), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_P, false),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}
