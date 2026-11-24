//! Writing a word the dictionary has no entry for out of entries it does have.
//!
//! A word that is not an entry (`everybody's`, `noone`) can often still be typed in a few
//! strokes: a word followed by a suffix entry (`everybody` then the entry whose definition is
//! `{^'s}`), or two words glued together. Nothing here knows any theory: a suffix is found by the
//! *shape of its definition* (`{^` + the text + `}`), and "glued" is whatever the loaded
//! dictionary and plugins say it is.
//!
//! - If a plugin declares a join key (`joins(bits)`, see [`super::plugin`]), every word after
//!   the first has that key added to its first stroke, costing no extra stroke.
//! - Otherwise, if the dictionary has the `{^}` attach entry (Plover, Lapwing), that stroke goes
//!   between the two words.
//! - Otherwise only a word plus a suffix can be composed.
//!
//! The cheapest way wins: fewest strokes, then fewest keys, then fewest pieces; at most
//! [`MAX_PIECES`] pieces. A real entry never reaches this code (callers ask only after a miss).
//!
//! Cost: a word of `n` letters has `n(n+1)/2` substrings, each looked up once (a word lookup and
//! a suffix lookup, both indexed: a hash probe or a binary search, never a scan of the
//! dictionary), and the search over them is `O(n^2 * MAX_PIECES)`. A composition without a join
//! key is then typed through a fresh translator to prove it really produces the word.

use super::commands::ranked_strokes;
use super::dict::Dictionary;
use super::translate::Translator;
use std::collections::HashMap;

/// The most entries one composed word is made of.
pub const MAX_PIECES: usize = 3;

/// A word's pieces and the strokes that type them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    pub strokes: Vec<u32>,
    /// The text each piece writes, in order: `["everybody", "'s"]`.
    pub pieces: Vec<String>,
    /// Whether a piece carries a join key, which only the theory's plugin can read back.
    pub joined: bool,
}

/// How a second word is attached to what is before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Glue {
    /// This bit, which a plugin's `joins` says continues the word, set on the word's first stroke.
    JoinBit(u32),
    /// This outline (the `{^}` entry) struck between the two words.
    Stroke(Vec<u32>),
    /// No way to attach one word to another: only a word and a suffix.
    Unavailable,
}

fn keys(strokes: &[u32]) -> u32 {
    strokes.iter().map(|s| s.count_ones()).sum()
}

/// The glue `base`'s dictionary and plugins offer.
#[must_use]
pub fn glue_for(base: &Translator) -> Glue {
    if let Some(bit) = (0..32).map(|b| 1u32 << b).find(|&bit| base.joins(bit)) {
        return Glue::JoinBit(bit);
    }
    best_outline(entries_defined_as(base.dictionary(), "", "{^}"))
        .map_or(Glue::Unavailable, Glue::Stroke)
}

fn best_outline(found: Vec<Vec<u32>>) -> Option<Vec<u32>> {
    found
        .into_iter()
        .min_by_key(|strokes| (strokes.len(), keys(strokes)))
}

/// The outlines that type `typed` and whose definition is written exactly `definition`
/// (`strokes_for` finds by what an entry types, so `'s` also finds the word `'s`).
fn entries_defined_as(dict: &Dictionary, typed: &str, definition: &str) -> Vec<Vec<u32>> {
    let mut found = dict.strokes_for(typed);
    found.retain(|strokes| dict.lookup(strokes) == Some(definition));
    found
}

/// The suffix entry for `tail`: the cheapest outline whose definition is exactly `{^tail}`.
fn suffix_outline(dict: &Dictionary, tail: &str) -> Option<Vec<u32>> {
    best_outline(entries_defined_as(dict, tail, &format!("{{^{tail}}}")))
}

#[derive(Clone)]
struct Entry {
    cost: (usize, u32),
    from: usize,
    from_pieces: usize,
    strokes: Vec<u32>,
    joined: bool,
}

type Memo = HashMap<(usize, usize), (Option<Vec<u32>>, Option<Vec<u32>>)>;

/// The cheapest composition of `word`.
///
/// Pieces come from `get_word` (a whole word's best outline, asked for
/// substrings of two letters or more), `get_suffix` (the best outline of the suffix that writes
/// this text, any length) and `glue`. `cancelled` is polled once per end position.
#[must_use]
pub fn compose_with(
    get_word: &dyn Fn(&str) -> Option<Vec<u32>>,
    get_suffix: &dyn Fn(&str) -> Option<Vec<u32>>,
    glue: &Glue,
    word: &str,
    cancelled: &dyn Fn() -> bool,
) -> Option<Composition> {
    let at: Vec<usize> = word
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(word.len()))
        .collect();
    let last = at.len() - 1;
    if last < 2 {
        return None;
    }
    // best[end][pieces]: the cheapest way to write the first `end` characters in `pieces` pieces
    let mut best: Vec<[Option<Entry>; MAX_PIECES + 1]> = vec![Default::default(); last + 1];
    let mut memo = Memo::new();
    for end in 1..=last {
        if cancelled() {
            return None;
        }
        for start in 0..end {
            if start > 0 && best[start].iter().all(Option::is_none) {
                continue;
            }
            let piece = &word[at[start]..at[end]];
            let (words, suffix) = memo.entry((start, end)).or_insert_with(|| {
                let words = (end - start >= 2 && !(start == 0 && end == last))
                    .then(|| get_word(piece))
                    .flatten();
                let suffix = (start > 0).then(|| get_suffix(piece)).flatten();
                (words, suffix)
            });
            let mut options: Vec<(Vec<u32>, bool)> = Vec::new();
            if let Some(outline) = words {
                match (start, glue) {
                    (0, _) => options.push((outline.clone(), false)),
                    (_, Glue::JoinBit(bit)) => {
                        let mut strokes = outline.clone();
                        strokes[0] |= bit;
                        options.push((strokes, true));
                    }
                    (_, Glue::Stroke(between)) => {
                        options.push(([between.as_slice(), outline].concat(), false));
                    }
                    (_, Glue::Unavailable) => {}
                }
            }
            if let Some(outline) = suffix {
                options.push((outline.clone(), false));
            }
            for (strokes, joined) in options {
                let origins: Vec<(usize, (usize, u32), bool)> = if start == 0 {
                    vec![(0, (0, 0), false)]
                } else {
                    (1..MAX_PIECES)
                        .filter_map(|p| best[start][p].as_ref().map(|e| (p, e.cost, e.joined)))
                        .collect()
                };
                for (from_pieces, before, was_joined) in origins {
                    let cost = (before.0 + strokes.len(), before.1 + keys(&strokes));
                    let slot = &mut best[end][from_pieces + 1];
                    if slot.as_ref().is_none_or(|held| cost < held.cost) {
                        *slot = Some(Entry {
                            cost,
                            from: start,
                            from_pieces,
                            strokes: strokes.clone(),
                            joined: joined || was_joined,
                        });
                    }
                }
            }
        }
    }
    let pieces_used = (1..=MAX_PIECES)
        .filter(|&p| best[last][p].is_some())
        .min_by_key(|&p| (best[last][p].as_ref().map(|e| e.cost), p))?;
    let joined = best[last][pieces_used].as_ref()?.joined;
    let (mut chunks, mut texts) = (Vec::new(), Vec::new());
    let (mut end, mut p) = (last, pieces_used);
    while end > 0 {
        let entry = best[end][p].as_ref()?;
        chunks.push(entry.strokes.clone());
        texts.push(word[at[entry.from]..at[end]].to_owned());
        (end, p) = (entry.from, entry.from_pieces);
    }
    chunks.reverse();
    texts.reverse();
    Some(Composition {
        strokes: chunks.concat(),
        pieces: texts,
        joined,
    })
}

/// `word` written from entries of `base`'s dictionary, or `None` when nothing composes it (or
/// `cancelled` went true). `base` must have nothing typed (use [`Translator::fresh`]).
#[must_use]
pub fn compose(base: &Translator, word: &str, cancelled: &dyn Fn() -> bool) -> Option<Composition> {
    let dict = base.dictionary();
    let glue = glue_for(base);
    let found = compose_with(
        &|piece| ranked_strokes(dict, piece).into_iter().next(),
        &|tail| suffix_outline(dict, tail),
        &glue,
        word,
        cancelled,
    )?;
    // Without a join key the dictionary alone decides what the strokes type (orthography can
    // change a joint: `{^ing}` after `make`), so prove it. A join key's reading is a plugin's:
    // with one loaded, ask it the way typing does; with none there is nobody to ask, and the
    // composition stands as the dictionary's join key promises it.
    let proven = if found.joined {
        !base.has_plugin() || types_exactly(base, &found.strokes, word, cancelled)
    } else {
        types_exactly(base, &found.strokes, word, cancelled)
    };
    proven.then_some(found)
}

/// How long the proof waits for a plugin's reading that missed its deadline, so a slow plugin
/// drops the row instead of holding the lookup.
const READING_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `strokes`, typed on a fresh translator over `base`'s dictionary and plugins, give
/// exactly `word`. A plugin's reading that comes late is waited for (up to [`READING_WAIT`], and
/// never past `cancelled`), as the engine settles it on a tick, so the proof sees what typing
/// ends up with.
fn types_exactly(
    base: &Translator,
    strokes: &[u32],
    word: &str,
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let mut typed = base.fresh();
    let started = std::time::Instant::now();
    for &stroke in strokes {
        typed.stroke(stroke);
        while typed.has_late() {
            if cancelled() || started.elapsed() > READING_WAIT {
                return false;
            }
            if typed.late_ready() {
                typed.settle_late();
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        if typed.was_unmatched() {
            return false;
        }
    }
    typed.text() == word
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::formatter::Options;
    use crate::steno::layout::Layout;
    use crate::steno::plugin::Chain;
    use std::sync::Arc;

    fn s(text: &str) -> u32 {
        Layout::plover().parse_stroke(text).unwrap()
    }

    const DICT: &str = r#"{
        "EFB": "everybody",
        "-Z": "{^'s}",
        "TPHO": "no",
        "WUPB": "one",
        "TK-LS": "{^}",
        "SKWRAOEUPB": "noone",
        "PHAOEU": "my"
    }"#;

    fn translator(json: &str) -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(json, &Layout::plover()).unwrap();
        Translator::new(dict)
    }

    fn compose_in(t: &Translator, word: &str) -> Option<Composition> {
        compose(&t.fresh(), word, &|| false)
    }

    #[test]
    fn a_word_and_a_suffix_entry_make_two_strokes() {
        let t = translator(DICT);
        let found = compose_in(&t, "everybody's").unwrap();
        assert_eq!(found.strokes, vec![s("EFB"), s("-Z")]);
        assert_eq!(found.pieces, ["everybody", "'s"]);
        assert!(!found.joined);
    }

    #[test]
    fn a_theory_with_no_join_key_puts_the_attach_stroke_between_two_words() {
        let t = translator(r#"{"TPHO": "no", "WUPB": "one", "TK-LS": "{^}"}"#);
        assert_eq!(glue_for(&t), Glue::Stroke(vec![s("TK-LS")]));
        let found = compose_in(&t, "noone").unwrap();
        assert_eq!(found.strokes, vec![s("TPHO"), s("TK-LS"), s("WUPB")]);
        assert_eq!(found.pieces, ["no", "one"]);
    }

    #[test]
    fn without_a_join_key_or_an_attach_entry_two_words_cannot_be_composed() {
        let t = translator(r#"{"TPHO": "no", "WUPB": "one", "-Z": "{^'s}"}"#);
        assert_eq!(glue_for(&t), Glue::Unavailable);
        assert_eq!(compose_in(&t, "noone"), None);
        // but a suffix still can
        assert!(compose_in(&t, "no's").is_some());
    }

    #[test]
    fn a_real_entry_is_never_replaced_by_a_composition() {
        let t = translator(DICT);
        // `noone` is an entry; the reverse lookup answers with it, not no + one
        let base = t.fresh();
        let segments = super::super::reverse::reverse_phrase(&base, base.dictionary(), "noone");
        let found = segments[0].as_ref().unwrap();
        assert_eq!(found.strokes, vec![s("SKWRAOEUPB")]);
        assert_eq!(found.composed_of, None);
    }

    #[test]
    fn the_reverse_lookup_marks_a_composed_word() {
        let t = translator(DICT);
        let base = t.fresh();
        let segments =
            super::super::reverse::reverse_phrase(&base, base.dictionary(), "everybody's my");
        let first = segments[0].as_ref().unwrap();
        assert_eq!(first.strokes, vec![s("EFB"), s("-Z")]);
        assert!(!first.fingerspelled);
        assert_eq!(
            super::super::reverse::composed_note(first.composed_of.as_ref().unwrap()),
            "(composed: everybody + 's)"
        );
        assert_eq!(segments[1].as_ref().unwrap().composed_of, None);
    }

    fn lookup<'a>(table: &'a [(&'a str, Vec<u32>)]) -> impl Fn(&str) -> Option<Vec<u32>> + 'a {
        move |piece| {
            table
                .iter()
                .find(|(t, _)| *t == piece)
                .map(|(_, o)| o.clone())
        }
    }

    #[test]
    fn fewest_strokes_then_keys_then_pieces() {
        let glue = Glue::Stroke(vec![]); // free glue, to isolate the ordering
        let run = |words: &[(&str, Vec<u32>)], suffixes: &[(&str, Vec<u32>)]| {
            compose_with(&lookup(words), &lookup(suffixes), &glue, "abcd", &|| false)
                .unwrap()
                .pieces
        };
        // Fewer strokes beats fewer keys: abc + d is 2 strokes (12 keys), ab + cd 4 strokes (4)
        assert_eq!(
            run(
                &[
                    ("ab", vec![1, 1]),
                    ("cd", vec![1, 1]),
                    ("abc", vec![0b1111])
                ],
                &[("d", vec![0xFF])]
            ),
            ["abc", "d"]
        );
        // Equal strokes: fewer keys wins
        assert_eq!(
            run(
                &[("ab", vec![1]), ("cd", vec![1]), ("abc", vec![0b1111])],
                &[("d", vec![1])]
            ),
            ["ab", "cd"]
        );
        // Equal strokes and keys: fewer pieces wins (2 over 3)
        assert_eq!(
            run(
                &[("ab", vec![1]), ("cd", vec![1]), ("abc", vec![1])],
                &[("d", vec![1]), ("c", vec![1])]
            )
            .len(),
            2
        );
    }

    #[test]
    fn at_most_three_pieces() {
        let none = |_: &str| -> Option<Vec<u32>> { None };
        let words = [
            ("ab", vec![1]),
            ("cd", vec![2]),
            ("ef", vec![4]),
            ("gh", vec![8]),
        ];
        let glue = Glue::Stroke(vec![]);
        assert!(compose_with(&lookup(&words), &none, &glue, "abcdefgh", &|| false).is_none());
        assert_eq!(
            compose_with(&lookup(&words), &none, &glue, "abcdef", &|| false)
                .unwrap()
                .pieces,
            ["ab", "cd", "ef"]
        );
    }

    #[test]
    fn a_join_key_costs_no_stroke_and_marks_the_second_word() {
        let words = [("no", vec![0b10]), ("one", vec![0b100, 0b1000])];
        let none = |_: &str| -> Option<Vec<u32>> { None };
        let found = compose_with(
            &lookup(&words),
            &none,
            &Glue::JoinBit(1 << 23),
            "noone",
            &|| false,
        )
        .unwrap();
        assert_eq!(found.strokes, vec![0b10, 0b100 | (1 << 23), 0b1000]);
        assert!(found.joined);
    }

    #[test]
    fn a_cancelled_composition_gives_up() {
        let t = translator(DICT);
        assert_eq!(compose(&t.fresh(), "everybody's", &|| true), None);
    }

    /// A real plugin that declares a join key (bit 23) and reads any two-stroke outline as "nomy", compiled for real.
    const JOIN_PLUGIN: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}
static READING: [u8; 4] = *b"nomy";
#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    if !input.contains(&b'/') { return -1; }
    (i64::from(READING.as_ptr() as i32) << 32) | 4
}
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 { i32::from(bits & (1 << 23) != 0) }
"#;

    #[test]
    fn a_plugins_join_key_is_used_instead_of_the_attach_stroke() {
        let dir = crate::test_dir::TestDir::new("keymux-compose-test");
        let (dict_path, plugin_path) = (dir.join("01-dict.json"), dir.join("02-join.rs"));
        std::fs::write(&dict_path, DICT).unwrap();
        std::fs::write(&plugin_path, JOIN_PLUGIN).unwrap();
        let chain = Chain::load_cached(
            &[dict_path, plugin_path],
            &dir.join("cache"),
            &Layout::plover(),
        );
        let t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        assert_eq!(glue_for(&t), Glue::JoinBit(1 << 23));
        let found = compose_in(&t, "nomy").unwrap();
        assert_eq!(found.strokes, vec![s("TPHO"), s("PHAOEU") | (1 << 23)]);
        assert_eq!(found.pieces, ["no", "my"]);
        assert!(found.joined);
        // a suffix still takes its own stroke, no join needed
        assert_eq!(
            compose_in(&t, "everybody's").unwrap().strokes,
            vec![s("EFB"), s("-Z")]
        );
    }

    #[test]
    fn a_config_join_key_is_the_glue_with_no_plugin_and_matches_a_plugins() {
        let dir = crate::test_dir::TestDir::new("keymux-compose-joinkey");
        let dict_path = dir.join("01-dict.json");
        std::fs::write(&dict_path, DICT).unwrap();
        let layout = Layout::plover_with_joins(&['+'], &['+']).unwrap();
        let chain = Chain::load_cached(&[dict_path], &dir.join("cache"), &layout);
        assert_eq!(chain.plugin_count(), 0);
        let t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        assert_eq!(glue_for(&t), Glue::JoinBit(1 << 23));
        let found = compose_in(&t, "nomy").unwrap();
        assert_eq!(found.strokes, vec![s("TPHO"), s("PHAOEU") | (1 << 23)]);
        assert!(found.joined);
        // without join_keys the join bit is not offered
        assert_ne!(glue_for(&translator(DICT)), Glue::JoinBit(1 << 23));
    }

    /// A dictionary and a fake plugin (the `+` join key, read as the outline lower-cased without
    /// its join mark and slashes, except that `answer` overrides what the plugin says for
    /// the outline it is asked) for the join-key proof.
    fn with_reader(read: impl Fn(&str) -> Option<String> + Send + 'static) -> Translator {
        let layout = Layout::plover_with_joins(&['+'], &['+']).unwrap();
        let mut dict = crate::steno::dict::Dictionary::default();
        dict.merge_json(DICT, &layout).unwrap();
        let chain = Chain::test_with_reader(
            dict,
            layout,
            Box::new(
                move |outline: &str, _: &str, _: Option<&crate::steno::strokes::Window>| {
                    read(outline)
                },
            ),
        );
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn a_join_key_composition_is_kept_when_the_plugin_reads_it_as_the_word() {
        let t = with_reader(|outline| {
            let plus = outline.contains('+');
            plus.then(|| "nomy".to_owned())
        });
        assert!(t.has_plugin());
        let found = compose_in(&t, "nomy").expect("the plugin reads it as the word");
        assert!(found.joined);
    }

    #[test]
    fn a_join_key_composition_the_plugin_reads_as_something_else_is_dropped() {
        let t = with_reader(|outline| outline.contains('+').then(|| "nome".to_owned()));
        assert_eq!(compose_in(&t, "nomy"), None);
        // Nothing read at all (the plugin declines): the strokes type no word either
        let t = with_reader(|_| None);
        assert_eq!(compose_in(&t, "nomy"), None);
    }

    #[test]
    fn a_plugin_that_answers_after_its_deadline_is_waited_for() {
        let mut t = with_reader(|outline| {
            std::thread::sleep(std::time::Duration::from_millis(60));
            outline.contains('+').then(|| "nomy".to_owned())
        });
        t.set_plugin_deadline(std::time::Duration::from_millis(5));
        assert!(
            compose_in(&t, "nomy").is_some(),
            "a late reading still proves it"
        );
    }

    #[test]
    fn a_dictionary_composition_is_still_proven_by_the_dictionary_with_a_plugin_loaded() {
        let t = with_reader(|_| None);
        assert_eq!(
            compose_in(&t, "everybody's").unwrap().strokes,
            vec![s("EFB"), s("-Z")]
        );
    }

    #[test]
    fn composing_stays_fast_over_a_large_dictionary() {
        // A miss pays for this on every key press of the live lookup: every substring is an
        // indexed probe, never a scan. 150k entries by default; KEYMUX_COMPOSE_ENTRIES=1000000
        // measures the million-entry case (see the README).
        use std::fmt::Write as _;
        let entries: u32 = std::env::var("KEYMUX_COMPOSE_ENTRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(150_000);
        let mut json = String::from(
            r#"{"EFB":"everybody","-Z":"{^'s}","TK-LS":"{^}","TPHO":"no","WUPB":"one""#,
        );
        let (mut state, mut count, mut i) = (0x9E37_79B9u32, 0u32, 0u32);
        let mut seen = std::collections::HashSet::new();
        while count < entries {
            state = state.wrapping_mul(2_654_435_761).wrapping_add(i);
            i += 1;
            let bits = (state ^ (i << 7)) & 0x007F_FFFF;
            if bits == 0 || !seen.insert((bits, i % 3)) {
                continue;
            }
            let outline = if i % 3 == 0 {
                crate::steno::layout::render_strokes(&[bits])
            } else {
                crate::steno::layout::render_strokes(&[bits, (bits >> 3) | 1])
            };
            write!(json, ",{outline:?}:\"w{i}\"").unwrap();
            count += 1;
        }
        json.push('}');
        let t = translator(&json);
        let base = t.fresh();
        let start = std::time::Instant::now();
        for word in ["everybody's", "noone", "everybody'sone", "zzzzzzzzzzzzzzzz"] {
            let _ = compose(&base, word, &|| false);
        }
        let elapsed = start.elapsed();
        eprintln!(
            "compose x4 over {} entries: {elapsed:?}",
            t.dictionary().len()
        );
        assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
    }
}
