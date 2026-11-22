//! Skew compensation through a real engine, on injected key times (no devices, no sleeping):
//! the split it fixes, the order it keeps, and which strokes pay for it.

use super::unload_tests::Rig;
use super::*;
use crate::config::{SkewConfig, SkewSide};
use crate::steno::journal;
use std::fs;

const DICT: &str = r#"{"S": "L", "-T": "R", "S-T": "LR", "SH": "LH", "S*": "{:firstup}LS"}"#;

fn engine(rig: &Rig, skew: SkewConfig) -> StenoEngine {
    fs::write(rig.dict_dir.join("00-skew.json"), DICT).unwrap();
    let mut config = rig.config();
    config.skew = skew;
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, true, &rig.config_dir, None, &history, "t").unwrap();
    engine.wait_until_loaded();
    engine
}

fn left(delay_ms: u32) -> SkewConfig {
    SkewConfig {
        delay_ms,
        half: SkewSide::Left,
        keys: Vec::new(),
    }
}

fn key(engine: &StenoEngine, outline: &str) -> KeyCode {
    let bits = engine.layout.parse_stroke(outline).unwrap();
    *engine.keys.iter().find(|(_, b)| **b == bits).unwrap().0
}

fn text(output: &StenoOutput) -> String {
    output
        .clone()
        .into_ops()
        .into_iter()
        .filter_map(|op| match op {
            OutputOp::Text(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// Feed `events` (milliseconds, key, down) in time order, ticking at every deadline on the
/// way as the daemon's pump does, and return what was typed and when, in milliseconds.
fn run(
    engine: &mut StenoEngine,
    events: &[(u64, KeyCode, bool)],
    until: u64,
) -> Vec<(u64, String)> {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    let mut typed = Vec::new();
    let tick_to = |engine: &mut StenoEngine, ms: u64, typed: &mut Vec<(u64, String)>| {
        for _ in 0..100 {
            let Some(due) = engine.next_deadline().filter(|due| *due <= at(ms)) else {
                break;
            };
            if let Some(out) = engine.tick(due) {
                typed.push((
                    u64::try_from(due.duration_since(base).as_millis()).unwrap(),
                    text(&out),
                ));
            }
        }
    };
    for &(ms, key, down) in events {
        tick_to(engine, ms, &mut typed);
        if let Some(out) = engine.key_event(key, down, at(ms)) {
            typed.push((ms, text(&out)));
        }
    }
    tick_to(engine, until, &mut typed);
    typed
}

fn texts(typed: &[(u64, String)]) -> Vec<&str> {
    typed.iter().map(|(_, t)| t.trim()).collect()
}

/// A left tap that is over before the right half's first key arrives.
fn roll(engine: &StenoEngine) -> Vec<(u64, KeyCode, bool)> {
    let (s, t) = (key(engine, "S"), key(engine, "-T"));
    vec![(0, s, true), (5, s, false), (12, t, true), (20, t, false)]
}

#[test]
fn without_skew_a_left_tap_gone_before_the_right_key_arrives_splits_the_stroke() {
    let rig = Rig::new("skew-split");
    let mut engine = engine(&rig, SkewConfig::default());
    let events = roll(&engine);
    let typed = run(&mut engine, &events, 100);
    assert_eq!(texts(&typed), ["L", "R"], "{typed:?}");
    assert!(!engine.skew.is_active(), "delay 0 queues nothing");
    assert!(engine.skew.is_empty());
}

#[test]
fn a_delay_that_covers_the_lag_joins_them() {
    let rig = Rig::new("skew-join");
    let mut engine = engine(&rig, left(15));
    let events = roll(&engine);
    let typed = run(&mut engine, &events, 100);
    assert_eq!(texts(&typed), ["LR"], "{typed:?}");
    // Too short a delay still splits (the left is up at 5+8 = 13, the right comes at 12... joined)
    let rig = Rig::new("skew-short");
    let mut engine = engine_short(&rig);
    let events = roll(&engine);
    assert_eq!(texts(&run(&mut engine, &events, 100)), ["L", "R"]);
}

fn engine_short(rig: &Rig) -> StenoEngine {
    engine(rig, left(6))
}

#[test]
fn a_delayed_half_keeps_its_own_timing_and_order() {
    let rig = Rig::new("skew-order");
    let mut engine = engine(&rig, left(10));
    let s = key(&engine, "S");
    // Two taps of the left half, 6 ms apart: two strokes, in order, 6 ms apart, 10 ms late
    let events = [(0, s, true), (3, s, false), (9, s, true), (12, s, false)];
    let typed = run(&mut engine, &events, 100);
    assert_eq!(
        typed.iter().map(|(ms, _)| *ms).collect::<Vec<_>>(),
        [13, 22],
        "{typed:?}"
    );
    assert_eq!(texts(&typed), ["L", "L"]);
}

/// When each stroke of `spec` (milliseconds, key as an outline, down) is sent, in milliseconds.
fn sent_at(skew: SkewConfig, spec: &[(u64, &str, bool)]) -> Vec<u64> {
    let rig = Rig::new("skew-latency");
    let mut engine = engine(&rig, skew);
    let events: Vec<_> = spec
        .iter()
        .map(|&(ms, outline, down)| (ms, key(&engine, outline), down))
        .collect();
    run(&mut engine, &events, 300)
        .iter()
        .map(|(ms, _)| *ms)
        .collect()
}

#[test]
fn strokes_ending_on_the_other_half_and_that_half_alone_wait_for_nothing() {
    let off = SkewConfig::default;
    // The right (slow) half lets go last, 40 ms after the left: sent when the right lets go
    let ends_on_slow = [
        (0, "S", true),
        (4, "-T", true),
        (20, "S", false),
        (60, "-T", false),
    ];
    assert_eq!(sent_at(off(), &ends_on_slow), [60]);
    assert_eq!(
        sent_at(left(10), &ends_on_slow),
        [60],
        "no latency when the stroke ends on the slow half"
    );
    // A right-half-only stroke is sent when its key comes up, exactly as without skew
    let right_only = [(0, "-T", true), (30, "-T", false)];
    assert_eq!(sent_at(off(), &right_only), [30]);
    assert_eq!(sent_at(left(10), &right_only), [30]);
    // The left (fast) half lets go last: the stroke waits for the delay, and only that
    let ends_on_fast = [
        (0, "S", true),
        (4, "-T", true),
        (20, "-T", false),
        (60, "S", false),
    ];
    assert_eq!(sent_at(off(), &ends_on_fast), [60]);
    assert_eq!(sent_at(left(10), &ends_on_fast), [70]);
    // A left-only stroke is the delay late
    let left_only = [(0, "S", true), (30, "S", false)];
    assert_eq!(sent_at(left(10), &left_only), [40]);
    // The left lets go 4 ms after the right: the wait is only what is left of the delay
    let close = [
        (0, "S", true),
        (4, "-T", true),
        (20, "-T", false),
        (24, "S", false),
    ];
    assert_eq!(sent_at(left(10), &close), [34]);
    // The same, from the left letting go 6 ms before the right
    let before = [
        (0, "S", true),
        (4, "-T", true),
        (20, "S", false),
        (26, "-T", false),
    ];
    assert_eq!(
        sent_at(left(10), &before),
        [30],
        "the left is replayed at 30"
    );
}

#[test]
fn leaving_the_layer_replays_what_is_held_back_at_once() {
    let rig = Rig::new("skew-flush");
    let mut engine = engine(&rig, left(30));
    let s = key(&engine, "S");
    let base = Instant::now();
    assert!(engine.key_event(s, true, base).is_none());
    assert!(engine
        .key_event(s, false, base + Duration::from_millis(5))
        .is_none());
    assert!(!engine.skew.is_empty());
    assert!(
        engine.next_deadline().is_some(),
        "the pump wakes for the held-back events"
    );
    engine.flush_skew(base + Duration::from_millis(6));
    let out = engine
        .tick(base + Duration::from_millis(6))
        .expect("the stroke is sent now");
    assert_eq!(text(&out).trim(), "L");
    assert!(engine.skew.is_empty());
    assert!(engine.held_keys.is_empty(), "no key is left down");
    assert_eq!(engine.next_deadline(), None);
}

#[test]
fn first_up_sends_on_the_replayed_timeline() {
    let rig = Rig::new("skew-firstup");
    let mut engine = engine(&rig, left(10));
    let (s, star) = (key(&engine, "S"), key(&engine, "*"));
    // S* is sent when a key first comes up, here the star (not delayed) at 30; S is replayed
    // at 10 and still down then. Letting S go at 80 afterwards sends nothing more.
    let events = [
        (0, s, true),
        (3, star, true),
        (30, star, false),
        (80, s, false),
    ];
    let typed = run(&mut engine, &events, 200);
    assert_eq!(typed.len(), 1, "{typed:?}");
    assert_eq!(typed[0].0, 30);
    assert_eq!(text_of(&typed[0].1), "LS");
}

fn text_of(text: &str) -> &str {
    text.trim()
}

#[test]
fn a_repeat_window_counts_from_the_replayed_moment() {
    let rig = Rig::new("skew-repeat");
    let mut engine = engine(&rig, left(10));
    let s = key(&engine, "S");
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    assert!(engine.key_event(s, true, at(0)).is_none());
    assert!(engine.key_event(s, false, at(5)).is_none());
    // Replayed at 10 and 15: the stroke is sent at 15
    assert!(engine.tick(at(10)).is_none());
    assert!(engine.tick(at(15)).is_some());
    assert_eq!(engine.last.map(|(_, at)| at), Some(at(15)));
}
