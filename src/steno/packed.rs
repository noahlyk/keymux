//! A dictionary file's entries as a few flat arrays, for files with millions of them.
//!
//! A hash map of heap-allocated outlines and translations costs an allocation or two per entry,
//! twice over once reverse lookup is indexed, so a theory with millions of entries took seconds
//! to restore and hundreds of megabytes to hold. Here the same entries are:
//!
//! - **Grouped by stroke count and sorted**, each group one flat `Vec<u32>` of outlines side by
//!   side and one `Vec<u32>` saying which translation each has. An exact lookup is a binary
//!   search; the entries of one length (what "nearby outlines" compares) are one contiguous run.
//! - **Translations kept once each**: a theory writes the same word under thousands of outlines.
//! - **Indexed by the text they type**, sorted, so a reverse lookup is a binary search and the
//!   words starting with a prefix are one range.
//!
//! The arrays are what the cache file holds, laid out as they are in memory, so a file is
//! restored by mapping it: nothing is read or decoded until a lookup touches the page it is on.

use super::formatter::plain_text;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

// The cache file is read in place, so the arrays are little-endian words on disk as in memory.
#[cfg(not(target_endian = "little"))]
compile_error!("the dictionary cache is mapped from disk as little-endian words");

/// A read-only memory map of a cache file, shared by the arrays that live in it.
pub struct Map(memmap2::Mmap);

impl std::fmt::Debug for Map {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Map({} bytes)", self.0.len())
    }
}

impl Map {
    /// Map `file`. The cache is only ever replaced by renaming a new file over it, never edited
    /// in place past its stamp slot, which nothing reads from the map.
    pub fn new(file: &std::fs::File) -> std::io::Result<std::sync::Arc<Self>> {
        // SAFETY: a mapped file that is truncated by someone else would fault on access; the
        // cache folder is keymux's own and its files are replaced by rename, not rewritten.
        let map = unsafe { memmap2::Mmap::map(file)? };
        Ok(std::sync::Arc::new(Self(map)))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// An array of `u32`s: built in memory, or read in place from a cache file's map.
#[derive(Clone)]
enum Words {
    Owned(Vec<u32>),
    Mapped {
        map: std::sync::Arc<Map>,
        /// Byte offset in the map: a multiple of 4, checked when opened
        start: usize,
        len: usize,
    },
}

impl Default for Words {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl std::fmt::Debug for Words {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Words({})", self.len())
    }
}

impl From<Vec<u32>> for Words {
    fn from(words: Vec<u32>) -> Self {
        Self::Owned(words)
    }
}

impl std::ops::Deref for Words {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        match self {
            Self::Owned(words) => words,
            #[allow(
                clippy::cast_ptr_alignment,
                reason = "`open_words` checked the offset is a multiple of 4, in a page-aligned map"
            )]
            Self::Mapped { map, start, len } => {
                // SAFETY: `open` checked that the words lie inside the map and start on a
                // 4-byte boundary (a map starts on a page), and the map is alive as long as
                // this holds it. Any bit pattern is a valid `u32`.
                unsafe {
                    std::slice::from_raw_parts(map.bytes().as_ptr().add(*start).cast::<u32>(), *len)
                }
            }
        }
    }
}

impl Words {
    /// The vector, for a table being built.
    fn owned(&mut self) -> &mut Vec<u32> {
        match self {
            Self::Owned(words) => words,
            Self::Mapped { .. } => unreachable!("only a table being built is added to"),
        }
    }
}

/// A string: built in memory, or read in place from a cache file's map.
#[derive(Clone)]
enum Chars {
    Owned(String),
    Mapped {
        map: std::sync::Arc<Map>,
        start: usize,
        len: usize,
    },
}

impl std::fmt::Debug for Chars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chars({})", self.len())
    }
}

impl Default for Chars {
    fn default() -> Self {
        Self::Owned(String::new())
    }
}

impl std::ops::Deref for Chars {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Owned(text) => text,
            Self::Mapped { map, start, len } => {
                // SAFETY: `open` checked that these bytes lie inside the map and are UTF-8
                unsafe { std::str::from_utf8_unchecked(&map.bytes()[*start..*start + *len]) }
            }
        }
    }
}

/// Where a run of a table's data is in the body of a cache file, from its start, in elements
/// (words, or bytes for text).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Section {
    at: u64,
    len: u64,
}

/// Where each array of a [`Packed`] is in a cache file's body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    /// For each group: its stroke count, its outlines, its translation numbers
    groups: Vec<(u32, Section, Section)>,
    bases: Section,
    raws_blob: Section,
    raws_ends: Section,
    typed_blob: Section,
    typed_ends: Section,
    typed_entry_ends: Section,
    typed_entries: Section,
}

/// Appends arrays to a cache file's body, each starting on an 8-byte boundary.
fn put(body: &mut Vec<u8>, bytes: impl IntoIterator<Item = u8>, len: usize) -> Section {
    while !body.len().is_multiple_of(8) {
        body.push(0);
    }
    let at = body.len() as u64;
    body.extend(bytes);
    Section {
        at,
        len: len as u64,
    }
}

fn put_words(body: &mut Vec<u8>, words: &[u32]) -> Section {
    while !body.len().is_multiple_of(8) {
        body.push(0);
    }
    let at = body.len() as u64;
    body.reserve(words.len() * 4);
    for word in words {
        body.extend_from_slice(&word.to_le_bytes());
    }
    Section {
        at,
        len: words.len() as u64,
    }
}

fn open_words(map: &std::sync::Arc<Map>, body: usize, section: Section) -> Option<Words> {
    let start = body.checked_add(usize::try_from(section.at).ok()?)?;
    let len = usize::try_from(section.len).ok()?;
    let end = start.checked_add(len.checked_mul(4)?)?;
    (start % 4 == 0 && end <= map.bytes().len()).then(|| Words::Mapped {
        map: std::sync::Arc::clone(map),
        start,
        len,
    })
}

fn open_chars(map: &std::sync::Arc<Map>, body: usize, section: Section) -> Option<Chars> {
    let start = body.checked_add(usize::try_from(section.at).ok()?)?;
    let len = usize::try_from(section.len).ok()?;
    let end = start.checked_add(len)?;
    std::str::from_utf8(map.bytes().get(start..end)?).ok()?;
    Some(Chars::Mapped {
        map: std::sync::Arc::clone(map),
        start,
        len,
    })
}

/// Strings kept end to end in one buffer.
#[derive(Debug, Default, Clone)]
struct Texts {
    blob: Chars,
    /// Where each string ends in `blob`
    ends: Words,
}

impl Texts {
    fn len(&self) -> usize {
        self.ends.len()
    }

    fn get(&self, index: usize) -> &str {
        let start = index
            .checked_sub(1)
            .map_or(0, |before| self.ends[before] as usize);
        &self.blob[start..self.ends[index] as usize]
    }

    fn push(&mut self, text: &str) -> u32 {
        let (Chars::Owned(blob), Words::Owned(ends)) = (&mut self.blob, &mut self.ends) else {
            unreachable!("only a table being built is added to");
        };
        blob.push_str(text);
        ends.push(u32::try_from(blob.len()).expect("a dictionary file under 4 GB of text"));
        u32::try_from(ends.len() - 1).expect("fewer than 4 billion texts")
    }

    /// The index of the first string not less than `text`: they must have been pushed sorted.
    fn lower_bound(&self, text: &str) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            if self.get(middle) < text {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

/// The entries of one stroke count: entry `i` is `strokes[i * len..(i + 1) * len]`.
#[derive(Debug, Default, Clone)]
struct Group {
    len: u32,
    strokes: Words,
    /// Which of the translations each entry has
    raw: Words,
}

impl Group {
    const fn width(&self) -> usize {
        self.len as usize
    }

    fn count(&self) -> usize {
        self.raw.len()
    }

    fn outline(&self, index: usize) -> &[u32] {
        &self.strokes[index * self.width()..(index + 1) * self.width()]
    }

    /// The position of `strokes`, or where it would go.
    fn search(&self, strokes: &[u32]) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.count());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.outline(middle).cmp(strokes) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    }
}

/// A file's entries, packed: see the module documentation.
#[derive(Debug, Default, Clone)]
pub struct Packed {
    /// By stroke count, ascending
    groups: Vec<Group>,
    /// The entry number each group starts at, to number every entry of the file with one `u32`
    bases: Words,
    /// Each distinct translation as written
    raws: Texts,
    /// Each distinct text a translation types, sorted
    typed: Texts,
    /// For each of `typed`, where its entries end in `typed_entries`
    typed_ends: Words,
    /// Entry numbers grouped by the text they type
    typed_entries: Words,
}

impl Packed {
    /// Append the arrays to a cache file's `body`, and say where they went.
    pub fn save(&self, body: &mut Vec<u8>) -> Layout {
        let texts = |body: &mut Vec<u8>, texts: &Texts| {
            let blob = put(body, texts.blob.bytes(), texts.blob.len());
            (blob, put_words(body, &texts.ends))
        };
        let groups = self
            .groups
            .iter()
            .map(|group| {
                (
                    group.len,
                    put_words(body, &group.strokes),
                    put_words(body, &group.raw),
                )
            })
            .collect();
        let bases = put_words(body, &self.bases);
        let (raws_blob, raws_ends) = texts(body, &self.raws);
        let (typed_blob, typed_ends) = texts(body, &self.typed);
        Layout {
            groups,
            bases,
            raws_blob,
            raws_ends,
            typed_blob,
            typed_ends,
            typed_entry_ends: put_words(body, &self.typed_ends),
            typed_entries: put_words(body, &self.typed_entries),
        }
    }

    /// The table `layout` describes, read in place from `map` where the body starts at `body`.
    /// `None` if the layout does not fit the file.
    pub fn open(layout: &Layout, map: &std::sync::Arc<Map>, body: usize) -> Option<Self> {
        let texts = |blob, ends| {
            Some(Texts {
                blob: open_chars(map, body, blob)?,
                ends: open_words(map, body, ends)?,
            })
        };
        let mut groups = Vec::with_capacity(layout.groups.len());
        for &(len, strokes, raw) in &layout.groups {
            groups.push(Group {
                len,
                strokes: open_words(map, body, strokes)?,
                raw: open_words(map, body, raw)?,
            });
        }
        Some(Self {
            groups,
            bases: open_words(map, body, layout.bases)?,
            raws: texts(layout.raws_blob, layout.raws_ends)?,
            typed: texts(layout.typed_blob, layout.typed_ends)?,
            typed_ends: open_words(map, body, layout.typed_entry_ends)?,
            typed_entries: open_words(map, body, layout.typed_entries)?,
        })
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.groups.iter().map(Group::count).sum()
    }

    pub const fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The most strokes in an outline.
    pub fn max_len(&self) -> usize {
        self.groups.last().map_or(0, Group::width)
    }

    fn group(&self, len: usize) -> Option<&Group> {
        self.groups.iter().find(|group| group.width() == len)
    }

    /// The translation of an outline, as written.
    pub fn get(&self, strokes: &[u32]) -> Option<&str> {
        let group = self.group(strokes.len())?;
        let index = group.search(strokes).ok()?;
        Some(self.raws.get(group.raw[index] as usize))
    }

    pub fn contains(&self, strokes: &[u32]) -> bool {
        self.group(strokes.len())
            .is_some_and(|group| group.search(strokes).is_ok())
    }

    /// Every entry, with the number of its translation.
    pub fn iter(&self) -> impl Iterator<Item = (&[u32], usize)> {
        self.groups.iter().flat_map(|group| {
            group
                .strokes
                .chunks_exact(group.width().max(1))
                .zip(group.raw.iter())
                .map(|(strokes, &raw)| (strokes, raw as usize))
        })
    }

    /// Every entry of `len` strokes.
    pub fn of_len(&self, len: usize) -> impl Iterator<Item = &[u32]> {
        self.group(len)
            .into_iter()
            .flat_map(|group| group.strokes.chunks_exact(group.width().max(1)))
    }

    /// The translation with this number, as written.
    pub fn raw(&self, raw: usize) -> &str {
        self.raws.get(raw)
    }

    /// Which translations satisfy `wanted`, by number.
    pub fn raws_where(&self, wanted: impl Fn(&str) -> bool) -> Vec<bool> {
        (0..self.raws.len())
            .map(|raw| wanted(self.raws.get(raw)))
            .collect()
    }

    /// The outlines whose translation types exactly `text`.
    pub fn strokes_typing<'a>(&'a self, text: &str) -> impl Iterator<Item = &'a [u32]> {
        let at = self.typed.lower_bound(text);
        let range = if at < self.typed.len() && self.typed.get(at) == text {
            self.typed_range(at)
        } else {
            0..0
        };
        self.typed_entries[range]
            .iter()
            .map(|&entry| self.outline_of(entry))
    }

    /// The texts that start with `prefix` (itself included), alphabetically.
    pub fn typed_starting_with(&self, prefix: &str) -> impl Iterator<Item = &str> {
        let first = self.typed.lower_bound(prefix);
        let end = (first..self.typed.len())
            .find(|&index| !self.typed.get(index).starts_with(prefix))
            .unwrap_or(self.typed.len());
        (first..end).map(|index| self.typed.get(index))
    }

    fn typed_range(&self, typed: usize) -> std::ops::Range<usize> {
        let start = typed
            .checked_sub(1)
            .map_or(0, |before| self.typed_ends[before] as usize);
        start..self.typed_ends[typed] as usize
    }

    /// The outline of the entry with this number.
    fn outline_of(&self, entry: u32) -> &[u32] {
        let group = self.bases.partition_point(|&base| base <= entry) - 1;
        self.groups[group].outline((entry - self.bases[group]) as usize)
    }
}

/// Collects entries and packs them.
#[derive(Default)]
pub struct Builder {
    raws: Texts,
    ids: FxHashMap<String, u32>,
    strokes: Vec<u32>,
    rows: Vec<Row>,
}

#[derive(Clone, Copy)]
struct Row {
    at: u32,
    len: u32,
    raw: u32,
}

impl Builder {
    /// Add an entry. Of two entries with the same outline the later one wins.
    pub fn add(&mut self, strokes: &[u32], translation: &str) {
        if strokes.is_empty() {
            return;
        }
        let raw = if let Some(&raw) = self.ids.get(translation) {
            raw
        } else {
            let raw = self.raws.push(translation);
            self.ids.insert(translation.to_string(), raw);
            raw
        };
        self.rows.push(Row {
            at: u32::try_from(self.strokes.len()).expect("fewer than 4 billion strokes"),
            len: u32::try_from(strokes.len()).expect("outlines of a few strokes"),
            raw,
        });
        self.strokes.extend_from_slice(strokes);
    }

    /// The packed entries; `is_deleted` says which translations are the deletion marker, which
    /// type nothing and are left out of the reverse index.
    pub fn finish(self, is_deleted: impl Fn(&str) -> bool) -> Packed {
        let Self {
            raws,
            strokes: flat,
            mut rows,
            ..
        } = self;
        let outline = |row: &Row| &flat[row.at as usize..(row.at + row.len) as usize];
        // Stable, so of two spellings of one outline the later is the last of its run
        rows.sort_by(|a, b| a.len.cmp(&b.len).then_with(|| outline(a).cmp(outline(b))));
        let mut groups: Vec<Group> = Vec::new();
        let mut last: Option<Row> = None;
        for row in rows {
            if let Some(before) = last.filter(|before| outline(before) == outline(&row)) {
                debug_assert_eq!(before.len, row.len);
                let group = groups.last_mut().expect("a group for the entry before");
                *group.raw.owned().last_mut().expect("that entry") = row.raw;
                continue;
            }
            if groups.last().is_none_or(|group| group.len != row.len) {
                groups.push(Group {
                    len: row.len,
                    ..Group::default()
                });
            }
            let group = groups.last_mut().expect("just pushed");
            group.strokes.owned().extend_from_slice(outline(&row));
            group.raw.owned().push(row.raw);
            last = Some(row);
        }
        let mut bases = Vec::with_capacity(groups.len());
        let mut total = 0_u32;
        for group in &groups {
            bases.push(total);
            total += u32::try_from(group.count()).expect("fewer than 4 billion entries");
        }
        let (typed, typed_ends, typed_entries) = typed_index(&raws, &groups, &bases, is_deleted);
        Packed {
            groups,
            bases: bases.into(),
            raws,
            typed,
            typed_ends: typed_ends.into(),
            typed_entries: typed_entries.into(),
        }
    }
}

/// The reverse index: the distinct typed texts sorted, and for each the entries that type it.
fn typed_index(
    raws: &Texts,
    groups: &[Group],
    bases: &[u32],
    is_deleted: impl Fn(&str) -> bool,
) -> (Texts, Vec<u32>, Vec<u32>) {
    // What each translation types, worked out once per translation, not once per entry
    let mut typed_of: Vec<(String, u32)> = (0..raws.len())
        .filter(|&raw| !is_deleted(raws.get(raw)))
        .filter_map(|raw| Some((plain_text(raws.get(raw))?, raw as u32)))
        .collect();
    typed_of.sort();
    let mut typed = Texts::default();
    let mut typed_id_of_raw = vec![u32::MAX; raws.len()];
    let mut last: Option<&str> = None;
    for (text, raw) in &typed_of {
        if last != Some(text.as_str()) {
            typed.push(text);
            last = Some(text);
        }
        typed_id_of_raw[*raw as usize] = u32::try_from(typed.len() - 1).expect("few texts");
    }
    // Entries counted into each text, then placed: a counting sort
    let mut ends = vec![0_u32; typed.len()];
    for group in groups {
        for &raw in group.raw.iter() {
            if let Some(slot) = ends.get_mut(typed_id_of_raw[raw as usize] as usize) {
                *slot += 1;
            }
        }
    }
    let mut running = 0;
    for end in &mut ends {
        running += *end;
        *end = running;
    }
    let mut next: Vec<u32> = std::iter::once(0)
        .chain(ends.iter().copied())
        .take(ends.len())
        .collect();
    let mut entries = vec![0_u32; running as usize];
    for (group, &base) in groups.iter().zip(bases) {
        for (index, &raw) in group.raw.iter().enumerate() {
            let id = typed_id_of_raw[raw as usize] as usize;
            if id < next.len() {
                entries[next[id] as usize] = base + index as u32;
                next[id] += 1;
            }
        }
    }
    (typed, ends, entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(entries: &[(&[u32], &str)]) -> Packed {
        let mut builder = Builder::default();
        for (strokes, text) in entries {
            builder.add(strokes, text);
        }
        builder.finish(|text| text == "{deleted}")
    }

    #[test]
    fn an_outline_is_found_by_its_strokes_and_length() {
        let packed = packed(&[
            (&[5], "cat"),
            (&[5, 9], "catty"),
            (&[9], "dog"),
            (&[1, 2, 3], "x"),
        ]);
        assert_eq!(packed.get(&[5]), Some("cat"));
        assert_eq!(packed.get(&[5, 9]), Some("catty"));
        assert_eq!(packed.get(&[9]), Some("dog"));
        assert_eq!(packed.get(&[1, 2, 3]), Some("x"));
        assert_eq!(packed.get(&[6]), None);
        assert_eq!(packed.get(&[5, 8]), None);
        assert_eq!(packed.get(&[]), None);
        assert_eq!(packed.len(), 4);
        assert_eq!(packed.max_len(), 3);
    }

    #[test]
    fn of_two_spellings_of_an_outline_the_later_wins() {
        let packed = packed(&[(&[5], "first"), (&[7], "other"), (&[5], "second")]);
        assert_eq!(packed.get(&[5]), Some("second"));
        assert_eq!(packed.len(), 2);
    }

    #[test]
    fn a_text_finds_every_outline_that_types_it() {
        let packed = packed(&[
            (&[5], "cat"),
            (&[6, 6], "cat"),
            (&[9], "{deleted}"),
            (&[1], "dog"),
        ]);
        let mut found: Vec<&[u32]> = packed.strokes_typing("cat").collect();
        found.sort();
        assert_eq!(found, [&[5][..], &[6, 6][..]]);
        assert_eq!(packed.strokes_typing("deleted").count(), 0);
        assert_eq!(packed.strokes_typing("ca").count(), 0);
        assert_eq!(packed.strokes_typing("zebra").count(), 0);
    }

    #[test]
    fn the_texts_with_a_prefix_are_one_alphabetical_run() {
        let packed = packed(&[
            (&[1], "comb"),
            (&[2], "come"),
            (&[3], "cat"),
            (&[4], "comet"),
        ]);
        let found: Vec<&str> = packed.typed_starting_with("com").collect();
        assert_eq!(found, ["comb", "come", "comet"]);
    }

    #[test]
    fn entries_of_one_length_are_one_run() {
        let packed = packed(&[(&[3], "a"), (&[1, 1], "b"), (&[2], "c")]);
        let ones: Vec<&[u32]> = packed.of_len(1).collect();
        assert_eq!(ones, [&[2][..], &[3][..]]);
        assert_eq!(packed.of_len(4).count(), 0);
    }

    #[test]
    fn a_packed_table_survives_the_cache_round_trip() {
        let packed = packed(&[(&[5], "cat"), (&[5, 9], "catty")]);
        let mut body = Vec::new();
        let layout = packed.save(&mut body);
        let path = std::env::temp_dir().join(format!("keymux-packed-{}", std::process::id()));
        std::fs::write(&path, &body).unwrap();
        let map = Map::new(&std::fs::File::open(&path).unwrap()).unwrap();
        let back = Packed::open(&layout, &map, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(back.get(&[5, 9]), Some("catty"));
        assert_eq!(back.strokes_typing("cat").count(), 1);
        assert_eq!(back.len(), 2);
        // A layout that points outside the file is refused, not trusted
        assert!(Packed::open(&layout, &map, body.len()).is_none());
    }
}
