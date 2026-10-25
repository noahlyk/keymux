//! Steno capture through the real `KeymapProcessor`: strokes are typed, unmapped
//! keys are swallowed, and the layer's own remaps still work.

use crate::config::{Config, Layer};
use crate::event_processor::{KeymapProcessor, ProcessResult};
use crate::keycode::KeyCode;
use std::path::{Path, PathBuf};

const CONFIG: &str = r#"(
    remaps: {
        KC_TAB: MO("chords"),
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

/// Switch to the steno layer. `MO` is momentary here, so the activation key stays held.
#[test]
fn stray_space_during_a_chord_does_not_disturb_the_stroke() {
    let fixture = Fixture::new("stray-space");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // Space lands in the middle of "cat". The tape is empty, so Space is an ordinary key and
    // reaches the screen, and the stroke still completes after it
    assert_eq!(p.process_key(KeyCode::KC_S, true), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_SPC, true),
        ProcessResult::EmitKey(KeyCode::KC_SPC, true)
    );
    assert_eq!(
        p.process_key(KeyCode::KC_SPC, false),
        ProcessResult::EmitKey(KeyCode::KC_SPC, false)
    );
    assert_eq!(p.process_key(KeyCode::KC_C, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_P, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_S, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, false), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_P, false),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

#[test]
fn a_key_swallowed_on_press_is_swallowed_on_release_too() {
    let fixture = Fixture::new("swallowed-release");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // With text on the tape, Space is swallowed on the steno layer. Hold it, then leave the
    // layer before letting go
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert_eq!(p.process_key(KeyCode::KC_SPC, true), ProcessResult::None);
    p.process_key(KeyCode::KC_TAB, false);
    assert_eq!(p.current_layer_name(), "base");
    // Its release must not reach the output as a key-up nobody pressed
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);
}

#[test]
fn backspace_is_swallowed_while_the_tape_has_text_and_works_once_it_is_empty() {
    let fixture = Fixture::new("backspace");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // "cat" is typed. Backspace doesn't delete on the steno layer while there is text to undo
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, false), ProcessResult::None);
    // Undo still removes all of "cat"
    assert_eq!(
        p.process_key(KeyCode::KC_T, true),
        ProcessResult::None
    );
    assert_eq!(
        p.process_key(KeyCode::KC_T, false),
        ProcessResult::Retype {
            backspaces: 3,
            text: String::new(),
        }
    );
    // The tape is empty now, so Backspace is an ordinary key again and deletes on the screen
    assert_eq!(
        p.process_key(KeyCode::KC_BSPC, true),
        ProcessResult::EmitKey(KeyCode::KC_BSPC, true)
    );
    assert_eq!(
        p.process_key(KeyCode::KC_BSPC, false),
        ProcessResult::EmitKey(KeyCode::KC_BSPC, false)
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
    assert_eq!(p.process_key(KeyCode::KC_P, true), ProcessResult::None);
    p.deactivate_layer_for_test(&Layer("chords".to_string()));
    assert_eq!(p.process_key(KeyCode::KC_S, false), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_C, false), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_P, false),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

#[test]
fn reentering_a_steno_layer_starts_without_a_leading_space() {
    let fixture = Fixture::new("reenter");
    let mut p = fixture.processor();
    enter_steno(&mut p);
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P];
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
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P];
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );

    // Space is swallowed while "cat" is on the tape: it types nothing
    assert_eq!(p.process_key(KeyCode::KC_SPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);

    // "and" gets its own leading space, since the screen has "cat" before it
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_I, KeyCode::KC_K]),
        ProcessResult::TypeString(" and".to_string(), false)
    );
}

#[test]
fn undo_takes_back_a_word_typed_after_a_space() {
    let fixture = Fixture::new("space-undo");
    let mut p = fixture.processor();
    enter_steno(&mut p);
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    p.process_key(KeyCode::KC_SPC, true);
    p.process_key(KeyCode::KC_SPC, false);
    chord(&mut p, &[KeyCode::KC_I, KeyCode::KC_K]);

    // `*` removes " and", the space that goes with it included, and leaves "cat"
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_T]),
        ProcessResult::Retype {
            backspaces: 4,
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
    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P];

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

/// Lapwing's Enter outlines through the real key path: `R-R` (J here) types a newline, and
/// `#R-R` presses Shift+Enter.
#[test]
fn enter_outlines_produce_their_output() {
    let fixture = Fixture::new("enter");
    let dict = fixture.dir.join("main.json");
    std::fs::write(
        &dict,
        r##"{"R-R": "{^~|\n^}", "#R-R": "{#Shift_l(Return)}{^}"}"##,
    )
    .unwrap();
    let config_path = fixture.dir.join("enter.ron");
    std::fs::write(&config_path, CONFIG.replace("DICT", &path_str(&dict))).unwrap();
    let config = Config::load(&config_path).unwrap();
    let mut p = KeymapProcessor::new(&config, config_path, 0);
    p.wait_for_steno_dictionaries();
    enter_steno(&mut p);

    // R-R: F is the left R and J the right R
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_F, KeyCode::KC_J]),
        ProcessResult::TypeString("\n".to_string(), false)
    );
    // #R-R: number key 1 (#), F (R-) and J (-R)
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_1, KeyCode::KC_F, KeyCode::KC_J]),
        ProcessResult::MultipleEvents(vec![
            (KeyCode::KC_LSFT, true),
            (KeyCode::KC_ENT, true),
            (KeyCode::KC_ENT, false),
            (KeyCode::KC_LSFT, false),
        ])
    );
}

#[test]
fn newline_and_tab_have_keys_to_type() {
    assert_eq!(super::char_to_key('\n').0, Some(evdev::Key::KEY_ENTER));
    assert_eq!(super::char_to_key('\t').0, Some(evdev::Key::KEY_TAB));
}

/// A hub of its own, so these tests don't see strokes from other tests through the global one.
fn private_hub() -> &'static crate::steno::overlay::OverlayHub {
    Box::leak(Box::default())
}

#[test]
fn the_overlay_follows_the_layer_the_held_keys_and_each_finished_stroke() {
    use crate::steno::layout::parse_stroke;

    let fixture = Fixture::new("overlay");
    let mut p = fixture.processor();
    let hub = private_hub();
    p.set_overlay_hub(hub);
    assert_eq!(hub.snapshot().layer, None);

    // Switching into steno shows the overlay on that very press
    enter_steno(&mut p);
    assert_eq!(hub.snapshot().layer.as_deref(), Some("chords"));

    // K- A- -T, pressed one key at a time: each key lights up as it goes down
    let (k, a, t) = (KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P);
    p.process_key(k, true);
    assert_eq!(hub.snapshot().held, parse_stroke("K").unwrap());
    p.process_key(a, true);
    p.process_key(t, true);
    let lit = hub.snapshot();
    assert_eq!(lit.chord, parse_stroke("KAT").unwrap());
    assert_eq!(lit.held, lit.chord);

    // Letting go of K keeps it in the stroke (dimmer) but no longer held
    p.process_key(k, false);
    let released = hub.snapshot();
    assert_eq!(released.chord, parse_stroke("KAT").unwrap());
    assert_eq!(released.held, parse_stroke("AT").unwrap());
    assert!(released.tape.is_empty(), "the stroke isn't finished yet");

    // The last release finishes it: on the tape, keys cleared
    p.process_key(a, false);
    p.process_key(t, false);
    let done = hub.snapshot();
    assert_eq!((done.chord, done.held), (0, 0));
    assert_eq!(done.tape.len(), 1);
    assert_eq!(done.tape[0].stroke, parse_stroke("KAT").unwrap());
    assert_eq!(done.tape[0].text, "\"cat\"");
    assert!(!done.tape[0].unmatched);

    // A stroke the dictionary doesn't have is marked, not silently dropped
    chord(&mut p, &[KeyCode::KC_Q]);
    let last = hub.snapshot().tape.back().cloned().unwrap();
    assert!(last.unmatched);

    // Leaving steno hides the overlay
    p.process_key(KeyCode::KC_TAB, false);
    assert_eq!(hub.snapshot().layer, None);
}
