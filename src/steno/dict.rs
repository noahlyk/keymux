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
use super::layout::Layout;
use super::orthography::{Orthography, WORDLIST_FILE};
use super::parts::Parts;
use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::warn;

/// Bumped when the cached layout changes, so older cache files are ignored.
const CACHE_LAYOUT: u32 = 4;

/// The value that deletes an entry of a dictionary after this one.
pub const DELETED: &str = "{plover:deleted}";

/// Whether a translation is the deletion marker, in any letter case.
const fn is_deleted(translation: &str) -> bool {
    translation.eq_ignore_ascii_case(DELETED)
}

/// What one file holds, parsed: the unit that is cached and shared.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Table {
    /// Each outline and its translation as written, deletions included
    entries: FxHashMap<Vec<u32>, String>,
    /// Stroke sequences grouped by the text they type, for reverse lookup
    by_text: FxHashMap<String, Vec<Vec<u32>>>,
    max_len: usize,
    /// How suffixes are spelled: Plover's rules and, when there is one, its word list
    orthography: Orthography,
}

/// Dictionary entries, as one or more layers of parsed tables, highest priority first.
///
/// One file parsed on its own is one table. A dictionary made of many files
/// ([`Self::layered`]) is those tables side by side, shared and not copied, so putting a changed
/// file's new table in place of its old one costs nothing in proportion to the other files. An
/// outline means what the first table that has it says, a deletion included.
#[derive(Debug, Clone)]
pub struct Dictionary {
    tables: Vec<Arc<Table>>,
}

impl Default for Dictionary {
    fn default() -> Self {
        Self {
            tables: vec![Arc::new(Table::default())],
        }
    }
}

/// The file a cache was built from: its path, and the sha256 of its contents when it was read.
pub type Source = (String, String);

/// What a cache file holds. Written borrowed, read owned.
#[derive(Deserialize)]
struct CacheIn {
    version: String,
    sources: Vec<Source>,
    dict: Table,
}

#[derive(Serialize)]
struct CacheOut<'a> {
    version: &'a str,
    sources: &'a [Source],
    dict: &'a Table,
}

/// Changes whenever the cache layout, the parser, or the steno [`Layout`] the files are parsed
/// against does, so a stale cache - or one from a theory with different extra keys - is never
/// trusted.
fn cache_version(layout: &Layout) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    layout.hash(&mut hasher);
    format!(
        "{CACHE_LAYOUT}-{}-{:x}",
        env!("CARGO_PKG_VERSION"),
        hasher.finish()
    )
}

/// How many entries a dictionary file had that we couldn't use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub loaded: usize,
    pub invalid: usize,
}

/// Read bytes as text the way Plover reads a dictionary: UTF-8, or failing that Latin-1.
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|_| bytes.iter().map(|&b| char::from(b)).collect())
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

impl Dictionary {
    /// Load files in priority order, earlier ones winning, skipping any that can't be read: for
    /// tests over files written on the spot. The daemon loads through [`Parts`].
    #[cfg(test)]
    pub(crate) fn load(paths: &[impl AsRef<Path>], layout: &Layout) -> Self {
        let mut dict = Self::default();
        for path in paths {
            if let Err(e) = dict.merge_file(path.as_ref(), layout) {
                warn!("steno dictionary skipped: {e:#}");
            }
        }
        dict
    }

    /// Merge one file: a JSON or RTF/CRE dictionary, or the orthography word list.
    pub fn merge_file(&mut self, path: &Path, layout: &Layout) -> Result<MergeStats> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        self.merge_bytes(path, &bytes, layout)
    }

    /// Like [`Self::merge_file`], for the contents of `path` already read.
    pub fn merge_bytes(&mut self, path: &Path, bytes: &[u8], layout: &Layout) -> Result<MergeStats> {
        let name = path.file_name().and_then(|n| n.to_str());
        if name == Some(WORDLIST_FILE) {
            let words = Orthography::from_wordlist(&decode_text(bytes));
            // The first word list found is the one used
            if !self.orthography().has_words() {
                self.own().orthography = words;
            }
            return Ok(MergeStats::default());
        }
        if has_extension(path, "rtf") {
            return self.merge_rtf(bytes, layout);
        }
        self.merge_json(&decode_text(bytes), layout)
            .with_context(|| path.display().to_string())
    }

    /// Merge one Plover JSON dictionary. Entries already present are kept.
    pub fn merge_json(&mut self, json: &str, layout: &Layout) -> Result<MergeStats> {
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).context("dictionary is not a JSON object")?;
        let mut stats = MergeStats::default();
        // Within one file the last of two spellings of the same outline wins, as in Plover
        let mut entries: FxHashMap<Vec<u32>, String> = FxHashMap::default();
        for (key, value) in raw {
            match (layout.parse_strokes(&key), value.as_str()) {
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
    pub fn merge_rtf(&mut self, bytes: &[u8], layout: &Layout) -> Result<MergeStats> {
        let mut stats = MergeStats::default();
        let mut entries: FxHashMap<Vec<u32>, String> = FxHashMap::default();
        for (outline, translation) in super::rtf::parse_rtf(bytes)? {
            match layout.parse_strokes(&outline) {
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

    /// One dictionary out of several already-parsed ones, highest priority first: an outline
    /// means what the first of them that has it says (a deletion included), and the first word
    /// list found sets the orthography. Nothing is copied: the result shares the others' tables,
    /// so a folder reloaded after one file changed costs that file, not all of them.
    #[must_use]
    pub fn layered<'a>(parts: impl IntoIterator<Item = &'a Self>) -> Self {
        Self {
            tables: parts
                .into_iter()
                .flat_map(|part| part.tables.iter().cloned())
                .collect(),
        }
    }

    /// The table new entries go into: the last, which is the only one a dictionary built entry
    /// by entry has.
    fn own(&mut self) -> &mut Table {
        if self.tables.is_empty() {
            self.tables.push(Arc::new(Table::default()));
        }
        let last = self.tables.len() - 1;
        Arc::make_mut(&mut self.tables[last])
    }

    /// Whether any table has an entry (a deletion counts) for the outline.
    fn has_entry(&self, strokes: &[u32]) -> bool {
        self.tables.iter().any(|table| table.entries.contains_key(strokes))
    }

    /// Add one entry by its outline bits directly, bypassing stroke-notation parsing - for a
    /// caller that already has `strokes`, such as one built with a theory-specific
    /// [`super::layout::Layout`] a plain dictionary file's text can't express on its own
    /// (`merge_json`/`merge_file` always parse against keymux's own default Plover layout).
    /// Unlike `merge_json`, an existing entry for the same outline is replaced, not kept.
    pub fn insert(&mut self, strokes: Vec<u32>, text: impl Into<String>) {
        self.own().entries.remove(&strokes);
        self.add(strokes, text.into());
    }

    /// Add an entry unless an earlier dictionary already has one for the outline.
    fn add(&mut self, strokes: Vec<u32>, text: String) -> bool {
        if self.has_entry(&strokes) {
            return false;
        }
        let own = self.own();
        own.max_len = own.max_len.max(strokes.len());
        if !is_deleted(&text) {
            if let Some(typed) = plain_text(&text) {
                own.by_text.entry(typed).or_default().push(strokes.clone());
            }
        }
        own.entries.insert(strokes, text);
        true
    }

    /// The translation of an exact outline, as written. `None` if there is none, or the
    /// outline was deleted.
    #[must_use]
    pub fn lookup(&self, strokes: &[u32]) -> Option<&str> {
        self.tables
            .iter()
            .find_map(|table| table.entries.get(strokes))
            .map(String::as_str)
            .filter(|translation| !is_deleted(translation))
    }

    /// Use this orthography, as if its word list had been read.
    pub fn set_orthography(&mut self, orthography: Orthography) {
        self.own().orthography = orthography;
    }

    /// How suffixes are spelled: the first word list among the tables, else the first table's
    /// plain rules.
    #[must_use]
    pub fn orthography(&self) -> &Orthography {
        static NONE: OnceLock<Orthography> = OnceLock::new();
        self.tables
            .iter()
            .map(|table| &table.orthography)
            .find(|orthography| orthography.has_words())
            .or_else(|| self.tables.first().map(|table| &table.orthography))
            .unwrap_or_else(|| NONE.get_or_init(Orthography::default))
    }

    /// The length in strokes of the longest outline.
    #[must_use]
    pub fn longest_key(&self) -> usize {
        self.tables.iter().map(|table| table.max_len).max().unwrap_or(0)
    }

    /// Every stroke sequence whose translation is exactly `text`, ignoring the
    /// leading space that spacing adds. Case matters.
    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        let mut found = Vec::new();
        for (i, table) in self.tables.iter().enumerate() {
            for strokes in table.by_text.get(text).into_iter().flatten() {
                // An earlier table's entry for the outline, even a deletion, hides this one
                let hidden = self.tables[..i]
                    .iter()
                    .any(|earlier| earlier.entries.contains_key(strokes));
                if !hidden {
                    found.push(strokes.clone());
                }
            }
        }
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
        let candidates: FxHashSet<&str> = self
            .tables
            .iter()
            .flat_map(|table| table.by_text.keys())
            .map(String::as_str)
            .filter(|text| {
                text.len() > prefix.len()
                    && text.starts_with(prefix)
                    && text
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '\'' || c == '-')
            })
            .collect();
        let mut found: Vec<&str> = candidates.into_iter().collect();
        found.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        // A word whose every outline was deleted or replaced by a later dictionary is not a word
        found.retain(|text| !self.strokes_for(text).is_empty());
        found.truncate(limit);
        found
    }

    /// Like `load`, but each file is restored from its own binary copy in `cache_dir` while its
    /// contents are what they were when the copy was made, and parsed (and the copy renewed)
    /// only otherwise: see [`Parts`].
    #[must_use]
    pub fn load_cached(paths: &[impl AsRef<Path>], cache_dir: &Path, layout: &Layout) -> Self {
        let files: Vec<PathBuf> = paths.iter().map(|path| path.as_ref().to_path_buf()).collect();
        let mut parts = Parts::default();
        parts.refresh(&files, cache_dir, layout);
        parts.layered(&files)
    }

    /// Every entry that is in force, for listing and search.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u32>, &String)> {
        self.tables.iter().enumerate().flat_map(move |(i, table)| {
            table.entries.iter().filter(move |(strokes, translation)| {
                !is_deleted(translation)
                    && !self.tables[..i]
                        .iter()
                        .any(|earlier| earlier.entries.contains_key(*strokes))
            })
        })
    }

    /// How many outlines have an entry (deletions included).
    #[must_use]
    pub fn len(&self) -> usize {
        match self.tables.as_slice() {
            [] => 0,
            [only] => only.entries.len(),
            tables => tables
                .iter()
                .enumerate()
                .map(|(i, table)| {
                    table
                        .entries
                        .keys()
                        .filter(|strokes| {
                            !tables[..i].iter().any(|earlier| earlier.entries.contains_key(*strokes))
                        })
                        .count()
                })
                .sum(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.iter().all(|table| table.entries.is_empty())
    }
}

/// The name of the cache file for the dictionary file at `path`.
///
/// One copy per file, named for the place it was read from, so editing a file writes over its own
/// copy instead of leaving one of every version behind.
#[must_use]
pub fn cache_name(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.display().to_string().as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// The cached dictionary, if it was built from files with these exact contents.
pub(super) fn read_cache(file: &Path, sources: &[Source], layout: &Layout) -> Option<Dictionary> {
    let bytes = fs::read(file).ok()?;
    let cache: CacheIn = bincode::deserialize(&bytes).ok()?;
    (cache.version == cache_version(layout) && cache.sources == sources).then(|| Dictionary {
        tables: vec![Arc::new(cache.dict)],
    })
}

/// Save the dictionary for these files. A cache that can't be written only costs time next
/// start, so failures are logged and not returned.
pub(super) fn write_cache(file: &Path, sources: &[Source], dict: &Dictionary, layout: &Layout) {
    // Only a dictionary read from one file is ever cached, and that is one table
    let [table] = dict.tables.as_slice() else {
        return;
    };
    let version = cache_version(layout);
    let bytes = match bincode::serialize(&CacheOut {
        version: &version,
        sources,
        dict: table,
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
    }
}

/// Copies kept in a cache folder, newest first. Each dictionary file has its own copy; beyond
/// this many, they are copies of files no longer in use.
const CACHE_KEPT: usize = 512;

/// Delete the oldest copies in `dir` beyond `CACHE_KEPT`, never one named in `keep`, and any
/// whole-set copy (`*.bin` directly in `legacy`) an older keymux left, which nothing reads now.
pub(super) fn prune_cache(dir: &Path, keep: &[PathBuf], legacy: &Path) {
    if let Ok(entries) = fs::read_dir(legacy) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "bin") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
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
        if !keep.contains(&path) {
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
        let stats = dict.merge_json(json, &Layout::plover()).unwrap();
        assert_eq!(dict.lookup(&[KAT]), Some("cat"));
        assert_eq!(dict.lookup(&[KPA, TKAOEU]), Some("example"));
        // Commands are entries like any other: they are run when the stroke is typed
        assert!(dict.lookup(&[Layout::plover().parse_strokes("-PB").unwrap()[0]]).is_some());
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
        dict.merge_json(&format!(r#"{{"KAT": "{DELETED}"}}"#), &Layout::plover())
            .unwrap();
        dict.merge_json(r#"{"KAT": "cat", "TKOG": "dog"}"#, &Layout::plover()).unwrap();
        assert_eq!(dict.lookup(&[KAT]), None);
        assert!(
            dict.strokes_for("cat").is_empty(),
            "a deleted entry has no reverse lookup either"
        );
        assert_eq!(dict.iter().count(), 1);
        // In any letter case
        let mut upper = Dictionary::default();
        upper
            .merge_json(&format!(r#"{{"KAT": "{}"}}"#, DELETED.to_uppercase()), &Layout::plover())
            .unwrap();
        assert_eq!(upper.lookup(&[KAT]), None);
    }

    #[test]
    fn number_keys_and_the_word_boundary_stroke_are_read_in_keys() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"1": "one", "1-9": "x", "/S": "{prefix^}"}"#, &Layout::plover())
            .unwrap();
        assert_eq!(dict.lookup(&[Layout::plover().parse_strokes("1").unwrap()[0]]), Some("one"));
        assert_eq!(
            dict.lookup(&[0, Layout::plover().parse_strokes("S").unwrap()[0]]),
            Some("{prefix^}")
        );
        assert_eq!(dict.longest_key(), 2);
    }

    #[test]
    fn within_one_file_the_later_of_two_spellings_of_an_outline_wins() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "first", "KA-T": "second"}"#, &Layout::plover())
            .unwrap();
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
        dict.merge_json(r#"{"KPA/TKAOEU": "example"}"#, &Layout::plover()).unwrap();
        assert_eq!(dict.longest_key(), 2);
    }

    #[test]
    fn strokes_for_finds_every_stroke_sequence_for_a_word() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#, &Layout::plover())
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

    /// Copies in the cache: one per dictionary file.
    fn cache_files(cache: &Path) -> usize {
        fs::read_dir(cache.join("parts")).map_or(0, |dir| dir.count())
    }

    #[test]
    fn a_cached_dictionary_is_restored_until_its_file_changes() {
        let (dir, file, cache) = cache_fixture("restore", r#"{"KAT": "cat"}"#);
        let first = Dictionary::load_cached(&[&file], &cache, &Layout::plover());
        assert_eq!(first.strokes_for("cat"), vec![vec![KAT]]);
        assert_eq!(cache_files(&cache), 1, "the first load saves a cache");

        // Unchanged file: the cache is used, so the same answer comes back
        let restored = Dictionary::load_cached(&[&file], &cache, &Layout::plover());
        assert_eq!(restored.strokes_for("cat"), vec![vec![KAT]]);
        assert_eq!(
            cache_files(&cache),
            1,
            "the cache is reused, not duplicated"
        );

        // Changed file: its contents no longer match, so the JSON is read again
        fs::write(&file, r#"{"KAT": "dog"}"#).unwrap();
        let changed = Dictionary::load_cached(&[&file], &cache, &Layout::plover());
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
    fn each_file_has_one_cache_copy_however_the_set_of_files_changes() {
        let (dir, file, cache) = cache_fixture("one-file", r#"{"KAT": "cat"}"#);
        let extra = dir.join("extra.json");
        fs::write(&extra, r#"{"TKOG": "dog"}"#).unwrap();
        let load = |files: &[&Path]| Dictionary::load_cached(files, &cache, &Layout::plover());
        let _ = load(&[&file]);
        let both = load(&[&file, &extra]);
        assert_eq!(
            both.strokes_for("dog"),
            vec![vec![(1 << 1) | (1 << 2) | (1 << 8) | (1 << 17)]]
        );
        let _ = load(&[&extra]);
        let _ = load(&[&file, &extra]);
        assert_eq!(cache_files(&cache), 2, "a copy per file, not per combination");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_is_replaced_by_parsing_the_json() {
        let (dir, file, cache) = cache_fixture("corrupt", r#"{"KAT": "cat"}"#);
        let _ = Dictionary::load_cached(&[&file], &cache, &Layout::plover());
        for entry in fs::read_dir(cache.join("parts")).unwrap() {
            fs::write(entry.unwrap().path(), b"not a dictionary").unwrap();
        }
        let dict = Dictionary::load_cached(&[&file], &cache, &Layout::plover());
        assert_eq!(dict.strokes_for("cat"), vec![vec![KAT]]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strokes_for_finds_every_outline_through_the_word_index() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KAT/-PB": "cat", "KWRAT": "{^}cat"}"#, &Layout::plover())
            .unwrap();
        // The leading space that spacing adds is ignored, and the index covers every entry
        assert_eq!(dict.strokes_for("cat").len(), 3);
    }

    #[test]
    fn earlier_merges_win() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "kitty"}"#, &Layout::plover()).unwrap();
        dict.merge_json(r#"{"KAT": "cat"}"#, &Layout::plover()).unwrap();
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
        let dict = Dictionary::load(&[json.as_path(), rtf.as_path(), words.as_path()], &Layout::plover());
        assert_eq!(dict.lookup(&[KAT]), Some("kitty"));
        assert_eq!(
            dict.lookup(&[Layout::plover().parse_strokes("-S").unwrap()[0]]),
            Some("{^s}")
        );
        assert_eq!(
            dict.lookup(&[Layout::plover().parse_strokes("TKOG").unwrap()[0]]),
            Some("dog")
        );
        assert!(dict.orthography().has_words());
        assert_eq!(dict.orthography().add_suffix("cat", "s"), "cats");

        let other_way = Dictionary::load(&[rtf.as_path(), json.as_path()], &Layout::plover());
        assert_eq!(other_way.lookup(&[KAT]), Some("cat"));
        assert!(!other_way.orthography().has_words());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_skipped() {
        let dict = Dictionary::load(&[Path::new("/nonexistent/steno/main.json")], &Layout::plover());
        assert!(dict.is_empty());
    }
}
