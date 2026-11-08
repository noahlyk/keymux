//! `{:engine:unload}` and the lazy reload after it, through a real engine over dictionary files
//! in a temporary folder (so the loader thread, the cache and the plugin pipeline all run).

use super::*;
use crate::steno::journal;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

const DICT: &str = r#"{
    "KAT": "cat", "TKOG": "dog", "-S": "{^s}", "*": "=undo", "SKWR": "{:engine:unload}",
    "SKWR*": "{:engine:sideways}", "TKPW": "good"
}"#;

/// A folder to itself, with a config folder inside it so the cache lands inside too.
pub(super) struct Rig {
    pub(super) root: PathBuf,
    pub(super) config_dir: PathBuf,
    pub(super) dict_dir: PathBuf,
}

impl Rig {
    pub(super) fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "keymux-unload-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let config_dir = root.join("cfg");
        let dict_dir = root.join("dicts");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&dict_dir).unwrap();
        fs::write(dict_dir.join("01.json"), DICT).unwrap();
        Self {
            root,
            config_dir,
            dict_dir,
        }
    }

    fn cache(&self) -> PathBuf {
        crate::steno::setup::cache_dir(&self.config_dir)
    }

    pub(super) fn config(&self) -> StenoConfig {
        StenoConfig {
            layout: "qwerty".to_string(),
            extra_keys: Vec::new(),
            join_keys: Vec::new(),
            layout_overrides: HashMap::new(),
            dictionaries: vec![self.dict_dir.to_string_lossy().into_owned()],
            stroke_timeout_ms: 5000,
            translation_timeout_ms: None,
            spaces_after: false,
            start_attached: true,
            start_capitalized: false,
            double_tap_window_ms: 300,
            repeat_delay_ms: 300,
            repeat_rate_ms: 40,
            disable_repeat: false,
            disable_first_up: false,
            disable_transparent: false,
            undo_stops_at_barrier: false,
            fresh_on_enter: false,
            disable_typing_history: false,
            typing_layout: "us".to_string(),
        }
    }

    fn engine(&self) -> StenoEngine {
        let history = journal::new_shared(crate::steno::formatter::Formatter::default());
        self.engine_with(&history)
    }

    fn engine_with(&self, history: &journal::SharedHistory) -> StenoEngine {
        StenoEngine::from_config(&self.config(), true, &self.config_dir, None, history, "t")
            .unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn keys_of(engine: &StenoEngine, outline: &str) -> Vec<KeyCode> {
    let bits = Layout::plover().parse_stroke(outline).unwrap();
    (0..32)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| {
            *engine
                .keys
                .iter()
                .find(|(_, bit)| **bit == 1 << i)
                .unwrap()
                .0
        })
        .collect()
}

/// Press and release the keys of `outline` without waiting for anything to load.
fn hit(engine: &mut StenoEngine, outline: &str) -> Vec<StenoOutput> {
    let now = Instant::now();
    let keys = keys_of(engine, outline);
    for key in &keys {
        engine.press(*key, now);
    }
    keys.iter().filter_map(|key| engine.release(*key)).collect()
}

/// Tick until `done` or ten seconds, collecting output.
pub(super) fn settle(engine: &mut StenoEngine, mut out: Vec<StenoOutput>) -> String {
    let end = Instant::now() + Duration::from_secs(10);
    while engine.loading && Instant::now() < end {
        out.extend(engine.tick(Instant::now()));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!engine.loading, "never finished loading");
    out.into_iter()
        .flat_map(StenoOutput::into_ops)
        .filter_map(|op| match op {
            OutputOp::Text(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// A stroke, loading first if it has to.
fn stroke(engine: &mut StenoEngine, outline: &str) -> String {
    let out = hit(engine, outline);
    settle(engine, out)
}

/// Wait until the loader thread has dropped what it held for this folder.
fn wait_dropped(rig: &Rig) {
    let end = Instant::now() + Duration::from_secs(10);
    while live::holds(&rig.cache()) {
        assert!(Instant::now() < end, "the hub still holds the dictionaries");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_stroke_after_an_unload_loads_the_dictionaries_and_translates_as_if_nothing_happened() {
    let rig = Rig::new("basic");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    assert_eq!(stroke(&mut engine, "KAT").trim(), "cat");

    // The unload stroke types nothing and is a control stroke
    assert_eq!(stroke(&mut engine, "SKWR"), "");
    assert!(engine.unloaded && !engine.loading);
    assert!(engine.dictionary_handle().is_none(), "nothing is held");
    wait_dropped(&rig);

    assert_eq!(stroke(&mut engine, "TKOG").trim(), "dog");
    assert!(!engine.unloaded);
    assert!(engine.dictionary_handle().is_some());
}

#[test]
fn history_and_joins_and_undo_carry_across_an_unload() {
    let rig = Rig::new("history");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    stroke(&mut engine, "KAT");
    stroke(&mut engine, "SKWR");
    assert_eq!(stroke(&mut engine, "-S"), "s", "the suffix still joins to cat");
    assert_eq!(engine.translator.text().trim(), "cats");
    // `*` takes back what was typed before the unload as well as after it
    stroke(&mut engine, "SKWR");
    let undone = hit(&mut engine, "*");
    settle(&mut engine, undone);
    assert_eq!(engine.translator.text().trim(), "cat");
}

#[test]
fn the_unload_stroke_is_transparent_and_stays_out_of_the_history() {
    let rig = Rig::new("transparent");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    stroke(&mut engine, "KAT");
    let before = journal::lock(&engine.translator.history()).clone();
    assert!(engine.translator.is_command(Layout::plover().parse_stroke("SKWR").unwrap()));
    stroke(&mut engine, "SKWR");
    assert!(engine.translator.was_transparent());
    let after = journal::lock(&engine.translator.history()).clone();
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
    // A verb it doesn't have does nothing
    stroke(&mut engine, "SKWR*");
    assert!(!engine.unloaded, "the stroke loaded them and unloaded nothing");
    assert!(engine.dictionary_handle().is_some());
}

#[test]
fn strokes_while_loading_are_queued_and_come_out_in_order() {
    let rig = Rig::new("queue");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    stroke(&mut engine, "SKWR");
    let mut out = Vec::new();
    // Three strokes with no tick between: the first starts the load, none is lost
    for outline in ["KAT", "TKOG", "TKPW"] {
        out.extend(hit(&mut engine, outline));
        assert!(engine.loading, "{outline} waits for the dictionaries");
    }
    assert!(out.is_empty(), "nothing translates before they arrive");
    assert_eq!(engine.backlog.len(), 3);
    assert_eq!(settle(&mut engine, out).trim(), "cat dog good");
}

#[test]
fn an_unload_in_the_queue_holds_back_the_strokes_after_it() {
    let rig = Rig::new("queued-unload");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    stroke(&mut engine, "SKWR");
    let mut out = Vec::new();
    for outline in ["KAT", "SKWR", "TKOG", "-S"] {
        out.extend(hit(&mut engine, outline));
    }
    assert_eq!(settle(&mut engine, out).trim(), "cat dogs");
    assert!(!engine.unloaded);
}

#[test]
fn unloading_twice_is_a_no_op() {
    let rig = Rig::new("twice");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    engine.unload();
    let epoch = engine.scope.epoch();
    engine.unload();
    assert_eq!(engine.scope.epoch(), epoch, "the second one did nothing");
    assert_eq!(stroke(&mut engine, "KAT").trim(), "cat");
}

#[test]
fn an_unload_closes_the_open_lookup_and_a_search_in_flight_keeps_its_own_copy() {
    let rig = Rig::new("lookup");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    engine.open_lookup();
    assert!(engine.lookup_active());
    // What a search thread holds: a translator over the same dictionary
    let in_flight = engine.translator.fresh();
    engine.unload();
    assert!(!engine.lookup_active(), "the lookup is closed by an unload");
    let kat = Layout::plover().parse_stroke("KAT").unwrap();
    assert_eq!(
        in_flight.dictionary().lookup(&[kat]),
        Some("cat"),
        "the copy it holds stays valid, and is freed when it is dropped"
    );
}

#[test]
#[allow(clippy::literal_string_with_formatting_args)] // a translation, not a format
fn a_stroke_is_what_opens_a_lookup_after_an_unload() {
    let rig = Rig::new("lookup-open");
    let mut dict = fs::read_to_string(rig.dict_dir.join("01.json")).unwrap();
    dict = dict.replace("\"TKPW\"", "\"-FP\": \"{plover:lookup}\", \"TKPW\"");
    fs::write(rig.dict_dir.join("01.json"), dict).unwrap();
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    stroke(&mut engine, "SKWR");
    stroke(&mut engine, "-FP");
    assert!(engine.lookup_active());
}

#[test]
fn a_reload_picks_up_an_edited_dictionary_and_files_are_not_read_while_unloaded() {
    let rig = Rig::new("edited");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    assert_eq!(stroke(&mut engine, "KAT").trim(), "cat");
    stroke(&mut engine, "SKWR");
    wait_dropped(&rig);
    fs::write(rig.dict_dir.join("01.json"), DICT.replace("\"cat\"", "\"kitten\"")).unwrap();
    // The change is noticed by the watcher, which must not load anything while unloaded
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!live::holds(&rig.cache()), "an edit must not wake the engine");
    assert_eq!(stroke(&mut engine, "KAT").trim(), "kitten");
}

#[test]
fn a_config_reload_while_unloaded_does_not_load_until_the_next_stroke() {
    let rig = Rig::new("config");
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine = rig.engine_with(&history);
    engine.wait_until_loaded();
    stroke(&mut engine, "KAT");
    stroke(&mut engine, "SKWR");
    wait_dropped(&rig);
    // The daemon rebuilds its layers on a config reload
    drop(engine);
    let mut fresh = rig.engine_with(&history);
    assert!(fresh.unloaded && fresh.updates.is_none() && !fresh.loading);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!live::holds(&rig.cache()), "nothing was loaded");
    assert_eq!(stroke(&mut fresh, "-S"), "s", "and the history carried over");
    assert!(live::holds(&rig.cache()));
}

#[test]
fn another_layer_with_the_same_dictionaries_is_unloaded_too() {
    let rig = Rig::new("two-layers");
    let mut one = rig.engine();
    let mut two = rig.engine();
    one.wait_until_loaded();
    two.wait_until_loaded();
    stroke(&mut one, "SKWR");
    two.tick(Instant::now());
    assert!(two.unloaded, "the second layer let go of its copy");
    wait_dropped(&rig);
    assert_eq!(stroke(&mut two, "KAT").trim(), "cat");
}

#[test]
fn an_unload_in_the_middle_of_a_held_chord_is_safe() {
    let rig = Rig::new("held");
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    let now = Instant::now();
    // KAT is half pressed when another layer's unload arrives
    let keys = keys_of(&engine, "KAT");
    engine.press(keys[0], now);
    engine.unload();
    for key in &keys[1..] {
        engine.press(*key, now);
    }
    let out: Vec<_> = keys.iter().filter_map(|key| engine.release(*key)).collect();
    assert_eq!(settle(&mut engine, out).trim(), "cat");
}

/// A plugin that doubles the letters of an outline it is asked about, and joins on bit 23.
const PLUGIN: &str = r#"
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
    let mut out: Vec<u8> = Vec::new();
    for &b in input { out.push(b); out.push(b); }
    let n = out.len();
    let p = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(p as i32) << 32) | i64::from(n as i32)
}
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
"#;

#[test]
fn the_plugin_is_instantiated_again_and_its_join_table_read_again() {
    let rig = Rig::new("plugin");
    fs::write(rig.dict_dir.join("02-plugin.rs"), PLUGIN).unwrap();
    let mut config = rig.config();
    config.extra_keys = vec!['+'];
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config(&config, true, &rig.config_dir, None, &history, "t").unwrap();
    engine.wait_until_loaded();
    let join = 1u32 << engine.layout.extra_start();
    let top = engine.layout.parse_stroke("TOP").unwrap();
    // TOP is in no dictionary, so the plugin reads it
    assert!(stroke(&mut engine, "TOP").contains("TTOOPP"));
    stroke(&mut engine, "SKWR");
    wait_dropped(&rig);
    assert!(!engine.translator.joins(top | join), "nothing is held");
    assert!(stroke(&mut engine, "TOP").contains("TTOOPP"));
    assert!(
        engine.translator.joins(top | (1 << 23)),
        "the new instance answers joins() from its own table"
    );
}

/// Resident memory in kilobytes: `(anonymous heap, file pages mapped)`.
fn rss() -> (u64, u64) {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (field("RssAnon:"), field("RssFile:"))
}

/// Memory before a load, loaded, after an unload and after the reload, with real numbers
/// printed. Heavy and sensitive to what else the process is doing, so run alone:
///
/// `KEYMUX_UNLOAD_ENTRIES=1000000 cargo test --release unload_gives_back -- --ignored --nocapture`
#[test]
#[ignore = "builds a dictionary of hundreds of thousands of entries and measures process memory; run alone"]
fn unload_gives_back_the_memory_a_large_dictionary_holds() {
    use std::fmt::Write as _;
    let entries: u32 = std::env::var("KEYMUX_UNLOAD_ENTRIES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(300_000);
    let rig = Rig::new("memory");
    let layout = Layout::plover();
    let mut json = String::from("{");
    for i in 0..entries {
        // Distinct two-stroke outlines, translations mostly distinct words
        let outline = crate::steno::layout::render_strokes(&[1 + i % 8_000_000, 1 + i / 4_000]);
        write!(json, "{outline:?}:\"word{i}x\",").unwrap();
    }
    json.push_str(&DICT[1..]);
    fs::write(rig.dict_dir.join("01.json"), json).unwrap();
    let _ = layout;

    let (anon0, file0) = rss();
    let started = Instant::now();
    let mut engine = rig.engine();
    engine.wait_until_loaded();
    let cold = started.elapsed();
    assert_eq!(stroke(&mut engine, "KAT").trim(), "cat");
    // Touch the dictionary the way typing does, so the mapped pages are really resident
    let handle = engine.dictionary_handle().unwrap();
    for i in (0..entries).step_by(97) {
        let _ = handle.strokes_for(&format!("word{i}x"));
    }
    drop(handle);
    let (anon1, file1) = rss();

    stroke(&mut engine, "SKWR");
    wait_dropped(&rig);
    std::thread::sleep(Duration::from_millis(300));
    let (anon2, file2) = rss();

    let started = Instant::now();
    assert_eq!(stroke(&mut engine, "TKOG").trim(), "dog");
    let warm = started.elapsed();
    let (anon3, file3) = rss();

    println!(
        "{entries} entries (KB)  anon / file-mapped\n  before load  {anon0} / {file0}\n  loaded       {anon1} / {file1}\n  unloaded     {anon2} / {file2}\n  reloaded     {anon3} / {file3}\n  first load {cold:?}, first stroke after unload {warm:?}"
    );
    let held = (anon1 + file1).saturating_sub(anon0 + file0);
    let freed = (anon1 + file1).saturating_sub(anon2 + file2);
    assert!(held > 10_000, "the dictionary should hold real memory ({held} KB)");
    assert!(
        freed * 10 >= held * 7,
        "unloading should give back most of the {held} KB, gave back {freed} KB"
    );
    // The reload maps the cache again but touches nothing: pages come back as lookups use them
    let _ = (anon3, file3);
}
