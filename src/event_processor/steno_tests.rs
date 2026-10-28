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
        self.processor_from(CONFIG)
    }

    /// A keyboard that starts on the steno layer, like a steno-only board with `default_layer`.
    fn steno_processor(&self) -> KeymapProcessor {
        self.processor_from(&CONFIG.replacen("(\n", "(\n    default_layer: Some(\"chords\"),\n", 1))
    }

    fn processor_from(&self, config_text: &str) -> KeymapProcessor {
        self.processor_with(config_text, r#"{"KAT": "cat", "-PB": "and"}"#)
    }

    fn processor_with(&self, config_text: &str, dictionary: &str) -> KeymapProcessor {
        let dict = self.dir.join("main.json");
        std::fs::write(&dict, dictionary).unwrap();
        let config_path = self.dir.join("config.ron");
        std::fs::write(&config_path, config_text.replace("DICT", &path_str(&dict))).unwrap();
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
    std::fs::write(&config_path, TOGGLE_CONFIG.replace("DICT", &path_str(&dict))).unwrap();
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

    // Space lands in the middle of "cat". It is swallowed, and the stroke still completes
    assert_eq!(p.process_key(KeyCode::KC_S, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_SPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_SPC, false), ProcessResult::None);
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
fn backspace_is_swallowed_whether_or_not_the_tape_has_text() {
    let fixture = Fixture::new("backspace");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // "cat" is typed. Backspace doesn't delete on the steno layer
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, false), ProcessResult::None);
    // Undo still removes all of "cat"
    assert_eq!(p.process_key(KeyCode::KC_T, true), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_T, false),
        ProcessResult::Retype {
            backspaces: 3,
            text: String::new(),
        }
    );
    // The tape is empty now, and Backspace still does nothing
    assert_eq!(p.process_key(KeyCode::KC_BSPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, false), ProcessResult::None);
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
    // Q is a stroke key, so it types the stroke it makes, with no entry, as its keys
    assert_eq!(p.process_key(KeyCode::KC_Q, true), ProcessResult::None);
    assert_eq!(
        p.process_key(KeyCode::KC_Q, false),
        ProcessResult::TypeString("S-".to_string(), false)
    );
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
fn undo_with_nothing_on_the_tape_presses_ctrl_backspace() {
    let fixture = Fixture::new("undo-empty-tape");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    // `*` with nothing typed on this layer takes back a word with the keys that do it
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_T]),
        ProcessResult::MultipleEvents(vec![
            (KeyCode::KC_LCTL, true),
            (KeyCode::KC_BSPC, true),
            (KeyCode::KC_BSPC, false),
            (KeyCode::KC_LCTL, false),
        ])
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

/// Press and release `keys` one after another, each alone, returning every result. A lookup
/// uses these as letters, so none of them may produce output.
fn type_keys(p: &mut KeymapProcessor, keys: &[KeyCode]) -> Vec<ProcessResult> {
    keys.iter()
        .flat_map(|&key| [p.process_key(key, true), p.process_key(key, false)])
        .collect()
}

#[test]
fn the_lookup_stroke_turns_the_keys_into_qwerty_input_to_a_query_and_nothing_is_typed() {
    use crate::steno::layout::parse_stroke;

    let fixture = Fixture::new("lookup");
    // KAT is both a word and, here, the stroke that opens a lookup: the lookup wins
    let config = CONFIG.replace(
        "dictionaries: [\"DICT\"],",
        "dictionaries: [\"DICT\"], lookup_stroke: Some(\"KAT\"),",
    );
    let mut p = fixture.processor_with(
        &config,
        r#"{"KAT": "cat", "-T": "the", "KAPB": "can", "KAPBD": "candy", "KAPBL": "candle"}"#,
    );
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);
    assert_eq!(hub.snapshot().lookup, None);

    // The stroke opens the lookup instead of typing "cat", and doesn't go on the tape
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]),
        ProcessResult::None
    );
    let open = hub.snapshot();
    assert_eq!(open.lookup.as_ref().map(|l| l.query.as_str()), Some(""));
    assert!(open.tape.is_empty());

    // Typing is QWERTY: these keys are stroke keys on the steno layer, but now they are letters
    // (C-A-N-D-Y), and none of it reaches the application
    let typed = type_keys(
        &mut p,
        &[
            KeyCode::KC_C,
            KeyCode::KC_A,
            KeyCode::KC_N,
            KeyCode::KC_D,
            KeyCode::KC_Y,
        ],
    );
    assert!(typed.iter().all(|r| *r == ProcessResult::None), "{typed:?}");
    let found = hub.snapshot().lookup.unwrap();
    assert_eq!(found.query, "candy");
    assert_eq!(found.rows[0].text, "candy");
    assert_eq!(found.rows[0].outline, "KAPBD");
    // The best stroke is lit on the overlay's keyboard
    assert_eq!(found.hint, parse_stroke("KAPBD").unwrap());
    assert!(hub.snapshot().tape.is_empty(), "looking things up types nothing");

    // Backspace edits the query, and now it also suggests longer words
    type_keys(&mut p, &[KeyCode::KC_BSPC, KeyCode::KC_BSPC]);
    let found = hub.snapshot().lookup.unwrap();
    assert_eq!(found.query, "can");
    let words: Vec<_> = found.rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(words, ["can", "candy", "candle"]);

    // Shift capitalizes: lookups are case-sensitive
    p.process_key(KeyCode::KC_LSFT, true);
    type_keys(&mut p, &[KeyCode::KC_X]);
    p.process_key(KeyCode::KC_LSFT, false);
    assert_eq!(hub.snapshot().lookup.unwrap().query, "canX");

    // Esc closes it, and is swallowed even though this layer remaps Esc
    assert_eq!(
        type_keys(&mut p, &[KeyCode::KC_ESC]),
        vec![ProcessResult::None; 2]
    );
    assert_eq!(hub.snapshot().lookup, None);

    // Back to normal steno: the same keys are strokes again, so the stroke reopens it
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert!(hub.snapshot().lookup.is_some());
}

#[test]
fn leaving_steno_in_the_middle_of_a_lookup_closes_it_and_does_not_keep_eating_keys() {
    let fixture = Fixture::new("lookup-leave");
    let config = CONFIG.replace(
        "dictionaries: [\"DICT\"],",
        "dictionaries: [\"DICT\"], lookup_stroke: Some(\"KAT\"),",
    );
    let mut p = fixture.processor_with(&config, r#"{"-T": "the", "KAPB": "can"}"#);
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);
    chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P]);
    assert!(hub.snapshot().lookup.is_some());

    // Let go of the layer key: the overlay hides, and with it the lookup
    p.process_key(KeyCode::KC_TAB, false);
    assert_eq!(hub.snapshot().layer, None);
    assert_eq!(hub.snapshot().lookup, None);

    // Back on the base layer a letter is just a letter again
    assert_ne!(
        p.process_key(KeyCode::KC_A, true),
        ProcessResult::None,
        "base layer keys must not be swallowed by a stale lookup"
    );
    p.process_key(KeyCode::KC_A, false);

    // Coming back to steno starts clean: the old lookup is gone
    enter_steno(&mut p);
    assert_eq!(hub.snapshot().lookup, None);
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_S, KeyCode::KC_C]),
        ProcessResult::TypeString("KA-".to_string(), false),
        "a stroke, not lookup letters"
    );
}

/// A fixture keyboard whose steno layer opens a lookup on `trigger`, with a tiny dictionary:
/// K-A--T is "cat" and -T is "the". On this layout those are the keys S C P and P.
fn lookup_trigger_processor(fixture: &Fixture, trigger: &str) -> KeymapProcessor {
    let config = CONFIG.replace(
        "dictionaries: [\"DICT\"],",
        &format!("dictionaries: [\"DICT\"], lookup_stroke: Some(\"{trigger}\"),"),
    );
    fixture.processor_with(&config, r#"{"KAT": "cat", "-T": "the"}"#)
}

const KAT: [KeyCode; 3] = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P];
const THE: [KeyCode; 1] = [KeyCode::KC_P];

#[test]
fn a_two_stroke_lookup_trigger_holds_the_first_stroke_back_and_opens_after_the_second() {
    let fixture = Fixture::new("lookup-two");
    let mut p = lookup_trigger_processor(&fixture, "KAT/-T");
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);

    // The first stroke might be the start of the trigger, so it types nothing yet and is not on
    // the tape, and no lookup is open
    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);
    assert!(hub.snapshot().tape.is_empty());
    assert_eq!(hub.snapshot().lookup, None);

    // The second stroke completes it: the lookup opens, and neither stroke typed or is on the tape
    assert_eq!(chord(&mut p, &THE), ProcessResult::None);
    assert!(hub.snapshot().lookup.is_some());
    assert!(hub.snapshot().tape.is_empty());

    // From here keys are QWERTY input to the query, as with a one-stroke trigger
    type_keys(&mut p, &[KeyCode::KC_C, KeyCode::KC_A]);
    assert_eq!(hub.snapshot().lookup.unwrap().query, "ca");
}

#[test]
fn held_back_strokes_are_typed_as_normal_when_the_trigger_does_not_follow() {
    let fixture = Fixture::new("lookup-replay");
    let mut p = lookup_trigger_processor(&fixture, "KAT/-T");
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);

    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);
    // KAT again is not -T: the first KAT was an ordinary stroke after all and types "cat". The
    // second one is looked at again, and is held back in its turn.
    assert_eq!(
        chord(&mut p, &KAT),
        ProcessResult::TypeString("cat".to_string(), false)
    );
    assert_eq!(hub.snapshot().tape.len(), 1);
    assert_eq!(hub.snapshot().lookup, None);

    // -T now completes KAT/-T, using the second KAT: the lookup opens, and the first "cat"
    // is still there
    assert_eq!(chord(&mut p, &THE), ProcessResult::None);
    assert!(hub.snapshot().lookup.is_some());
    assert_eq!(hub.snapshot().tape.len(), 1);
}

#[test]
fn a_held_back_stroke_followed_by_something_else_types_both() {
    let fixture = Fixture::new("lookup-both");
    let mut p = lookup_trigger_processor(&fixture, "KAT/KAT");
    enter_steno(&mut p);

    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);
    // -T is not KAT: both strokes are ordinary, and arrive as one edit, in order
    assert_eq!(
        chord(&mut p, &THE),
        ProcessResult::TypeString("cat the".to_string(), false)
    );
}

#[test]
fn a_three_stroke_trigger_opens_only_after_the_third() {
    let fixture = Fixture::new("lookup-three");
    let mut p = lookup_trigger_processor(&fixture, "KAT/-T/KAT");
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);

    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);
    assert_eq!(chord(&mut p, &THE), ProcessResult::None);
    assert_eq!(hub.snapshot().lookup, None, "two of three is not enough");
    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);
    assert!(hub.snapshot().lookup.is_some());
    assert!(hub.snapshot().tape.is_empty());
}

#[test]
fn backspace_does_not_delete_text_before_a_held_back_stroke() {
    let fixture = Fixture::new("lookup-bspc");
    let mut p = lookup_trigger_processor(&fixture, "KAT/-T");
    enter_steno(&mut p);
    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);

    // Nothing has been typed for the held stroke, so Backspace would eat whatever came
    // before it. It is swallowed instead, as it is whenever there is something to undo.
    assert_eq!(p.process_key(KeyCode::KC_BSPC, true), ProcessResult::None);
    assert_eq!(p.process_key(KeyCode::KC_BSPC, false), ProcessResult::None);
}

#[test]
fn a_two_stroke_word_still_combines_after_a_stroke_that_matched_nothing() {
    // From a real tape: a stroke that is in no dictionary, then a two-stroke word (here
    // PER/TPEBGT, "perfect") typed on an empty line, used to come out as "per effect" until
    // you left and rejoined steno. On this layout PER is E N J and TPEBGT is W E N K L P.
    let fixture = Fixture::new("unmatched-then-two-stroke");
    let mut p = fixture.processor_with(
        CONFIG,
        r#"{"PER": "per", "TPEBGT": "effect", "PER/TPEBGT": "perfect"}"#,
    );
    enter_steno(&mut p);

    // A stroke with no entry (just K-): it types its keys
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_S]),
        ProcessResult::TypeString("K-".to_string(), false)
    );
    assert_eq!(
        chord(&mut p, &[KeyCode::KC_E, KeyCode::KC_N, KeyCode::KC_J]),
        ProcessResult::TypeString(" per".to_string(), false)
    );
    let tpebgt = [
        KeyCode::KC_W,
        KeyCode::KC_E,
        KeyCode::KC_N,
        KeyCode::KC_K,
        KeyCode::KC_L,
        KeyCode::KC_P,
    ];
    // The second stroke finishes the word: "fect" goes after "per", not " effect"
    assert_eq!(
        chord(&mut p, &tpebgt),
        ProcessResult::TypeString("fect".to_string(), false)
    );
}

#[test]
fn half_a_trigger_is_dropped_when_you_leave_the_layer() {
    let fixture = Fixture::new("lookup-leave-half");
    let mut p = lookup_trigger_processor(&fixture, "KAT/-T");
    let hub = private_hub();
    p.set_overlay_hub(hub);
    enter_steno(&mut p);
    assert_eq!(chord(&mut p, &KAT), ProcessResult::None);

    p.process_key(KeyCode::KC_TAB, false);
    enter_steno(&mut p);
    // The old KAT is gone: -T alone is just "the", and opens nothing
    assert_eq!(
        chord(&mut p, &THE),
        ProcessResult::TypeString("the".to_string(), false)
    );
    assert_eq!(hub.snapshot().lookup, None);
}

/// A hub of its own, so these tests don't see strokes from other tests through the global one.
fn private_hub() -> &'static crate::steno::overlay::OverlayHub {
    Box::leak(Box::default())
}

/// Press and release one key on a keyboard.
fn touch(p: &mut KeymapProcessor, key: KeyCode) {
    p.process_key(key, true);
    p.process_key(key, false);
}

#[test]
fn the_overlay_follows_whichever_keyboard_was_touched_last() {
    let fixture = Fixture::new("two-keyboards");
    let hub = private_hub();
    // One keyboard is steno all the time, the other is a plain keyboard on the base layer
    let mut steno_board = fixture.steno_processor();
    let mut plain_board = fixture.processor();
    steno_board.set_overlay_hub(hub);
    plain_board.set_overlay_hub(hub);

    // The steno keyboard shows it from the start; the plain one, starting later, doesn't hide it
    assert_eq!(hub.snapshot().layer.as_deref(), Some("chords"));

    // Touching the plain keyboard makes it the current one, and it isn't in steno
    touch(&mut plain_board, KeyCode::KC_A);
    assert_eq!(hub.snapshot().layer, None);

    // Touching the steno keyboard brings it back, and a stroke lands on the tape
    touch(&mut steno_board, KeyCode::KC_S);
    assert_eq!(hub.snapshot().layer.as_deref(), Some("chords"));
    assert_eq!(hub.snapshot().tape.len(), 1);

    // And back and forth, every time
    touch(&mut plain_board, KeyCode::KC_B);
    assert_eq!(hub.snapshot().layer, None);
    touch(&mut steno_board, KeyCode::KC_S);
    assert_eq!(hub.snapshot().layer.as_deref(), Some("chords"));

    // A plain keyboard that switches into steno itself shows it, and leaving hides it again
    plain_board.process_key(KeyCode::KC_TAB, true);
    assert_eq!(hub.snapshot().layer.as_deref(), Some("chords"));
    plain_board.process_key(KeyCode::KC_TAB, false);
    assert_eq!(hub.snapshot().layer, None);
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

#[test]
fn a_key_that_reaches_the_screen_makes_the_next_word_start_fresh() {
    // Delete is not a stroke key: it reaches the application and edits the text at the cursor
    // behind the translator's back. After it, the translator no longer knows what is before
    // the cursor, so the next word is not given a leading space as if it followed "cat".
    let fixture = Fixture::new("edit-key-resets");
    let mut p = fixture.processor();
    enter_steno(&mut p);

    let cat = [KeyCode::KC_S, KeyCode::KC_C, KeyCode::KC_P];
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );
    // Without anything in between, the second word follows the first
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString(" cat".to_string(), false)
    );

    p.process_key(KeyCode::KC_DEL, true);
    p.process_key(KeyCode::KC_DEL, false);
    assert_eq!(
        chord(&mut p, &cat),
        ProcessResult::TypeString("cat".to_string(), false)
    );
}

fn key_events(writes: &[Vec<evdev::InputEvent>]) -> Vec<(u16, i32)> {
    writes
        .iter()
        .flatten()
        .filter(|e| e.event_type() == evdev::EventType::KEY)
        .map(|e| (e.code(), e.value()))
        .collect()
}

fn sizes(backspaces: usize, text: &str) -> Vec<usize> {
    super::retype_keystrokes(backspaces, text)
        .iter()
        .map(Vec::len)
        .collect()
}

#[test]
fn a_word_goes_out_at_once_with_nothing_to_wait_for() {
    let now = std::time::Instant::now();
    // 12 keystrokes is the most that fits, and still goes out together
    for (backspaces, text) in [(0, "about"), (3, "increase"), (0, "abcdefghijkl")] {
        let parts = super::Pacer::default().schedule(now, &sizes(backspaces, text));
        assert_eq!(parts.len(), 1, "{text}");
        assert_eq!(parts[0].0, now, "{text}");
    }
}

#[test]
fn a_long_word_waits_only_as_long_as_it_must() {
    let now = std::time::Instant::now();
    let word = "supercalifragilisticexpialidocious";
    let parts = super::Pacer::default().schedule(now, &sizes(0, word));
    // 12 letters now, 12 once those have been read, then the last 10
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].0, now);
    assert_eq!(parts[2].0 - now, super::UNREAD_WINDOW * 2);
    assert_eq!(parts.iter().map(|(_, r)| r.len()).sum::<usize>(), word.len());
}

#[test]
fn nothing_in_flight_ever_goes_over_the_budget() {
    let now = std::time::Instant::now();
    let mut pacer = super::Pacer::default();
    // Words arriving 1 ms apart, long and short, some with shifted letters and backspaces
    let mut all = Vec::new();
    for (i, text) in ["Hello World", "supercalifragilisticexpialidocious", "ab", "XYZXYZXYZ"]
        .iter()
        .cycle()
        .take(40)
        .enumerate()
    {
        let sizes = sizes(i % 7, text);
        let at = now + std::time::Duration::from_millis(i as u64);
        for (time, range) in pacer.schedule(at, &sizes) {
            all.push((time, sizes[range].iter().sum::<usize>()));
        }
    }
    for &(time, _) in &all {
        let in_window: usize = all
            .iter()
            .filter(|&&(t, _)| t <= time && time < t + super::UNREAD_WINDOW)
            .map(|&(_, events)| events)
            .sum();
        assert!(in_window <= super::SAFE_EVENTS, "{in_window} events in one window");
    }
}

#[test]
fn a_second_word_straight_after_waits_for_the_first_to_be_read() {
    let now = std::time::Instant::now();
    let mut pacer = super::Pacer::default();
    pacer.schedule(now, &sizes(0, "abcdefghijkl"));
    let later = now + std::time::Duration::from_millis(1);
    let parts = pacer.schedule(later, &sizes(0, "next"));
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].0, now + super::UNREAD_WINDOW);
}

#[test]
fn after_a_quiet_moment_a_full_burst_goes_out_at_once_again() {
    let now = std::time::Instant::now();
    let mut pacer = super::Pacer::default();
    pacer.schedule(now, &sizes(0, "abcdefghijkl"));
    let later = now + super::UNREAD_WINDOW;
    let parts = pacer.schedule(later, &sizes(0, "abcdefghijkl"));
    assert_eq!(parts, vec![(later, 0..12)]);
}

#[test]
fn pacing_never_changes_the_keys_or_their_order() {
    let keystrokes = super::retype_keystrokes(30, "Hello, World");
    let sizes: Vec<usize> = keystrokes.iter().map(Vec::len).collect();
    let parts = super::Pacer::default().schedule(std::time::Instant::now(), &sizes);
    let paced: Vec<Vec<evdev::InputEvent>> = parts
        .iter()
        .map(|(_, range)| keystrokes[range.clone()].concat())
        .collect();
    assert_eq!(key_events(&paced), key_events(&[keystrokes.concat()]));
}

#[test]
fn shift_is_never_held_across_two_parts() {
    let keystrokes = super::retype_keystrokes(5, "ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    let sizes: Vec<usize> = keystrokes.iter().map(Vec::len).collect();
    let parts = super::Pacer::default().schedule(std::time::Instant::now(), &sizes);
    assert!(parts.len() > 1);
    for (_, range) in parts {
        let shifts: Vec<i32> = key_events(&[keystrokes[range].concat()])
            .into_iter()
            .filter(|&(code, _)| code == evdev::Key::KEY_LEFTSHIFT.code())
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            shifts.iter().filter(|&&v| v == 1).count(),
            shifts.iter().filter(|&&v| v == 0).count()
        );
    }
}
