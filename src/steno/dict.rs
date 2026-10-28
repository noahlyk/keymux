//! Plover-format dictionaries: JSON objects mapping stroke sequences to translations.
//!
//! Keys are one or more strokes separated by `/` (e.g. `KPA/TKAOEU`), with number keys as digits
//! (`1-9`) and a leading slash for a word boundary (`/S`). Values are Plover's translation
//! language and are kept as written: they are formatted when a stroke is typed, because what
//! one means depends on what came before it (see [`super::formatter`]). A value of
//! `{plover:deleted}` hides the same outline in every dictionary after this one.
//!
//! Besides `.json`, a dictionary can be an `.rtf` file (RTF/CRE, see [`super::rtf`]), and the
//! word list that orthography needs (`american_english_words.txt`) is read from the same place.

use super::formatter::plain_text;
use super::layout::parse_strokes;
use super::orthography::{Orthography, WORDLIST_FILE};
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Bumped when the cached layout changes, so older cache files are ignored.
const CACHE_LAYOUT: u32 = 3;

/// The value that deletes an entry of a dictionary after this one.
pub const DELETED: &str = "{plover:deleted}";

/// Whether a translation is the deletion marker, in any letter case.
#[must_use]
pub const fn is_deleted(translation: &str) -> bool {
    translation.eq_ignore_ascii_case(DELETED)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Dictionary {
    /// Each outline and its translation as written, deletions included
    entries: FxHashMap<Vec<u32>, String>,
    /// Stroke sequences grouped by the text they type, for reverse lookup
    by_text: FxHashMap<String, Vec<Vec<u32>>>,
    max_len: usize,
    /// How suffixes are spelled: Plover's rules and, when there is one, its word list
    orthography: Orthography,
}

/// The file a cache was built from: its path, and the sha256 of its contents when it was read.
pub type Source = (String, String);

/// The path and content hash of each file, which says whether a dictionary built from them
/// is still current. A file that can't be read counts as `missing`.
#[must_use]
pub fn fingerprint(paths: &[impl AsRef<Path>]) -> Vec<Source> {
    paths
        .iter()
        .map(|path| {
            let path = path.as_ref();
            let digest = fs::read(path).map_or_else(
                |_| "missing".to_string(),
                |bytes| format!("{:x}", Sha256::digest(&bytes)),
            );
            (path.display().to_string(), digest)
        })
        .collect()
}

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
    pub invalid: usize,
}

/// Read bytes as text the way Plover reads a dictionary: UTF-8, or failing that Latin-1.
#[must_use]
pub fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| bytes.iter().map(|&b| char::from(b)).collect())
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

impl Dictionary {
    /// Load dictionaries in priority order. Earlier files win on conflicts.
    /// A file that can't be read is logged and skipped rather than failing the layer.
    #[must_use]
    pub fn load(paths: &[impl AsRef<Path>]) -> Self {
        Self::load_reporting(paths).0
    }

    /// Like `load`, and also returns what went wrong with each file that was skipped.
    #[must_use]
    pub fn load_reporting(paths: &[impl AsRef<Path>]) -> (Self, Vec<String>) {
        let mut dict = Self::default();
        let mut problems = Vec::new();
        for path in paths {
            let path = path.as_ref();
            match dict.merge_file(path) {
                Ok(stats) => info!(
                    "steno dictionary {}: {} entries ({} invalid skipped)",
                    path.display(),
                    stats.loaded,
                    stats.invalid
                ),
                Err(e) => {
                    warn!("steno dictionary skipped: {e:#}");
                    problems.push(format!("{e:#}"));
                }
            }
        }
        (dict, problems)
    }

    /// Merge one file: a JSON or RTF/CRE dictionary, or the orthography word list.
    pub fn merge_file(&mut self, path: &Path) -> Result<MergeStats> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name().and_then(|n| n.to_str());
        if name == Some(WORDLIST_FILE) {
            let words = Orthography::from_wordlist(&decode_text(&bytes));
            // The first word list found is the one used
            if !self.orthography.has_words() {
                self.orthography = words;
            }
            return Ok(MergeStats::default());
        }
        if has_extension(path, "rtf") {
            return self.merge_rtf(&bytes);
        }
        self.merge_json(&decode_text(&bytes))
    }

    /// Merge one Plover JSON dictionary. Entries already present are kept.
    pub fn merge_json(&mut self, json: &str) -> Result<MergeStats> {
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).context("dictionary is not a JSON object")?;
        let mut stats = MergeStats::default();
        // Within one file the last of two spellings of the same outline wins, as in Plover
        let mut entries: FxHashMap<Vec<u32>, String> = FxHashMap::default();
        for (key, value) in raw {
            match (parse_strokes(&key), value.as_str()) {
                (Some(strokes), Some(text)) => {
                    entries.insert(strokes, text.to_string());
                }
                _ => stats.invalid += 1,
            }
        }
        for (strokes, text) in entries {
            if self.add(strokes, text) {
                stats.loaded += 1;
            }
        }
        Ok(stats)
    }

    /// Merge the entries of one RTF/CRE dictionary.
    pub fn merge_rtf(&mut self, bytes: &[u8]) -> Result<MergeStats> {
        let mut stats = MergeStats::default();
        let mut entries: FxHashMap<Vec<u32>, String> = FxHashMap::default();
        for (outline, translation) in super::rtf::parse_rtf(bytes)? {
            match parse_strokes(&outline) {
                Some(strokes) => {
                    entries.insert(strokes, translation);
                }
                None => stats.invalid += 1,
            }
        }
        for (strokes, text) in entries {
            if self.add(strokes, text) {
                stats.loaded += 1;
            }
        }
        Ok(stats)
    }

    /// Add an entry unless an earlier dictionary already has one for the outline.
    fn add(&mut self, strokes: Vec<u32>, text: String) -> bool {
        let Entry::Vacant(slot) = self.entries.entry(strokes.clone()) else {
            return false;
        };
        self.max_len = self.max_len.max(strokes.len());
        if !is_deleted(&text) {
            if let Some(typed) = plain_text(&text) {
                self.by_text.entry(typed).or_default().push(strokes);
            }
        }
        slot.insert(text);
        true
    }

    /// The translation of an exact outline, as written. `None` if there is none, or the
    /// outline was deleted.
    #[must_use]
    pub fn lookup(&self, strokes: &[u32]) -> Option<&str> {
        self.entries
            .get(strokes)
            .map(String::as_str)
            .filter(|translation| !is_deleted(translation))
    }

    /// Use this orthography, as if its word list had been read.
    pub fn set_orthography(&mut self, orthography: Orthography) {
        self.orthography = orthography;
    }

    /// How suffixes are spelled.
    #[must_use]
    pub const fn orthography(&self) -> &Orthography {
        &self.orthography
    }

    /// The length in strokes of the longest outline.
    #[must_use]
    pub const fn longest_key(&self) -> usize {
        self.max_len
    }

    /// Every stroke sequence whose translation is exactly `text`, ignoring the
    /// leading space that spacing adds. Case matters.
    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        let mut found = self.by_text.get(text).cloned().unwrap_or_default();
        found.sort();
        found
    }

    /// Words that start with `prefix` but are longer than it, shortest first and then
    /// alphabetical, at most `limit`. Only plain words count (letters, digits, `'` and `-`), so
    /// punctuation and command-like entries don't crowd the list. Case matters.
    ///
    /// Scans every distinct translation, so keep `prefix` to a couple of characters or more.
    #[must_use]
    pub fn completions(&self, prefix: &str, limit: usize) -> Vec<&str> {
        let mut found: Vec<&str> = self
            .by_text
            .keys()
            .map(String::as_str)
            .filter(|text| {
                text.len() > prefix.len()
                    && text.starts_with(prefix)
                    && text.chars().all(|c| c.is_alphanumeric() || c == '\'' || c == '-')
            })
            .collect();
        found.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        found.truncate(limit);
        found
    }

    /// Like `load`, but restores a binary copy from `cache_dir` when every file still has the
    /// contents it had when the copy was made. Otherwise the JSON is parsed and a new copy saved.
    #[must_use]
    pub fn load_cached(paths: &[impl AsRef<Path>], cache_dir: &Path) -> Self {
        let names: Vec<String> = paths.iter().map(|path| path.as_ref().display().to_string()).collect();
        Self::load_cached_reporting(paths, &fingerprint(paths), cache_dir, &cache_name(&names)).0
    }

    /// Like `load_cached`, for files whose `fingerprint` is already known, kept under `name`
    /// (see [`cache_name`]). Also returns what went wrong with each file that couldn't be
    /// read. Nothing is cached then, so a file caught mid-save is never remembered as the
    /// real thing.
    #[must_use]
    pub fn load_cached_reporting(
        paths: &[impl AsRef<Path>],
        sources: &[Source],
        cache_dir: &Path,
        name: &str,
    ) -> (Self, Vec<String>) {
        let file = cache_dir.join(format!("{name}.bin"));
        if let Some(dict) = read_cache(&file, sources) {
            info!("steno dictionaries restored from {}", file.display());
            return (dict, Vec::new());
        }
        let (dict, problems) = Self::load_reporting(paths);
        if problems.is_empty() {
            write_cache(&file, sources, &dict);
        }
        (dict, problems)
    }

    /// Every entry that is in force, for listing and search.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u32>, &String)> {
        self.entries.iter().filter(|(_, translation)| !is_deleted(translation))
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

/// The name of the cache file for a dictionary made from `sources`.
///
/// That is the folders and files it was asked for, not the files found inside them. Adding,
/// removing or editing a dictionary then writes over the one cache file instead of leaving a
/// copy of every version behind.
#[must_use]
pub fn cache_name(sources: &[String]) -> String {
    let mut hasher = Sha256::new();
    for source in sources {
        hasher.update(source.as_bytes());
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
        .map_or(Ok(()), super::setup::create_dir_owned)
        .and_then(|()| fs::write(&tmp, &bytes))
        .and_then(|()| fs::rename(&tmp, file));
    if let Err(e) = saved {
        warn!("steno cache {}: {e}", file.display());
        return;
    }
    if let Some(dir) = file.parent() {
        super::setup::hand_to_owner_of_parent(dir);
        super::setup::hand_to_owner_of_parent(file);
        prune_cache(dir, file);
    }
}

/// Copies kept in a cache folder, newest first. Each set of dictionary files has its own copy,
/// so a few cover every layer; beyond that, they are copies of sets no longer in use.
const CACHE_KEPT: usize = 6;

/// Delete all but the newest `CACHE_KEPT` copies, never the one just written (`keep`).
fn prune_cache(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut copies: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "bin"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    copies.sort_by_key(|copy| std::cmp::Reverse(copy.0));
    for (_, path) in copies.into_iter().skip(CACHE_KEPT) {
        if path != keep {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);
    const KPA: u32 = (1 << 2) | (1 << 3) | (1 << 7);
    const TKAOEU: u32 = (1 << 1) | (1 << 2) | (1 << 7) | (1 << 8) | (1 << 10) | (1 << 11);

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "the JSON holds Plover commands in braces, not a format string"
    )]
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
        assert_eq!(dict.lookup(&[KAT]), Some("cat"));
        assert_eq!(dict.lookup(&[KPA, TKAOEU]), Some("example"));
        // Commands are entries like any other: they are run when the stroke is typed
        assert!(dict.lookup(&[parse_strokes("-PB").unwrap()[0]]).is_some());
        assert_eq!(
            stats,
            MergeStats {
                loaded: 3,
                invalid: 2,
            }
        );
    }

    #[test]
    fn a_deleted_entry_hides_the_same_outline_in_later_dictionaries() {
        let mut dict = Dictionary::default();
        dict.merge_json(&format!(r#"{{"KAT": "{DELETED}"}}"#)).unwrap();
        dict.merge_json(r#"{"KAT": "cat", "TKOG": "dog"}"#).unwrap();
        assert_eq!(dict.lookup(&[KAT]), None);
        assert!(dict.strokes_for("cat").is_empty(), "a deleted entry has no reverse lookup either");
        assert_eq!(dict.iter().count(), 1);
        // In any letter case
        let mut upper = Dictionary::default();
        upper.merge_json(&format!(r#"{{"KAT": "{}"}}"#, DELETED.to_uppercase())).unwrap();
        assert_eq!(upper.lookup(&[KAT]), None);
    }

    #[test]
    fn number_keys_and_the_word_boundary_stroke_are_read_in_keys() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"1": "one", "1-9": "x", "/S": "{prefix^}"}"#).unwrap();
        assert_eq!(dict.lookup(&[parse_strokes("1").unwrap()[0]]), Some("one"));
        assert_eq!(dict.lookup(&[0, parse_strokes("S").unwrap()[0]]), Some("{prefix^}"));
        assert_eq!(dict.longest_key(), 2);
    }

    #[test]
    fn within_one_file_the_later_of_two_spellings_of_an_outline_wins() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "first", "KA-T": "second"}"#).unwrap();
        assert_eq!(dict.lookup(&[KAT]), Some("second"));
    }

    #[test]
    fn latin_1_files_are_read_like_plover_reads_them() {
        assert_eq!(decode_text("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(decode_text(&[b'c', b'a', b'f', 0xe9]), "caf\u{e9}");
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
    fn adding_and_removing_dictionaries_rewrites_one_cache_file_instead_of_piling_up() {
        let (dir, file, cache) = cache_fixture("one-file", r#"{"KAT": "cat"}"#);
        let extra = dir.join("extra.json");
        fs::write(&extra, r#"{"TKOG": "dog"}"#).unwrap();
        let asked = cache_name(&[dir.display().to_string()]);
        let load = |files: &[&Path]| {
            Dictionary::load_cached_reporting(files, &fingerprint(files), &cache, &asked).0
        };
        // The same folder, as its files come and go: always the one cache file
        let _ = load(&[&file]);
        let both = load(&[&file, &extra]);
        assert_eq!(both.strokes_for("dog"), vec![vec![(1 << 1) | (1 << 2) | (1 << 8) | (1 << 17)]]);
        let _ = load(&[&extra]);
        let _ = load(&[&file, &extra]);
        assert_eq!(cache_files(&cache), 1);
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
        assert_eq!(dict.lookup(&[KAT]), Some("kitty"));
    }

    #[test]
    fn an_rtf_dictionary_loads_like_a_json_one_and_the_word_list_from_the_same_place() {
        let dir = std::env::temp_dir().join(format!("keymux-dict-rtf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let rtf = dir.join("pro.rtf");
        fs::write(
            &rtf,
            "{\\rtf1\\ansi\\cxdict\r\n{\\*\\cxs KAT}cat\r\n{\\*\\cxs -S}\\cxds s\r\n}",
        )
        .unwrap();
        let json = dir.join("mine.json");
        fs::write(&json, r#"{"KAT": "kitty", "TKOG": "dog"}"#).unwrap();
        let words = dir.join(WORDLIST_FILE);
        fs::write(&words, "cats 5\ncarries 9\n").unwrap();

        // The first dictionary wins, whichever format it is
        let dict = Dictionary::load(&[json.as_path(), rtf.as_path(), words.as_path()]);
        assert_eq!(dict.lookup(&[KAT]), Some("kitty"));
        assert_eq!(dict.lookup(&[parse_strokes("-S").unwrap()[0]]), Some("{^s}"));
        assert_eq!(dict.lookup(&[parse_strokes("TKOG").unwrap()[0]]), Some("dog"));
        assert!(dict.orthography().has_words());
        assert_eq!(dict.orthography().add_suffix("cat", "s"), "cats");

        let other_way = Dictionary::load(&[rtf.as_path(), json.as_path()]);
        assert_eq!(other_way.lookup(&[KAT]), Some("cat"));
        assert!(!other_way.orthography().has_words());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_skipped() {
        let dict = Dictionary::load(&[Path::new("/nonexistent/steno/main.json")]);
        assert!(dict.is_empty());
    }
}
