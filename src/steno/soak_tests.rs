//! The engine under a virtual keyboard, thousands of times over.
//!
//! The same strokes go in again and again with the timing shaken up each run - the keys of a
//! chord in a different order, different gaps between them, the daemon stalled now and then -
//! and every run must type exactly the same thing. Nothing here touches a device, so it runs
//! alongside normal typing. Too slow for every `cargo test`; run it on purpose:
//!
//! ```sh
//! cargo test --release --lib soak -- --ignored --nocapture
//! KEYMUX_SOAK_RUNS=100000 cargo test --release --lib soak -- --ignored --nocapture
//! ```
//!
//! A failure names the run that differed and its seed: the same seed replays the same timing.

use super::dict::Dictionary;
use super::engine::StenoEngine;
use super::layout::{parse_stroke, Layout};
use super::tape::describe;
use super::translate::{OutputOp, StenoOutput};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEFAULT_RUNS: usize = 10_000;

/// How often the daemon's loop checks the timeouts while no key is pressed.
const TICK: Duration = Duration::from_millis(5);

fn runs() -> usize {
    std::env::var("KEYMUX_SOAK_RUNS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(DEFAULT_RUNS)
}

/// The words of one Monkeytype run, with the outlines the tape recorded for them, misstrokes
/// and all.
const DICT: &str = r#"{
    "KAUL": "call", "THE": "they", "U": "you", "PHAE": "may", "TPAES": "face",
    "TKPWEPBS": "against", "UT": "but", "RAET": "great", "ORT": "otter", "ORD": "order",
    "-T": "the", "-D": "do", "PERPB": "person", "AOUS": "use", "TKP-P": "fkp",
    "TK-P": "develop", "TPH": "in", "O*ET": "both", "OET": "to get", "OLD": "old",
    "HOLD": "hold", "STKEUPB": "begin", "KORS": "course", "HRAEUG": "laying",
    "HRAEUBG": "laboring", "HRAOEUBG": "like", "PHOFT": "most", "WEUFP": "which",
    "KAT": "cat", "*": "=undo"
}"#;

/// That run as the tape recorded it: every misstroke, and the `*` that took it back.
const SCRIPT: &[&str] = &[
    "KAUL", "THE", "U", "PHAE", "TPAES", "*", "TPAES", "TKPWEPBS", "*", "TKPWEPBS", "UT",
    "RAET", "ORT", "*", "ORD", "-T", "-D", "PERPB", "AOUS", "TKP-P", "*", "TK-P", "TPH",
    "O*ET", "OET", "*", "OLD", "HOLD", "STKEUPB", "KORS", "*", "KORS", "HRAEUG", "*",
    "HRAEUBG", "*", "HRAOEUBG", "PHOFT", "WEUFP",
];

const EXPECTED: &str = "call they you may face against but great order the do person use \
                        develop in both old hold begin course like most which";

/// xorshift64*: the same seed gives the same run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn millis(&mut self, low: u64, high: u64) -> Duration {
        Duration::from_millis(low + self.below(high - low + 1))
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = usize::try_from(self.below(i as u64 + 1)).unwrap();
            items.swap(i, j);
        }
    }
}

fn seed(run: usize) -> u64 {
    0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(run as u64 + 1) | 1
}

/// A keyboard whose keys a script presses, fed to the engine the way the daemon feeds it: each
/// press and release at its moment, and a tick every few milliseconds between them.
struct VirtualKeyboard {
    engine: StenoEngine,
    rng: Rng,
    now: Instant,
    /// What the daemon's keystrokes would leave on screen
    screen: String,
    /// The tape description of every output, in order
    tape: Vec<String>,
    /// The daemon is away: nothing ticks until it is back, and then it reads every event that
    /// happened meanwhile at the moment it happened
    stalled: bool,
    stall_next: bool,
}

impl VirtualKeyboard {
    fn new(seed: u64) -> Self {
        let mut dict = Dictionary::default();
        dict.merge_json(DICT, &Layout::plover()).unwrap();
        Self {
            engine: StenoEngine::for_tests(dict, &HashMap::new()),
            rng: Rng(seed),
            now: Instant::now(),
            screen: String::new(),
            tape: Vec::new(),
            stalled: false,
            stall_next: false,
        }
    }

    /// One physical key per key of the stroke, picking either of the two `*` keys.
    fn keys(&mut self, notation: &str) -> Vec<KeyCode> {
        let bits = parse_stroke(notation).unwrap_or_else(|| panic!("bad stroke {notation}"));
        let mut keys = Vec::new();
        for bit in (0..32).map(|i| 1u32 << i).filter(|bit| bits & bit != 0) {
            let mut candidates: Vec<KeyCode> = self
                .engine
                .key_map()
                .iter()
                .filter(|(_, &b)| b == bit)
                .map(|(&key, _)| key)
                .collect();
            assert!(!candidates.is_empty(), "no key for bit {bit:#x} of {notation}");
            candidates.sort_by_key(|key| key.code());
            let pick = usize::try_from(self.rng.below(candidates.len() as u64)).unwrap();
            keys.push(candidates[pick]);
        }
        keys
    }

    /// Time passes, with the daemon ticking along unless it is stalled.
    fn advance(&mut self, by: Duration) {
        let end = self.now + by;
        if !self.stalled {
            while self.now + TICK < end {
                self.now += TICK;
                self.tick();
            }
        }
        self.now = end;
        if !self.stalled {
            self.tick();
        }
    }

    fn tick(&mut self) {
        if let Some(output) = self.engine.tick(self.now) {
            self.take(output);
        }
    }

    fn take(&mut self, output: StenoOutput) {
        self.tape.push(describe(Some(&output), false));
        apply(&mut self.screen, &output);
    }

    fn press(&mut self, key: KeyCode) {
        self.engine.press(key, self.now);
    }

    fn release(&mut self, key: KeyCode) {
        if let Some(output) = self.engine.release_at(key, self.now) {
            self.take(output);
        }
    }

    /// Stroke `notation`: its keys go down in some order, stay a moment, and come up in some
    /// order. If the daemon stalls over this stroke, it reads all of that afterwards.
    fn stroke(&mut self, notation: &str) {
        self.stalled = std::mem::take(&mut self.stall_next);
        let mut keys = self.keys(notation);
        self.rng.shuffle(&mut keys);
        for &key in &keys {
            let gap = self.rng.millis(0, 25);
            self.advance(gap);
            self.press(key);
        }
        let hold = self.rng.millis(5, 60);
        self.advance(hold);
        self.rng.shuffle(&mut keys);
        for &key in &keys {
            let gap = self.rng.millis(0, 25);
            self.advance(gap);
            self.release(key);
        }
        if std::mem::take(&mut self.stalled) {
            self.tick();
        }
    }

    /// The gap between strokes. Now and then the daemon will be away for the next one.
    fn pause(&mut self) {
        let gap = self.rng.millis(20, 400);
        self.advance(gap);
        self.stall_next = self.rng.below(20) == 0;
    }
}

/// What keystrokes for `output` leave on screen.
fn apply(screen: &mut String, output: &StenoOutput) {
    match output {
        StenoOutput::Type(text) => screen.push_str(text),
        StenoOutput::Retype { backspaces, text } => {
            backspace(screen, *backspaces);
            screen.push_str(text);
        }
        StenoOutput::Ops(ops) => {
            for op in ops {
                match op {
                    OutputOp::Backspaces(count) => backspace(screen, *count),
                    OutputOp::Text(text) => screen.push_str(text),
                    OutputOp::Keys(_) | OutputOp::Command(_) => {}
                }
            }
        }
    }
}

fn backspace(screen: &mut String, count: usize) {
    for _ in 0..count {
        screen.pop();
    }
}

/// The tape and the screen after the script, with run `run`'s timing.
fn typed(run: usize) -> (Vec<String>, String) {
    let mut keyboard = VirtualKeyboard::new(seed(run));
    for stroke in SCRIPT {
        keyboard.stroke(stroke);
        keyboard.pause();
    }
    (keyboard.tape, keyboard.screen)
}

fn assert_runs_identical(runs: usize) {
    let (tape, screen) = typed(0);
    assert_eq!(screen.trim(), EXPECTED, "run 0 tape:\n{}", tape.join("\n"));
    let started = Instant::now();
    for run in 1..runs {
        let (other_tape, other_screen) = typed(run);
        if other_tape == tape && other_screen == screen {
            continue;
        }
        let first = other_tape
            .iter()
            .zip(&tape)
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| tape.len().min(other_tape.len()));
        panic!(
            "run {run} (seed {:#x}) differs from run 0 at output {first} \
             (stroke {}):\n  run 0:   {:?}\n  run {run}: {:?}\n  screen: {other_screen:?}",
            seed(run),
            SCRIPT.get(first).unwrap_or(&"?"),
            tape.get(first),
            other_tape.get(first),
        );
    }
    eprintln!("soak: {runs} runs typed the same text, in {:.1?}", started.elapsed());
}

/// A handful of runs, so the harness itself is checked on every `cargo test`.
#[test]
fn a_few_runs_type_the_same_text() {
    assert_runs_identical(20);
}

#[test]
#[ignore = "thousands of runs: cargo test --release --lib soak -- --ignored --nocapture"]
fn soak_the_same_strokes_type_the_same_text_whatever_the_timing() {
    assert_runs_identical(runs());
}

/// A stroke tapped and then held repeats: once the hold is 300ms old and every 40ms after,
/// however its keys landed. Released after 480ms that is the tap, the repeat that starts at
/// 300ms, and four more.
fn held_double_tap(run: usize, stalled_while_held: bool) -> VirtualKeyboard {
    let mut keyboard = VirtualKeyboard::new(seed(run));
    keyboard.stroke("KAT");
    let gap = keyboard.rng.millis(50, 150);
    keyboard.advance(gap);
    let mut keys = keyboard.keys("KAT");
    keyboard.rng.shuffle(&mut keys);
    for &key in &keys {
        let gap = keyboard.rng.millis(0, 10);
        keyboard.advance(gap);
        keyboard.press(key);
    }
    keyboard.stalled = stalled_while_held;
    keyboard.advance(Duration::from_millis(480));
    keyboard.rng.shuffle(&mut keys);
    for &key in &keys {
        keyboard.release(key);
    }
    if std::mem::take(&mut keyboard.stalled) {
        keyboard.tick();
    }
    keyboard
}

#[test]
#[ignore = "thousands of runs: cargo test --release --lib soak -- --ignored --nocapture"]
fn soak_a_held_double_tap_repeats_the_same_number_of_times_every_run() {
    for run in 0..runs() {
        let keyboard = held_double_tap(run, false);
        assert_eq!(
            keyboard.screen.trim(),
            "cat cat cat cat cat cat",
            "run {run} (seed {:#x}) tape:\n{}",
            seed(run),
            keyboard.tape.join("\n")
        );
    }
}

/// The daemon away for the whole hold reads the press and the release together afterwards:
/// that is one stroke, never a repeat it missed the ticks for.
#[test]
#[ignore = "thousands of runs: cargo test --release --lib soak -- --ignored --nocapture"]
fn soak_a_stall_over_a_held_stroke_never_repeats_it() {
    for run in 0..runs() {
        let keyboard = held_double_tap(run, true);
        assert_eq!(
            keyboard.screen.trim(),
            "cat cat",
            "run {run} (seed {:#x}) tape:\n{}",
            seed(run),
            keyboard.tape.join("\n")
        );
    }
}

#[test]
fn a_held_double_tap_repeats_and_a_stall_over_it_does_not() {
    assert_eq!(
        held_double_tap(0, false).screen.trim(),
        "cat cat cat cat cat cat"
    );
    assert_eq!(held_double_tap(0, true).screen.trim(), "cat cat");
}
