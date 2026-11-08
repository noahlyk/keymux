//! A plugin's `send_early` export, through a real engine over a steno folder with the plugin in
//! it: a mod that changes when a stroke is sent, not what it translates to.

use super::unload_tests::{settle, Rig};
use super::*;
use crate::steno::journal;
use std::fs;

/// A pure mod: no `fallback`, only `send_early`. Bit 23 (the layer's first extra key) is sticky:
/// with it the only key still down and any other key pressed, the stroke goes now.
const STICKY: &str = r#"
const STICKY: i64 = 1 << 23;
#[no_mangle]
pub extern "C" fn send_early(held: i64, stroke: i64) -> i32 {
    i32::from(held == STICKY && stroke & !STICKY != 0)
}
"#;

fn engine(rig: &Rig) -> StenoEngine {
    fs::write(
        rig.dict_dir.join("00-joined.json"),
        r#"{"+KAT": "{^cat}", "+TKOG": "{^dog}"}"#,
    )
    .unwrap();
    fs::write(rig.dict_dir.join("02-sticky.rs"), STICKY).unwrap();
    let mut config = rig.config();
    config.extra_keys = vec!['+'];
    config.join_keys = vec!['+'];
    config
        .layout_overrides
        .insert("+".into(), KeyAction::Key(KeyCode::KC_Q));
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, true, &rig.config_dir, None, &history, "t").unwrap();
    engine.wait_until_loaded();
    engine
}

fn key(engine: &StenoEngine, bit: u32) -> KeyCode {
    *engine.keys.iter().find(|(_, b)| **b == bit).unwrap().0
}

fn keys(engine: &StenoEngine, outline: &str) -> Vec<KeyCode> {
    let bits = engine.layout.parse_stroke(outline).unwrap();
    (0..32)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| key(engine, 1 << i))
        .collect()
}

/// Press `outline`'s keys and let them go, with whatever else is held staying down.
fn strike(engine: &mut StenoEngine, outline: &str) -> Vec<StenoOutput> {
    let now = Instant::now();
    let keys = keys(engine, outline);
    for k in &keys {
        engine.press(*k, now);
    }
    keys.iter().filter_map(|k| engine.release(*k)).collect()
}

#[test]
fn a_held_key_a_plugin_makes_sticky_sends_each_stroke_when_the_others_come_up() {
    let rig = Rig::new("sticky");
    let mut engine = engine(&rig);
    let plus = key(&engine, 1 << engine.layout.extra_start());
    // A word's first stroke as ever, then + held for the strokes that continue it
    let out = strike(&mut engine, "TKOG");
    assert!(settle(&mut engine, out).contains("dog"));
    engine.press(plus, Instant::now());
    // Each stroke goes when its own keys come up, the held key part of it
    let out = strike(&mut engine, "KAT");
    let text = settle(&mut engine, out);
    assert_eq!(text, "cat", "the stroke goes with + still down, joined");
    let out = strike(&mut engine, "TKOG");
    let text = settle(&mut engine, out);
    assert_eq!(text, "dog", "and the next one, + still down");
    // Letting go of the held key alone sends nothing
    assert!(engine.release(plus).is_none());
}

#[test]
fn without_the_mod_a_held_key_still_holds_the_stroke_back() {
    let rig = Rig::new("not-sticky");
    let mut config = rig.config();
    config.extra_keys = vec!['+'];
    config.join_keys = vec!['+'];
    config
        .layout_overrides
        .insert("+".into(), KeyAction::Key(KeyCode::KC_Q));
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, true, &rig.config_dir, None, &history, "t").unwrap();
    engine.wait_until_loaded();
    let plus = key(&engine, 1 << engine.layout.extra_start());
    engine.press(plus, Instant::now());
    assert!(
        strike(&mut engine, "KAT").is_empty(),
        "the stroke waits for + to come up"
    );
    assert!(engine.release(plus).is_some());
}
