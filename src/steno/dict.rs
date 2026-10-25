//! Plover-format dictionaries: JSON objects mapping stroke sequences to text.
//!
//! Keys are one or more strokes separated by `/` (e.g. `KPA/TKAOEU`). Values are
//! parsed once at load time. Entries using commands we don't support are counted
//! and skipped.

use super::format::{parse_pieces, render, FormatState, Piece};
use super::layout::parse_stroke;
use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Bumped when the cached layout changes, so older cache files are ignored.
const CACHE_LAYOUT: u32 = 2;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Dictionary {
    entries: FxHashMap<Vec<u32>, Vec<Piece>>,
    /// Stroke sequences grouped by the text they type, for reverse lookup
    by_text: FxHashMap<String, Vec<Vec<u32>>>,
    max_len: usize,
    /// The first stroke of every entry longer than one stroke. A stroke in this set
    /// is incomplete on its own: more strokes may still finish an entry it starts.
    openers: FxHashSet<u32>,
}

/// The file a cache was built from: its path, and the sha256 of its contents when it was read.
type Source = (String, String);

/// What a cache file holds. Written borrowed, read owned.
#[derive(Deserialize)]
struct CacheIn {
    version: String,
    sources: Vec<Source>,
    dict: Dictionary,
}

#[derive(Serialize)]
struct CacheOut<'a> {
    version: &'a str,
    sources: &'a [Source],
    dict: &'a Dictionary,
}

/// Changes whenever the cache layout or the parser does, so a stale cache is never trusted.
fn cache_version() -> String {
    format!("{CACHE_LAYOUT}-{}", env!("CARGO_PKG_VERSION"))
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
                if strokes.len() > 1 {
                    self.openers.insert(strokes[0]);
                }
                let text = render(&pieces, &mut FormatState::default())
                    .trim_start()
                    .to_string();
                self.by_text.entry(text).or_default().push(strokes);
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

    /// Whether some entry longer than one stroke starts with `stroke`.
    #[must_use]
    pub fn opens_longer_entry(&self, stroke: u32) -> bool {
        self.openers.contains(&stroke)
    }

    /// Every stroke sequence whose translation is exactly `text`, ignoring the
    /// leading space that spacing adds. Case matters.
    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        let mut found = self.by_text.get(text).cloned().unwrap_or_default();
        found.sort();
        found
    }

    /// Like `load`, but restores a binary copy from `cache_dir` when every file still has the
    /// contents it had when the copy was made. Otherwise the JSON is parsed and a new copy saved.
    #[must_use]
    pub fn load_cached(paths: &[impl AsRef<Path>], cache_dir: &Path) -> Self {
        let sources: Vec<Source> = paths
            .iter()
            .map(|path| {
                let path = path.as_ref();
                let digest = fs::read(path).map_or_else(
                    |_| "missing".to_string(),
                    |bytes| format!("{:x}", Sha256::digest(&bytes)),
                );
                (path.display().to_string(), digest)
            })
            .collect();
        let file = cache_dir.join(format!("{}.bin", cache_name(&sources)));
        if let Some(dict) = read_cache(&file, &sources) {
            info!("steno dictionaries restored from {}", file.display());
            return dict;
        }
        let dict = Self::load(paths);
        write_cache(&file, &sources, &dict);
        dict
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

/// A stable name for the cache of these files: a hash of their paths, not their contents,
/// so an edited file overwrites its old cache instead of leaving it behind.
fn cache_name(sources: &[Source]) -> String {
    let mut hasher = Sha256::new();
    for (path, _) in sources {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// The cached dictionary, if it was built from files with these exact contents.
fn read_cache(file: &Path, sources: &[Source]) -> Option<Dictionary> {
    let bytes = fs::read(file).ok()?;
    let cache: CacheIn = bincode::deserialize(&bytes).ok()?;
    (cache.version == cache_version() && cache.sources == sources).then_some(cache.dict)
}

/// Save the dictionary for these files. A cache that can't be written only costs time next
/// start, so failures are logged and not returned.
fn write_cache(file: &Path, sources: &[Source], dict: &Dictionary) {
    let version = cache_version();
    let bytes = match bincode::serialize(&CacheOut {
        version: &version,
        sources,
        dict,
    }) {
        Ok(bytes) => bytes,
        Err(e) => return warn!("steno cache not saved: {e}"),
    };
    let tmp: PathBuf = file.with_extension("bin.part");
    let saved = file
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&tmp, &bytes))
        .and_then(|()| fs::rename(&tmp, file));
    if let Err(e) = saved {
        warn!("steno cache {}: {e}", file.display());
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

    /// A fresh temp directory holding one dictionary file with `json`, plus its cache directory.
    fn cache_fixture(name: &str, json: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("keymux-dict-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.json");
        fs::write(&file, json).unwrap();
        (dir.clone(), file, dir.join("cache"))
    }

    fn cache_files(cache: &Path) -> usize {
        fs::read_dir(cache).map_or(0, |dir| dir.count())
    }

    #[test]
    fn a_cached_dictionary_is_restored_until_its_file_changes() {
        let (dir, file, cache) = cache_fixture("restore", r#"{"KAT": "cat"}"#);
        let first = Dictionary::load_cached(&[&file], &cache);
        assert_eq!(first.strokes_for("cat"), vec![vec![KAT]]);
        assert_eq!(cache_files(&cache), 1, "the first load saves a cache");

        // Unchanged file: the cache is used, so the same answer comes back
        let restored = Dictionary::load_cached(&[&file], &cache);
        assert_eq!(restored.strokes_for("cat"), vec![vec![KAT]]);
        assert_eq!(
            cache_files(&cache),
            1,
            "the cache is reused, not duplicated"
        );

        // Changed file: the fingerprint no longer matches, so the JSON is read again
        fs::write(&file, r#"{"KAT": "dog"}"#).unwrap();
        let changed = Dictionary::load_cached(&[&file], &cache);
        assert_eq!(changed.strokes_for("dog"), vec![vec![KAT]]);
        assert!(changed.strokes_for("cat").is_empty());
        assert_eq!(
            cache_files(&cache),
            1,
            "the changed file overwrites its cache"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_is_replaced_by_parsing_the_json() {
        let (dir, file, cache) = cache_fixture("corrupt", r#"{"KAT": "cat"}"#);
        let _ = Dictionary::load_cached(&[&file], &cache);
        for entry in fs::read_dir(&cache).unwrap() {
            fs::write(entry.unwrap().path(), b"not a dictionary").unwrap();
        }
        let dict = Dictionary::load_cached(&[&file], &cache);
        assert_eq!(dict.strokes_for("cat"), vec![vec![KAT]]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strokes_for_finds_every_outline_through_the_word_index() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KAT/-PB": "cat", "KWRAT": "{^}cat"}"#)
            .unwrap();
        // The leading space that spacing adds is ignored, and the index covers every entry
        assert_eq!(dict.strokes_for("cat").len(), 3);
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
