//! Plover-format dictionaries: JSON objects mapping stroke sequences to text.
//!
//! Keys are one or more strokes separated by `/` (e.g. `KPA/TKAOEU`). Values are
//! parsed once at load time. Entries using commands we don't support are counted
//! and skipped.

use super::format::{parse_pieces, render, FormatState, Piece};
use super::layout::parse_stroke;
use anyhow::{Context, Result};
use std::collections::{hash_map::Entry, HashMap};
use std::path::Path;
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct Dictionary {
    entries: HashMap<Vec<u32>, Vec<Piece>>,
    max_len: usize,
}

/// How many entries a dictionary file had that we couldn't use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub loaded: usize,
    pub unsupported: usize,
    pub invalid: usize,
}

impl Dictionary {
    /// Load dictionaries in priority order. Earlier files win on conflicts.
    /// A file that can't be read is logged and skipped rather than failing the layer.
    #[must_use]
    pub fn load(paths: &[impl AsRef<Path>]) -> Self {
        let mut dict = Self::default();
        for path in paths {
            let path = path.as_ref();
            match std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))
                .and_then(|text| dict.merge_json(&text))
            {
                Ok(stats) => info!(
                    "steno dictionary {}: {} entries ({} unsupported, {} invalid skipped)",
                    path.display(),
                    stats.loaded,
                    stats.unsupported,
                    stats.invalid
                ),
                Err(e) => warn!("steno dictionary skipped: {e:#}"),
            }
        }
        dict
    }

    /// Merge one Plover JSON dictionary. Entries already present are kept.
    pub fn merge_json(&mut self, json: &str) -> Result<MergeStats> {
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).context("dictionary is not a JSON object")?;

        let mut stats = MergeStats::default();
        for (key, value) in raw {
            let (Some(strokes), Some(text)) = (parse_strokes(&key), value.as_str()) else {
                stats.invalid += 1;
                continue;
            };
            let Some(pieces) = parse_pieces(text) else {
                stats.unsupported += 1;
                continue;
            };
            if let Entry::Vacant(slot) = self.entries.entry(strokes.clone()) {
                self.max_len = self.max_len.max(strokes.len());
                slot.insert(pieces);
                stats.loaded += 1;
            }
        }
        Ok(stats)
    }

    /// The pieces for an exact stroke sequence.
    #[must_use]
    pub fn get(&self, strokes: &[u32]) -> Option<&[Piece]> {
        self.entries.get(strokes).map(Vec::as_slice)
    }

    /// Every stroke sequence whose translation is exactly `text`, ignoring the
    /// leading space that spacing adds. Case matters.
    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        let mut found: Vec<Vec<u32>> = self
            .entries
            .iter()
            .filter(|(_, pieces)| render(pieces, &mut FormatState::default()).trim_start() == text)
            .map(|(strokes, _)| strokes.clone())
            .collect();
        found.sort();
        found
    }

    /// Every entry, for listing and search.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u32>, &Vec<Piece>)> {
        self.entries.iter()
    }

    /// Length in strokes of the longest entry.
    #[must_use]
    pub const fn max_len(&self) -> usize {
        self.max_len
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parse `KPA/TKAOEU` into stroke bitmasks. `None` if any part isn't a stroke.
fn parse_strokes(key: &str) -> Option<Vec<u32>> {
    key.split('/').map(parse_stroke).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);
    const KPA: u32 = (1 << 2) | (1 << 3) | (1 << 7);
    const TKAOEU: u32 = (1 << 1) | (1 << 2) | (1 << 7) | (1 << 8) | (1 << 10) | (1 << 11);

    #[test]
    fn loads_entries_and_counts_the_rest() {
        let json = r#"{
            "KAT": "cat",
            "KPA/TKAOEU": "example",
            "-PB": "{PLOVER:ADD_TRANSLATION}",
            "not a stroke": "x",
            "TK": 5
        }"#;
        let mut dict = Dictionary::default();
        let stats = dict.merge_json(json).unwrap();
        assert!(dict.get(&[KAT]).is_some());
        assert!(dict.get(&[KPA, TKAOEU]).is_some());
        assert_eq!(
            stats,
            MergeStats {
                loaded: 2,
                unsupported: 1,
                invalid: 2,
            }
        );
    }

    #[test]
    fn longest_entry_length_is_tracked() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KPA/TKAOEU": "example"}"#).unwrap();
        assert_eq!(dict.max_len(), 2);
    }

    #[test]
    fn strokes_for_finds_every_stroke_sequence_for_a_word() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#)
            .unwrap();
        assert_eq!(dict.strokes_for("cat"), vec![vec![KAT]]);
        assert_eq!(dict.strokes_for("example"), vec![vec![KPA, TKAOEU]]);
        assert!(dict.strokes_for("dog").is_empty());
    }

    #[test]
    fn earlier_merges_win() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "kitty"}"#).unwrap();
        dict.merge_json(r#"{"KAT": "cat"}"#).unwrap();
        assert_eq!(
            dict.get(&[KAT]),
            Some(&[Piece::Text("kitty".to_string())][..])
        );
    }

    #[test]
    fn missing_file_is_skipped() {
        let dict = Dictionary::load(&[Path::new("/nonexistent/steno/main.json")]);
        assert!(dict.is_empty());
    }
}
