//! What the steno lookup commands answer, as data: the engine setup they resolve against, and
//! the rows a query produces.
//!
//! `keymux steno lookup`, `keys` and `stroke` all end up here, both when given a query on the
//! command line and while one is typed live in the terminal (see [`super::interactive`]), so
//! there is one answer, drawn the same way ([`super::view`]) every time. The overlay's live
//! lookup ([`super::lookup`]) shares the same reverse-lookup engine ([`super::reverse`]).

use super::commands::proper_outlines_in;
use super::engine::resolve_dictionary_path;
use super::formatter::Options;
use super::layout::{preset, without_extra_keys, Layout};
use super::plugin::Chain;
use super::reverse::{self, Query};
use super::setup::{cache_dir, default_dictionaries};
use super::source::{expand, is_plugin_file};
use super::translate::Translator;
use crate::config::{Config, KeyAction, KeyColor, LayerKind};
use crate::keycode::KeyCode;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The steno layer the commands resolve against, as the daemon builds its engine from it.
///
/// That is the layer's own stroke keys (`extra_keys`, the join key), key assignments and
/// dictionary list. Everything falls back to keymux's defaults with no readable config or no
/// steno layer in it, so the commands work on a fresh install.
///
/// The layer is the one literally named `"steno"` if there is one, otherwise the
/// alphabetically-first steno layer.
#[derive(Debug, Clone)]
pub struct EngineSetup {
    /// The layer's name, if the config has a steno layer
    pub layer: Option<String>,
    /// Stroke notation: the standard 23 keys plus the layer's `extra_keys`, with the overlay's
    /// `key_groups` applied so a key pair underlines the way it does on the tape
    pub layout: Layout,
    /// The built-in physical layout the layer starts from
    pub preset: String,
    /// The layer's `layout_overrides`
    pub overrides: HashMap<String, KeyAction>,
    /// Where the layer reads dictionaries and plugins from, highest priority first
    pub roots: Vec<PathBuf>,
    /// The overlay's colour for each extra key, by its name
    pub extra_colors: HashMap<String, KeyColor>,
    config_dir: PathBuf,
}

impl EngineSetup {
    /// The layer in `config_path`, or the defaults.
    #[must_use]
    pub fn load(config_path: &Path, config_dir: &Path, home: Option<&Path>) -> Self {
        let config = Config::load_or_hint(config_path).ok();
        let mut layers: Vec<_> = config
            .iter()
            .flat_map(|c| &c.layers)
            .filter_map(|(layer, cfg)| match &cfg.kind {
                LayerKind::Steno(steno) => Some((layer, steno)),
                LayerKind::Remap => None,
            })
            .collect();
        layers.sort_by(|(a, _), (b, _)| a.0.cmp(&b.0));
        let chosen = layers
            .iter()
            .find(|(layer, _)| layer.0 == "steno")
            .or_else(|| layers.first());
        let overlay = config.as_ref().map(|c| &c.overlay);

        let mut setup = Self {
            layer: None,
            layout: Layout::plover(),
            preset: "qwerty".to_string(),
            overrides: HashMap::new(),
            roots: default_dictionaries(config_dir),
            extra_colors: overlay.map_or_else(HashMap::new, |o| o.extra_colors.clone()),
            config_dir: config_dir.to_path_buf(),
        };
        if let Some((layer, steno)) = chosen {
            setup.layer = Some(layer.0.clone());
            // A layer whose extra_keys don't build a layout is `keymux check`'s to report; the
            // commands carry on with the plain 23 keys
            setup.layout = Layout::plover_with(&steno.extra_keys).unwrap_or_else(|_| Layout::plover());
            setup.preset.clone_from(&steno.layout);
            setup.overrides.clone_from(&steno.layout_overrides);
            if !steno.dictionaries.is_empty() {
                setup.roots = steno
                    .dictionaries
                    .iter()
                    .map(|raw| resolve_dictionary_path(raw, config_dir, home))
                    .collect();
            }
        }
        if let Some(overlay) = overlay {
            for (name, group) in &overlay.key_groups {
                // A name that isn't a key is `keymux check`'s to report
                let _ = setup.layout.set_key_group(name, *group);
            }
        }
        setup
    }

    /// The physical key for each stroke key, keyed by stroke bit: the preset's main keys with
    /// the layer's overrides applied (an override for an extra key such as `+` included).
    ///
    /// # Errors
    /// When the preset is unknown or the overrides don't build a key map.
    pub fn keys_by_bit(&self) -> Result<HashMap<u32, KeyCode>> {
        // Main keys only, so each stroke key names one physical key
        let preset = without_extra_keys(preset(&self.preset).map_err(|e| anyhow!(e))?);
        let map = self
            .layout
            .build_key_map(&preset, &self.overrides)
            .map_err(|errs| anyhow!(errs.join("; ")))?;
        Ok(map.into_iter().map(|(key, bit)| (bit, key)).collect())
    }

    /// The layer's dictionaries and plugins as the engine uses them, wrapped in a translator
    /// with nothing typed. The layer's spacing options (`spaces_after`, `start_capitalized`) are
    /// left out on purpose: an outline types a word the same either way, and answers are
    /// checked by the text they type, which those options would only decorate. A plugin stays in the chain, so a join key
    /// it declares, or an outline it answers, works here as it does in the daemon.
    ///
    /// Also says so when a plugin file did not load (the daemon only logs that): answers that
    /// lean on it, such as a join key, are then missing.
    #[must_use]
    pub fn translator(&self) -> (Translator, Option<String>) {
        let cache = cache_dir(&self.config_dir);
        let files = expand(&self.roots, &cache);
        let chain = Chain::load_cached(&files, &cache, &self.layout);
        let wanted = files.iter().filter(|f| is_plugin_file(f)).count();
        let problem = (chain.plugin_count() < wanted).then(|| {
            format!(
                "{} of {wanted} plugin(s) did not load (they are compiled with rustc for \
                 wasm32-unknown-unknown): answers that need them, such as a join key, are missing",
                wanted - chain.plugin_count()
            )
        });
        (
            Translator::with_shared(
                Arc::new(chain),
                Options {
                    start_attached: true,
                    ..Options::default()
                },
            ),
            problem,
        )
    }
}

/// A loaded engine setup: what the lookup commands and the interactive session ask questions of.
pub struct Lookup {
    pub setup: EngineSetup,
    /// The layer's dictionaries and plugins, nothing typed
    pub base: Translator,
    /// The physical key for each stroke key; empty when the layer's key assignments don't build
    /// (see `keys_problem`)
    pub keys_by_bit: HashMap<u32, KeyCode>,
    /// Why `keys_by_bit` is empty, if it is
    pub keys_problem: Option<String>,
    /// Set when a plugin in the dictionary folder did not load
    pub plugin_problem: Option<String>,
}

impl Lookup {
    /// Load `setup`'s dictionaries (cached copies make this quick after the first run).
    #[must_use]
    pub fn load(setup: EngineSetup) -> Self {
        let (keys_by_bit, keys_problem) = match setup.keys_by_bit() {
            Ok(map) => (map, None),
            Err(e) => (HashMap::new(), Some(e.to_string())),
        };
        let (base, plugin_problem) = setup.translator();
        Self {
            setup,
            base,
            keys_by_bit,
            keys_problem,
            plugin_problem,
        }
    }

    /// The answer to `query` (see [`answer`]).
    #[must_use]
    pub fn answer(&self, query: &str, ask: Ask, cancelled: &dyn Fn() -> bool) -> Answer {
        answer(&self.setup.layout, &self.base.fresh(), query, ask, cancelled)
    }

    /// How to draw an answer.
    #[must_use]
    pub const fn view(&self, show_keys: bool, color: bool) -> super::view::View<'_> {
        super::view::View {
            layout: &self.setup.layout,
            keys_by_bit: &self.keys_by_bit,
            extra_colors: &self.setup.extra_colors,
            show_keys,
            color,
        }
    }
}

/// How a row's outline came to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    /// A dictionary entry
    Entry,
    /// A word with no entry, written from entries ("everybody", "'s")
    Composed(Vec<String>),
    /// A word with no entry, spelled one letter a stroke
    Fingerspelled,
    /// Nothing can type the word
    Missing,
    /// A word that starts with the query, offered after the exact answers
    Completion,
    /// What a stroke types; the row's word is the text, or a note that it types nothing
    Forward,
    /// An entry one key or two from the stroke asked for
    Nearby,
}

/// One line of an answer: a word and one outline that types it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The word. Empty on the second and later outlines of a word.
    pub word: String,
    /// The strokes of the outline; empty for [`Mark::Missing`]
    pub strokes: Vec<u32>,
    pub mark: Mark,
}

/// What a query found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub rows: Vec<Row>,
    /// Set when the query could not be read at all (stroke mode, not stroke notation)
    pub problem: Option<String>,
}

impl Answer {
    /// The strokes and keys of the best outline of each word: the first row of every word that
    /// has one, completions and nearby entries left out.
    #[must_use]
    pub fn totals(&self) -> (usize, u32) {
        self.rows
            .iter()
            .filter(|row| !row.word.is_empty() && !row.strokes.is_empty())
            .filter(|row| matches!(row.mark, Mark::Entry | Mark::Composed(_) | Mark::Fingerspelled))
            .fold((0, 0), |(strokes, keys), row| {
                (
                    strokes + row.strokes.len(),
                    keys + row.strokes.iter().map(|s| s.count_ones()).sum::<u32>(),
                )
            })
    }
}

/// How to answer.
#[derive(Debug, Clone, Copy)]
pub struct Ask {
    /// Only the fewest-stroke outline of each word: no other outlines, no completions
    pub fast: bool,
    /// Read the query as stroke notation whatever its case (`stroke`, and `kat` in stroke mode)
    pub stroke_mode: bool,
    /// Most rows that merely extend the answer: completions after a word, entries near a stroke
    pub extra: usize,
}

/// Words that merely start with the query are only offered from this length on.
const MIN_PREFIX: usize = 2;

/// The answer to `query`, over `base` (a translator with nothing typed, [`Translator::fresh`]).
///
/// Stroke notation (no lowercase letter, or any text in stroke mode) is resolved forward: what
/// it types, then the entries nearest to it. Anything else is text: each word's outlines, the
/// best first (the one that types the phrase right, see [`reverse::reverse_phrase`]), then
/// every other outline that types the word properly; a word with no entry is composed from
/// entries or fingerspelled, and marked. A single word is followed by the words that start
/// with it. `cancelled` lets a caller drop a search a keystroke has made stale.
#[must_use]
pub fn answer(
    layout: &Layout,
    base: &Translator,
    query: &str,
    ask: Ask,
    cancelled: &dyn Fn() -> bool,
) -> Answer {
    let query = query.trim();
    if query.is_empty() {
        return Answer::default();
    }
    let parsed = if ask.stroke_mode {
        match layout.parse_strokes_trimmed(&query.to_uppercase()) {
            Some(strokes) => Query::Stroke(strokes),
            None => {
                return Answer {
                    rows: Vec::new(),
                    problem: Some(format!("\"{query}\" is not stroke notation")),
                }
            }
        }
    } else {
        reverse::classify_with(layout, query)
    };
    match parsed {
        Query::Stroke(strokes) => forward(base, &strokes, ask),
        Query::Text(text) => reverse_rows(base, &text, ask, cancelled),
    }
}

fn forward_row(base: &Translator, strokes: &[u32], mark: Mark) -> Row {
    let mut probe = base.clone();
    for &bits in strokes {
        probe.stroke(bits);
    }
    // A stroke nothing matches is typed as its own keys: that is no translation, not a word
    let text = if probe.was_unmatched() {
        String::new()
    } else {
        probe.text()
    };
    Row {
        word: if text.is_empty() {
            "(no translation)".to_string()
        } else {
            format!("{text:?}")
        },
        strokes: strokes.to_vec(),
        mark,
    }
}

fn forward(base: &Translator, strokes: &[u32], ask: Ask) -> Answer {
    let mut rows = vec![forward_row(base, strokes, Mark::Forward)];
    if !ask.fast {
        for outline in reverse::nearby_strokes(base.dictionary(), strokes, ask.extra) {
            rows.push(forward_row(base, &outline, Mark::Nearby));
        }
    }
    Answer { rows, problem: None }
}

fn reverse_rows(base: &Translator, text: &str, ask: Ask, cancelled: &dyn Fn() -> bool) -> Answer {
    let spans = super::commands::word_spans(text);
    let words: Vec<&str> = spans.iter().map(|&(s, e)| &text[s..e]).collect();
    let segments = reverse::reverse_phrase_cancellable(base, base.dictionary(), text, cancelled);
    let mut rows = Vec::new();
    for (word, segment) in words.iter().zip(&segments) {
        let Some(segment) = segment else {
            rows.push(Row {
                word: (*word).to_string(),
                strokes: Vec::new(),
                mark: Mark::Missing,
            });
            continue;
        };
        let mark = match (&segment.composed_of, segment.fingerspelled) {
            (Some(pieces), _) => Mark::Composed(pieces.clone()),
            (None, true) => Mark::Fingerspelled,
            (None, false) => Mark::Entry,
        };
        let real = mark == Mark::Entry;
        rows.push(Row {
            word: (*word).to_string(),
            strokes: segment.strokes.clone(),
            mark,
        });
        if real && !ask.fast {
            for outline in proper_outlines_in(base, word) {
                if outline != segment.strokes {
                    rows.push(Row {
                        word: String::new(),
                        strokes: outline,
                        mark: Mark::Entry,
                    });
                }
            }
        }
    }
    if let [word] = words.as_slice() {
        if !ask.fast && word.chars().count() >= MIN_PREFIX && !cancelled() {
            let mut added = 0;
            for completion in base.dictionary().completions(word, ask.extra * 2) {
                if added == ask.extra || cancelled() {
                    break;
                }
                if completion != *word {
                    if let Some(outline) = proper_outlines_in(base, completion).into_iter().next() {
                        rows.push(Row {
                            word: completion.to_string(),
                            strokes: outline,
                            mark: Mark::Completion,
                        });
                        added += 1;
                    }
                }
            }
        }
    }
    Answer { rows, problem: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::dict::Dictionary;
    use crate::steno::layout::preset;

    const DICT: &str = r#"{
        "-T": "the",
        "KAT": "cat",
        "KAPB": "can",
        "KAPBD": "candy",
        "KAPB/TKEU": "candy",
        "KAPBL": "candle",
        "EFB": "everybody",
        "-Z": "{^'s}",
        "TPHO": "no",
        "WUPB": "one",
        "TK-LS": "{^}",
        "TPH": "{&n}",
        "SR*": "{&v}",
        "SKW*": "{&i}",
        "PH*": "{&m}",
        "AEU": "I",
        "APL": "am",
        "KAEUBG": "cake",
        "KA": "ca"
    }"#;

    fn base() -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(DICT, &Layout::plover()).unwrap();
        Translator::new(dict)
    }

    const ALL: Ask = Ask {
        fast: false,
        stroke_mode: false,
        extra: 8,
    };

    fn ask(query: &str, ask: Ask) -> Answer {
        answer(&Layout::plover(), &base().fresh(), query, ask, &|| false)
    }

    fn outlines(answer: &Answer) -> Vec<(String, String, Mark)> {
        answer
            .rows
            .iter()
            .map(|r| {
                (
                    r.word.clone(),
                    Layout::plover().render_strokes(&r.strokes),
                    r.mark.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_word_lists_every_outline_fewest_strokes_first_then_longer_words() {
        let rows = outlines(&ask("candy", ALL));
        assert_eq!(rows[0], ("candy".into(), "KAPBD".into(), Mark::Entry));
        assert_eq!(rows[1], (String::new(), "KAPB/TKEU".into(), Mark::Entry));
        // nothing else starts with "candy"
        assert_eq!(rows.len(), 2);
        let ca = outlines(&ask("can", ALL));
        assert_eq!(ca[0].1, "KAPB");
        let completions: Vec<_> = ca.iter().filter(|r| r.2 == Mark::Completion).map(|r| r.0.as_str()).collect();
        assert_eq!(completions, ["candy", "candle"]);
    }

    #[test]
    fn fast_is_the_best_outline_alone() {
        let fast = Ask { fast: true, ..ALL };
        assert_eq!(outlines(&ask("candy", fast)).len(), 1);
        assert_eq!(outlines(&ask("can", fast)).len(), 1);
    }

    #[test]
    fn a_phrase_is_one_row_per_word_and_a_word_with_no_entry_is_fingerspelled() {
        let rows = outlines(&ask("I am nvim", ALL));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "I");
        assert_eq!(rows[2].0, "nvim");
        assert_eq!(rows[2].2, Mark::Fingerspelled);
        assert_eq!(rows[2].1, "TPH/SR*/SKW*/PH*");
    }

    #[test]
    fn a_composed_word_is_marked_with_its_pieces() {
        let rows = outlines(&ask("everybody's", ALL));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "EFB/-Z");
        assert_eq!(rows[0].2, Mark::Composed(vec!["everybody".into(), "'s".into()]));
        // glued through the {^} entry
        let noone = outlines(&ask("noone", ALL));
        assert_eq!(noone[0].1, "TPHO/TK-LS/WUPB");
    }

    #[test]
    fn totals_count_the_best_outline_of_each_word() {
        let answer = ask("the cat can", ALL);
        assert_eq!(answer.totals(), (3, 1 + 3 + 4));
    }

    #[test]
    fn a_word_nothing_can_type_is_missing() {
        let rows = outlines(&ask("cat zzz9", ALL));
        assert_eq!(rows[1].2, Mark::Missing);
    }

    #[test]
    fn a_stroke_resolves_forward_with_the_nearest_entries_after() {
        let rows = outlines(&ask("KAPBD", ALL));
        assert_eq!(rows[0], ("\"candy\"".into(), "KAPBD".into(), Mark::Forward));
        assert!(rows[1..].iter().all(|r| r.2 == Mark::Nearby));
        assert!(rows.len() > 1);
        let free = outlines(&ask("STKPWHR", ALL));
        assert_eq!(free[0].0, "(no translation)");
    }

    #[test]
    fn stroke_mode_reads_lowercase_as_notation_and_refuses_anything_else() {
        let stroke = Ask { stroke_mode: true, ..ALL };
        assert_eq!(outlines(&ask("kapbd", stroke))[0].1, "KAPBD");
        let bad = ask("hello world", stroke);
        assert!(bad.rows.is_empty());
        assert!(bad.problem.unwrap().contains("not stroke notation"));
    }

    #[test]
    fn an_empty_query_has_no_answer() {
        assert_eq!(ask("   ", ALL), Answer::default());
    }

    #[test]
    fn an_extra_key_reads_and_prints_in_the_layers_layout() {
        let layout = Layout::plover_with(&['+']).unwrap();
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"ED": "ed", "ED/+-D": "ended"}"#, &layout).unwrap();
        let base = Translator::new(dict);
        let found = answer(&layout, &base.fresh(), "ED/+-D", ALL, &|| false);
        assert_eq!(found.rows[0].word, "\"ended\"");
        assert_eq!(layout.render_strokes(&found.rows[0].strokes), "ED/+-D");
    }

    /// A config directory with `config` as its config.ron and `files` in its steno folder.
    fn config_dir(name: &str, config: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-answer-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("steno")).unwrap();
        std::fs::write(dir.join("config.ron"), config).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join("steno").join(file), text).unwrap();
        }
        dir
    }

    fn setup_in(dir: &Path) -> EngineSetup {
        EngineSetup::load(&dir.join("config.ron"), dir, None)
    }

    const JOIN_CONFIG: &str = r#"(
        layers: { "steno": (kind: Steno((
            extra_keys: ['+'],
            layout_overrides: { "+": Key(KC_Q) },
        ))) },
    )"#;

    #[test]
    fn with_no_config_the_setup_is_the_plain_defaults() {
        let dir = std::env::temp_dir().join("keymux-answer-test-no-such-dir");
        let setup = setup_in(&dir);
        assert_eq!(setup.layer, None);
        assert!(setup.layout.extra_keys().is_empty());
        assert_eq!(setup.roots, default_dictionaries(&dir));
        assert!(setup.keys_by_bit().unwrap().contains_key(&(1 << 1)));
    }

    #[test]
    fn the_layer_named_steno_wins_and_otherwise_the_first_by_name() {
        let named = config_dir(
            "named",
            r#"(layers: {
                "aaa": (kind: Steno((layout: "qwerty"))),
                "steno": (kind: Steno((extra_keys: ['+']))),
            })"#,
            &[],
        );
        assert_eq!(setup_in(&named).layer.as_deref(), Some("steno"));
        assert_eq!(setup_in(&named).layout.extra_keys().len(), 1);
        let unnamed = config_dir(
            "unnamed",
            r#"(layers: {
                "zzz": (kind: Steno((extra_keys: ['+']))),
                "aaa": (kind: Steno(())),
            })"#,
            &[],
        );
        assert_eq!(setup_in(&unnamed).layer.as_deref(), Some("aaa"));
    }

    #[test]
    fn an_override_for_an_extra_key_builds_the_key_map() {
        // `keymux steno keys` used to die here with: layout override for unknown steno key "+"
        let dir = config_dir("extra_override", JOIN_CONFIG, &[]);
        let keys = setup_in(&dir).keys_by_bit().unwrap();
        let join = 1u32 << setup_in(&dir).layout.extra_start();
        assert_eq!(keys.get(&join), Some(&KeyCode::KC_Q));
        assert_eq!(keys.get(&(1 << 1)), preset("qwerty").unwrap().get("T-"));
    }

    #[test]
    fn a_layers_own_dictionary_list_is_what_is_searched() {
        let dir = config_dir(
            "own_list",
            r#"(layers: { "steno": (kind: Steno((dictionaries: ["steno/mine.json"]))) })"#,
            &[
                ("mine.json", r#"{"KAT": "cat"}"#),
                ("other.json", r#"{"TKOG": "dog"}"#),
            ],
        );
        let lookup = Lookup::load(setup_in(&dir));
        assert_eq!(lookup.answer("cat", ALL, &|| false).rows[0].mark, Mark::Entry);
        assert_ne!(lookup.answer("dog", ALL, &|| false).rows[0].mark, Mark::Entry);
    }

    #[test]
    fn a_layer_with_spaces_after_still_finds_every_outline() {
        // Spacing options change how text is spaced, not which outline types a word
        let dir = config_dir(
            "spaces_after",
            r#"(layers: { "steno": (kind: Steno((spaces_after: true, start_capitalized: true))) })"#,
            &[("d.json", r#"{"-T": "the", "KAT": "cat"}"#)],
        );
        let lookup = Lookup::load(setup_in(&dir));
        let found = lookup.answer("the cat", ALL, &|| false);
        assert_eq!(found.rows.iter().filter(|r| r.mark == Mark::Entry).count(), 2);
    }

    /// A plugin that only declares a join key (bit 23), compiled for real.
    const JOIN_PLUGIN: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}
#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 { -1 }
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 { i32::from(bits & (1 << 23) != 0) }
"#;

    #[test]
    fn a_word_is_composed_with_the_layers_plugin_join_key() {
        // The CLI used to load dictionaries without their plugins, so a join key was never
        // seen and "nomy" was fingerspelled
        let dir = config_dir(
            "join_plugin",
            JOIN_CONFIG,
            &[
                ("01-d.json", r#"{"TPHO": "no", "PHAOEU": "my"}"#),
                ("02-join.rs", JOIN_PLUGIN),
            ],
        );
        let lookup = Lookup::load(setup_in(&dir));
        assert_eq!(lookup.plugin_problem, None);
        let found = lookup.answer("nomy", ALL, &|| false);
        assert_eq!(found.rows[0].mark, Mark::Composed(vec!["no".into(), "my".into()]));
        assert_eq!(
            lookup.setup.layout.render_strokes(&found.rows[0].strokes),
            "TPHO/+PHAOEU"
        );
    }

    #[test]
    fn a_plugin_that_does_not_compile_is_reported() {
        let dir = config_dir(
            "bad_plugin",
            JOIN_CONFIG,
            &[("01-d.json", r#"{"TPHO": "no"}"#), ("02-bad.rs", "this is not rust")],
        );
        let lookup = Lookup::load(setup_in(&dir));
        assert!(lookup.plugin_problem.unwrap().contains("1 of 1 plugin"));
    }

    #[test]
    fn a_cancelled_search_still_returns_without_panicking() {
        let found = answer(&Layout::plover(), &base().fresh(), "can", ALL, &|| true);
        assert!(found.rows.len() <= 3);
    }
}
