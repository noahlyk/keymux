//! The steno hot path in numbers: `cargo bench`.
//!
//! A stroke's cost from the keyboard to the text it types, measured over a generated
//! dictionary of fifty thousand entries so the lookups and the longest-match search work
//! as hard as they do on a real theory. Compare runs with `cargo bench -- --baseline`.

use criterion::{criterion_group, criterion_main, Criterion};
use keymux::config::StenoConfig;
use keymux::keycode::KeyCode;
use keymux::steno::dict::Dictionary;
use keymux::steno::formatter::Formatter;
use keymux::steno::journal::new_shared;
use keymux::steno::layout::{build_key_map, parse_stroke, preset, render_strokes, Layout};
use keymux::steno::translate::Translator;
use keymux::steno::StenoEngine;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

const GENERATED_ENTRIES: usize = 50_000;

/// Every standard key down at once: an outline the generator never makes.
const NOTHING_MATCHES: u32 = (1 << 23) - 1;

/// A dictionary the size of a real theory: generated outlines of one to three strokes, plus
/// the few fixed ones the benchmarks stroke. The fixed ones come last, so they win.
fn dictionary_json() -> String {
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        seed.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let mut stroke = move || loop {
        let bits = u32::try_from(next() & u64::from(NOTHING_MATCHES)).unwrap_or(0);
        if bits != 0 && bits != NOTHING_MATCHES {
            return bits;
        }
    };
    let mut json = String::from("{\n");
    for i in 0..GENERATED_ENTRIES {
        let strokes: Vec<u32> = (0..=(i % 3)).map(|_| stroke()).collect();
        let _ = writeln!(json, "{:?}: \"w{i}\",", render_strokes(&strokes));
    }
    json.push_str(
        r#""KAT": "cat", "KAT/HROG": "catalog", "TAOEUP": "type", "STKPW": "bench",
        "S/T/K": "stk", "S/T/K/P/W/H/R/A/O/E": "deep", "*": "=undo"
        }"#,
    );
    json
}

fn dictionary() -> Dictionary {
    let mut dict = Dictionary::default();
    dict.merge_json(&dictionary_json(), &Layout::plover())
        .expect("generated dictionary parses");
    dict
}

fn stroke(notation: &str) -> u32 {
    parse_stroke(notation).unwrap_or_else(|| panic!("bad stroke {notation}"))
}

/// A real engine over the dictionary written to disk, loaded the way the daemon loads it.
fn engine() -> StenoEngine {
    let dir = std::env::temp_dir().join(format!("keymux-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bench.json");
    std::fs::write(&path, dictionary_json()).unwrap();
    let config: StenoConfig =
        ron::from_str(&format!("(dictionaries: [{:?}])", path.display())).unwrap();
    let history = new_shared(Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, false, &dir, None, &history, "bench").unwrap();
    while engine.dictionary_handle().is_none() {
        engine.tick(Instant::now());
        std::thread::sleep(Duration::from_millis(2));
    }
    engine
}

/// One physical key for each key of the stroke, on the qwerty preset.
fn keys_of(bits: u32) -> Vec<KeyCode> {
    let map = build_key_map(&preset("qwerty").unwrap(), &HashMap::new()).unwrap();
    (0..32)
        .map(|i| 1u32 << i)
        .filter(|bit| bits & bit != 0)
        .map(|bit| {
            *map.iter()
                .find(|(_, &b)| b == bit)
                .unwrap_or_else(|| panic!("no key for bit {bit:#x}"))
                .0
        })
        .collect()
}

fn translate(c: &mut Criterion) {
    let dict = dictionary();
    let (kat, undo, nothing, type_) = (
        stroke("KAT"),
        stroke("*"),
        NOTHING_MATCHES,
        stroke("TAOEUP"),
    );
    let mut group = c.benchmark_group("translate");

    let mut t = Translator::new(dict.clone());
    group.bench_function("hit", |b| b.iter(|| black_box(t.stroke(black_box(kat)))));

    let mut t = Translator::new(dict.clone());
    group.bench_function("miss", |b| {
        b.iter(|| black_box(t.stroke(black_box(nothing))));
    });

    let mut t = Translator::new(dict.clone());
    group.bench_function("undo", |b| {
        b.iter(|| {
            t.stroke(black_box(kat));
            black_box(t.stroke(black_box(undo)))
        });
    });

    // Ten single-stroke words behind it: the longest-match window is as wide as the
    // dictionary's longest outline, and every width is tried
    let mut t = Translator::new(dict);
    for _ in 0..10 {
        t.stroke(kat);
    }
    group.bench_function("longest-match", |b| {
        b.iter(|| black_box(t.stroke(black_box(type_))));
    });
    group.finish();
}

fn engine_chord(c: &mut Criterion) {
    let mut engine = engine();
    let keys = keys_of(stroke("STKPW"));
    assert_eq!(keys.len(), 5);
    c.bench_function("engine/chord", |b| {
        b.iter(|| {
            let now = Instant::now();
            for &key in &keys {
                engine.press(key, now);
            }
            let mut out = None;
            for &key in &keys {
                out = engine.release_at(key, now);
            }
            engine.tick(now);
            black_box(out)
        });
    });
}

fn dictionary_lookup(c: &mut Criterion) {
    let dict = dictionary();
    let one = [stroke("KAT")];
    let three = [stroke("S"), stroke("T"), stroke("K")];
    assert!(dict.lookup(&one).is_some() && dict.lookup(&three).is_some());
    let mut group = c.benchmark_group("dictionary");
    group.bench_function("lookup/1-stroke", |b| {
        b.iter(|| black_box(dict.lookup(black_box(&one))));
    });
    group.bench_function("lookup/3-stroke", |b| {
        b.iter(|| black_box(dict.lookup(black_box(&three))));
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = translate, engine_chord, dictionary_lookup
}
criterion_main!(benches);
