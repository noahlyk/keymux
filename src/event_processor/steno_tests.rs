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
        let mut keymap = KeymapProcessor::new(&config, config_path, 0);
        keymap.wait_for_steno_dictionaries();
        keymap
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
    p.wait_for_steno_dictionaries();

    // Press and release: the layer stays on
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(p.current_layer_name(), "steno");

    // Strokes work while latched
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O]),
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
#[test]
fn stray_space_during_a_chord_does_not_disturb_the_stroke() {
    let fixture = Fixture::new("stray-space");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // Space lands in the middle of "cat": it types a space, and the stroke still completes after it
    assert_eq!(p.process_key(KeyCode::KC_S, true), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_SPC, true),
        ProcessResult::TypeString(" ".to_string(), false)
    );
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_O, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_S, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, false), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_O, false),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

#[test]
fn a_key_swallowed_on_press_is_swallowed_on_release_too() {
    let fixture = Fixture::new("swallowed-release");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // Hold space on the steno layer, then leave the layer before letting go
    assert_eq!(
        p.process_key(KeyCode::KC_SPC, true),
        ProcessResult::TypeString(" ".to_string(), false)
    );
    p.process_key(KeyCode::KC_TAB, false);
    assert_eq!(p.current_layer_name(), "base");
    // Its release must not reach the output as a key-up nobody pressed
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);
}

#[test]
fn backspace_on_a_steno_layer_reaches_the_screen_and_fixes_undo() {
    let fixture = Fixture::new("backspace");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // "cat" is typed, then Backspace removes its last letter by hand
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O]);
    assert_eq!(
        p.process_key(KeyCode::KC_BSPC, true),
        ProcessResult::EmitKey(KeyCode::KC_BSPC, true)
    );
    assert_eq!(
        p.process_key(KeyCode::KC_BSPC, false),
        ProcessResult::EmitKey(KeyCode::KC_BSPC, false)
    );
    // Undo now removes what's left, "ca", and nothing more
    assert_eq!(
        p.process_key(KeyCode::KC_T, true),
        ProcessResult::None
    );
    assert_eq!(
        p.process_key(KeyCode::KC_T, false),
        ProcessResult::Retype {
            backspaces: 2,
            text: String::new(),
        }
    );
}

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
    let result = chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O]);
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
    assert_eq!(p.process_key(KeyCode::KC_Q, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_Q, false), ProcessResult::None);
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
    assert_eq!(p.process_key(KeyCode::KC_O, true), ProcessResult::None);
    p.deactivate_layer_for_test(&Layer("chords".to_string()));
    assert_eq!(p.process_key(KeyCode::KC_S, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, false), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_O, false),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

#[test]
fn reentering_a_steno_layer_starts_without_a_leading_space() {
    let fixture = Fixture::new("reenter");
    let mut p = fixture.processor();
    enter_steno(&mut p);
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O];
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );

    // Leaving and coming back starts a new sentence, so "cat" gets no leading space
    p.deactivate_layer_for_test(&Layer("chords".to_string()));
    enter_steno(&mut p);
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

#[test]
fn space_types_a_space_and_the_next_word_does_not_add_another() {
    let fixture = Fixture::new("space-word");
    let mut p = fixture.processor();
    enter_steno(&mut p);
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O];
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );

    assert_eq!(
        p.process_key(KeyCode::KC_SPC, true),
        ProcessResult::TypeString(" ".to_string(), false)
    );
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);

    // "and" follows the typed space, so it has no prefix space of its own
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_U, KeyCode::KC_J]),
        ProcessResult::TypeString("and".to_string(), false)
    );
}

#[test]
fn undo_takes_back_a_word_typed_after_a_space() {
    let fixture = Fixture::new("space-undo");
    let mut p = fixture.processor();
    enter_steno(&mut p);
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O]);
    p.process_key(KeyCode::KC_SPC, true);
    p.process_key(KeyCode::KC_SPC, false);
    chord(&mut p, &[KeyCode::KC_U, KeyCode::KC_J]);

    // `*` removes "and" and leaves the space the user typed
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_T]),
        ProcessResult::Retype {
            backspaces: 3,
            text: String::new(),
        }
    );
}

#[test]
fn toggling_back_into_steno_starts_without_a_prefix_space() {
    let fixture = Fixture::new("toggle-reenter");
    let dict = fixture.dir.join("main.json");
    std::fs::write(&dict, r#"{"KAT": "cat"}"#).unwrap();
    let config_path = fixture.dir.join("toggle.ron");
    std::fs::write(&config_path, TOGGLE_CONFIG.replace("DICT", &path_str(&dict))).unwrap();
    let config = Config::load(&config_path).unwrap();
    let mut p = KeymapProcessor::new(&config, config_path, 0);
    p.wait_for_steno_dictionaries();
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_O];

    // Right Ctrl on, "cat", Right Ctrl off
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(p.current_layer_name(), "base");

    // Some typing on base, then back into steno: "cat" still gets no prefix space
    p.process_key(KeyCode::KC_Q, true);
    p.process_key(KeyCode::KC_Q, false);
    p.process_key(KeyCode::KC_RCTL, true);
    p.process_key(KeyCode::KC_RCTL, false);
    assert_eq!(p.current_layer_name(), "steno");
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}
