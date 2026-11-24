//! Text with characters the keyboard has no key for: which way each piece goes, in what
//! order, and that a backspace stays one character however the character was typed.

use super::emission_tests::Recorder;
use super::output::{self, Pacer, Run, Sink, Unicode};
use super::ProcessResult;
use crate::config::UnicodeOutput;
use crate::steno::translate::OutputOp;
use crate::unicode::{Failure, Helper};
use evdev::{EventType, InputEvent, Key};
use std::cell::RefCell;
use std::io;

/// What left keymux, in order: keys on the virtual keyboard, or a job for the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    /// A key event (code, value)
    Key(u16, i32),
    /// A job the helper typed: backspaces, text
    Helper(usize, String),
}

/// The virtual keyboard and the helper writing to one log, so their order shows.
struct Both<'a>(&'a RefCell<Vec<Out>>);

impl Sink for Both<'_> {
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()> {
        self.0.borrow_mut().extend(
            events
                .iter()
                .filter(|e| e.event_type() == EventType::KEY)
                .map(|e| Out::Key(e.code(), e.value())),
        );
        Ok(())
    }
}

/// A helper that types (into the shared log) or fails as told.
struct FakeHelper<'a> {
    log: &'a RefCell<Vec<Out>>,
    fails: Option<Failure>,
}

impl Helper for FakeHelper<'_> {
    fn type_text(&self, backspaces: usize, text: &str) -> Result<(), Failure> {
        if let Some(failure) = self.fails {
            return Err(failure);
        }
        self.log
            .borrow_mut()
            .push(Out::Helper(backspaces, text.to_string()));
        Ok(())
    }
}

fn send(mode: UnicodeOutput, fails: Option<Failure>, result: ProcessResult) -> Vec<Out> {
    let log = RefCell::new(Vec::new());
    let helper = FakeHelper { log: &log, fails };
    let unicode = Unicode {
        mode,
        helper: Some(&helper),
    };
    output::emit_result(&mut Both(&log), &mut Pacer::default(), unicode, result).unwrap();
    log.into_inner()
}

/// The key presses only, as evdev key codes.
fn presses(out: &[Out]) -> Vec<u16> {
    out.iter()
        .filter_map(|o| match o {
            Out::Key(code, 1) => Some(*code),
            _ => None,
        })
        .collect()
}

const BS: u16 = Key::KEY_BACKSPACE.code();

#[test]
fn text_is_cut_into_runs_with_keys_and_runs_without() {
    assert_eq!(output::runs(""), vec![]);
    assert_eq!(output::runs("night"), vec![Run::Keys("night")]);
    assert_eq!(output::runs("—"), vec![Run::Unicode("—")]);
    assert_eq!(
        output::runs("NĪT -> night"),
        vec![Run::Keys("N"), Run::Unicode("Ī"), Run::Keys("T -> night")]
    );
    assert_eq!(
        output::runs("a—–b°³²"),
        vec![
            Run::Keys("a"),
            Run::Unicode("—–"),
            Run::Keys("b"),
            Run::Unicode("°³²")
        ]
    );
    // Enter and Tab have keys; a control character with none is a run of its own
    assert_eq!(output::runs("a\n\tb"), vec![Run::Keys("a\n\tb")]);
    assert_eq!(
        output::runs("a\rb"),
        vec![Run::Keys("a"), Run::Unicode("\r"), Run::Keys("b")]
    );
}

#[test]
fn keyable_text_goes_out_as_keys_exactly_as_before_whatever_the_mode() {
    let result = || ProcessResult::Retype {
        backspaces: 2,
        text: "Ab -> c".into(),
    };
    let before = {
        let mut recorder = Recorder::default();
        output::emit_result(
            &mut recorder,
            &mut Pacer::default(),
            Unicode::default(),
            result(),
        )
        .unwrap();
        recorder.keys()
    };
    for mode in [
        UnicodeOutput::Auto,
        UnicodeOutput::Wayland,
        UnicodeOutput::CtrlShiftU,
        UnicodeOutput::Off,
    ] {
        let out = send(mode, None, result());
        assert!(
            !out.iter().any(|o| matches!(o, Out::Helper(..))),
            "{mode:?}"
        );
        let keys: Vec<(u16, i32)> = out
            .iter()
            .map(|o| match o {
                Out::Key(code, value) => (*code, *value),
                Out::Helper(..) => unreachable!(),
            })
            .collect();
        assert_eq!(keys, before, "{mode:?}");
    }
}

#[test]
fn text_with_a_character_without_a_key_goes_whole_to_the_helper_with_its_backspaces() {
    let out = send(
        UnicodeOutput::Auto,
        None,
        ProcessResult::Retype {
            backspaces: 3,
            text: "NĪT -> night".into(),
        },
    );
    // Not split: the backspaces and every character, the keyable ones too, in one job
    assert_eq!(out, vec![Out::Helper(3, "NĪT -> night".into())]);
}

#[test]
fn keys_before_and_after_a_helper_job_stay_in_order() {
    let out = send(
        UnicodeOutput::Wayland,
        None,
        ProcessResult::Ops(vec![
            OutputOp::Text("so".into()),
            OutputOp::Backspaces(1),
            OutputOp::Text(" — ".into()),
            OutputOp::Text("then".into()),
        ]),
    );
    let helper_at = out
        .iter()
        .position(|o| matches!(o, Out::Helper(..)))
        .unwrap();
    assert_eq!(out[helper_at], Out::Helper(1, " — ".into()));
    assert_eq!(
        presses(&out[..helper_at]),
        vec![Key::KEY_S.code(), Key::KEY_O.code()]
    );
    assert_eq!(
        presses(&out[helper_at + 1..]),
        [Key::KEY_T, Key::KEY_H, Key::KEY_E, Key::KEY_N].map(Key::code)
    );
}

#[test]
fn keys_sent_just_before_a_helper_job_have_time_to_be_read_first() {
    let log = RefCell::new(Vec::new());
    let helper = FakeHelper {
        log: &log,
        fails: None,
    };
    let unicode = Unicode {
        mode: UnicodeOutput::Auto,
        helper: Some(&helper),
    };
    let mut pacer = Pacer::default();
    let started = std::time::Instant::now();
    output::emit_result(
        &mut Both(&log),
        &mut pacer,
        unicode,
        ProcessResult::Ops(vec![OutputOp::Text("a".into()), OutputOp::Text("é".into())]),
    )
    .unwrap();
    assert!(
        started.elapsed() >= output::UNREAD_WINDOW,
        "the helper waited for the keys before it: {:?}",
        started.elapsed()
    );
    // With nothing sent before, a job goes at once
    let started = std::time::Instant::now();
    output::emit_result(
        &mut Both(&log),
        &mut Pacer::default(),
        unicode,
        ProcessResult::TypeString("é".into()),
    )
    .unwrap();
    assert!(started.elapsed() < output::UNREAD_WINDOW);
}

#[test]
fn without_a_helper_each_character_without_a_key_is_one_placeholder() {
    for fails in [Failure::NoHelper, Failure::NothingTyped] {
        let out = send(
            UnicodeOutput::Auto,
            Some(fails),
            ProcessResult::Retype {
                backspaces: 1,
                text: "NĪT—".into(),
            },
        );
        // One key per character, so the steno engine's count of what is on screen holds
        let slash = Key::KEY_SLASH.code();
        assert_eq!(
            presses(&out),
            vec![
                BS,
                Key::KEY_LEFTSHIFT.code(),
                Key::KEY_N.code(),
                slash,
                Key::KEY_T.code(),
                slash
            ],
            "{fails:?}"
        );
    }
    assert_eq!(output::placeholders("NĪT -> night"), "N?T -> night");
    assert_eq!(output::placeholders("—"), "?");
    // Off types the placeholders without asking any helper
    let out = send(
        UnicodeOutput::Off,
        None,
        ProcessResult::TypeString("é".into()),
    );
    assert_eq!(out.len(), 4, "shift and / pressed and released: {out:?}");
}

#[test]
fn a_helper_that_failed_mid_text_is_not_typed_over() {
    let out = send(
        UnicodeOutput::Auto,
        Some(Failure::Unknown),
        ProcessResult::TypeString("é".into()),
    );
    assert_eq!(out, vec![], "part of it may be on screen: nothing more");
}

#[test]
fn undoing_unicode_text_deletes_one_backspace_per_character() {
    // What the steno engine sends to take back `NĪT —`: a backspace per char, not per byte
    let text = "NĪT —";
    let undo = ProcessResult::Retype {
        backspaces: text.chars().count(),
        text: String::new(),
    };
    let out = send(UnicodeOutput::Auto, None, undo);
    assert_eq!(presses(&out), vec![BS; 5], "keys only: nothing to type");
    assert_ne!(text.len(), 5, "the bytes would be more");
    // A correction that replaces it goes to the helper with the same count
    let out = send(
        UnicodeOutput::Auto,
        None,
        ProcessResult::Ops(vec![OutputOp::Backspaces(5), OutputOp::Text("Ā".into())]),
    );
    assert_eq!(out, vec![Out::Helper(5, "Ā".into())]);
}

#[test]
fn ctrl_shift_u_types_the_code_point_in_hex_between_the_keys_around_it() {
    let out = send(
        UnicodeOutput::CtrlShiftU,
        None,
        ProcessResult::Retype {
            backspaces: 1,
            text: "a—b".into(),
        },
    );
    assert!(!out.iter().any(|o| matches!(o, Out::Helper(..))));
    let keys: Vec<(u16, i32)> = out
        .iter()
        .map(|o| match o {
            Out::Key(code, value) => (*code, *value),
            Out::Helper(..) => unreachable!(),
        })
        .collect();
    let tap = |key: Key| [(key.code(), 1), (key.code(), 0)];
    let mut expected = Vec::new();
    expected.extend(tap(Key::KEY_BACKSPACE));
    expected.extend(tap(Key::KEY_A));
    let (ctrl, shift) = (Key::KEY_LEFTCTRL.code(), Key::KEY_LEFTSHIFT.code());
    expected.extend([(ctrl, 1), (shift, 1)]);
    expected.extend(tap(Key::KEY_U));
    expected.extend([(shift, 0), (ctrl, 0)]);
    // U+2014, then Space to finish it
    for key in [
        Key::KEY_2,
        Key::KEY_0,
        Key::KEY_1,
        Key::KEY_4,
        Key::KEY_SPACE,
    ] {
        expected.extend(tap(key));
    }
    expected.extend(tap(Key::KEY_B));
    assert_eq!(keys, expected);
}
