//! Plover-format dictionaries: JSON objects mapping stroke strings to text.
//!
//! Only single-stroke entries with plain text are loaded for now. Multi-stroke
//! entries (keys containing `/`) and entries with formatting commands (`{...}`)
//! are counted and skipped.

use super::layout::parse_stroke;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct Dictionary {
    entries: HashMap<u32, String>,
}

/// How many entries a dictionary file had that we couldn't use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub loaded: usize,
    pub multi_stroke: usize,
    pub formatted: usize,
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
                    "steno dictionary {}: {} entries ({} multi-stroke, {} formatted, {} invalid skipped)",
                    path.display(),
                    stats.loaded,
                    stats.multi_stroke,
                    stats.formatted,
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
        for (stroke, value) in raw {
            if stroke.contains('/') {
                stats.multi_stroke += 1;
                continue;
            }
            let (Some(bits), Some(text)) = (parse_stroke(&stroke), value.as_str()) else {
                stats.invalid += 1;
                continue;
            };
            if text.contains('{') {
                stats.formatted += 1;
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = self.entries.entry(bits) {
                slot.insert(text.to_string());
                stats.loaded += 1;
            }
        }
        Ok(stats)
    }

    #[must_use]
    pub fn lookup(&self, stroke: u32) -> Option<&str> {
        self.entries.get(&stroke).map(String::as_str)
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

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);

    #[test]
    fn loads_single_strokes_and_counts_the_rest() {
        let json = r#"{
            "KAT": "cat",
            "KPA/TKAOEU": "example",
            "-PB": "{^}and{^}",
            "not a stroke": "x",
            "TK": 5
        }"#;
        let mut dict = Dictionary::default();
        let stats = dict.merge_json(json).unwrap();
        assert_eq!(dict.lookup(KAT), Some("cat"));
        assert_eq!(
            stats,
            MergeStats {
                loaded: 1,
                multi_stroke: 1,
                formatted: 1,
                invalid: 2,
            }
        );
    }

    #[test]
    fn earlier_merges_win() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "kitty"}"#).unwrap();
        dict.merge_json(r#"{"KAT": "cat"}"#).unwrap();
        assert_eq!(dict.lookup(KAT), Some("kitty"));
    }

    #[test]
    fn missing_file_is_skipped() {
        let dict = Dictionary::load(&[Path::new("/nonexistent/steno/main.json")]);
        assert!(dict.is_empty());
    }
}
