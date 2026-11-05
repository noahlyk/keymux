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
use super::packed::{Builder, Layout as PackedLayout, Map, Packed};
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
const CACHE_LAYOUT: u32 = 7;

/// The value that deletes an entry of a dictionary after this one.
pub const DELETED: &str = "{plover:deleted}";

/// Whether a translation is the deletion marker, in any letter case.
const fn is_deleted(translation: &str) -> bool {
    translation.eq_ignore_ascii_case(DELETED)
}

/// What one file holds, parsed: the unit that is cached and shared.
///
/// A file's entries are [`Packed`]. Entries added one at a time (a dictionary built in code) are
/// kept loose beside them and win over them; a file is read in one go, so only it has packed ones.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Table {
    /// The file's entries, deletions included. Not serialized with the rest: the cache file
    /// holds them as raw arrays, mapped when it is read.
    #[serde(skip)]
    packed: Packed,
    /// Entries added one by one, and their translations as written, deletions included
    entries: FxHashMap<Vec<u32>, String>,
    /// Loose stroke sequences grouped by the text they type, for reverse lookup
    by_text: FxHashMap<String, Vec<Vec<u32>>>,
    max_len: usize,
    /// How suffixes are spelled: Plover's rules and, when there is one, its word list
    orthography: Orthography,
}

impl Table {
    fn get(&self, strokes: &[u32]) -> Option<&str> {
        self.entries
            .get(strokes)
            .map(String::as_str)
            .or_else(|| self.packed.get(strokes))
    }

    fn has(&self, strokes: &[u32]) -> bool {
        self.entries.contains_key(strokes) || self.packed.contains(strokes)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.packed.is_empty()
    }

    /// Add a loose entry, replacing one with the same outline.
    fn put(&mut self, strokes: Vec<u32>, text: String) {
        self.max_len = self.max_len.max(strokes.len());
        if !is_deleted(&text) {
            if let Some(typed) = plain_text(&text) {
                self.by_text.entry(typed).or_default().push(strokes.clone());
            }
        }
        self.entries.insert(strokes, text);
    }

    fn stored(&self) -> usize {
        self.packed.len() + self.entries.len()
    }

    /// How many outlines, a loose one that replaces a packed one counted once.
    fn len(&self) -> usize {
        let replacing = self
            .entries
            .keys()
            .filter(|strokes| self.packed.contains(strokes))
            .count();
        self.packed.len() + self.entries.len() - replacing
    }

    /// Every entry: its outline and translation as written.
    fn iter(&self) -> impl Iterator<Item = (&[u32], &str)> {
        let loose = self
            .entries
            .iter()
            .map(|(strokes, text)| (strokes.as_slice(), text.as_str()));
        let packed = self
            .packed
            .iter()
            .filter(|(strokes, _)| self.entries.is_empty() || !self.entries.contains_key(*strokes))
            .map(|(strokes, raw)| (strokes, self.packed.raw(raw)));
        loose.chain(packed)
    }

    /// Like [`Self::iter`], for the entries whose translation satisfies `wanted`, which is asked
    /// once of each distinct packed translation, not of each entry.
    fn iter_where<'a>(
        &'a self,
        wanted: &'a (impl Fn(&str) -> bool + 'a),
    ) -> impl Iterator<Item = (&'a [u32], &'a str)> + 'a {
        let raws = self.packed.raws_where(wanted);
        let loose = self
            .entries
            .iter()
            .filter(move |(_, text)| wanted(text))
            .map(|(strokes, text)| (strokes.as_slice(), text.as_str()));
        let packed = self
            .packed
            .iter()
            .filter(move |&(strokes, raw)| {
                raws[raw] && (self.entries.is_empty() || !self.entries.contains_key(strokes))
            })
            .map(|(strokes, raw)| (strokes, self.packed.raw(raw)));
        loose.chain(packed)
    }

    /// Every outline of `len` strokes.
    fn of_len(&self, len: usize) -> impl Iterator<Item = &[u32]> {
        self.entries
            .keys()
            .filter(move |strokes| strokes.len() == len)
            .map(Vec::as_slice)
            .chain(
                self.packed.of_len(len).filter(|strokes| {
                    self.entries.is_empty() || !self.entries.contains_key(*strokes)
                }),
            )
    }

    /// The outlines whose translation types exactly `text`.
    fn strokes_typing(&self, text: &str) -> impl Iterator<Item = &[u32]> {
        self.by_text
            .get(text)
            .into_iter()
            .flatten()
            .map(Vec::as_slice)
            .chain(self.packed.strokes_typing(text))
    }
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

/// What a cache file starts with after its stamp slot, ahead of the raw arrays: written borrowed,
/// read owned.
#[derive(Deserialize)]
struct CacheIn {
    version: String,
    sources: Vec<Source>,
    /// Everything about the table but its packed entries
    table: Table,
    arrays: PackedLayout,
}

#[derive(Serialize)]
struct CacheOut<'a> {
    version: &'a str,
    sources: &'a [Source],
    table: &'a Table,
    arrays: &'a PackedLayout,
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
fn decode_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    std::str::from_utf8(bytes).map_or_else(
        |_| {
            bytes
                .iter()
                .map(|&b| char::from(b))
                .collect::<String>()
                .into()
        },
        std::borrow::Cow::Borrowed,
    )
}

/// Call `each` with every key of a JSON object and its value if that is a string, in order,
/// without building the object: the strings are borrowed from `json` unless they have escapes.
fn read_entries(json: &str, each: impl FnMut(&str, Option<&str>)) -> serde_json::Result<()> {
    use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
    use std::borrow::Cow;
    use std::fmt;

    /// A string, borrowed when it can be
    struct Text<'de>(Cow<'de, str>);
    impl<'de> Deserialize<'de> for Text<'de> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Borrow;
            impl<'de> Visitor<'de> for Borrow {
                type Value = Text<'de>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a string")
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Text<'de>, E> {
                    Ok(Text(Cow::Borrowed(v)))
                }
                fn visit_str<E>(self, v: &str) -> Result<Text<'de>, E> {
                    Ok(Text(Cow::Owned(v.to_string())))
                }
                fn visit_string<E>(self, v: String) -> Result<Text<'de>, E> {
                    Ok(Text(Cow::Owned(v)))
                }
            }
            deserializer.deserialize_str(Borrow)
        }
    }

    /// A value: its text if it is a string, nothing for anything else
    struct Value<'de>(Option<Cow<'de, str>>);
    impl<'de> Deserialize<'de> for Value<'de> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Any;
            impl<'de> Visitor<'de> for Any {
                type Value = Value<'de>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("any JSON value")
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Value<'de>, E> {
                    Ok(Value(Some(Cow::Borrowed(v))))
                }
                fn visit_str<E>(self, v: &str) -> Result<Value<'de>, E> {
                    Ok(Value(Some(Cow::Owned(v.to_string()))))
                }
                fn visit_string<E>(self, v: String) -> Result<Value<'de>, E> {
                    Ok(Value(Some(Cow::Owned(v))))
                }
                fn visit_bool<E>(self, _: bool) -> Result<Value<'de>, E> {
                    Ok(Value(None))
                }
                fn visit_i64<E>(self, _: i64) -> Result<Value<'de>, E> {
                    Ok(Value(None))
                }
                fn visit_u64<E>(self, _: u64) -> Result<Value<'de>, E> {
                    Ok(Value(None))
                }
                fn visit_f64<E>(self, _: f64) -> Result<Value<'de>, E> {
                    Ok(Value(None))
                }
                fn visit_unit<E>(self) -> Result<Value<'de>, E> {
                    Ok(Value(None))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value<'de>, A::Error> {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    Ok(Value(None))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value<'de>, A::Error> {
                    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                    Ok(Value(None))
                }
            }
            deserializer.deserialize_any(Any)
        }
    }

    struct Entries<F>(F);
    impl<'de, F: FnMut(&str, Option<&str>)> Visitor<'de> for Entries<F> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
            while let Some((Text(key), Value(value))) = map.next_entry()? {
                (self.0)(&key, value.as_deref());
            }
            Ok(())
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(json);
    deserializer.deserialize_map(Entries(each))?;
    deserializer.end()
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
    pub fn merge_bytes(
        &mut self,
        path: &Path,
        bytes: &[u8],
        layout: &Layout,
    ) -> Result<MergeStats> {
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
        let mut stats = MergeStats::default();
        let fresh = self.tables.iter().all(|table| table.is_empty());
        let mut builder = Builder::default();
        let mut loose: Vec<(Vec<u32>, String)> = Vec::new();
        // Within one file the last of two spellings of the same outline wins, as in Plover
        read_entries(json, |key, text| match (layout.parse_strokes(key), text) {
            (Some(strokes), Some(text)) if fresh => {
                builder.add(&strokes, text);
            }
            (Some(strokes), Some(text)) => loose.push((strokes, text.to_string())),
            _ => stats.invalid += 1,
        })
        .context("dictionary is not a JSON object")?;
        if fresh {
            let own = self.own();
            own.packed = builder.finish(is_deleted);
            own.max_len = own.max_len.max(own.packed.max_len());
            stats.loaded = own.packed.len();
        } else {
            let mut last: FxHashMap<Vec<u32>, String> = FxHashMap::default();
            last.extend(loose);
            for (strokes, text) in last {
                if self.add(strokes, text) {
                    stats.loaded += 1;
                }
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
        self.tables.iter().any(|table| table.has(strokes))
    }

    /// Add one entry by its outline bits directly, bypassing stroke-notation parsing - for a
    /// caller that already has `strokes`, such as one built with a theory-specific
    /// [`super::layout::Layout`] a plain dictionary file's text can't express on its own
    /// (`merge_json`/`merge_file` always parse against keymux's own default Plover layout).
    /// Unlike `merge_json`, an existing entry for the same outline is replaced, not kept.
    pub fn insert(&mut self, strokes: Vec<u32>, text: impl Into<String>) {
        // A loose entry wins over a packed one, so this replaces either
        let own = self.own();
        own.entries.remove(&strokes);
        own.put(strokes, text.into());
    }

    /// Add an entry unless an earlier dictionary already has one for the outline.
    fn add(&mut self, strokes: Vec<u32>, text: String) -> bool {
        if self.has_entry(&strokes) {
            return false;
        }
        self.own().put(strokes, text);
        true
    }

    /// The translation of an exact outline, as written. `None` if there is none, or the
    /// outline was deleted.
    #[must_use]
    pub fn lookup(&self, strokes: &[u32]) -> Option<&str> {
        self.tables
            .iter()
            .find_map(|table| table.get(strokes))
            .filter(|translation| !is_deleted(translation))
    }

    /// Use this orthography, as if its word list had been read.
    #[cfg(test)]
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
        self.tables
            .iter()
            .map(|table| table.max_len)
            .max()
            .unwrap_or(0)
    }

    /// Every stroke sequence whose translation is exactly `text`, ignoring the
    /// leading space that spacing adds. Case matters.
    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        let mut found = Vec::new();
        for (i, table) in self.tables.iter().enumerate() {
            for strokes in table.strokes_typing(text) {
                // An earlier table's entry for the outline, even a deletion, hides this one
                if !self.hidden_before(i, strokes) {
                    found.push(strokes.to_vec());
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
    pub fn completions<'a>(&'a self, prefix: &str, limit: usize) -> Vec<&'a str> {
        let candidates: FxHashSet<&str> = self
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .by_text
                    .keys()
                    .map(String::as_str)
                    .filter(|text| text.starts_with(prefix))
                    .chain(table.packed.typed_starting_with(prefix))
            })
            .filter(|text| {
                text.len() > prefix.len()
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
        let files: Vec<PathBuf> = paths
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
        let mut parts = Parts::default();
        parts.refresh(&files, cache_dir, layout);
        parts.layered(&files)
    }

    /// Whether a table before the `i`th has an entry (a deletion counts) for the outline.
    fn hidden_before(&self, i: usize, strokes: &[u32]) -> bool {
        self.tables[..i].iter().any(|earlier| earlier.has(strokes))
    }

    /// Every entry that is in force, for listing and search.
    pub fn iter(&self) -> impl Iterator<Item = (&[u32], &str)> {
        self.tables.iter().enumerate().flat_map(move |(i, table)| {
            table.iter().filter(move |&(strokes, translation)| {
                !is_deleted(translation) && !self.hidden_before(i, strokes)
            })
        })
    }

    /// Every entry in force whose translation, as written, satisfies `wanted`: what [`Self::iter`]
    /// and a filter would find, without asking about each entry of a packed file, only about each
    /// distinct translation.
    pub fn iter_where<'a>(
        &'a self,
        wanted: &'a (impl Fn(&str) -> bool + 'a),
    ) -> impl Iterator<Item = (&'a [u32], &'a str)> + 'a {
        self.tables.iter().enumerate().flat_map(move |(i, table)| {
            table
                .iter_where(wanted)
                .filter(move |&(strokes, translation)| {
                    !is_deleted(translation) && !self.hidden_before(i, strokes)
                })
        })
    }

    /// The `limit` outlines of as many strokes as `strokes` with the fewest keys different from
    /// it (never `strokes` itself), nearest first and then in key order.
    ///
    /// Compares only the entries of that length, which a packed file keeps as one run, and
    /// checks that an entry is in force only when it would make the cut.
    #[must_use]
    pub fn nearest(&self, strokes: &[u32], limit: usize) -> Vec<Vec<u32>> {
        use std::collections::BinaryHeap;
        if limit == 0 || strokes.is_empty() {
            return Vec::new();
        }
        let mut best: BinaryHeap<(u32, &[u32])> = BinaryHeap::with_capacity(limit + 1);
        for (i, table) in self.tables.iter().enumerate() {
            for outline in table.of_len(strokes.len()) {
                let distance: u32 = outline
                    .iter()
                    .zip(strokes)
                    .map(|(&a, &b)| (a ^ b).count_ones())
                    .sum();
                let candidate = (distance, outline);
                if best.len() == limit && best.peek().is_some_and(|worst| *worst <= candidate) {
                    continue;
                }
                if outline == strokes
                    || self.hidden_before(i, outline)
                    || table.get(outline).is_none_or(is_deleted)
                {
                    continue;
                }
                best.push(candidate);
                if best.len() > limit {
                    best.pop();
                }
            }
        }
        best.into_sorted_vec()
            .into_iter()
            .map(|(_, outline)| outline.to_vec())
            .collect()
    }

    /// Every outline with an entry (deletions included), each once.
    fn outlines(&self) -> impl Iterator<Item = &[u32]> {
        self.tables.iter().enumerate().flat_map(move |(i, table)| {
            table
                .iter()
                .map(|(strokes, _)| strokes)
                .filter(move |strokes| !self.hidden_before(i, strokes))
        })
    }

    /// How many entries the tables hold, an outline in two of them counted twice. Reads none of
    /// them, where [`Self::len`] looks up each in the tables before it.
    #[must_use]
    pub fn stored(&self) -> usize {
        self.tables.iter().map(|table| table.stored()).sum()
    }

    /// How many outlines have an entry (deletions included). With several tables this looks
    /// up every outline in the tables before it, so it costs milliseconds on a full theory;
    /// [`Self::has_at_least`] is the cheap question.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.tables.as_slice() {
            [] => 0,
            [only] => only.len(),
            _ => self.outlines().count(),
        }
    }

    /// Whether at least `count` outlines have an entry, looking no further than that.
    #[must_use]
    pub fn has_at_least(&self, count: usize) -> bool {
        count == 0 || self.outlines().nth(count - 1).is_some()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.iter().all(|table| table.is_empty())
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

/// What `stat` showed of the file a cache was made from, as bytes: size, then modification time
/// in seconds and nanoseconds.
pub(super) type StampKey = [u8; 20];

/// Bytes at the start of a cache file for its source's [`StampKey`]: a flag (1 if one is set),
/// then the key, padded to 24. Fixed in size so a key can be written without rewriting the
/// dictionary behind it.
const STAMP_SLOT: usize = 24;

/// A cache file is the stamp slot, the length of the header, the header, and after that (on an
/// 8-byte boundary) the arrays of the packed entries, as they are in memory.
const LENGTH_BYTES: usize = 8;

/// A cache file opened: its header, and the table with its entries read in place from the map.
fn open_cache(file: &Path, layout: &Layout) -> Option<(CacheIn, Table, std::sync::Arc<Map>)> {
    let map = Map::new(&fs::File::open(file).ok()?).ok()?;
    let bytes = map.bytes();
    let length = usize::try_from(u64::from_le_bytes(
        bytes
            .get(STAMP_SLOT..STAMP_SLOT + LENGTH_BYTES)?
            .try_into()
            .ok()?,
    ))
    .ok()?;
    let header_at = STAMP_SLOT + LENGTH_BYTES;
    let header = bytes.get(header_at..header_at.checked_add(length)?)?;
    let mut cache: CacheIn = bincode::deserialize(header).ok()?;
    if cache.version != cache_version(layout) {
        return None;
    }
    let body = (header_at + length).next_multiple_of(8);
    let mut table = std::mem::take(&mut cache.table);
    table.packed = Packed::open(&cache.arrays, &map, body)?;
    Some((cache, table, map))
}

/// The cached dictionary, if it was built from files with these exact contents.
pub(super) fn read_cache(file: &Path, sources: &[Source], layout: &Layout) -> Option<Dictionary> {
    let (cache, table, _) = open_cache(file, layout)?;
    (cache.sources == sources).then(|| Dictionary {
        tables: vec![Arc::new(table)],
    })
}

/// The cached dictionary and the sha256 of the file it was built from, if the file still looks
/// the way it did then (`stamp`) - so the file need not be read to be sure.
pub(super) fn read_cache_by_stamp(
    file: &Path,
    source: &Path,
    stamp: &StampKey,
    layout: &Layout,
) -> Option<(Dictionary, String)> {
    let (cache, table, map) = open_cache(file, layout)?;
    let slot = map.bytes().get(..STAMP_SLOT)?;
    if slot[0] != 1 || slot[1..=stamp.len()] != stamp[..] {
        return None;
    }
    let [(path, digest)] = cache.sources.as_slice() else {
        return None;
    };
    (*path == source.display().to_string()).then(|| {
        (
            Dictionary {
                tables: vec![Arc::new(table)],
            },
            digest.clone(),
        )
    })
}

/// Record in a cache file that its source looked like `stamp` when it was read, or (`None`) that
/// that can't be told.
pub(super) fn set_cache_stamp(file: &Path, stamp: Option<&StampKey>) {
    use std::io::Write as _;
    let mut slot = [0_u8; STAMP_SLOT];
    if let Some(stamp) = stamp {
        slot[0] = 1;
        slot[1..=stamp.len()].copy_from_slice(stamp);
    }
    let written = fs::OpenOptions::new()
        .write(true)
        .open(file)
        .and_then(|mut cache| cache.write_all(&slot));
    if let Err(e) = written {
        warn!("steno cache {}: {e}", file.display());
    }
}

/// Save the dictionary for these files. A cache that can't be written only costs time next
/// start, so failures are logged and not returned.
pub(super) fn write_cache(
    file: &Path,
    sources: &[Source],
    stamp: Option<&StampKey>,
    dict: &Dictionary,
    layout: &Layout,
) {
    // Only a dictionary read from one file is ever cached, and that is one table
    let [table] = dict.tables.as_slice() else {
        return;
    };
    let version = cache_version(layout);
    let mut body = Vec::new();
    let arrays = table.packed.save(&mut body);
    let header = match bincode::serialize(&CacheOut {
        version: &version,
        sources,
        table,
        arrays: &arrays,
    }) {
        Ok(header) => header,
        Err(e) => return warn!("steno cache not saved: {e}"),
    };
    let mut bytes = vec![0_u8; STAMP_SLOT];
    if let Some(stamp) = stamp {
        bytes[0] = 1;
        bytes[1..=stamp.len()].copy_from_slice(stamp);
    }
    bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.resize(bytes.len().next_multiple_of(8), 0);
    bytes.extend_from_slice(&body);
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
        assert!(dict
            .lookup(&[Layout::plover().parse_strokes("-PB").unwrap()[0]])
            .is_some());
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
        dict.merge_json(r#"{"KAT": "cat", "TKOG": "dog"}"#, &Layout::plover())
            .unwrap();
        assert_eq!(dict.lookup(&[KAT]), None);
        assert!(
            dict.strokes_for("cat").is_empty(),
            "a deleted entry has no reverse lookup either"
        );
        assert_eq!(dict.iter().count(), 1);
        // In any letter case
        let mut upper = Dictionary::default();
        upper
            .merge_json(
                &format!(r#"{{"KAT": "{}"}}"#, DELETED.to_uppercase()),
                &Layout::plover(),
            )
            .unwrap();
        assert_eq!(upper.lookup(&[KAT]), None);
    }

    #[test]
    fn number_keys_and_the_word_boundary_stroke_are_read_in_keys() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"1": "one", "1-9": "x", "/S": "{prefix^}"}"#,
            &Layout::plover(),
        )
        .unwrap();
        assert_eq!(
            dict.lookup(&[Layout::plover().parse_strokes("1").unwrap()[0]]),
            Some("one")
        );
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
    fn has_at_least_counts_an_outline_in_two_tables_once() {
        let mut high = Dictionary::default();
        high.merge_json(r#"{"KAT": "cat", "TKOG": "dog"}"#, &Layout::plover())
            .unwrap();
        let mut low = Dictionary::default();
        low.merge_json(r#"{"KAT": "kitten", "PWEURD": "bird"}"#, &Layout::plover())
            .unwrap();
        let dict = Dictionary::layered([&high, &low]);
        assert_eq!(dict.len(), 3);
        assert!(dict.has_at_least(0));
        assert!(dict.has_at_least(3));
        assert!(!dict.has_at_least(4));
        assert!(!Dictionary::default().has_at_least(1));
    }

    #[test]
    fn longest_entry_length_is_tracked() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KPA/TKAOEU": "example"}"#, &Layout::plover())
            .unwrap();
        assert_eq!(dict.longest_key(), 2);
    }

    #[test]
    fn strokes_for_finds_every_stroke_sequence_for_a_word() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"KAT": "cat", "KPA/TKAOEU": "example"}"#,
            &Layout::plover(),
        )
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
        assert_eq!(
            cache_files(&cache),
            2,
            "a copy per file, not per combination"
        );
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
        dict.merge_json(
            r#"{"KAT": "cat", "KAT/-PB": "cat", "KWRAT": "{^}cat"}"#,
            &Layout::plover(),
        )
        .unwrap();
        // The leading space that spacing adds is ignored, and the index covers every entry
        assert_eq!(dict.strokes_for("cat").len(), 3);
    }

    #[test]
    fn earlier_merges_win() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "kitty"}"#, &Layout::plover())
            .unwrap();
        dict.merge_json(r#"{"KAT": "cat"}"#, &Layout::plover())
            .unwrap();
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
        let dict = Dictionary::load(
            &[json.as_path(), rtf.as_path(), words.as_path()],
            &Layout::plover(),
        );
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
        let dict = Dictionary::load(
            &[Path::new("/nonexistent/steno/main.json")],
            &Layout::plover(),
        );
        assert!(dict.is_empty());
    }
}
