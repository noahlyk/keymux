//! `keymux steno` commands that read or edit dictionaries, and show the tape.

use super::dict::Dictionary;
use super::format::{parse_pieces, render, FormatState};
use super::layout::{build_key_map, parse_stroke, preset, render_stroke, SLOTS, SLOT_SOUNDS};
use super::numbers::number_text;
use super::setup::{default_dictionaries, steno_dir};
use super::tape;
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
    print_table(&chord_rows(&dict, &qwerty_keys_by_bit()?, text));
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
    /// Each stroke key pressed, with the sound it stands for, e.g. `K- (k), W- (w)`.
    pub sounds: String,
}

/// The rows that type `text`, one per stroke.
///
/// Each word uses the translation with the fewest strokes, then the fewest keys. The word
/// is written on the first row of a multi-stroke word only. A word with no entry gets a
/// single row marked "no stroke".
#[must_use]
pub fn chord_rows(dict: &Dictionary, keys_by_bit: &HashMap<u32, KeyCode>, text: &str) -> Vec<ChordRow> {
    let mut rows = Vec::new();
    for word in text.split_whitespace() {
        let best = dict.strokes_for(word).into_iter().min_by_key(|strokes| {
            (strokes.len(), strokes.iter().map(|s| s.count_ones()).sum::<u32>())
        });
        let Some(strokes) = best else {
            rows.push(ChordRow {
                word: word.to_string(),
                stroke: "no stroke".to_string(),
                keys: Vec::new(),
                sounds: String::new(),
            });
            continue;
        };
        for (i, &bits) in strokes.iter().enumerate() {
            rows.push(ChordRow {
                word: if i == 0 { word.to_string() } else { String::new() },
                stroke: render_stroke(bits),
                keys: stroke_keys(bits, keys_by_bit),
                sounds: stroke_sounds(bits),
            });
        }
    }
    rows
}

/// The physical keys for each stroke key pressed in `bits`, in stroke order.
fn stroke_keys(bits: u32, keys_by_bit: &HashMap<u32, KeyCode>) -> Vec<String> {
    (0..SLOTS.len())
        .map(|slot| 1 << slot)
        .filter(|bit| bits & bit != 0)
        .map(|bit| keys_by_bit.get(&bit).map_or_else(|| "?".to_string(), |&key| key_label(key)))
        .collect()
}

/// The sound behind each stroke key in `bits`, e.g. `K- (k), W- (w), -G (g)`.
fn stroke_sounds(bits: u32) -> String {
    (0..SLOTS.len())
        .filter(|&slot| bits & (1 << slot) != 0)
        .map(|slot| format!("{} ({})", SLOTS[slot].1, SLOT_SOUNDS[slot]))
        .collect::<Vec<_>>()
        .join(", ")
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
    let keys_w = rows.iter().map(|r| r.keys.join(" ").len()).max().unwrap_or(0).max("Keys".len());
    println!("{:<word_w$}  {:<stroke_w$}  {:<keys_w$}  Sounds", "Word", "Stroke", "Keys");
    for row in rows {
        println!(
            "{:<word_w$}  {:<stroke_w$}  {:<keys_w$}  {}",
            row.word,
            row.stroke,
            row.keys.join(" "),
            row.sounds
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
        let rows = chord_rows(&sample_dict(), &qwerty_keys_by_bit(), "the");
        assert_eq!(
            rows,
            vec![ChordRow {
                word: "the".to_string(),
                stroke: "-T".to_string(),
                keys: vec!["O".to_string()],
                sounds: "-T (t)".to_string(),
            }]
        );
    }

    #[test]
    fn quick_breaks_down_into_its_sounds() {
        let rows = chord_rows(&sample_dict(), &qwerty_keys_by_bit(), "quick");
        assert_eq!(rows[0].stroke, "KWEUG");
        assert_eq!(rows[0].keys, ["S", "D", "N", "M", "K"]);
        assert_eq!(rows[0].sounds, "K- (k), W- (w), -E (e), -U (u), -G (g)");
    }

    #[test]
    fn word_uses_the_fewest_strokes() {
        // The one-stroke form beats the two-stroke "jump" + "ed" form
        let rows = chord_rows(&sample_dict(), &qwerty_keys_by_bit(), "jumped");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stroke, render_stroke(parse_stroke("SKWR*UPLD").unwrap()));
    }

    #[test]
    fn unknown_word_gets_a_no_stroke_row() {
        let rows = chord_rows(&sample_dict(), &qwerty_keys_by_bit(), "zzyzx");
        assert_eq!(rows[0].stroke, "no stroke");
        assert!(rows[0].keys.is_empty());
    }

    #[test]
    fn key_labels_show_punctuation_as_characters() {
        assert_eq!(key_label(KeyCode::KC_A), "A");
        assert_eq!(key_label(KeyCode::KC_COMM), ",");
        assert_eq!(key_label(KeyCode::KC_SCLN), ";");
        assert_eq!(key_label(KeyCode::KC_QUOT), "'");
    }
}
