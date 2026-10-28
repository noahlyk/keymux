//! keymux against Plover: the same dictionaries and strokes go through both, and the text on
//! screen after every stroke, and the key combinations and commands sent, must be the same.
//!
//! The reference outputs come from running Plover's own translator and formatter
//! (`tools/plover-golden`). The cases cover every operator of the translation language, and
//! thousands of random stroke sequences over a dictionary that has all of them, with and without
//! undo, in the three ways a session can start, and with spaces before or after.

use super::dict::Dictionary;
use super::formatter::{Op, Options};
use super::layout::parse_stroke;
use super::orthography::Orthography;
use super::translate::{Translator, UNDO_STROKE};
use serde_json::Value;
use std::sync::Arc;

fn stroke(text: &str) -> u32 {
    if text == "*" {
        return UNDO_STROKE;
    }
    parse_stroke(text).unwrap_or_else(|| panic!("{text} is not a stroke"))
}

/// The dictionaries of a set, highest priority first.
fn dictionary(tables: &Value) -> Dictionary {
    let mut dict = Dictionary::default();
    for table in tables.as_array().unwrap() {
        dict.merge_json(&table.to_string()).unwrap();
    }
    dict.set_orthography(Orthography::from_wordlist(include_str!(
        "testdata/plover_golden_words.txt"
    )));
    dict
}

fn options(config: &Value) -> Options {
    let flag = |name: &str| config[name].as_bool().unwrap();
    Options {
        start_attached: flag("start_attached"),
        start_capitalized: flag("start_capitalized"),
        spaces_after: flag("spaces_after"),
        ..Options::default()
    }
}

/// What Plover reports for a step: combinations and commands, as written.
fn events(ops: &[Op]) -> Vec<Value> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Combo(combo) => Some(serde_json::json!(["combo", combo])),
            Op::Command(command) => Some(serde_json::json!(["command", command])),
            _ => None,
        })
        .collect()
}

#[test]
fn every_reference_case_types_what_plover_types() {
    let data: Value = serde_json::from_str(include_str!("testdata/plover_golden.json")).unwrap();
    let sets: std::collections::HashMap<String, Arc<Dictionary>> = data["sets"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, tables)| (name.clone(), Arc::new(dictionary(tables))))
        .collect();
    let mut failures = Vec::new();
    let cases = data["cases"].as_array().unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let dict = Arc::clone(&sets[case["set"].as_str().unwrap()]);
        let mut translator = Translator::with_shared(dict, options(&case["config"]));
        let strokes = case["strokes"].as_array().unwrap();
        for (step, stroke_text) in strokes.iter().enumerate() {
            let ops = translator.stroke_ops(stroke(stroke_text.as_str().unwrap()));
            let want_text = case["texts"][step].as_str().unwrap();
            let want_events = case["events"][step].as_array().unwrap();
            if translator.text() != want_text || events(&ops) != *want_events {
                let typed: Vec<&str> = strokes.iter().map(|s| s.as_str().unwrap()).collect();
                failures.push(format!(
                    "{name}: after stroke {} of {}\n    plover {want_text:?} {want_events:?}\n    keymux {:?} {:?}",
                    step + 1,
                    typed.join(" "),
                    translator.text(),
                    events(&ops)
                ));
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from Plover. The first ones:\n{}",
        failures.len(),
        cases.len(),
        failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}
