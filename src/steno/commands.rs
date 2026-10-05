//! `keymux steno` commands that read or edit dictionaries, and show the tape.

use super::dict::Dictionary;
use super::format::{parse_pieces, render, FormatState};
use super::layout::{parse_stroke, render_stroke};
use super::numbers::number_text;
use super::setup::{default_dictionaries, steno_dir};
use super::tape;
use anyhow::{bail, Context, Result};
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
}
