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

#[test]
#[ignore = "needs Lapwing's dictionaries in ~/.config/keymux/steno/lapwing; run with --ignored"]
fn plover_plugin_commands_are_skipped_not_typed() {
    // KPA is "capitalize next" in Lapwing, written with `{:case:...}`. keymux has no plugin
    // commands, so the entry is skipped at load time instead of typed out as text.
    let dict = lapwing_dictionary();
    assert!(dict.get(&[parse_stroke("KPA").unwrap()]).is_none());
}
