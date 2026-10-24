//! Checks against Lapwing's own theory. The single-stroke rows in `testdata/` come from the
//! Lapwing book (lapwing.aerick.ca), with each text taken from Lapwing's dictionaries.
//!
//! The always-on tests use only the committed rows. The `#[ignore]`d tests load the real
//! dictionaries from `~/.config/keymux/steno/lapwing/` (run `keymux steno theory lapwing`
//! first), the same files a Lapwing layer uses in production.

use super::dict::Dictionary;
use super::layout::parse_stroke;
use super::setup::LAPWING_FILES;
use super::translate::{StenoOutput, Translator};
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
    "H-PB": "{^}-{^}", "S-P": "{^ ^}", "S-PD": "{*?}", "SP-LS": "{*!}"
}"#;

/// Type `strokes` in order on a fresh translator, returning the text on screen.
fn type_strokes(json: &str, strokes: &[&str]) -> String {
    let mut dict = Dictionary::default();
    dict.merge_json(json).unwrap();
    let mut translator = Translator::new(dict);
    let mut typed = String::new();
    for stroke in strokes {
        match translator.stroke(parse_stroke(stroke).unwrap()) {
            None => {}
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
#[ignore = "known bug: `{^ ^}` parses as attach text \" ^\", so S-P types a stray caret"]
fn ch20_insert_space_then_next_word_has_one_space() {
    // S-P inserts a space by itself, so the next word must not add a second one
    assert_eq!(type_strokes(CH20, &["KAT", "S-P", "TKOG"]), "cat dog");
}

#[test]
#[ignore = "known bug: `{*!}` only removes a space that is already at the end of the text"]
fn ch20_retro_delete_space_suppresses_the_last_space() {
    // Lapwing ch20: SP-LS "retroactively suppress last space"
    assert_eq!(type_strokes(CH20, &["KAT", "TKOG", "SP-LS"]), "catdog");
}

#[test]
#[ignore = "known gap: keymux skips Plover's {:case:...} commands, so KPA types nothing (Lapwing ch20: capitalize next)"]
fn ch20_capitalize_next_word() {
    assert_eq!(type_strokes(CH20, &["KAT", "KPA", "TKOG"]), "cat Dog");
}

#[test]
#[ignore = "known gap: keymux skips Plover's {:retro_case:...} commands, so KA*PD types nothing (Lapwing ch20: retro capitalize)"]
fn ch20_retro_capitalize_last_word() {
    assert_eq!(type_strokes(CH20, &["KAT", "KA*PD"]), "Cat");
}

#[test]
#[ignore = "known bug: unsupported {~|...} quote commands are typed as literal text"]
fn ch20_opening_quote_attaches_to_the_next_word() {
    let json = r#"{"KAT": "cat", "TKOG": "dog", "KW-GS": "{~|\"^}"}"#;
    assert_eq!(type_strokes(json, &["KAT", "KW-GS", "TKOG"]), r#"cat "dog"#);
}

#[test]
#[ignore = "known bug: unsupported Enter command ({^~|\\n^}) is typed as literal text"]
fn ch20_enter_key_is_typed() {
    let json = r#"{"KAT": "cat", "R-R": "{^~|\n^}"}"#;
    assert_eq!(type_strokes(json, &["KAT", "R-R"]), "cat\n");
}

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn plover_plugin_commands_are_skipped_not_typed() {
    // KPA is "capitalize next" in Lapwing, written with `{:case:...}`. keymux has no plugin
    // commands, so the entry is skipped at load time instead of typed out as text.
    let dict = lapwing_dictionary();
    assert!(dict.get(&[parse_stroke("KPA").unwrap()]).is_none());
}
