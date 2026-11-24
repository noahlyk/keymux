//! A plugin's `history_len` and `history_stroke` imports, through real plugins compiled to
//! wasm and a real translator: the strokes before the run being asked about, one at a time.

use super::formatter::Options;
use super::layout::Layout;
use super::plugin::{Before, Chain};
use super::strokes::StrokeLog;
use super::translate::Translator;
use super::worker::Budget;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A folder for one test, gone when the test is.
struct Dir(crate::test_dir::TestDir);

impl Dir {
    fn new(test: &str) -> Self {
        Self(crate::test_dir::TestDir::new(&format!(
            "keymux-history-test-{test}"
        )))
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn chain(&self, files: &[PathBuf]) -> Chain {
        Chain::load_cached(files, &self.0.join("cache"), &Layout::plover())
    }
}

/// The plugin the tests read with. It reads every outline as `LEN|S0,S1,...` (`LEN` what
/// `history_len` said, `-` if `len` is false; then `history_stroke(k)` for `k` from 0, up to and
/// including the first -1 or until there are `most` of them). Bit 23 is its join key.
fn reader(most: u32, len: bool) -> String {
    format!(
        r#"
#[link(wasm_import_module = "keymux")]
extern "C" {{
    fn history_len() -> i32;
    fn history_stroke(k: i32) -> i64;
}}

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {{
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}}

#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 {{
    let len = if {len} {{ unsafe {{ history_len() }}.to_string() }} else {{ "-".to_string() }};
    let mut seen = Vec::new();
    for k in 0..{most} {{
        let stroke = unsafe {{ history_stroke(k) }};
        seen.push(stroke.to_string());
        if stroke < 0 {{
            break;
        }}
    }}
    let mut out = format!("{{len}}|{{}}", seen.join(",")).into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}}

#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {{
    i32::from(bits & (1 << 23) != 0)
}}
"#
    )
}

fn bits(text: &str) -> u32 {
    if text == "*" {
        return super::translate::UNDO_STROKE;
    }
    let (join, text) = text
        .strip_prefix('+')
        .map_or((0, text), |rest| (1 << 23, rest));
    join | Layout::plover().parse_stroke(text).unwrap()
}

fn translator(dir: &Dir, dict: &str, plugin: &str) -> Translator {
    let chain = dir.chain(&[
        dir.file("01-dict.json", dict),
        dir.file("02-reader.rs", plugin),
    ]);
    Translator::with_shared(
        Arc::new(chain),
        Options {
            start_attached: true,
            ..Options::default()
        },
    )
}

fn strike(t: &mut Translator, strokes: &[&str]) {
    for stroke in strokes {
        t.stroke(bits(stroke));
    }
}

/// What the plugin said when asked about a stroke whose history is `before` (strokes by name).
fn said(len: Option<usize>, before: &[&str]) -> String {
    let mut seen: Vec<String> = before.iter().map(|s| bits(s).to_string()).collect();
    seen.push("-1".into());
    format!(
        "{}|{}",
        len.map_or_else(|| "-".to_string(), |n| n.to_string()),
        seen.join(",")
    )
}

const UNDO_DICT: &str = r#"{"*": "=undo", "HREFT": "{#Left}"}"#;

#[test]
fn history_is_read_back_from_the_stroke_before_the_run() {
    let dir = Dir::new("walk");
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["KAT", "TOP", "TKOG"]);
    assert_eq!(
        t.text(),
        format!(
            "{} {} {}",
            said(Some(0), &[]),
            said(Some(1), &["KAT"]),
            said(Some(2), &["TOP", "KAT"])
        )
    );
}

#[test]
fn the_runs_own_strokes_are_not_history() {
    let dir = Dir::new("run");
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["PWAT", "KAT", "+TOP", "+TKOG"]);
    assert_eq!(
        t.text(),
        format!("{} {}", said(Some(0), &[]), said(Some(1), &["PWAT"])),
        "the three strokes are one word, read with only PWAT before it"
    );
}

#[test]
fn undo_takes_strokes_out_of_history() {
    let dir = Dir::new("undo");
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["KAT", "TOP", "TKOG", "*"]);
    assert_eq!(
        t.text(),
        format!("{} {}", said(Some(0), &[]), said(Some(1), &["KAT"]))
    );
    strike(&mut t, &["*", "PWAT", "SKP"]);
    assert_eq!(
        t.text(),
        format!(
            "{} {} {}",
            said(Some(0), &[]),
            said(Some(1), &["KAT"]),
            said(Some(2), &["PWAT", "KAT"])
        ),
        "KAT, then PWAT (after TOP was taken back) and SKP"
    );
}

#[test]
fn a_wall_empties_history() {
    let dir = Dir::new("walls");
    // A window change, a key press: start_fresh
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["KAT", "TOP"]);
    t.start_fresh();
    strike(&mut t, &["TKOG", "PWAT"]);
    assert_eq!(
        t.text(),
        format!("{} {}", said(Some(0), &[]), said(Some(1), &["TKOG"]))
    );
    // A stroke that moves the cursor: nothing before it is history, itself included
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["KAT", "HREFT", "TOP", "TKOG"]);
    assert!(
        t.text().ends_with(&format!(
            "{} {}",
            said(Some(0), &[]),
            said(Some(1), &["TOP"])
        )),
        "{}",
        t.text()
    );
    // Text typed by hand
    let mut t = translator(&dir, UNDO_DICT, &reader(16, true));
    strike(&mut t, &["KAT"]);
    {
        let shared = t.history();
        let mut history = super::journal::lock(&shared);
        for ch in " hi ".chars() {
            history.typed(ch);
        }
    }
    strike(&mut t, &["TOP", "TKOG"]);
    assert!(
        t.text().ends_with(&format!(
            "{} {}",
            said(Some(0), &[]),
            said(Some(1), &["TOP"])
        )),
        "{}",
        t.text()
    );
}

#[test]
fn a_plugin_reading_only_a_few_strokes_makes_only_those_calls_however_long_the_history() {
    let dir = Dir::new("long");
    let chain = dir.chain(&[dir.file("02-reader.rs", &reader(3, false))]);
    let log = StrokeLog::default();
    let strokes: Vec<u32> = (1..=200_000).collect();
    log.push(&strokes);
    let budget = Budget::default();
    budget.start(Instant::now() + Duration::from_secs(30));
    let kat = bits("KAT");
    let started = Instant::now();
    let read = chain.lookup_within(
        &[kat],
        Before {
            text: "",
            base: 200_000,
            log: Some(&log),
        },
        &budget,
    );
    assert_eq!(read.as_deref(), Some("-|200000,199999,199998"));
    assert_eq!(log.reads(), 3, "k = 0, 1, 2 and nothing else");
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(log.kept(), 200_000, "nothing was copied or changed");
}

#[test]
fn a_question_outside_a_lookup_with_history_sees_none() {
    let dir = Dir::new("outside");
    let chain = dir.chain(&[dir.file("02-reader.rs", &reader(8, true))]);
    assert_eq!(
        chain.lookup(&[bits("KAT")]).as_deref(),
        Some("0|-1"),
        "asked with no strokes known"
    );
}

/// Records what the history functions answer in `init`, `context` and, on the instance that
/// answers `joins` and `send_early`, in those: all must see nothing.
const OUTSIDE: &str = r#"
use std::sync::atomic::{AtomicI64, Ordering::SeqCst};
#[link(wasm_import_module = "keymux")]
extern "C" {
    fn history_len() -> i32;
    fn history_stroke(k: i32) -> i64;
}
static IN_INIT: AtomicI64 = AtomicI64::new(99);
static IN_CONTEXT: AtomicI64 = AtomicI64::new(99);

fn sees() -> i64 {
    // 0 only if there is no history and no stroke
    unsafe { i64::from(history_len()) + (history_stroke(0) + 1) + (history_stroke(1) + 1) }
}

#[no_mangle]
pub extern "C" fn init() { IN_INIT.store(sees(), SeqCst); }
#[no_mangle]
pub extern "C" fn context(_ptr: i32, _len: i32) { IN_CONTEXT.store(sees(), SeqCst); }
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}
#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 {
    let mut out = format!("init={} context={}", IN_INIT.load(SeqCst), IN_CONTEXT.load(SeqCst)).into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 { i32::from(bits == 1 << 23 && sees() == 0) }
#[no_mangle]
pub extern "C" fn send_early(_held: i64, _stroke: i64) -> i32 { i32::from(sees() == 0) }
"#;

#[test]
fn init_context_joins_and_send_early_see_no_history() {
    let dir = Dir::new("outside-calls");
    let chain = dir.chain(&[dir.file("02-outside.rs", OUTSIDE)]);
    let log = StrokeLog::default();
    log.push(&[5, 6, 7]);
    let budget = Budget::default();
    budget.start(Instant::now() + Duration::from_secs(30));
    let read = chain.lookup_within(
        &[bits("KAT")],
        Before {
            text: "text",
            base: 3,
            log: Some(&log),
        },
        &budget,
    );
    assert_eq!(read.as_deref(), Some("init=0 context=0"));
    assert!(chain.joins(1 << 23), "joins saw -1 and 0");
    assert!(chain.send_early(0, 1), "send_early saw -1 and 0");
    assert_eq!(log.reads(), 0, "and the log was not asked");
}

#[test]
fn a_plugin_that_does_not_import_the_functions_is_asked_exactly_as_before() {
    let dir = Dir::new("plain");
    let chain = dir.chain(&[dir.file("02-echo.rs", super::plugin::tests::ECHO_CONTEXT)]);
    let log = StrokeLog::default();
    log.push(&[5, 6, 7]);
    let budget = Budget::default();
    budget.start(Instant::now() + Duration::from_secs(30));
    for base in [1, 2, 3] {
        let read = chain.lookup_within(
            &[bits("KAT")],
            Before {
                text: "x",
                base,
                log: Some(&log),
            },
            &budget,
        );
        assert_eq!(read.as_deref(), Some("(x|KAT)"));
    }
    assert_eq!(log.reads(), 0);
}

/// Sums every stroke it can read, many times over, and says what it found.
const SUMMER: &str = r#"
#[link(wasm_import_module = "keymux")]
extern "C" {
    fn history_len() -> i32;
    fn history_stroke(k: i32) -> i64;
}
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}
#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 {
    let mut results = Vec::new();
    for _ in 0..40 {
        let mut sum = 0i64;
        let mut count = 0;
        loop {
            let s = unsafe { history_stroke(count) };
            if s < 0 { break; }
            sum += s;
            count += 1;
        }
        results.push(format!("{}/{}/{}", unsafe { history_len() }, count, sum));
    }
    results.dedup();
    let mut out = results.join(" ").into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#;

#[test]
fn strokes_struck_while_a_question_is_answered_do_not_change_what_it_reads() {
    let dir = Dir::new("concurrent");
    let chain = Arc::new(dir.chain(&[dir.file("02-summer.rs", SUMMER)]));
    let log = StrokeLog::default();
    let before: Vec<u32> = (1..=3000).collect();
    log.push(&before);
    let sum: i64 = before.iter().map(|&s| i64::from(s)).sum();
    let striker = {
        let log = log.clone();
        std::thread::spawn(move || {
            for round in 0..200u32 {
                log.push(&[9_000_000 + round; 500]);
                log.wall();
                if round % 3 == 0 {
                    log.pop(300);
                }
            }
        })
    };
    for _ in 0..30 {
        let budget = Budget::default();
        budget.start(Instant::now() + Duration::from_secs(30));
        let read = chain.lookup_within(
            &[bits("KAT")],
            Before {
                text: "",
                base: 3000,
                log: Some(&log),
            },
            &budget,
        );
        assert_eq!(read, Some(format!("3000/3000/{sum}")));
    }
    striker.join().unwrap();
}

#[test]
fn a_question_cut_under_by_undo_reads_nothing_instead_of_other_strokes() {
    let dir = Dir::new("cut");
    let chain = dir.chain(&[dir.file("02-reader.rs", &reader(4, true))]);
    let log = StrokeLog::default();
    log.push(&[1, 2, 3, 4]);
    let budget = Budget::default();
    budget.start(Instant::now() + Duration::from_secs(30));
    let ask = |base: u64| {
        chain.lookup_within(
            &[bits("KAT")],
            Before {
                text: "",
                base,
                log: Some(&log),
            },
            &budget,
        )
    };
    assert_eq!(ask(4).as_deref(), Some("4|4,3,2,1"));
    log.pop(3);
    log.push(&[8, 9, 10]);
    // A question about the same position now is another question, with the new strokes
    assert_eq!(ask(4).as_deref(), Some("4|10,9,8,1"));
}

#[test]
fn strokes_consumed_into_an_entry_stay_history_and_undoing_it_gives_back_one() {
    let dir = Dir::new("consumed");
    let mut t = translator(
        &dir,
        r#"{"*": "=undo", "KAT/TOP": "cattop"}"#,
        &reader(16, true),
    );
    strike(&mut t, &["KAT", "TOP", "TKOG"]);
    assert_eq!(
        t.text(),
        format!("cattop {}", said(Some(2), &["TOP", "KAT"])),
        "both strokes of the brief are history"
    );
    strike(&mut t, &["*", "*", "PWAT"]);
    assert_eq!(
        t.text(),
        format!("{} {}", said(Some(0), &[]), said(Some(1), &["KAT"])),
        "taking the brief back leaves the one stroke it replaced"
    );
}
