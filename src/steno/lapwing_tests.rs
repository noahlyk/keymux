//! Checks against Lapwing's own theory. The single-stroke rows in `testdata/` come from the
//! Lapwing book (lapwing.aerick.ca), with each text taken from Lapwing's dictionaries.
//!
//! The always-on tests use only the committed rows. The `#[ignore]`d tests load the real
//! dictionaries from `~/.config/keymux/steno/lapwing/` (run `keymux steno theory lapwing`
//! first), the same files a Lapwing layer uses in production.

use super::dict::Dictionary;
use super::format::KeyPress;
use super::layout::parse_stroke;
use super::setup::LAPWING_FILES;
use super::numbers::NUMBER_BIT;
use super::translate::{StenoOutput, Translator, UNDO_STROKE};
use crate::keycode::KeyCode;
use std::path::PathBuf;

/// `stroke<TAB>text<TAB>chapter` per line
const VECTORS: &str = include_str!("testdata/lapwing_vectors.tsv");

fn vectors() -> Vec<(&'static str, &'static str)> {
    VECTORS
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            (fields.next().unwrap(), fields.next().unwrap())
        })
        .collect()
}

/// Translate one stroke on a fresh translator, as the first word of a sentence.
fn first_word(dict: Dictionary, stroke: &str) -> Option<StenoOutput> {
    Translator::new(dict).stroke(parse_stroke(stroke).unwrap())
}

#[test]
fn every_book_vector_is_a_valid_stroke() {
    for (stroke, _) in vectors() {
        assert!(parse_stroke(stroke).is_some(), "{stroke} should parse");
    }
}

#[test]
fn every_book_vector_types_its_word() {
    // Build a dictionary from the rows themselves, then check the engine returns each word
    let json = format!(
        "{{{}}}",
        vectors()
            .iter()
            .map(|(stroke, text)| format!("\"{stroke}\": \"{text}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut dict = Dictionary::default();
    dict.merge_json(&json).unwrap();
    for (stroke, text) in vectors() {
        assert_eq!(
            first_word(dict.clone(), stroke),
            Some(StenoOutput::Type(text.to_string())),
            "{stroke} should type {text}"
        );
    }
}

/// The real Lapwing dictionaries, loaded the way a Lapwing layer loads them.
fn lapwing_dictionary() -> Dictionary {
    let home = std::env::var("HOME").unwrap();
    let dir = PathBuf::from(home).join(".config/keymux/steno/lapwing");
    let paths: Vec<PathBuf> = LAPWING_FILES
        .iter()
        .map(|file| dir.join(file))
        .collect();
    Dictionary::load(&paths)
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn real_lapwing_dictionaries_type_every_book_vector() {
    let dict = lapwing_dictionary();
    for (stroke, text) in vectors() {
        assert_eq!(
            first_word(dict.clone(), stroke),
            Some(StenoOutput::Type(text.to_string())),
            "{stroke} should type {text} from Lapwing's dictionaries"
        );
    }
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn real_lapwing_dictionaries_have_no_invalid_strokes() {
    let home = std::env::var("HOME").unwrap();
    let dir = PathBuf::from(home).join(".config/keymux/steno/lapwing");
    for file in LAPWING_FILES {
        let json = std::fs::read_to_string(dir.join(file)).unwrap();
        let stats = Dictionary::default().merge_json(&json).unwrap();
        assert_eq!(stats.invalid, 0, "{file} has strokes keymux cannot parse");
    }
}

/// Real Lapwing entries from chapter 20 (punctuation and commands), with the words they're
/// used with in these tests.
const CH20: &str = r#"{
    "KAT": "cat", "SKP": "and", "TKOG": "dog", "-T": "the",
    "TP-PL": "{.}", "KW-PL": "{?}", "TP-BG": "{!}", "KW-BG": "{,}",
    "H-PB": "{^}-{^}", "S-P": "{^ ^}", "S-PD": "{*?}", "SP-LS": "{*!}",
    "KPA": "{}{:case:cap_first_word}", "KA*PD": "{:retro_case:cap_first_word}"
}"#;

/// Type `strokes` in order on a fresh translator, returning the text on screen.
fn type_strokes(json: &str, strokes: &[&str]) -> String {
    let mut dict = Dictionary::default();
    dict.merge_json(json).unwrap();
    type_with(dict, strokes)
}

/// Type `strokes` one stroke at a time, as the keyboard would, and return the text on screen.
fn type_with(dict: Dictionary, strokes: &[&str]) -> String {
    let mut translator = Translator::new(dict);
    let mut typed = String::new();
    for stroke in strokes {
        match translator.stroke(parse_stroke(stroke).unwrap()) {
            None | Some(StenoOutput::Keys(_)) => {}
            Some(StenoOutput::Type(text)) => typed.push_str(&text),
            Some(StenoOutput::Retype { backspaces, text }) => {
                for _ in 0..backspaces {
                    typed.pop();
                }
                typed.push_str(&text);
            }
        }
    }
    typed
}

#[test]
fn ch20_sentence_punctuation_attaches_and_capitalizes_the_next_word() {
    assert_eq!(type_strokes(CH20, &["KAT", "TP-PL", "SKP"]), "cat. And");
    assert_eq!(type_strokes(CH20, &["KAT", "KW-PL", "SKP"]), "cat? And");
    assert_eq!(type_strokes(CH20, &["KAT", "TP-BG", "SKP"]), "cat! And");
}

#[test]
fn ch20_comma_attaches_without_capitalizing() {
    assert_eq!(type_strokes(CH20, &["KAT", "KW-BG", "-T"]), "cat, the");
}

#[test]
fn ch20_hyphen_attaches_both_sides() {
    assert_eq!(type_strokes(CH20, &["KAT", "H-PB", "TKOG"]), "cat-dog");
}

#[test]
fn ch20_insert_space_then_next_word_has_one_space() {
    // S-P inserts a space by itself, so the next word must not add a second one
    assert_eq!(type_strokes(CH20, &["KAT", "S-P", "TKOG"]), "cat dog");
}

#[test]
fn ch20_retro_delete_space_suppresses_the_last_space() {
    // Lapwing ch20: SP-LS "retroactively suppress last space"
    assert_eq!(type_strokes(CH20, &["KAT", "TKOG", "SP-LS"]), "catdog");
}

#[test]
fn ch20_capitalize_next_word() {
    assert_eq!(type_strokes(CH20, &["KAT", "KPA", "TKOG"]), "cat Dog");
}

#[test]
fn ch20_retro_capitalize_last_word() {
    assert_eq!(type_strokes(CH20, &["KAT", "KA*PD"]), "Cat");
}

#[test]
fn ch20_opening_quote_attaches_to_the_next_word() {
    let json = r#"{"KAT": "cat", "TKOG": "dog", "KW-GS": "{~|\"^}"}"#;
    assert_eq!(type_strokes(json, &["KAT", "KW-GS", "TKOG"]), r#"cat "dog"#);
}

#[test]
fn ch20_enter_key_is_typed() {
    let json = r#"{"KAT": "cat", "R-R": "{^~|\n^}"}"#;
    assert_eq!(type_strokes(json, &["KAT", "R-R"]), "cat\n");
}

/// `stroke<TAB>text<TAB>chapter` for worked examples in the book, each one checked against
/// the dictionary entry it uses.
const EXAMPLES: &str = include_str!("testdata/lapwing_examples.tsv");

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn book_worked_examples_type_what_the_book_shows() {
    let dict = lapwing_dictionary();
    let mut failures = Vec::new();
    for line in EXAMPLES.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let (stroke, expected, chapter) = (
            fields.next().unwrap(),
            fields.next().unwrap(),
            fields.next().unwrap(),
        );
        let strokes: Vec<&str> = stroke.split('/').collect();
        let typed = type_with(dict.clone(), &strokes);
        if typed.trim() != expected.trim() {
            failures.push(format!("{chapter} {stroke}: book says {expected:?}, typed {typed:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} worked examples differ from the book:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Random steno sessions, checked after every stroke: the screen, rebuilt from the outputs
/// the translator returns, must equal the translator's own record of the text. Covers
/// undo, space, backspace and repeat, interleaved with real Lapwing strokes.
#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn random_sessions_keep_the_screen_and_the_translator_in_step() {
    let dict = lapwing_dictionary();
    let mut pool: Vec<u32> = EXAMPLES
        .lines()
        .flat_map(|line| line.split('\t').next().unwrap().split('/'))
        .map(|s| parse_stroke(s).unwrap())
        .collect();
    pool.extend(vectors().iter().map(|(s, _)| parse_stroke(s).unwrap()));
    pool.push(UNDO_STROKE);
    pool.push(NUMBER_BIT);

    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        ((seed >> 33) as usize) % n
    };
    for session in 0..200 {
        let mut translator = Translator::new(dict.clone());
        let mut screen = String::new();
        for step in 0..150 {
            match next(100) {
                // Space, backspace and undo are the keys the steno layer handles itself
                0..=3 => {
                    let output = translator.space();
                    apply_output(&mut screen, Some(output));
                }
                4..=5 => {
                    translator.backspace();
                    screen.pop();
                }
                6..=12 => {
                    let output = translator.stroke(UNDO_STROKE);
                    apply_output(&mut screen, output);
                }
                _ => {
                    let stroke = pool[next(pool.len())];
                    let output = translator.stroke(stroke);
                    apply_output(&mut screen, output);
                }
            }
            assert_eq!(
                screen,
                translator.text(),
                "session {session}, step {step}: the screen and the translator disagree"
            );
        }
    }
}

/// Apply one translator output to the simulated screen.
fn apply_output(screen: &mut String, output: Option<StenoOutput>) {
    match output {
        None | Some(StenoOutput::Keys(_)) => {}
        Some(StenoOutput::Type(text)) => screen.push_str(&text),
        Some(StenoOutput::Retype { backspaces, text }) => {
            for _ in 0..backspaces {
                screen.pop();
            }
            screen.push_str(&text);
        }
    }
}

/// Chapter 26's movement commands, with the fixture entries they use.
const CH26: &str = r##"{
    "#TPH-R": "{#Left}{^}", "#TPH*R": "{#Shift_l(Left)}{^}",
    "#TPH-RB": "{#Control_L(Left)}{^}", "KAT": "cat"
}"##;

/// The one key press a stroke makes, from the translator's output.
fn key_presses(json: &str, strokes: &[&str]) -> Vec<KeyPress> {
    let mut dict = Dictionary::default();
    dict.merge_json(json).unwrap();
    let mut translator = Translator::new(dict);
    let mut presses = Vec::new();
    for stroke in strokes {
        if let Some(StenoOutput::Keys(keys)) = translator.stroke(parse_stroke(stroke).unwrap()) {
            presses.extend(keys);
        }
    }
    presses
}

#[test]
fn ch26_basic_movement_presses_its_key() {
    assert_eq!(
        key_presses(CH26, &["#TPH-R"]),
        vec![KeyPress {
            modifiers: vec![],
            key: KeyCode::KC_LEFT
        }]
    );
}

#[test]
fn ch26_selection_holds_shift_while_moving() {
    assert_eq!(
        key_presses(CH26, &["#TPH*R"]),
        vec![KeyPress {
            modifiers: vec![KeyCode::KC_LSFT],
            key: KeyCode::KC_LEFT
        }]
    );
}

#[test]
fn ch26_word_movement_holds_control() {
    assert_eq!(
        key_presses(CH26, &["#TPH-RB"]),
        vec![KeyPress {
            modifiers: vec![KeyCode::KC_LCTL],
            key: KeyCode::KC_LEFT
        }]
    );
}

#[test]
fn ch26_number_key_repeats_the_last_movement() {
    // Lapwing: `#TPH-R/#/#` moves left three times
    assert_eq!(key_presses(CH26, &["#TPH-R", "#", "#"]).len(), 3);
}

#[test]
fn ch26_undo_after_movement_leaves_text_alone() {
    let mut dict = Dictionary::default();
    dict.merge_json(CH26).unwrap();
    let mut translator = Translator::new(dict);
    translator.stroke(parse_stroke("KAT").unwrap());
    translator.stroke(parse_stroke("#TPH-R").unwrap());
    // The undo ends at the movement: the word on screen is not deleted
    assert_eq!(translator.stroke(UNDO_STROKE), None);
    assert!(matches!(
        translator.stroke(UNDO_STROKE),
        Some(StenoOutput::Retype { backspaces: 3, .. })
    ));
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn real_lapwing_movement_table_presses_its_keys() {
    // Chapter 26's movement table, checked against Lapwing's own dictionary
    let dict = lapwing_dictionary();
    let table = [
        ("#TPH-R", KeyCode::KC_LEFT),
        ("#TPH-P", KeyCode::KC_UP),
        ("#TPH-B", KeyCode::KC_DOWN),
        ("#TPH-G", KeyCode::KC_RGHT),
        ("#TPH-FR", KeyCode::KC_HOME),
        ("#TPH-LG", KeyCode::KC_END),
        ("#TPH-RPG", KeyCode::KC_PGUP),
        ("#TPH-FBL", KeyCode::KC_PGDN),
    ];
    for (stroke, key) in table {
        let mut translator = Translator::new(dict.clone());
        let output = translator.stroke(parse_stroke(stroke).unwrap());
        let Some(StenoOutput::Keys(presses)) = output else {
            panic!("{stroke} should press a key, got {output:?}");
        };
        assert_eq!(presses[0].key, key, "{stroke}");
    }
}

/// Entries keymux can't type, and why. Anything not in this list is a gap in the engine.
fn expected_unsupported(value: &str) -> Option<&'static str> {
    if value.starts_with('=') {
        // Plover functions such as `=undo` and `=repeat_last_translation`. keymux handles
        // `*` and `#` itself, so these entries are never looked up.
        Some("plover function, handled by the translator")
    } else if value.contains("PLOVER:") {
        // Plover's own windows (lookup, add translation, configure), not typing
        Some("plover application command, not typing")
    } else if value.contains(":stitch") {
        // Plover's stitching plugin: not taught anywhere in the book
        Some("plover stitching, not taught in the book")
    } else if value.contains("MODE:")
        || value.contains("upper_first_word")
        || value.contains("{*-|}")
        || value.contains("{*}")
    {
        // Plover-only modes and casing that the book never uses
        Some("plover mode or casing, not taught in the book")
    } else {
        None
    }
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn every_unsupported_lapwing_entry_is_a_known_category() {
    use std::collections::BTreeMap;
    let home = std::env::var("HOME").unwrap();
    let dir = PathBuf::from(home).join(".config/keymux/steno/lapwing");
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut gaps = Vec::new();
    for file in LAPWING_FILES {
        let json = std::fs::read_to_string(dir.join(file)).unwrap();
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        for (stroke, value) in raw {
            let text = value.as_str().unwrap_or_default();
            if super::format::parse_pieces(text).is_some() {
                continue;
            }
            match expected_unsupported(text) {
                Some(category) => *counts.entry(category).or_default() += 1,
                None => gaps.push(format!("{file}: {stroke} = {text}")),
            }
        }
    }
    println!("unsupported Lapwing entries by category: {counts:?}");
    assert!(
        gaps.is_empty(),
        "{} entries keymux can't type and doesn't know why:\n{}",
        gaps.len(),
        gaps.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn real_lapwing_dictionaries_support_chapter_20_case_commands() {
    // KPA is "capitalize next" (`{:case:cap_first_word}`), not a skipped plugin command
    let dict = lapwing_dictionary();
    assert!(dict.get(&[parse_stroke("KPA").unwrap()]).is_some());
    assert!(dict.get(&[parse_stroke("KA*PD").unwrap()]).is_some());
}
