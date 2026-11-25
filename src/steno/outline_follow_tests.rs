//! `{:outline:next}` and `{:outline:explain}` following a whole translation, through a real
//! engine over a steno folder: multi-stroke entries, a plugin's joined run, a mod that continues
//! the outline, a pending `{:surround:next:...}`, undo and the double strike.

use super::unload_tests::Rig;
use super::*;
use crate::steno::journal;
use std::fs;

#[allow(clippy::literal_string_with_formatting_args)] // dictionary entries, not formats
const DICT: &str = r##"{
    "PHRO*PBT": "{:outline:next}", "PHRO*EPBT": "{:outline:explain}",
    "TPHAT": "national", "KPA": "{-|}", "TKAOEU": "die", "KPA/TKAOEU": "today",
    "#TR-RL": "{:surround:next:`}", "+-Z": "{^ize}"
}"##;

/// Reads the one joined run `TPHAT/+STKPWAEUGS`, and nothing else.
const READER: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    if input != b"TPHAT/+STKPWAEUGS" {
        return -1;
    }
    let mut out: Vec<u8> = b"internationalization".to_vec();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#;

/// An engine over the rig's folder plus [`DICT`], `+` an extra key; `joins` makes it the
/// layer's join key, and `files` are more files for the folder.
fn engine(rig: &Rig, joins: bool, files: &[(&str, &str)]) -> StenoEngine {
    fs::write(rig.dict_dir.join("00-outline.json"), DICT).unwrap();
    for (name, text) in files {
        fs::write(rig.dict_dir.join(name), text).unwrap();
    }
    let mut config = rig.config();
    config.extra_keys = vec!['+'];
    if joins {
        config.join_keys = vec!['+'];
    }
    // No reading is late by accident on a busy machine
    config.plugin_deadline_ms = 10_000;
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, true, &rig.config_dir, None, &history, "t").unwrap();
    engine.wait_until_loaded();
    engine
}

/// Strike each stroke (`*` is undo), and say what is on screen.
fn typed(engine: &mut StenoEngine, strokes: &str) -> String {
    for text in strokes.split_whitespace() {
        let stroke = engine
            .layout
            .parse_stroke(text)
            .unwrap_or_else(|| panic!("{text} is not a stroke"));
        engine.translate(stroke, false);
    }
    engine.translator.text()
}

fn joined_engine(name: &str) -> (Rig, StenoEngine) {
    let rig = Rig::new(name);
    let engine = engine(&rig, true, &[("03-reader.rs", READER)]);
    (rig, engine)
}

#[test]
fn explain_follows_a_joined_run_and_retypes_it_whole() {
    let (_rig, mut e) = joined_engine("outline-explain-run");
    assert_eq!(typed(&mut e, "PHRO*EPBT TPHAT"), "TPHAT -> national");
    assert_eq!(
        typed(&mut e, "+STKPWAEUGS"),
        "TPHAT/+STKPWAEUGS -> internationalization"
    );
    // A stroke that starts a translation of its own ends it, and translates as ever
    assert_eq!(
        typed(&mut e, "KAT"),
        "TPHAT/+STKPWAEUGS -> internationalization cat"
    );
}

#[test]
fn next_follows_a_joined_run() {
    let (_rig, mut e) = joined_engine("outline-next-run");
    assert_eq!(typed(&mut e, "PHRO*PBT TPHAT"), "TPHAT");
    assert_eq!(typed(&mut e, "+STKPWAEUGS"), "TPHAT/+STKPWAEUGS");
    assert_eq!(typed(&mut e, "TKOG"), "TPHAT/+STKPWAEUGS dog");
}

#[test]
fn explain_follows_a_multi_stroke_entry() {
    let (_rig, mut e) = joined_engine("outline-explain-entry");
    assert_eq!(typed(&mut e, "PHRO*EPBT KPA"), "KPA -> {-|}");
    assert_eq!(typed(&mut e, "TKAOEU"), "KPA/TKAOEU -> today");
    let (_rig, mut e) = joined_engine("outline-next-entry");
    assert_eq!(typed(&mut e, "PHRO*PBT KPA TKAOEU KAT"), "KPA/TKAOEU cat");
}

#[test]
fn a_pending_surround_wraps_the_explanation_each_time_it_is_typed() {
    let (_rig, mut e) = joined_engine("outline-surround");
    assert_eq!(
        typed(&mut e, "#TR-RL PHRO*EPBT TPHAT"),
        "`TPHAT -> national`"
    );
    assert_eq!(
        typed(&mut e, "+STKPWAEUGS"),
        "`TPHAT/+STKPWAEUGS -> internationalization`"
    );
    // Used up: the next word is not wrapped
    assert_eq!(
        typed(&mut e, "KAT"),
        "`TPHAT/+STKPWAEUGS -> internationalization` cat"
    );
}

#[test]
fn undo_takes_back_the_whole_explanation() {
    let (_rig, mut e) = joined_engine("outline-undo");
    assert_eq!(
        typed(&mut e, "KAT PHRO*EPBT TPHAT +STKPWAEUGS *"),
        "cat",
        "the explanation goes as one, the shorter one is not put back"
    );
    assert_eq!(typed(&mut e, "*"), "");
    // Undo in the middle of an entry too, and following has ended
    assert_eq!(typed(&mut e, "PHRO*PBT KPA *"), "");
    assert_eq!(typed(&mut e, "TKAOEU"), "die");
}

#[test]
fn the_arming_stroke_struck_twice_types_itself_once() {
    let (_rig, mut e) = joined_engine("outline-double");
    assert_eq!(typed(&mut e, "PHRO*PBT PHRO*PBT"), "PHRO*PBT");
    assert_eq!(typed(&mut e, "KAT"), "PHRO*PBT cat");
    let (_rig, mut e) = joined_engine("outline-double-explain");
    assert_eq!(
        typed(&mut e, "PHRO*EPBT PHRO*EPBT KAT"),
        "PHRO*EPBT -> {:outline:explain} cat"
    );
    // Struck again while following: following ends and it arms again
    let (_rig, mut e) = joined_engine("outline-rearm");
    assert_eq!(
        typed(&mut e, "PHRO*EPBT TPHAT PHRO*EPBT KAT"),
        "TPHAT -> national KAT -> cat"
    );
}

#[test]
fn a_mod_continues_the_outline_past_the_engines_grouping() {
    // `+` is no join key here: the engine starts a new translation with `+-Z`
    let rig = Rig::new("outline-no-mod");
    let mut e = engine(&rig, false, &[]);
    assert_eq!(typed(&mut e, "PHRO*EPBT TPHAT +-Z"), "TPHAT -> nationalize");
    // The documented mod makes every `+` stroke continue it
    let rig = Rig::new("outline-mod");
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/plugins/outline-join.rs"),
    )
    .unwrap();
    let mut e = engine(&rig, false, &[("04-outline-join.rs", &source)]);
    assert_eq!(
        typed(&mut e, "PHRO*EPBT TPHAT +-Z"),
        "TPHAT/+-Z -> national {^ize}"
    );
    assert_eq!(typed(&mut e, "KAT"), "TPHAT/+-Z -> national {^ize} cat");
    // And it changes nothing outside following
    assert_eq!(
        typed(&mut e, "TPHAT +-Z"),
        "TPHAT/+-Z -> national {^ize} cat nationalize"
    );
}

/// A stand-in for a theory's notation file: `TPH` is `N`, `-PL` is `M`, number strokes in letters.
const NOTATION: &str = r#"{"number": "letters", "left": {"TPH": "N"}, "right": {"PL": "M"}}"#;

#[test]
fn the_outline_commands_write_the_theorys_notation_when_its_folder_has_one() {
    let rig = Rig::new("outline-notation");
    let mut e = engine(
        &rig,
        false,
        &[(crate::steno::notation::FILE_NAME, NOTATION)],
    );
    assert_eq!(typed(&mut e, "PHRO*PBT TPHAT"), "NAT");
    assert_eq!(
        typed(&mut e, "PHRO*EPBT KPA TKAOEU"),
        "NAT KPA/TKAOEU -> today"
    );
    let rig = Rig::new("outline-notation-explain");
    let mut e = engine(
        &rig,
        false,
        &[(crate::steno::notation::FILE_NAME, NOTATION)],
    );
    assert_eq!(
        typed(&mut e, "PHRO*EPBT #TR-RL"),
        "#TR-RL -> {:surround:next:`}"
    );
    // The tape writes strokes the same way
    let path = rig.dict_dir.join("tape.txt");
    e.tape = Some(tape::Tape::with_layout(path.clone(), e.layout.clone()));
    e.load_notation();
    e.translate(e.layout.parse_stroke("TPHAT").unwrap(), false);
    e.tape.as_ref().unwrap().flush();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.split_whitespace().nth(1), Some("NAT"), "{text}");
}

#[test]
fn without_a_notation_file_the_outline_commands_write_plovers_notation() {
    let rig = Rig::new("outline-plover");
    let mut e = engine(&rig, false, &[]);
    assert_eq!(typed(&mut e, "PHRO*PBT TPHAT"), "TPHAT");
    assert_eq!(typed(&mut e, "PHRO*PBT #TR-RL"), "TPHAT 2R-R8");
}

/// A strict stand-in for lyk's notation: `TPH` is `N`, `-GS` is `ŠN`, `-PL` is `M`, and only
/// the outlines in `agree` have a known split (any other stroke is written a key each).
const STRICT: &str = r#"{"number": "letters", "left": {"TPH": "N"},
    "right": {"GS": "ŠN", "PL": "M"}, "agree": ["TPH-GS", "TPHAT", "+-PL"]}"#;

/// An engine over [`DICT`] and [`STRICT`] (plus `files`), with a tape file and its own overlay.
fn strict_engine(rig: &Rig, files: &[(&str, &str)]) -> StenoEngine {
    let words = r#"{"TPH-GS": "nation", "+-PL": "{^ism}"}"#;
    let mut all = vec![
        (crate::steno::notation::FILE_NAME, STRICT),
        ("01-words.json", words),
    ];
    all.extend_from_slice(files);
    let mut e = engine(rig, false, &all);
    e.hub = Box::leak(Box::default());
    e.tape = Some(tape::Tape::with_layout(
        rig.dict_dir.join("tape.txt"),
        e.layout.clone(),
    ));
    e.load_notation();
    e
}

/// Each stroke line of the tape file, as it writes the stroke.
fn tape_file_strokes(rig: &Rig, e: &StenoEngine) -> Vec<String> {
    e.tape.as_ref().unwrap().flush();
    fs::read_to_string(rig.dict_dir.join("tape.txt"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// Each line of the overlay's tape, as it draws the stroke.
fn overlay_strokes(e: &StenoEngine) -> Vec<String> {
    e.hub
        .snapshot()
        .tape
        .iter()
        .map(|entry| {
            e.notation
                .render_stroke_pieces_split(entry.stroke, entry.split.as_deref())
                .into_iter()
                .map(|piece| piece.text)
                .collect()
        })
        .collect()
}

#[test]
fn a_stroke_outline_next_took_is_on_the_tape_as_the_notation_it_typed() {
    let rig = Rig::new("outline-tape-next");
    let mut e = strict_engine(&rig, &[]);
    assert_eq!(typed(&mut e, "PHRO*PBT TPH-GS"), "N-ŠN");
    // The arming stroke is on the tape too, raw: it has no known split
    assert_eq!(tape_file_strokes(&rig, &e), ["PHRO*PBT", "N-ŠN"]);
    assert_eq!(overlay_strokes(&e), ["PHRO*PBT", "N-ŠN"]);
    // Translated as ever afterwards, and written the same way
    assert_eq!(typed(&mut e, "TPH-GS"), "N-ŠN nation");
    assert_eq!(tape_file_strokes(&rig, &e), ["PHRO*PBT", "N-ŠN", "N-ŠN"]);
    assert_eq!(overlay_strokes(&e), ["PHRO*PBT", "N-ŠN", "N-ŠN"]);
}

#[test]
fn explain_of_a_run_a_mod_joins_writes_each_stroke_as_its_tape_line() {
    let rig = Rig::new("outline-tape-explain-run");
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/plugins/outline-join.rs"),
    )
    .unwrap();
    let mut e = strict_engine(&rig, &[("04-outline-join.rs", &source)]);
    // `TPHAT/+-PL` is not known whole: each translation is written as its own outline is
    assert_eq!(
        typed(&mut e, "PHRO*EPBT TPHAT +-PL"),
        "NAT/+-M -> national {^ism}"
    );
    assert_eq!(tape_file_strokes(&rig, &e), ["PHRO*EPBT", "NAT", "+-M"]);
    assert_eq!(overlay_strokes(&e), ["PHRO*EPBT", "NAT", "+-M"]);
}

#[test]
fn a_stroke_with_no_known_split_stays_raw_when_an_outline_command_takes_it() {
    let rig = Rig::new("outline-tape-raw");
    let mut e = strict_engine(&rig, &[]);
    assert_eq!(typed(&mut e, "PHRO*PBT TPH-PL"), "TPH-PL");
    assert_eq!(tape_file_strokes(&rig, &e), ["PHRO*PBT", "TPH-PL"]);
    assert_eq!(overlay_strokes(&e), ["PHRO*PBT", "TPH-PL"]);
    // `{:outline:last}` writes the last translation as its tape line reads
    let rig = Rig::new("outline-tape-last");
    let mut e = strict_engine(&rig, &[("02-last.json", r#"{"HRAFT": "{:outline:last}"}"#)]);
    assert_eq!(typed(&mut e, "TPH-GS HRAFT"), "nation N-ŠN");
    assert_eq!(tape_file_strokes(&rig, &e), ["N-ŠN", "HRAFT"]);
}
