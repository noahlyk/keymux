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
//! The arrays are what the cache file holds, so restoring one is copying bytes, not rebuilding
//! a map.

use super::formatter::plain_text;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// Strings kept end to end in one buffer.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Texts {
    blob: String,
    /// Where each string ends in `blob`
    ends: Vec<u32>,
}

impl Texts {
    const fn len(&self) -> usize {
        self.ends.len()
    }

    fn get(&self, index: usize) -> &str {
        let start = index
            .checked_sub(1)
            .map_or(0, |before| self.ends[before] as usize);
        &self.blob[start..self.ends[index] as usize]
    }

    fn push(&mut self, text: &str) -> u32 {
        self.blob.push_str(text);
        self.ends
            .push(u32::try_from(self.blob.len()).expect("a dictionary file under 4 GB of text"));
        u32::try_from(self.ends.len() - 1).expect("fewer than 4 billion texts")
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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Group {
    len: u32,
    strokes: Vec<u32>,
    /// Which of the translations each entry has
    raw: Vec<u32>,
}

impl Group {
    const fn width(&self) -> usize {
        self.len as usize
    }

    const fn count(&self) -> usize {
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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Packed {
    /// By stroke count, ascending
    groups: Vec<Group>,
    /// The entry number each group starts at, to number every entry of the file with one `u32`
    bases: Vec<u32>,
    /// Each distinct translation as written
    raws: Texts,
    /// Each distinct text a translation types, sorted
    typed: Texts,
    /// For each of `typed`, where its entries end in `typed_entries`
    typed_ends: Vec<u32>,
    /// Entry numbers grouped by the text they type
    typed_entries: Vec<u32>,
}

impl Packed {
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
                .zip(&group.raw)
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
                *group.raw.last_mut().expect("that entry") = row.raw;
                continue;
            }
            if groups.last().is_none_or(|group| group.len != row.len) {
                groups.push(Group {
                    len: row.len,
                    ..Group::default()
                });
            }
            let group = groups.last_mut().expect("just pushed");
            group.strokes.extend_from_slice(outline(&row));
            group.raw.push(row.raw);
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
            bases,
            raws,
            typed,
            typed_ends,
            typed_entries,
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
        for &raw in &group.raw {
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
        let bytes = bincode::serialize(&packed).unwrap();
        let back: Packed = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.get(&[5, 9]), Some("catty"));
        assert_eq!(back.strokes_typing("cat").count(), 1);
    }
}
