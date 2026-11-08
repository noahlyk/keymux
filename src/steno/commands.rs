//! `keymux steno` commands that read or edit dictionaries, and show the tape.
//!
//! `lookup`, `keys` and `stroke` are one thing: they answer a query ([`super::answer`]), drawn
//! the one way ([`super::view`]), and with no query open the interactive session
//! ([`super::interactive`]) that does the same live as you type. They resolve against the
//! steno layer the daemon runs ([`EngineSetup`]): its dictionary chain and plugin, extra keys,
//! join key and key assignments.

use super::answer::{Ask, EngineSetup, Lookup};
use super::dict::Dictionary;
use super::formatter::leaves_attached;
use super::interactive::{self, Session};
use super::layout::{extra_keys, parse_stroke, preset, Layout};
use super::setup::{prepare_dir, steno_dir, USER_DICTIONARY};
use super::tape;
use super::translate::Translator;
use crate::keycode::KeyCode;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

/// The session user's home directory, for the `~/` in a layer's `dictionaries`.
fn session_home() -> Option<std::path::PathBuf> {
    let (uid, _) = crate::get_actual_user_uid();
    crate::get_user_home_dir(uid).ok()
}

/// The steno layer the real config has (or the defaults), as the daemon would build it.
#[must_use]
fn engine_setup(config_path: &Path, config_dir: &Path) -> EngineSetup {
    EngineSetup::load(config_path, config_dir, session_home().as_deref())
}

/// What `keymux steno lookup`, `keys` and `stroke` take.
#[derive(Debug, Clone, Copy, Default)]
pub struct LookupOptions {
    /// Show the physical keys that press each stroke
    pub show_keys: bool,
    /// Only the best outline of each word
    pub fast: bool,
    /// The query is stroke notation whatever its case (`stroke`)
    pub stroke_mode: bool,
}

/// The one lookup, forward or backward, for `lookup`, `keys` and `stroke`.
///
/// With a query, print its answer. With none, open the interactive session: type, and the
/// answer follows. Without a terminal that reads one query per line from standard input
/// instead. See [`super::answer`] for what the answer holds and [`super::interactive`] for the
/// keys.
///
/// # Errors
/// When the terminal or standard output can't be used.
pub fn lookup(
    config_path: &Path,
    config_dir: &Path,
    query: Option<&str>,
    options: LookupOptions,
) -> Result<()> {
    let lookup = Lookup::load(engine_setup(config_path, config_dir));
    if let Some(problem) = &lookup.plugin_problem {
        eprintln!("warning: {problem}");
    }
    if options.show_keys {
        if let Some(problem) = &lookup.keys_problem {
            eprintln!("cannot show keys: {problem}");
        }
    }
    let session = Session::new(options.show_keys, options.fast, options.stroke_mode);
    match query {
        Some(query) => {
            if query.trim().is_empty() {
                println!("Nothing to look up");
                return Ok(());
            }
            let ask = Ask {
                fast: options.fast,
                stroke_mode: options.stroke_mode,
                extra: 8,
            };
            let answer = lookup.answer(query, ask, &|| false);
            let view = lookup.view(options.show_keys, console::colors_enabled());
            for line in view.lines(&answer) {
                println!("{line}");
            }
            if answer.rows.is_empty() && answer.problem.is_none() {
                println!("No stroke for \"{}\"", query.trim());
            }
            Ok(())
        }
        None if interactive::is_interactive() => interactive::run(&lookup, session),
        None => interactive::run_lines(
            &lookup,
            &session,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        ),
    }
}

/// The sound each of [`Layout::plover`]'s 23 keys stands for, in the same order: `KWEUBG` is
/// K (k), W (w), E (e), U (u), B and G (ck). Purely descriptive, for the `layout` command - a
/// theory's own keys have whatever sounds its own config says they do.
const PLOVER_SLOT_SOUNDS: [&str; 23] = [
    "s", "t", "k", "p", "w", "h", "r", "a", "o", "*", "e", "u", "f", "r", "p", "b", "l", "g", "t",
    "s", "d", "z", "#",
];

/// Print the steno layout a real config resolves to — its own preset and `layout_overrides`
/// if it has a steno layer, the built-in QWERTY preset otherwise.
///
/// Each steno key, the physical key that presses it, and the sound it stands for. Extra keys
/// for a steno key (a second key that presses it too), and the layer's own extra stroke keys
/// (a join key), follow as further rows.
pub fn layout(config_path: &Path, config_dir: &Path) -> Result<()> {
    let setup = engine_setup(config_path, config_dir);
    let plover = Layout::plover();
    let keys_by_bit = setup.keys_by_bit()?;
    let extras: Vec<[String; 3]> = extra_keys(&preset(&setup.preset).map_err(|e| anyhow!(e))?)
        .into_iter()
        .map(|(slot, key)| {
            [
                plover.key_at(slot).unwrap().name.clone(),
                key_label(key),
                PLOVER_SLOT_SOUNDS[slot].to_string(),
            ]
        })
        .collect();
    // The layer's own stroke keys beyond the standard 23, such as the join key
    let own: Vec<[String; 3]> = setup
        .layout
        .extra_keys()
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let bit = 1u32 << (setup.layout.extra_start() + i);
            [
                key.name.clone(),
                keys_by_bit
                    .get(&bit)
                    .map_or_else(String::new, |&k| key_label(k)),
                "extra key".to_string(),
            ]
        })
        .collect();
    // Sounds order: the number key (the last slot) first, then the rest in slot order
    let number_slot = plover.number_slot();
    let rows = std::iter::once(number_slot)
        .chain(0..number_slot)
        .map(|slot| {
            let name = &plover.key_at(slot).unwrap().name;
            let press = keys_by_bit
                .get(&(1 << slot))
                .map_or_else(String::new, |&key| key_label(key));
            [name.clone(), press, PLOVER_SLOT_SOUNDS[slot].to_string()]
        });
    // Combined keys press several stroke keys at once, so list them after the single ones
    let mut combined: Vec<[String; 3]> = keys_by_bit
        .iter()
        .filter(|(bits, _)| bits.count_ones() > 1)
        .map(|(&bits, &key)| {
            let parts: Vec<(&str, &str)> = (0..23)
                .filter(|slot| bits & (1 << slot) != 0)
                .map(|slot| {
                    (
                        plover.key_at(slot).unwrap().name.as_str(),
                        PLOVER_SLOT_SOUNDS[slot],
                    )
                })
                .collect();
            [
                parts
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join("+"),
                key_label(key),
                parts
                    .iter()
                    .map(|(_, sound)| *sound)
                    .collect::<Vec<_>>()
                    .join(" + "),
            ]
        })
        .collect();
    combined.sort_by(|a, b| a[0].cmp(&b[0]));
    let rows: Vec<[String; 3]> = rows.chain(own).chain(extras).chain(combined).collect();
    let widths = [0, 1, 2].map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0));
    let headers = ["Steno", "Press", "Sound"];
    let line = |cells: [&str; 3]| {
        println!(
            "{:<w0$}  {:<w1$}  {}",
            cells[0],
            cells[1],
            cells[2],
            w0 = widths[0].max(headers[0].len()),
            w1 = widths[1].max(headers[1].len()),
        );
    };
    line(headers);
    for row in &rows {
        line([&row[0], &row[1], &row[2]]);
    }
    Ok(())
}

/// Stroke sequences tried per word before giving up on an exact match.
const SEARCH_BUDGET: usize = 20_000;

/// Whether `outline` types `word` properly in a sentence: after "the" it gives " word", and the
/// next word still gets its space, so no glued prefix or suffix. Punctuation is left alone,
/// since it attaches to the word before it.
fn types_in_context(base: &Translator, word: &str, outline: &[u32]) -> bool {
    if !word.chars().any(char::is_alphanumeric) {
        return true;
    }
    let Some(the) = parse_stroke("-T") else {
        return false;
    };
    let mut translator = base.clone();
    translator.stroke(the);
    for &bits in outline {
        translator.stroke(bits);
    }
    translator.stroke(the);
    translator.text() == format!("the {word} the")
}

/// The physical keys that press each stroke, one group per stroke. Keys within a stroke run
/// together; strokes are split by `/`, as in the outline.
pub(crate) fn keys_text(strokes: &[u32], keys_by_bit: &HashMap<u32, KeyCode>) -> String {
    let keys: Vec<String> = strokes
        .iter()
        .map(|&bits| stroke_keys(bits, keys_by_bit).concat())
        .collect();
    keys.join("/")
}

/// Byte ranges of the words in `text`. Each whitespace-separated chunk is split so that
/// punctuation on either end is its own word: `now,` is `now` then `,`. Punctuation
/// inside a word stays with it, so `12:29am` is one word.
pub(crate) fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut chunk_start: Option<usize> = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (c.is_whitespace(), chunk_start) {
            (false, None) => chunk_start = Some(i),
            (true, Some(start)) => {
                spans.extend(split_punctuation(text, start, i));
                chunk_start = None;
            }
            _ => {}
        }
    }
    spans
}

/// Splits `text[start..end]`, a chunk with no whitespace, into its core and the
/// punctuation around it. A chunk with no letters or digits splits into single characters.
fn split_punctuation(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let chunk = &text[start..end];
    let is_punctuation = |c: char| !c.is_alphanumeric();
    let core = chunk.trim_matches(is_punctuation);
    if core.is_empty() {
        return chars_as_spans(text, start, end);
    }
    let core_start = start + chunk.find(core).unwrap_or(0);
    let core_end = core_start + core.len();
    let mut spans = chars_as_spans(text, start, core_start);
    spans.push((core_start, core_end));
    spans.extend(chars_as_spans(text, core_end, end));
    spans
}

/// Each character of `text[start..end]` as its own span.
fn chars_as_spans(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    text[start..end]
        .char_indices()
        .map(|(i, c)| (start + i, start + i + c.len_utf8()))
        .collect()
}

#[cfg(test)]
fn proper_outlines(dict: &Dictionary, word: &str) -> Vec<Vec<u32>> {
    proper_outlines_in(&Translator::new(dict.clone()), word)
}

/// The outlines that type `word` properly in a sentence, fewest strokes first. Prefixes and
/// suffixes that glue to a neighbouring word (`{under^}`, `{^under}`) are left out.
///
/// Nothing is copied, so the live lookup can call this on every key press. `base` must have
/// nothing typed: use [`Translator::fresh`].
#[must_use]
pub fn proper_outlines_in(base: &Translator, word: &str) -> Vec<Vec<u32>> {
    ranked_strokes(base.dictionary(), word)
        .into_iter()
        .filter(|outline| types_in_context(base, word, outline))
        .collect()
}

/// Every stroke sequence that translates `word` on its own, fewest strokes first, then fewest keys.
/// An entry that glues to the next word (`{be^}`, a prefix) is left out: it types `be`, but
/// the next word would attach to it, so it doesn't type `be` as a word.
pub(crate) fn ranked_strokes(dict: &Dictionary, word: &str) -> Vec<Vec<u32>> {
    let mut found = dict.strokes_for(word);
    found.retain(|strokes| !dict.lookup(strokes).is_some_and(leaves_attached));
    found.sort_by_key(|strokes| {
        (
            strokes.len(),
            strokes.iter().map(|s| s.count_ones()).sum::<u32>(),
        )
    });
    found
}

/// One stroke sequence per word that together type the text exactly, or `None`.
/// `wants[i]` is the text that should be typed once word `i` is done.
/// `base` is a fresh translator, so the run starts with no leading space.
pub(crate) fn exact_strokes(
    base: &Translator,
    wants: &[String],
    candidates: &[Vec<Vec<u32>>],
) -> Option<Vec<Vec<u32>>> {
    let mut chosen = Vec::new();
    let mut budget = SEARCH_BUDGET;
    search(base, wants, candidates, &mut budget, &mut chosen).then_some(chosen)
}

/// Depth-first search over each word's stroke sequences. A choice is kept when the
/// text so far is exactly the words up to it. Each try runs on a copy of the
/// translator, so a rejected choice leaves nothing behind.
fn search(
    translator: &Translator,
    wants: &[String],
    candidates: &[Vec<Vec<u32>>],
    budget: &mut usize,
    chosen: &mut Vec<Vec<u32>>,
) -> bool {
    let i = chosen.len();
    if i == wants.len() {
        return true;
    }
    for option in &candidates[i] {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let mut next = translator.clone();
        let mut fits = true;
        for &bits in option {
            next.stroke(bits);
            fits &= !next.was_unmatched();
        }
        if fits && next.text() == wants[i] {
            chosen.push(option.clone());
            if search(&next, wants, candidates, budget, chosen) {
                return true;
            }
            chosen.pop();
        }
    }
    false
}

/// The physical keys for each stroke key pressed in `bits`, in stroke order.
fn stroke_keys(bits: u32, keys_by_bit: &HashMap<u32, KeyCode>) -> Vec<String> {
    // Cover the stroke with the widest combined keys that fit inside it, then press
    // single keys for whatever is left
    let mut combined: Vec<(u32, KeyCode)> = keys_by_bit
        .iter()
        .filter(|(&mask, _)| mask.count_ones() > 1 && mask & !bits == 0)
        .map(|(&mask, &key)| (mask, key))
        .collect();
    combined.sort_by_key(|&(mask, _)| (std::cmp::Reverse(mask.count_ones()), mask));

    let mut remaining = bits;
    let mut presses: Vec<(u32, KeyCode)> = Vec::new();
    for (mask, key) in combined {
        if remaining & mask == mask {
            remaining &= !mask;
            presses.push((mask, key));
        }
    }
    // Every slot, the layer's own extra keys (a join key) after the standard 23
    for slot in 0..u32::BITS {
        let bit = 1 << slot;
        if remaining & bit != 0 {
            if let Some(&key) = keys_by_bit.get(&bit) {
                presses.push((bit, key));
            }
        }
    }

    // Sounds order (#STKPWHRAO*EUFRPBLGTSDZ): each press sits where its first stroke key sits,
    // with the number key first
    presses.sort_by_key(|&(mask, _)| display_rank(mask));
    presses.into_iter().map(|(_, key)| key_label(key)).collect()
}

/// Where a press sits in the sounds order: an extra key (`+-D`) reads first as in the outline,
/// then the number key, which is the last slot in the standard 23 but reads first among them,
/// then the rest in slot order.
const fn display_rank(mask: u32) -> u32 {
    let number_bit = 1 << 22;
    if mask >> 23 != 0 {
        0
    } else if mask & number_bit != 0 {
        1
    } else {
        mask.trailing_zeros() + 2
    }
}

/// How a physical key is written on the page: `KC_SCLN` is `;`, `KC_A` is `A`.
#[must_use]
fn key_label(key: KeyCode) -> String {
    let name = format!("{key:?}");
    let name = name.strip_prefix("KC_").unwrap_or(&name);
    match name {
        "COMM" => ",".to_string(),
        "DOT" => ".".to_string(),
        "SLSH" => "/".to_string(),
        "SCLN" => ";".to_string(),
        "QUOT" => "'".to_string(),
        "LBRC" => "[".to_string(),
        "RBRC" => "]".to_string(),
        "MINS" => "-".to_string(),
        "EQL" => "=".to_string(),
        "GRV" => "`".to_string(),
        "BSLS" => "\\".to_string(),
        other => other.to_string(),
    }
}

/// Add or replace a translation in your own dictionary, which comes before every other one.
pub fn add(config_path: &Path, config_dir: &Path, stroke: &str, text: &str) -> Result<()> {
    let layout = engine_setup(config_path, config_dir).layout;
    let strokes = parse_strokes(&layout, stroke)?;
    let key = format_strokes(&layout, &strokes);

    prepare_dir(config_dir);
    let dir = steno_dir(config_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(USER_DICTIONARY);
    let mut entries: serde_json::Map<String, serde_json::Value> =
        match std::fs::read_to_string(&path) {
            Ok(json) => serde_json::from_str(&json)
                .with_context(|| format!("{} is not a JSON object", path.display()))?,
            Err(_) => serde_json::Map::new(),
        };
    let replaced = entries
        .insert(key.clone(), serde_json::Value::String(text.to_string()))
        .is_some();

    // Write to a temp file and rename, so a crash can't leave a half-written dictionary
    let tmp = dir.join(format!("{USER_DICTIONARY}.tmp"));
    let json = serde_json::to_string_pretty(&entries)?;
    std::fs::write(&tmp, format!("{json}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;

    let verb = if replaced { "Replaced" } else { "Added" };
    println!("{verb} {key} → {text:?} in {}", path.display());
    Ok(())
}

/// Print the last `lines` tape entries, and with `follow`, keep printing new ones.
pub fn tail_tape(home: &Path, lines: usize, follow: bool) -> Result<()> {
    let path = tape::default_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.is_empty() && !follow {
        println!("No steno tape yet at {}", path.display());
        return Ok(());
    }
    let all: Vec<&str> = existing.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }

    // A stroke that repeats rewrites its own line, so follow from the start of the last line
    // shown, not the end of the file
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let trimmed = existing.trim_end_matches('\n');
    let mut line_start = trimmed.rfind('\n').map_or(0, |i| i + 1);
    let mut shown = trimmed[line_start..].to_owned();
    let mut stdout = std::io::stdout();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let len = file.metadata().map_or(0, |m| m.len());
        if len < line_start as u64 {
            // The tape was cleared or replaced: start over
            line_start = 0;
            shown.clear();
        }
        file.seek(SeekFrom::Start(line_start as u64))?;
        let mut tail = String::new();
        file.read_to_string(&mut tail)?;
        // Only whole lines: the next one may still be being written
        let Some(end) = tail.rfind('\n') else {
            continue;
        };
        let complete = &tail[..=end];
        let first = format!("{shown}\n");
        let new = if shown.is_empty() {
            complete
        } else if let Some(rest) = complete.strip_prefix(&first) {
            rest
        } else if tty {
            // The line shown last was rewritten, such as by a stroke that repeated: draw it again
            print!("\x1b[1A\r\x1b[2K");
            complete
        } else {
            &complete[complete.find('\n').map_or(0, |i| i + 1)..]
        };
        if !new.is_empty() {
            print!("{new}");
            stdout.flush()?;
        }
        let whole = complete.trim_end_matches('\n');
        let last = whole.rfind('\n').map_or(0, |i| i + 1);
        line_start += last;
        whole[last..].clone_into(&mut shown);
    }
}

/// Parse `KPA/TKAOEU` into its stroke bitmasks.
fn parse_strokes(layout: &Layout, text: &str) -> Result<Vec<u32>> {
    text.split('/')
        .map(|part| layout.parse_stroke(part).with_context(|| format!("\"{part}\" is not a steno stroke")))
        .collect()
}

fn format_strokes(layout: &Layout, strokes: &[u32]) -> String {
    layout.render_strokes(strokes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyAction;
    use crate::steno::layout::{build_key_map, without_extra_keys};

    #[test]
    fn strokes_round_trip_through_their_text() {
        let strokes = parse_strokes(&Layout::plover(), "KPA/TKAOEU").unwrap();
        assert_eq!(format_strokes(&Layout::plover(), &strokes), "KPA/TKAOEU");
    }

    #[test]
    fn invalid_strokes_are_rejected() {
        assert!(parse_strokes(&Layout::plover(), "not a stroke").is_err());
    }

    fn qwerty_keys_by_bit() -> HashMap<u32, KeyCode> {
        let preset = without_extra_keys(preset("qwerty").unwrap());
        build_key_map(&preset, &HashMap::new())
            .unwrap()
            .into_iter()
            .map(|(key, bit)| (bit, key))
            .collect()
    }

    #[test]
    fn the_number_key_is_pressed_first_in_sounds_order() {
        // The number key is the last slot but reads first: #STKPWHRAO*EUFRPBLGTSDZ
        let stroke = Layout::plover().parse_stroke("#SK").unwrap();
        assert_eq!(stroke_keys(stroke, &qwerty_keys_by_bit()), ["1", "A", "S"]);
    }

    #[test]
    fn a_stroke_presses_each_key_s_own_physical_key() {
        let keys = qwerty_keys_by_bit();
        // K- and -T are single keys; A- and O- each press their own key too, since the
        // default preset has no combined keys
        let stroke = Layout::plover().parse_stroke("KAO-T").unwrap();
        assert_eq!(stroke_keys(stroke, &keys), ["S", "C", "V", "P"]);
        assert_eq!(
            stroke_keys(Layout::plover().parse_stroke("KA").unwrap(), &keys),
            ["S", "C"]
        );
    }

    #[test]
    fn a_combined_key_added_by_an_override_still_replaces_the_pair_it_presses() {
        let preset = without_extra_keys(preset("qwerty").unwrap());
        let mut overrides = HashMap::new();
        overrides.insert("A-+O-".to_string(), KeyAction::Key(KeyCode::KC_X));
        let keys: HashMap<u32, KeyCode> = build_key_map(&preset, &overrides)
            .unwrap()
            .into_iter()
            .map(|(key, bit)| (bit, key))
            .collect();
        let stroke = Layout::plover().parse_stroke("KAO-T").unwrap();
        assert_eq!(stroke_keys(stroke, &keys), ["S", "X", "P"]);
    }

    #[test]
    fn a_join_key_is_pressed_by_its_own_physical_key_and_read_first() {
        // `+-D` reads join key first, so it is pressed first: Q, then D
        let layout = Layout::plover_with(&['+']).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("+".to_string(), KeyAction::Key(KeyCode::KC_Q));
        let keys: HashMap<u32, KeyCode> = layout
            .build_key_map(&without_extra_keys(preset("qwerty").unwrap()), &overrides)
            .unwrap()
            .into_iter()
            .map(|(key, bit)| (bit, key))
            .collect();
        let stroke = layout.parse_stroke("+-D").unwrap();
        assert_eq!(stroke_keys(stroke, &keys), ["Q", "["]);
    }

    #[test]
    fn glued_outlines_are_not_listed_for_the_word() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"-T": "the", "UPBD": "under", "UPB/TKER": "{under^}", "TPH-PB": "{^under}"}"#,
            &Layout::plover(),
        )
        .unwrap();
        assert_eq!(
            proper_outlines(&dict, "under"),
            vec![vec![Layout::plover().parse_stroke("UPBD").unwrap()]]
        );
    }

    #[test]
    fn punctuation_inside_a_word_stays_with_it() {
        assert_eq!(word_spans("12:29am, ok"), [(0, 7), (7, 8), (9, 11)]);
        assert_eq!(
            word_spans("(a)..."),
            [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6)]
        );
    }

    #[test]
    fn key_labels_show_punctuation_as_characters() {
        assert_eq!(key_label(KeyCode::KC_A), "A");
        assert_eq!(key_label(KeyCode::KC_COMM), ",");
        assert_eq!(key_label(KeyCode::KC_SCLN), ";");
        assert_eq!(key_label(KeyCode::KC_QUOT), "'");
    }

    #[test]
    fn exact_strokes_pick_the_outline_that_types_the_text() {
        // AOT attaches "out" to the next word, so "out with" needs the plain "A" instead
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"AOT": "{out^}", "A": "out", "W": "with"}"#,
            &Layout::plover(),
        )
        .unwrap();
        let base = Translator::new(dict.clone());
        let candidates: Vec<Vec<Vec<u32>>> = ["out", "with"]
            .iter()
            .map(|w| ranked_strokes(&dict, w))
            .collect();
        let wants = vec!["out".to_string(), "out with".to_string()];
        let chosen = exact_strokes(&base, &wants, &candidates).unwrap();
        assert_eq!(
            chosen,
            vec![
                vec![Layout::plover().parse_stroke("A").unwrap()],
                vec![Layout::plover().parse_stroke("W").unwrap()]
            ]
        );
    }

    #[test]
    fn show_keys_names_the_physical_keys_in_the_output() {
        // End to end through the view the commands print
        let dict_dir = std::env::temp_dir().join(format!("keymux-commands-keys-{}", std::process::id()));
        std::fs::create_dir_all(dict_dir.join("steno")).unwrap();
        std::fs::write(dict_dir.join("steno/d.json"), r#"{"KWEUG": "quick"}"#).unwrap();
        std::fs::write(dict_dir.join("config.ron"), "(layers: {})").unwrap();
        let lookup = Lookup::load(EngineSetup::load(
            &dict_dir.join("config.ron"),
            &dict_dir,
            None,
        ));
        let ask = Ask {
            fast: false,
            stroke_mode: false,
            extra: 3,
        };
        let answer = lookup.answer("quick", ask, &|| false);
        let lines = lookup.view(true, false).lines(&answer);
        assert!(lines[0].starts_with("word") && lines[0].ends_with("keys"), "{lines:?}");
        // The default preset has no combined keys, so -E and -U each press their own key
        assert!(lines[1].starts_with("quick") && lines[1].contains("KWEUG") && lines[1].ends_with("SDNML"), "{lines:?}");
    }
}

