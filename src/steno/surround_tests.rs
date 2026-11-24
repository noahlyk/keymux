//! `{:surround:next:OPEN:CLOSE}` and `{:surround:last:OPEN:CLOSE}`: a word typed between a pair
//! of marks, armed before it or put around it after, and what undo does with each.

use super::dict::Dictionary;
use super::formatter::Options;
use super::layout::Layout;
use super::meta::surround_args;
use super::translate::{Translator, UNDO_STROKE};

const DICT: &str = r#"{
    "KAT": "cat", "TKOG": "dog", "EBGS": "ex", "EBGS/AFRP": "example",
    "PRA": "{:surround:next:(:)}", "PRAL": "{:surround:last:(:)}",
    "KWO": "{:surround:next:\"}", "KWOL": "{:surround:last:\"}",
    "PWR": "{:surround:next:\\{:\\}}", "KHR": "{:surround:last:\\:}",
    "TP-PL": "{.}", "KPA": "{}{-|}", "R-R": "{#Return}", "-G": "{^ing}"
}"#;

fn translator() -> Translator {
    let mut dict = Dictionary::default();
    dict.merge_json(DICT, &Layout::plover()).unwrap();
    Translator::new(dict)
}

fn strike(t: &mut Translator, strokes: &[&str]) {
    for text in strokes {
        let stroke = if *text == "*" {
            UNDO_STROKE
        } else {
            Layout::plover().parse_stroke(text).unwrap()
        };
        t.stroke(stroke);
    }
}

fn typed(strokes: &[&str]) -> String {
    let mut t = translator();
    strike(&mut t, strokes);
    t.text()
}

#[test]
fn next_types_the_next_word_between_the_marks() {
    assert_eq!(typed(&["KAT", "PRA", "TKOG"]), "cat (dog)");
    // Only the one word: the marks are used up
    assert_eq!(typed(&["KAT", "PRA", "TKOG", "KAT"]), "cat (dog) cat");
    // Punctuation and suffixes attach after the close mark
    assert_eq!(typed(&["KAT", "PRA", "TKOG", "TP-PL"]), "cat (dog).");
    assert_eq!(typed(&["KAT", "KWO", "TKOG", "-G"]), "cat \"dog\"ing");
    // A brace is escaped as in any text
    assert_eq!(typed(&["KAT", "PWR", "TKOG"]), "cat {dog}");
}

#[test]
fn next_wraps_a_whole_outline_of_several_strokes() {
    assert_eq!(typed(&["KAT", "PRA", "EBGS"]), "cat (ex)");
    assert_eq!(typed(&["KAT", "PRA", "EBGS", "AFRP"]), "cat (example)");
}

#[test]
fn next_keeps_the_capital_and_passes_through_a_case_stroke() {
    assert_eq!(typed(&["KAT", "TP-PL", "KWO", "TKOG"]), "cat. \"Dog\"");
    assert_eq!(typed(&["KAT", "KWO", "KPA", "TKOG"]), "cat \"Dog\"");
}

#[test]
fn a_key_stroke_after_next_drops_the_marks() {
    let text = typed(&["KAT", "PRA", "R-R", "TKOG"]);
    assert!(!text.contains('(') && !text.contains(')'), "{text:?}");
}

#[test]
fn undo_takes_the_wrapped_word_back_and_leaves_the_marks_armed() {
    let mut t = translator();
    strike(&mut t, &["KAT", "PRA", "TKOG"]);
    strike(&mut t, &["*"]);
    assert_eq!(t.text(), "cat");
    strike(&mut t, &["TKOG"]);
    assert_eq!(t.text(), "cat (dog)");
}

#[test]
fn undo_while_armed_only_disarms() {
    let mut t = translator();
    strike(&mut t, &["KAT", "PRA", "*"]);
    assert_eq!(t.text(), "cat");
    strike(&mut t, &["TKOG"]);
    assert_eq!(t.text(), "cat dog");
    // And after a wrapped word: the word, then the marks
    let mut t = translator();
    strike(&mut t, &["KAT", "PRA", "TKOG", "*", "*", "TKOG"]);
    assert_eq!(t.text(), "cat dog");
}

#[test]
fn last_puts_the_marks_around_the_word_just_typed() {
    assert_eq!(typed(&["KAT", "TKOG", "PRAL"]), "cat (dog)");
    assert_eq!(typed(&["KAT", "TKOG", "KWOL", "KAT"]), "cat \"dog\" cat");
    assert_eq!(typed(&["KAT", "KHR"]), ":cat:");
    // The text since the last space, punctuation and all
    assert_eq!(typed(&["KAT", "TKOG", "TP-PL", "PRAL"]), "cat (dog.)");
    // Nothing typed: nothing to wrap
    assert_eq!(typed(&["PRAL"]), "");
}

#[test]
fn undo_after_last_puts_the_word_back() {
    let mut t = translator();
    strike(&mut t, &["KAT", "TKOG", "PRAL"]);
    strike(&mut t, &["*"]);
    assert_eq!(t.text(), "cat dog");
    strike(&mut t, &["*"]);
    assert_eq!(t.text(), "cat");
}

#[test]
fn both_work_with_spaces_after() {
    let mut t = translator();
    t.set_options(Options {
        spaces_after: true,
        ..Options::default()
    });
    strike(&mut t, &["KAT", "PRA", "TKOG", "KAT"]);
    assert_eq!(t.text(), "cat (dog) cat ");
    strike(&mut t, &["PRAL"]);
    assert_eq!(t.text(), "cat (dog) (cat) ");
    strike(&mut t, &["*", "*"]);
    assert_eq!(t.text(), "cat (dog) ");
}

#[test]
fn the_arguments_are_a_verb_and_one_or_two_marks() {
    let marks = |param: &str| surround_args(param).map(|(next, s)| (next, s.open, s.close));
    assert_eq!(marks("next:(:)"), Some((true, "(".into(), ")".into())));
    assert_eq!(marks("LAST:\""), Some((false, "\"".into(), "\"".into())));
    assert_eq!(marks(r"next:\:"), Some((true, ":".into(), ":".into())));
    assert_eq!(marks(r"next:a\\:b"), Some((true, r"a\".into(), "b".into())));
    assert_eq!(marks("next:<<:>>"), Some((true, "<<".into(), ">>".into())));
    assert_eq!(marks("next"), None);
    assert_eq!(marks("next:"), None);
    assert_eq!(marks("middle:(:)"), None);
    assert_eq!(marks("next:(:):x"), None);
}
