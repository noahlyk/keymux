//! `keymux steno` commands that read or edit dictionaries, and show the tape.

use super::dict::Dictionary;
use super::format::{parse_pieces, render, FormatState};
use super::layout::{build_key_map, parse_stroke, preset, render_stroke, SLOTS, SLOT_SOUNDS};
use super::numbers::number_text;
use super::setup::{default_dictionaries, steno_dir};
use super::tape;
use super::translate::Translator;
use crate::keycode::KeyCode;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

/// Every dictionary a layer would use with the default config.
#[must_use]
pub fn load_dictionary(config_dir: &Path) -> Dictionary {
    Dictionary::load(&default_dictionaries(config_dir))
}

/// Print the stroke(s) that type `word`.
pub fn lookup(config_dir: &Path, word: &str) {
    let dict = load_dictionary(config_dir);
    let found = dict.strokes_for(word);
    if found.is_empty() {
        println!("No stroke for \"{word}\"");
        return;
    }
    for strokes in found {
        println!("{}", format_strokes(&strokes));
    }
}

/// Print the text a stroke (or `KPA/TKAOEU`-style sequence) types.
pub fn show_stroke(config_dir: &Path, text: &str) -> Result<()> {
    let strokes = parse_strokes(text)?;
    let dict = load_dictionary(config_dir);
    if let Some(pieces) = dict.get(&strokes) {
        println!("{:?}", render(pieces, &mut FormatState::default()));
    } else if let Some(digits) = strokes
        .first()
        .filter(|_| strokes.len() == 1)
        .and_then(|&s| number_text(s))
    {
        println!("{digits:?} (number stroke)");
    } else {
        println!("{} has no translation", format_strokes(&strokes));
    }
    Ok(())
}

/// Print the keys that type `text` on the built-in QWERTY layout, one row per stroke.
pub fn keys(config_dir: &Path, text: &str) -> Result<()> {
    let dict = load_dictionary(config_dir);
    print_table(&chord_rows(dict, &qwerty_keys_by_bit()?, text));
    Ok(())
}

/// Print the built-in QWERTY layout: each steno key, the physical key that presses it,
/// and the sound it stands for.
pub fn layout() -> Result<()> {
    let keys_by_bit = qwerty_keys_by_bit()?;
    let rows: Vec<[String; 3]> = SLOTS
        .iter()
        .enumerate()
        .map(|(slot, &(_, name))| {
            let press = keys_by_bit
                .get(&(1 << slot))
                .map_or_else(String::new, |&key| key_label(key));
            [name.to_string(), press, SLOT_SOUNDS[slot].to_string()]
        })
        .collect();
    // Combined keys press several stroke keys at once, so list them after the single ones
    let mut combined: Vec<[String; 3]> = keys_by_bit
        .iter()
        .filter(|(bits, _)| bits.count_ones() > 1)
        .map(|(&bits, &key)| {
            let parts: Vec<(&str, &str)> = SLOTS
                .iter()
                .enumerate()
                .filter(|(slot, _)| bits & (1 << slot) != 0)
                .map(|(slot, &(_, name))| (name, SLOT_SOUNDS[slot]))
                .collect();
            [
                parts.iter().map(|(name, _)| *name).collect::<Vec<_>>().join("+"),
                key_label(key),
                parts.iter().map(|(_, sound)| *sound).collect::<Vec<_>>().join(" + "),
            ]
        })
        .collect();
    combined.sort_by(|a, b| a[0].cmp(&b[0]));
    let rows: Vec<[String; 3]> = rows.into_iter().chain(combined).collect();
    let widths = [0, 1, 2].map(|col| {
        rows.iter().map(|row| row[col].len()).max().unwrap_or(0)
    });
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

/// The physical key for each stroke key on the built-in QWERTY layout, keyed by stroke bit.
fn qwerty_keys_by_bit() -> Result<HashMap<u32, KeyCode>> {
    let preset = preset("qwerty").map_err(|e| anyhow!(e))?;
    let layout = build_key_map(&preset, &HashMap::new()).map_err(|errs| anyhow!(errs.join("; ")))?;
    Ok(layout.into_iter().map(|(key, bit)| (bit, key)).collect())
}

/// One line of `keymux steno keys`: a word, one of its strokes, and the keys that press it.
#[derive(Debug, PartialEq, Eq)]
pub struct ChordRow {
    pub word: String,
    pub stroke: String,
    pub keys: Vec<String>,
    /// The sounds of the stroke keys pressed, run together, e.g. `kweug`.
    pub sounds: String,
}

/// Stroke sequences tried per word before giving up on an exact match.
const SEARCH_BUDGET: usize = 20_000;

/// The rows that type `text`, one per stroke.
///
/// Words are typed through the same translator a steno layer uses, so spacing and
/// attached suffixes come out as written. Where a word has more than one stroke
/// sequence, the first that types the text exactly is used. When no sequence types
/// the text exactly, each word gets its fewest-stroke translation. A word with no
/// entry gets a single row marked "no stroke".
#[must_use]
pub fn chord_rows(dict: Dictionary, keys_by_bit: &HashMap<u32, KeyCode>, text: &str) -> Vec<ChordRow> {
    let spans = word_spans(text);
    let words: Vec<&str> = spans.iter().map(|&(start, end)| &text[start..end]).collect();
    let candidates: Vec<Vec<Vec<u32>>> = words.iter().map(|word| ranked_strokes(&dict, word)).collect();

    // Words with no entry can't be typed, so each run of known words is searched on its own,
    // starting from a fresh translator
    let base = Translator::new(dict);
    let mut chosen: Vec<Option<Vec<u32>>> = Vec::with_capacity(words.len());
    let mut start = 0;
    while start < words.len() {
        if candidates[start].is_empty() {
            chosen.push(None);
            start += 1;
            continue;
        }
        let end = (start..words.len())
            .find(|&i| candidates[i].is_empty())
            .unwrap_or(words.len());
        // What the run should type after each word: the text from its first word to that word's end
        let run_start = spans[start].0;
        let wants: Vec<String> = spans[start..end]
            .iter()
            .map(|&(_, word_end)| text[run_start..word_end].split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        let run_candidates = &candidates[start..end];
        match exact_strokes(&base, &wants, run_candidates) {
            Some(exact) => chosen.extend(exact.into_iter().map(Some)),
            None => chosen.extend(run_candidates.iter().map(|options| options.first().cloned())),
        }
        start = end;
    }

    let mut rows = Vec::new();
    for (word, strokes) in words.iter().zip(chosen) {
        let Some(strokes) = strokes else {
            rows.push(ChordRow {
                word: (*word).to_string(),
                stroke: "no stroke".to_string(),
                keys: Vec::new(),
                sounds: String::new(),
            });
            continue;
        };
        for (i, &bits) in strokes.iter().enumerate() {
            rows.push(ChordRow {
                word: if i == 0 { (*word).to_string() } else { String::new() },
                stroke: render_stroke(bits),
                keys: stroke_keys(bits, keys_by_bit),
                sounds: stroke_sounds(bits),
            });
        }
    }
    rows
}

/// Byte ranges of the words in `text`. Each whitespace-separated chunk is split so that
/// punctuation on either end is its own word: `now,` is `now` then `,`. Punctuation
/// inside a word stays with it, so `12:29am` is one word.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut chunk_start: Option<usize> = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
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

/// Every stroke sequence that translates `word`, fewest strokes first, then fewest keys.
fn ranked_strokes(dict: &Dictionary, word: &str) -> Vec<Vec<u32>> {
    let mut found = dict.strokes_for(word);
    found.sort_by_key(|strokes| {
        (strokes.len(), strokes.iter().map(|s| s.count_ones()).sum::<u32>())
    });
    found
}

/// One stroke sequence per word that together type the text exactly, or `None`.
/// `wants[i]` is the text that should be typed once word `i` is done.
/// `base` is a fresh translator, so the run starts with no leading space.
fn exact_strokes(base: &Translator, wants: &[String], candidates: &[Vec<Vec<u32>>]) -> Option<Vec<Vec<u32>>> {
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
    for slot in 0..SLOTS.len() {
        let bit = 1 << slot;
        if remaining & bit != 0 {
            if let Some(&key) = keys_by_bit.get(&bit) {
                presses.push((bit, key));
            }
        }
    }

    // Stroke order: each press sits where its lowest stroke key sits
    presses.sort_by_key(|&(mask, _)| mask.trailing_zeros());
    presses.into_iter().map(|(_, key)| key_label(key)).collect()
}

/// The sounds of the stroke keys in `bits`, run together in stroke order, e.g. `hrubg`.
fn stroke_sounds(bits: u32) -> String {
    (0..SLOTS.len())
        .filter(|&slot| bits & (1 << slot) != 0)
        .map(|slot| SLOT_SOUNDS[slot])
        .collect()
}

/// How a physical key is written on the page: `KC_SCLN` is `;`, `KC_A` is `A`.
#[must_use]
pub fn key_label(key: KeyCode) -> String {
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

fn print_table(rows: &[ChordRow]) {
    let word_w = rows.iter().map(|r| r.word.len()).max().unwrap_or(0).max("Word".len());
    let stroke_w = rows.iter().map(|r| r.stroke.len()).max().unwrap_or(0).max("Stroke".len());
    let sounds_w = rows.iter().map(|r| r.sounds.len()).max().unwrap_or(0).max("Sounds".len());
    println!("{:<word_w$}  {:<sounds_w$}  {:<stroke_w$}  Keys", "Word", "Sounds", "Stroke");
    for row in rows {
        println!(
            "{:<word_w$}  {:<sounds_w$}  {:<stroke_w$}  {}",
            row.word,
            row.sounds,
            row.stroke,
            row.keys.join(" ")
        );
    }
}

/// Add or replace a translation in the user dictionary.
pub fn add(config_dir: &Path, stroke: &str, text: &str) -> Result<()> {
    let strokes = parse_strokes(stroke)?;
    if parse_pieces(text).is_none() {
        bail!("\"{text}\" uses a command keymux doesn't support yet");
    }
    let key = format_strokes(&strokes);

    let dir = steno_dir(config_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("user.json");
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
    let tmp = dir.join("user.json.tmp");
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
    let mut offset = existing.len() as u64;
    if !follow {
        return Ok(());
    }

    let mut stdout = std::io::stdout();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let len = file.metadata().map_or(0, |m| m.len());
        if len < offset {
            // The tape was cleared or replaced: start over
            offset = 0;
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut new = String::new();
        file.read_to_string(&mut new)?;
        print!("{new}");
        stdout.flush()?;
        offset = len;
    }
}

/// Parse `KPA/TKAOEU` into its stroke bitmasks.
fn parse_strokes(text: &str) -> Result<Vec<u32>> {
    text.split('/')
        .map(|part| parse_stroke(part).with_context(|| format!("\"{part}\" is not a steno stroke")))
        .collect()
}

fn format_strokes(strokes: &[u32]) -> String {
    strokes
        .iter()
        .map(|&stroke| render_stroke(stroke))
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strokes_round_trip_through_their_text() {
        let strokes = parse_strokes("KPA/TKAOEU").unwrap();
        assert_eq!(format_strokes(&strokes), "KPA/TKAOEU");
    }

    #[test]
    fn invalid_strokes_are_rejected() {
        assert!(parse_strokes("not a stroke").is_err());
    }

    fn qwerty_keys_by_bit() -> HashMap<u32, KeyCode> {
        let preset = preset("qwerty").unwrap();
        build_key_map(&preset, &HashMap::new())
            .unwrap()
            .into_iter()
            .map(|(key, bit)| (bit, key))
            .collect()
    }

    fn sample_dict() -> Dictionary {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"-T": "the", "KWEUG": "quick", "SKWR*UPLD": "jumped",
                "SKWRUPL/-PD": "jumped", "TKOG": "dog"}"#,
        )
        .unwrap();
        dict
    }

    #[test]
    fn one_stroke_word_shows_its_keys_and_sounds() {
        let rows = chord_rows(sample_dict(), &qwerty_keys_by_bit(), "the");
        assert_eq!(
            rows,
            vec![ChordRow {
                word: "the".to_string(),
                stroke: "-T".to_string(),
                keys: vec!["O".to_string()],
                sounds: "t".to_string(),
            }]
        );
    }

    #[test]
    fn keys_pick_the_stroke_that_types_the_text_exactly() {
        // AOT attaches "out" to the next word, so "out with" needs the plain "A" instead
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"AOT": "{out^}", "A": "out", "W": "with"}"#).unwrap();
        let rows = chord_rows(dict, &qwerty_keys_by_bit(), "out with");
        let strokes: Vec<&str> = rows.iter().map(|row| row.stroke.as_str()).collect();
        assert_eq!(strokes, [render_stroke(parse_stroke("A").unwrap()), render_stroke(parse_stroke("W").unwrap())]);
    }

    #[test]
    fn unknown_word_does_not_stop_the_rest_from_matching() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"AOT": "{out^}", "A": "out", "W": "with"}"#).unwrap();
        let rows = chord_rows(dict, &qwerty_keys_by_bit(), "out zzyzx with");
        let strokes: Vec<String> = rows.iter().map(|row| row.stroke.clone()).collect();
        assert_eq!(
            strokes,
            [
                render_stroke(parse_stroke("A").unwrap()),
                "no stroke".to_string(),
                render_stroke(parse_stroke("W").unwrap()),
            ]
        );
    }

    #[test]
    fn quick_breaks_down_into_its_sounds() {
        let rows = chord_rows(sample_dict(), &qwerty_keys_by_bit(), "quick");
        assert_eq!(rows[0].stroke, "KWEUG");
        assert_eq!(rows[0].keys, ["S", "D", ",", "K"]);
        assert_eq!(rows[0].sounds, "kweug");
    }

    #[test]
    fn combined_key_replaces_the_pair_it_presses() {
        let keys = qwerty_keys_by_bit();
        // K- and -T are single keys; A- and O- together are the combined X
        let stroke = parse_stroke("KAO-T").unwrap();
        assert_eq!(stroke_keys(stroke, &keys), ["S", "X", "O"]);
        // A lone A- still presses its single key
        assert_eq!(stroke_keys(parse_stroke("KA").unwrap(), &keys), ["S", "C"]);
    }

    #[test]
    fn word_uses_the_fewest_strokes() {
        // The one-stroke form beats the two-stroke "jump" + "ed" form
        let rows = chord_rows(sample_dict(), &qwerty_keys_by_bit(), "jumped");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stroke, render_stroke(parse_stroke("SKWR*UPLD").unwrap()));
    }

    #[test]
    fn unknown_word_gets_a_no_stroke_row() {
        let rows = chord_rows(sample_dict(), &qwerty_keys_by_bit(), "zzyzx");
        assert_eq!(rows[0].stroke, "no stroke");
        assert!(rows[0].keys.is_empty());
    }

    #[test]
    fn trailing_punctuation_is_its_own_word() {
        let mut dict = sample_dict();
        dict.merge_json(r#"{"TPHOU": "now", "KW-B": ","}"#).unwrap();
        let rows = chord_rows(dict, &qwerty_keys_by_bit(), "now, the");
        let words: Vec<&str> = rows.iter().map(|row| row.word.as_str()).collect();
        assert_eq!(words, ["now", ",", "the"]);
        assert_eq!(rows[0].stroke, render_stroke(parse_stroke("TPHOU").unwrap()));
        assert_eq!(rows[1].stroke, render_stroke(parse_stroke("KW-B").unwrap()));
    }

    #[test]
    fn punctuation_inside_a_word_stays_with_it() {
        assert_eq!(word_spans("12:29am, ok"), [(0, 7), (7, 8), (9, 11)]);
        assert_eq!(word_spans("(a)..."), [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6)]);
    }

    #[test]
    fn key_labels_show_punctuation_as_characters() {
        assert_eq!(key_label(KeyCode::KC_A), "A");
        assert_eq!(key_label(KeyCode::KC_COMM), ",");
        assert_eq!(key_label(KeyCode::KC_SCLN), ";");
        assert_eq!(key_label(KeyCode::KC_QUOT), "'");
    }
}
