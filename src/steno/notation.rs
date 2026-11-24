//! Theory notation: how keymux writes strokes for people, and reads them back, when the
//! theory says so.
//!
//! keymux writes strokes in Plover's notation (`TPHAEPL`, `2R-R8`). A theory may ship, beside
//! its dictionaries, a file called [`FILE_NAME`] that gives key groups a short spelling of
//! their own: `TPH` written `N`, `-PL` written `M`, so `TPHAEPL` reads `NAEM`. keymux never
//! knows any theory's groups: they are only the file's data. With no file, everything is
//! Plover's notation exactly as before.
//!
//! The file (JSON; unknown fields are ignored):
//!
//! ```json
//! {
//!   "number": "letters",
//!   "left":  {"TPH": "N", "TP": "F", "TPHR": "FL"},
//!   "right": {"PB": "N", "PL": "M"},
//!   "outlines": {"TPHR-R": "NR-R"}
//! }
//! ```
//!
//! - `number`: `"letters"` writes a stroke with the number key as `#` and letters (`#TR-RL`);
//!   `"digits"`, the default, keeps Plover's digits (`2R-R8`) for a stroke that can have them.
//! - `left` and `right`: a set of one hand's keys (the right hand's without its `-`) and the
//!   letters it is written as. An entry whose keys are not keys of that hand, whose spelling is
//!   not ASCII letters, or holds a vowel key's letter, is dropped with a warning; so is a second
//!   entry with a spelling already taken (spellings must read back one way).
//! - `outlines`: a whole outline (as a dictionary key) and how to write it, ahead of the table.
//!   Kept only if it reads back as the same strokes.
//!
//! Writing a stroke ([`Notation::render_stroke`]): extra keys, then `#` (unless digits are kept
//! and the stroke can be written with them, when it is Plover's whole), then the left hand, the
//! vowels and star as they are, a `-` when there is no vowel or star and the right hand has keys,
//! then the right hand. A hand is written in tokens: take the lowest key left, the table's
//! largest set of keys that starts at it, is all down and is contiguous among the keys still to
//! write (no such key between its lowest and highest key that is not part of it), write its
//! spelling, and carry on; a key no entry covers is written as its own letter. So with `SR`
//! written `V`, `SR` is `V` but `STR` stays `STR`.
//!
//! Reading ([`Notation::parse_stroke`]): text that is exactly Plover's notation of what it
//! parses to is Plover's; otherwise the short form (extra keys and `#` anywhere, the hands split
//! at the first vowel, star or `-`, each hand read greedily, longest spelling first, from the
//! table's spellings and that hand's own letters); otherwise Plover's notation read leniently.
//! So both `TPHAEPL` and `NAEM` read as the same stroke.
//!
//! A [`Split`] says how a stroke's hands divide into groups when something knows better than the
//! table: an `outlines` entry (each of its strokes split as its short form reads), or a plugin
//! that read the word and reported its sound groups ([`Notation::render_strokes_split`];
//! precedence: `outlines` entry > report > table). The overlay draws a stroke's groups fused on
//! its keyboard ([`Notation::diagram_groups`]), a compound entry as the entries its spelling is
//! made of (`TPHR`, `FL`, as `TP` `F` and `HR` `L`).

use super::layout::Layout;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

/// The name of a theory's notation file, beside its dictionaries. Hidden, so it is never read
/// as a dictionary.
pub const FILE_NAME: &str = ".keymux-notation.json";

/// How a stroke with the number key is written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NumberStyle {
    /// Plover's digits (`2R-R8`) for a stroke that can be written with them
    #[default]
    Digits,
    /// The number key and letters (`#TR-RL`)
    Letters,
}

/// One piece of a written stroke: its text and the stroke keys it stands for (`0` for the `-`
/// that only marks where the hands part). Joined, the texts are the written stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    pub keys: u32,
}

/// A theory's notation file, read against one layout.
#[derive(Debug, Default, PartialEq, Eq)]
struct Table {
    number: NumberStyle,
    /// (keys, spelling) of each hand, in file order
    left: Vec<(u32, String)>,
    right: Vec<(u32, String)>,
    /// What each hand's text is read with: the spellings and the hand's own key letters,
    /// longest first
    left_tokens: Vec<(String, u32)>,
    right_tokens: Vec<(String, u32)>,
    /// Whole outlines written their own way, and the way back
    outlines: HashMap<Vec<u32>, String>,
    by_short: HashMap<String, Vec<u32>>,
    /// The key groups each stroke of an `outlines` entry is written with (see [`Split`])
    outline_groups: HashMap<Vec<u32>, Vec<Split>>,
    /// Each compound entry's keys and the smaller entries (or single keys) its spelling is made
    /// of, in key order: `TPHR` (FL) is `TP` (F) and `HR` (L). An entry that is no compound of
    /// others is not here
    parts: HashMap<u32, Vec<(u32, String)>>,
}

/// How one stroke's hands split into key groups, each written as one piece.
///
/// The sets of two keys or more that go together (each within one hand). Every key of a hand in
/// no group is written alone. A stroke written by the table alone has no `Split`: the table
/// decides.
pub type Split = Vec<u32>;

/// How a layer writes and reads strokes: its [`Layout`] and, when the theory ships one, its
/// notation file. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notation {
    layout: Layout,
    table: Option<Arc<Table>>,
    /// The file's text, as read: what the overlay is sent to read the same notation
    source: Option<Arc<str>>,
}

impl From<Layout> for Notation {
    fn from(layout: Layout) -> Self {
        Self::plain(layout)
    }
}

impl Notation {
    /// Plover's notation in `layout`, what keymux writes with no notation file.
    #[must_use]
    pub const fn plain(layout: Layout) -> Self {
        Self {
            layout,
            table: None,
            source: None,
        }
    }

    /// The notation the first [`FILE_NAME`] in `roots`' folders gives (searched as
    /// dictionaries are, see [`super::source::find_in_folders`]), leaving out `skip` (the
    /// cache). Plain when there is none, or it cannot be read (logged).
    #[must_use]
    pub fn load(layout: Layout, roots: &[PathBuf], skip: &Path) -> Self {
        match super::source::find_in_folders(roots, skip, FILE_NAME) {
            Some(path) => Self::load_file(layout, &path),
            None => Self::plain(layout),
        }
    }

    /// The notation in the file at `path`; plain when it cannot be read (logged).
    #[must_use]
    pub fn load_file(layout: Layout, path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                warn!("steno notation {}: not read: {e}", path.display());
                return Self::plain(layout);
            }
        };
        match Self::from_json(layout.clone(), &text) {
            Ok(notation) => {
                tracing::info!("steno notation from {}", path.display());
                notation
            }
            Err(e) => {
                warn!("steno notation {}: not used: {e}", path.display());
                Self::plain(layout)
            }
        }
    }

    /// The notation a file's text gives. Entries that cannot be used are dropped with a
    /// warning; only text that is not a JSON object at all is an error.
    ///
    /// # Errors
    /// When `text` is not a JSON object.
    pub fn from_json(layout: Layout, text: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let object = value
            .as_object()
            .ok_or_else(|| "not a JSON object".to_string())?;
        let mut table = Table {
            number: match object.get("number").and_then(serde_json::Value::as_str) {
                Some("letters") => NumberStyle::Letters,
                Some("digits") | None => NumberStyle::Digits,
                Some(other) => {
                    warn!("steno notation: number {other:?} is not \"letters\" or \"digits\"; digits it is");
                    NumberStyle::Digits
                }
            },
            ..Table::default()
        };
        table.left = hand_entries(&layout, object.get("left"), Hand::Left);
        table.right = hand_entries(&layout, object.get("right"), Hand::Right);
        table.left_tokens = tokens(&layout, &table.left, Hand::Left);
        table.right_tokens = tokens(&layout, &table.right, Hand::Right);
        table.parts = compound_parts(&layout, &table.left)
            .into_iter()
            .chain(compound_parts(&layout, &table.right))
            .collect();
        let mut notation = Self {
            layout,
            table: Some(Arc::new(table)),
            source: Some(Arc::from(text)),
        };
        // Outlines are checked by reading them back with the table, so they come last
        let outlines = object
            .get("outlines")
            .and_then(serde_json::Value::as_object);
        let mut kept = Vec::new();
        for (raw, short) in outlines.into_iter().flatten() {
            let Some(short) = short.as_str() else {
                warn!("steno notation: outline {raw:?} is not written as text; dropped");
                continue;
            };
            let Some(strokes) = notation.layout.parse_strokes(raw) else {
                warn!("steno notation: outline {raw:?} is not stroke notation; dropped");
                continue;
            };
            if notation.parse_strokes(short).as_ref() != Some(&strokes) {
                warn!("steno notation: outline {raw:?} written {short:?} does not read back as {raw:?}; dropped");
                continue;
            }
            let groups = short
                .split('/')
                .map(|part| notation.written_groups(part))
                .collect::<Option<Vec<Split>>>();
            kept.push((strokes, short.to_string(), groups));
        }
        if let Some(table) = notation.table.as_mut().and_then(Arc::get_mut) {
            for (strokes, short, groups) in kept {
                table.by_short.insert(short.clone(), strokes.clone());
                if let Some(groups) = groups.filter(|g| g.len() == strokes.len()) {
                    table.outline_groups.insert(strokes.clone(), groups);
                }
                table.outlines.insert(strokes, short);
            }
        }
        notation.check_round_trips();
        Ok(notation)
    }

    /// The layout strokes are made of.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Whether this is Plover's notation alone, with no notation file.
    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.table.is_none()
    }

    /// The notation file's text as it was read; `None` for Plover's notation alone.
    #[must_use]
    pub const fn source(&self) -> Option<&Arc<str>> {
        self.source.as_ref()
    }

    /// One stroke, written in pieces (see [`Piece`]).
    #[must_use]
    pub fn render_stroke_pieces(&self, bits: u32) -> Vec<Piece> {
        self.render_stroke_pieces_split(bits, None)
    }

    /// One stroke, written in pieces with its hands split as `split` says (see [`Split`]), or by
    /// the table when `None`. A group that is not all down in `bits`, or holds keys of both
    /// hands, is not used.
    #[must_use]
    pub fn render_stroke_pieces_split(&self, bits: u32, split: Option<&[u32]>) -> Vec<Piece> {
        let layout = &self.layout;
        let plover = || {
            layout
                .render_stroke_parts(bits)
                .into_iter()
                .map(|(text, slot)| Piece {
                    text,
                    keys: slot.map_or(0, |slot| 1 << slot),
                })
                .collect()
        };
        let Some(table) = &self.table else {
            return plover();
        };
        let number_slot = layout.number_slot();
        let number = bits & (1 << number_slot) != 0;
        if number && table.number == NumberStyle::Digits && layout.can_be_digits(bits) {
            return plover();
        }
        let mut out = Vec::new();
        let extra_start = layout.extra_start();
        for (i, key) in layout.extra_keys().iter().enumerate() {
            let bit = 1 << (extra_start + i);
            if bits & bit != 0 {
                out.push(Piece {
                    text: key.char.to_string(),
                    keys: bit,
                });
            }
        }
        if number {
            out.push(Piece {
                text: self.char_of(number_slot).to_string(),
                keys: 1 << number_slot,
            });
        }
        let left_end = layout.left_end();
        let right_start = layout.right_start();
        let mask = |range: std::ops::Range<usize>| range.fold(0u32, |m, slot| m | (1 << slot));
        let middle = bits & mask(left_end..right_start);
        let right = bits & mask(right_start..number_slot);
        let left = bits & mask(0..left_end);
        let hand = |keys: u32, entries: &[(u32, String)], out: &mut Vec<Piece>| match split {
            Some(groups) => self.split_hand(keys, groups, entries, out),
            None => self.cluster(keys, entries, out),
        };
        hand(left, &table.left, &mut out);
        for slot in left_end..right_start {
            if middle & (1 << slot) != 0 {
                out.push(Piece {
                    text: self.char_of(slot).to_string(),
                    keys: 1 << slot,
                });
            }
        }
        if middle == 0 && right != 0 {
            out.push(Piece {
                text: "-".to_string(),
                keys: 0,
            });
        }
        hand(right, &table.right, &mut out);
        out
    }

    /// One stroke, written out.
    #[must_use]
    pub fn render_stroke(&self, bits: u32) -> String {
        if self.table.is_none() {
            return self.layout.render_stroke(bits);
        }
        self.render_stroke_pieces(bits)
            .into_iter()
            .map(|piece| piece.text)
            .collect()
    }

    /// An outline, its strokes separated by `/`: the file's own way of writing this exact
    /// outline if it has one, otherwise stroke by stroke.
    #[must_use]
    pub fn render_strokes(&self, strokes: &[u32]) -> String {
        let Some(table) = &self.table else {
            return self.layout.render_strokes(strokes);
        };
        if let Some(short) = table.outlines.get(strokes) {
            return short.clone();
        }
        strokes
            .iter()
            .map(|&bits| self.render_stroke(bits))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The file's own way of writing this exact outline, if it has one.
    #[must_use]
    pub fn outline_override(&self, strokes: &[u32]) -> Option<&str> {
        self.table
            .as_ref()?
            .outlines
            .get(strokes)
            .map(String::as_str)
    }

    /// An outline written for people when something says how its strokes split (see [`Split`]):
    /// the file's own way of writing this exact outline first, then `split` (one per stroke,
    /// such as a plugin reports for a word it read), then the table. **Precedence: `outlines`
    /// entry > `split` > table.**
    #[must_use]
    pub fn render_strokes_split(&self, strokes: &[u32], split: Option<&[Split]>) -> String {
        let split = split.filter(|split| split.len() == strokes.len());
        match (&self.table, split) {
            (Some(table), Some(split)) if !table.outlines.contains_key(strokes) => strokes
                .iter()
                .zip(split)
                .map(|(&bits, groups)| {
                    self.render_stroke_pieces_split(bits, Some(groups))
                        .into_iter()
                        .map(|piece| piece.text)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("/"),
            _ => self.render_strokes(strokes),
        }
    }

    /// How each stroke of this exact outline is split, when the file writes the outline its own
    /// way (its `outlines` entry); `None` when the table decides.
    #[must_use]
    pub fn outline_split(&self, strokes: &[u32]) -> Option<&[Split]> {
        self.table
            .as_ref()?
            .outline_groups
            .get(strokes)
            .map(Vec::as_slice)
    }

    /// The key groups the table writes one stroke with: each piece of two keys or more.
    #[must_use]
    pub fn table_split(&self, bits: u32) -> Split {
        self.render_stroke_pieces(bits)
            .into_iter()
            .filter(|piece| piece.keys.count_ones() > 1)
            .map(|piece| piece.keys)
            .collect()
    }

    /// The key groups to draw as one on a keyboard diagram for a stroke written with `split`
    /// (the table's when `None`), and each one's label: every piece of two keys or more of the
    /// hands, a compound entry as the entries its spelling is made of (`TPHR`, written `FL`, is
    /// `TP` labelled `F` and `HR` labelled `L`), down to the finest (a compound of compounds is
    /// taken apart too). Keys in no group stand alone. Empty for
    /// Plover's notation alone.
    #[must_use]
    pub fn diagram_groups(&self, bits: u32, split: Option<&[u32]>) -> Vec<(u32, String)> {
        let Some(table) = &self.table else {
            return Vec::new();
        };
        let layout = &self.layout;
        let mask = |range: std::ops::Range<usize>| range.fold(0u32, |m, slot| m | (1 << slot));
        let hands = mask(0..layout.left_end()) | mask(layout.right_start()..layout.number_slot());
        let mut out = Vec::new();
        for piece in self.render_stroke_pieces_split(bits, split) {
            if piece.keys.count_ones() < 2 || piece.keys & !hands != 0 {
                continue;
            }
            finest(&table.parts, piece.keys, piece.text, &mut out);
        }
        out
    }

    /// Read one stroke, Plover's notation or the short form; `None` for anything else and for a
    /// stroke with no keys.
    #[must_use]
    pub fn parse_stroke(&self, text: &str) -> Option<u32> {
        self.parse_bits(text).filter(|&bits| bits != 0)
    }

    /// Read an outline, strokes separated by `/` (a leading `/` is the empty stroke that marks a
    /// word boundary, as [`Layout::parse_strokes`] has it). A whole outline the file writes its
    /// own way is read back first.
    #[must_use]
    pub fn parse_strokes(&self, text: &str) -> Option<Vec<u32>> {
        let Some(table) = &self.table else {
            return self.layout.parse_strokes(text);
        };
        if let Some(strokes) = table.by_short.get(text) {
            return Some(strokes.clone());
        }
        let parts: Vec<&str> = text.split('/').collect();
        let mut strokes = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() {
                if i == 0 && parts.len() > 1 {
                    strokes.push(0);
                    continue;
                }
                return None;
            }
            strokes.push(self.parse_bits(part)?);
        }
        Some(strokes)
    }

    /// Like [`Self::parse_strokes`], for what people type: spaces around a slash are not part
    /// of a stroke.
    #[must_use]
    pub fn parse_strokes_trimmed(&self, text: &str) -> Option<Vec<u32>> {
        self.parse_strokes(&text.split('/').map(str::trim).collect::<Vec<_>>().join("/"))
    }

    /// One stroke's keys, `0` for a lone `-`.
    fn parse_bits(&self, text: &str) -> Option<u32> {
        let raw = self.layout.parse_stroke_bits(text);
        let Some(table) = &self.table else {
            return raw;
        };
        if let Some(bits) = raw {
            if self.layout.render_stroke(bits) == text {
                return Some(bits);
            }
        }
        self.parse_short(table, text, None).or(raw)
    }

    /// The key groups one stroke's written text splits its hands into (see [`Split`]): none for
    /// text that is exactly Plover's notation (every key its own letter), else the short form's
    /// pieces of two keys or more. `None` when it reads as neither.
    fn written_groups(&self, text: &str) -> Option<Split> {
        let table = self.table.as_ref()?;
        if let Some(bits) = self.layout.parse_stroke_bits(text) {
            if self.layout.render_stroke(bits) == text {
                return Some(Vec::new());
            }
        }
        let mut groups = Vec::new();
        self.parse_short(table, text, Some(&mut groups))?;
        Some(groups)
    }

    /// The short form: extra keys and `#` anywhere, the left hand up to the first vowel, star or
    /// `-`, those as they are, then the right hand.
    fn parse_short(
        &self,
        table: &Table,
        text: &str,
        mut groups: Option<&mut Vec<u32>>,
    ) -> Option<u32> {
        let layout = &self.layout;
        let extra_start = layout.extra_start();
        let number_slot = layout.number_slot();
        let mut bits = 0u32;
        let mut rest = String::with_capacity(text.len());
        for ch in text.chars() {
            if let Some(i) = layout.extra_keys().iter().position(|k| k.char == ch) {
                let bit = 1 << (extra_start + i);
                if bits & bit != 0 {
                    return None;
                }
                bits |= bit;
            } else if ch == self.char_of(number_slot) {
                if bits & (1 << number_slot) != 0 {
                    return None;
                }
                bits |= 1 << number_slot;
            } else {
                rest.push(ch);
            }
        }
        let left_end = layout.left_end();
        let right_start = layout.right_start();
        let middle_slot = |ch: char| (left_end..right_start).find(|&slot| self.char_of(slot) == ch);
        let is_middle = |ch: char| ch == '-' || middle_slot(ch).is_some();
        let start = rest.find(is_middle).unwrap_or(rest.len());
        let end = rest[start..]
            .find(|ch: char| !is_middle(ch))
            .map_or(rest.len(), |i| start + i);
        bits |= read_hand(&rest[..start], &table.left_tokens, groups.as_deref_mut())?;
        let mut cursor = left_end;
        let mut dashed = false;
        for ch in rest[start..end].chars() {
            if ch == '-' {
                if dashed {
                    return None;
                }
                dashed = true;
                continue;
            }
            let slot = middle_slot(ch).filter(|&slot| slot >= cursor)?;
            bits |= 1 << slot;
            cursor = slot + 1;
        }
        bits |= read_hand(&rest[end..], &table.right_tokens, groups)?;
        Some(bits)
    }

    /// Write one hand's keys as tokens: at the lowest key left, the largest entry starting
    /// there that is all down and contiguous among the keys still to write (no such key lies
    /// inside its span without being part of it), else that key's own letter.
    fn cluster(&self, mut keys: u32, entries: &[(u32, String)], out: &mut Vec<Piece>) {
        while keys != 0 {
            let low = keys.isolate_lowest_one();
            let best = entries
                .iter()
                .filter(|(chord, _)| {
                    chord.isolate_lowest_one() == low && chord & !keys == 0 && {
                        // Contiguous among the keys still to write: none of them lies between
                        // the chord's lowest and highest key without being part of it
                        let high = 31 - chord.leading_zeros();
                        let span = (u32::MAX >> (31 - high)) & !(low - 1);
                        keys & span & !chord == 0
                    }
                })
                .fold(None::<&(u32, String)>, |best, entry| match best {
                    Some(b) if b.0.count_ones() >= entry.0.count_ones() => Some(b),
                    _ => Some(entry),
                });
            let (chord, text) = best.map_or_else(
                || (low, self.char_of(low.trailing_zeros() as usize).to_string()),
                |(chord, text)| (*chord, text.clone()),
            );
            out.push(Piece { text, keys: chord });
            keys &= !chord;
        }
    }

    /// Write one hand's keys as `groups` split them: from the lowest key left, the group holding
    /// it (all down, all in this hand), written as the table's spelling for exactly those keys or
    /// else as their own letters, else the key's own letter.
    fn split_hand(
        &self,
        mut keys: u32,
        groups: &[u32],
        entries: &[(u32, String)],
        out: &mut Vec<Piece>,
    ) {
        let hand = keys;
        while keys != 0 {
            let low = keys.isolate_lowest_one();
            let group = groups
                .iter()
                .copied()
                .find(|&g| g & low != 0 && g & !keys == 0 && g & !hand == 0)
                .unwrap_or(low);
            let text = entries
                .iter()
                .find(|(chord, _)| *chord == group)
                .map_or_else(|| self.letters_of(group), |(_, text)| text.clone());
            out.push(Piece { text, keys: group });
            keys &= !group;
        }
    }

    /// Keys written as their own letters, in key order.
    fn letters_of(&self, keys: u32) -> String {
        (0..32)
            .filter(|slot| keys & (1 << slot) != 0)
            .map(|slot| self.char_of(slot))
            .collect()
    }

    fn char_of(&self, slot: usize) -> char {
        self.layout.key_at(slot).map_or('?', |key| key.char)
    }

    /// Every set of one hand's keys, written and read back, must come back as itself; a table
    /// whose spellings read back another way is reported once (it still works for the rest).
    fn check_round_trips(&self) {
        let wrong = self.round_trip_failures();
        if let Some((raw, written)) = wrong.first() {
            warn!(
                "steno notation: {} key set(s) do not read back as themselves, such as {raw} written {written}",
                wrong.len()
            );
        }
    }

    /// The sets of one hand's keys that do not read back as themselves once written: (Plover's
    /// notation, this notation).
    fn round_trip_failures(&self) -> Vec<(String, String)> {
        let layout = &self.layout;
        let mask = |range: std::ops::Range<usize>| range.fold(0u32, |m, slot| m | (1 << slot));
        let mut wrong = Vec::new();
        for hand in [Hand::Left, Hand::Right] {
            let keys = mask(hand_slots(layout, hand));
            let mut sub = keys;
            // Every non-empty subset of the hand's keys
            while sub != 0 {
                let written = self.render_stroke(sub);
                if self.parse_stroke(&written) != Some(sub) {
                    wrong.push((layout.render_stroke(sub), written));
                }
                sub = (sub - 1) & keys;
            }
        }
        wrong
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hand {
    Left,
    Right,
}

/// The slots of one hand.
const fn hand_slots(layout: &Layout, hand: Hand) -> std::ops::Range<usize> {
    match hand {
        Hand::Left => 0..layout.left_end(),
        Hand::Right => layout.right_start()..layout.number_slot(),
    }
}

/// One hand's table entries that can be used, in file order.
fn hand_entries(
    layout: &Layout,
    value: Option<&serde_json::Value>,
    hand: Hand,
) -> Vec<(u32, String)> {
    let Some(value) = value else {
        return Vec::new();
    };
    let side = if hand == Hand::Left { "left" } else { "right" };
    let Some(object) = value.as_object() else {
        warn!("steno notation: {side} is not an object; ignored");
        return Vec::new();
    };
    let mask = hand_slots(layout, hand).fold(0u32, |m, slot| m | (1 << slot));
    let middle: Vec<char> = (layout.left_end()..layout.right_start())
        .filter_map(|slot| layout.key_at(slot).map(|k| k.char))
        .collect();
    let mut entries: Vec<(u32, String)> = Vec::new();
    for (keys, spelling) in object {
        let written = if hand == Hand::Left {
            keys.clone()
        } else {
            format!("-{keys}")
        };
        let chord = layout
            .parse_stroke(&written)
            .filter(|&bits| bits & !mask == 0);
        let Some(chord) = chord else {
            warn!("steno notation: {side} {keys:?} is not a set of {side}-hand keys; dropped");
            continue;
        };
        let Some(spelling) = spelling.as_str() else {
            warn!("steno notation: {side} {keys:?} is not written as text; dropped");
            continue;
        };
        if spelling.is_empty() || !spelling.chars().all(|c| c.is_ascii_alphabetic()) {
            warn!("steno notation: {side} {keys:?} written {spelling:?}: not letters; dropped");
            continue;
        }
        let spelling = spelling.to_ascii_uppercase();
        if spelling.chars().any(|c| middle.contains(&c)) {
            warn!("steno notation: {side} {keys:?} written {spelling:?} has a vowel key's letter, which splits the hands; dropped");
            continue;
        }
        if entries.iter().any(|(_, taken)| *taken == spelling) {
            warn!("steno notation: {side} {keys:?} written {spelling:?}: that spelling is taken; dropped");
            continue;
        }
        if entries.iter().any(|(taken, _)| *taken == chord) {
            warn!("steno notation: {side} {keys:?} is listed twice; the first is kept");
            continue;
        }
        entries.push((chord, spelling));
    }
    entries
}

/// What one hand's text is read with: its entries' spellings and its keys' own letters, longest
/// first (an entry before a key letter of the same length).
fn tokens(layout: &Layout, entries: &[(u32, String)], hand: Hand) -> Vec<(String, u32)> {
    let mut tokens: Vec<(String, u32)> = entries
        .iter()
        .map(|(chord, spelling)| (spelling.clone(), *chord))
        .chain(
            hand_slots(layout, hand)
                .filter_map(|slot| layout.key_at(slot).map(|k| (k.char.to_string(), 1 << slot))),
        )
        .collect();
    tokens.sort_by_key(|(text, _)| std::cmp::Reverse(text.len()));
    tokens
}

/// One hand's keys from its text, greedily, longest token first; `None` if some text is left
/// that no token reads.
fn read_hand(
    mut text: &str,
    tokens: &[(String, u32)],
    mut groups: Option<&mut Vec<u32>>,
) -> Option<u32> {
    let mut bits = 0;
    while !text.is_empty() {
        let (token, keys) = tokens
            .iter()
            .find(|(token, _)| text.starts_with(token.as_str()))?;
        bits |= keys;
        if keys.count_ones() > 1 {
            if let Some(groups) = groups.as_deref_mut() {
                groups.push(*keys);
            }
        }
        text = &text[token.len()..];
    }
    Some(bits)
}

/// A group of keys as the finest groups its spelling is made of, through compounds of compounds
/// (`-RBGT`, `RKT`, is `-RBG` `RK` and `-T`, and `-RBG` is `-R` and `-BG` `K`): only the groups
/// of two keys or more go to `out`.
fn finest(
    parts: &HashMap<u32, Vec<(u32, String)>>,
    keys: u32,
    text: String,
    out: &mut Vec<(u32, String)>,
) {
    match parts.get(&keys) {
        Some(made_of) => {
            for (keys, text) in made_of {
                finest(parts, *keys, text.clone(), out);
            }
        }
        None if keys.count_ones() > 1 => out.push((keys, text)),
        None => {}
    }
}

/// Each compound entry of one hand and the smaller entries or single keys its spelling is made
/// of (see `Table::parts`): read from its lowest key, the entry with the most keys whose spelling
/// the rest of the text starts with, else the key's own letter, as long as the whole spelling is
/// used up. An entry its spelling cannot be made up of so is not a compound.
fn compound_parts(layout: &Layout, entries: &[(u32, String)]) -> Vec<(u32, Vec<(u32, String)>)> {
    fn go(
        layout: &Layout,
        others: &[&(u32, String)],
        keys: u32,
        text: &str,
    ) -> Option<Vec<(u32, String)>> {
        if keys == 0 {
            return text.is_empty().then(Vec::new);
        }
        let low = keys.isolate_lowest_one();
        let mut candidates: Vec<(u32, String)> = others
            .iter()
            .filter(|(chord, _)| chord.isolate_lowest_one() == low && chord & !keys == 0)
            .map(|(chord, spelling)| (*chord, spelling.clone()))
            .collect();
        candidates.sort_by_key(|(chord, _)| std::cmp::Reverse(chord.count_ones()));
        let letter = layout
            .key_at(low.trailing_zeros() as usize)
            .map(|key| key.char.to_string())?;
        candidates.push((low, letter));
        candidates.into_iter().find_map(|(chord, spelling)| {
            let rest = text.strip_prefix(spelling.as_str())?;
            let mut parts = go(layout, others, keys & !chord, rest)?;
            parts.insert(0, (chord, spelling));
            Some(parts)
        })
    }
    entries
        .iter()
        .filter_map(|(keys, spelling)| {
            let others: Vec<&(u32, String)> =
                entries.iter().filter(|(other, _)| other != keys).collect();
            let parts = go(layout, &others, *keys, spelling)?;
            (parts.len() > 1).then_some((*keys, parts))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a theory's table (the shape lyk's has).
    const TABLE: &str = r#"{
        "left": {"TPH": "N", "TP": "F", "HR": "L", "PH": "M", "TK": "D", "PW": "B",
                 "TKPW": "G", "SR": "V", "KP": "X", "SKWR": "J", "STKPW": "Z", "KWR": "Y",
                 "TPHR": "FL", "PHR": "PL"},
        "right": {"PB": "N", "PL": "M", "BG": "K", "FP": "CH", "RB": "SH", "PBLG": "J",
                  "PBG": "NG"}
    }"#;

    fn table(text: &str) -> Notation {
        Notation::from_json(Layout::plover_with(&['+']).unwrap(), text).unwrap()
    }

    fn plain() -> Notation {
        Notation::plain(Layout::plover_with(&['+']).unwrap())
    }

    fn raw(notation: &Notation, text: &str) -> u32 {
        notation.layout().parse_stroke(text).unwrap()
    }

    fn written(notation: &Notation, text: &str) -> String {
        notation.render_stroke(raw(notation, text))
    }

    #[test]
    fn key_groups_are_written_with_their_short_spelling() {
        let n = table(TABLE);
        assert_eq!(written(&n, "TPHAEPL"), "NAEM");
        assert_eq!(written(&n, "TPHROUR"), "FLOUR");
        assert_eq!(written(&n, "-PB"), "-N");
        assert_eq!(written(&n, "SKWRAPBLG"), "JAJ");
        assert_eq!(written(&n, "KAT"), "KAT");
        assert_eq!(written(&n, "PHR"), "PL");
        // Extra keys come first, as ever
        assert_eq!(written(&n, "+-PBG"), "+-NG");
    }

    #[test]
    fn a_chord_is_only_used_when_no_key_still_to_write_lies_inside_it() {
        let n = table(r#"{"left": {"SR": "V"}, "right": {"RB": "SH", "PB": "N", "FRB": "FRB"}}"#);
        assert_eq!(written(&n, "STR"), "STR");
        assert_eq!(written(&n, "SR"), "V");
        assert_eq!(written(&n, "-RPB"), "-RN");
        assert_eq!(written(&n, "-RB"), "-SH");
        // A spelling that is its own keys' letters only stops a shorter one from applying
        assert_eq!(written(&n, "-FRB"), "-FRB");
        assert_eq!(n.round_trip_failures(), []);
        for text in ["STR", "-RPB", "-FRB"] {
            assert_eq!(n.parse_stroke(&written(&n, text)), Some(raw(&n, text)));
        }
    }

    #[test]
    fn without_a_compound_the_tokens_follow_their_lowest_key() {
        let n = table(r#"{"left": {"TPH": "N", "HR": "L"}}"#);
        assert_eq!(written(&n, "TPHROUR"), "NROUR");
        assert_eq!(n.parse_stroke("NROUR"), Some(raw(&n, "TPHROUR")));
    }

    #[test]
    fn a_spelling_that_reads_back_as_other_keys_is_found() {
        assert_eq!(table(TABLE).round_trip_failures(), []);
        // `HR` written `T` reads back as the key `T`
        let bad = table(r#"{"left": {"HR": "T"}}"#);
        assert!(bad
            .round_trip_failures()
            .contains(&("HR".to_string(), "T".to_string())));
    }

    #[test]
    fn number_strokes_keep_digits_unless_the_file_says_letters() {
        let letters = table(r#"{"number": "letters"}"#);
        assert_eq!(written(&letters, "2R-R8"), "#TR-RL");
        assert_eq!(letters.parse_stroke("#TR-RL"), Some(raw(&letters, "2R-R8")));
        let digits = table(r#"{"left": {"TPH": "N"}}"#);
        assert_eq!(written(&digits, "2R-R8"), "2R-R8");
        assert_eq!(written(&plain(), "2R-R8"), "2R-R8");
        // A number stroke with no key that doubles as a digit is `#` and letters either way
        assert_eq!(written(&table(TABLE), "#-BG"), "#-K");
        assert_eq!(table(TABLE).parse_stroke("#-K"), Some(raw(&digits, "#-BG")));
    }

    #[test]
    fn plover_notation_stays_plover_and_the_short_form_reads_too() {
        let n = table(TABLE);
        let enemy = raw(&n, "TPHAEPL");
        assert_eq!(n.parse_stroke("TPHAEPL"), Some(enemy));
        assert_eq!(n.parse_stroke("NAEM"), Some(enemy));
        // `PL` is not how Plover writes `P-L`, so it is the short form: P H R
        assert_eq!(n.parse_stroke("PL"), Some(raw(&n, "PHR")));
        assert_eq!(n.parse_stroke("P-L"), Some(raw(&n, "P-L")));
        assert_eq!(n.parse_stroke("-N"), Some(raw(&n, "-PB")));
        assert_eq!(n.parse_stroke("FLOUR"), Some(raw(&n, "TPHROUR")));
        assert_eq!(n.parse_stroke("NAEQ"), None);
        assert_eq!(
            n.parse_strokes("/NAEM/KAT"),
            Some(vec![0, enemy, raw(&n, "KAT")])
        );
        assert_eq!(
            n.parse_strokes_trimmed("NAEM / KAT").map(|s| s.len()),
            Some(2)
        );
    }

    #[test]
    fn a_whole_outline_override_wins_and_one_that_does_not_read_back_is_dropped() {
        let n = table(
            r#"{"left": {"TPH": "N", "TPHR": "FL"},
                "outlines": {"TPHR-R": "NR-R", "KAT": "KOT", "TPHAEU/KAT": "NAEU/KAT"}}"#,
        );
        let fr = raw(&n, "TPHR-R");
        assert_eq!(n.render_strokes(&[fr]), "NR-R");
        assert_eq!(n.render_stroke(fr), "FL-R");
        assert_eq!(n.parse_strokes("NR-R"), Some(vec![fr]));
        assert_eq!(n.render_strokes(&[raw(&n, "KAT")]), "KAT");
        assert_eq!(n.outline_override(&[raw(&n, "KAT")]), None);
        assert_eq!(
            n.outline_override(&[raw(&n, "TPHAEU"), raw(&n, "KAT")]),
            Some("NAEU/KAT")
        );
    }

    #[test]
    fn unusable_entries_are_dropped_and_the_rest_kept() {
        let n = table(
            r#"{"left": {"TPH": "N", "PB": "Q", "TK": "", "PW": "B1", "HR": "LA", "SR": "N",
                         "KP": "x"},
                "right": {"T": "", "PB": "N", "AU": "M"}, "colour": "blue"}"#,
        );
        let t = n.table.as_ref().unwrap();
        assert_eq!(
            t.left.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            ["N", "X"]
        );
        assert_eq!(
            t.right.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            ["N"]
        );
        assert!(Notation::from_json(Layout::plover(), "[1, 2]").is_err());
        assert!(Notation::from_json(Layout::plover(), "not json").is_err());
    }

    #[test]
    fn written_strokes_read_back_as_themselves() {
        let tables = [plain(), table(TABLE), table(r#"{"number": "letters"}"#)];
        // A spread of strokes over every key, extra key and number key included
        let mut state = 0x2545_f491_u32;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let bits = state & ((1 << 24) - 1);
            if bits == 0 {
                continue;
            }
            for n in &tables {
                let text = n.render_stroke(bits);
                assert_eq!(n.parse_stroke(&text), Some(bits), "{text}");
            }
        }
    }

    #[test]
    fn with_no_table_it_is_plover_notation_exactly() {
        let n = plain();
        let layout = n.layout().clone();
        for text in ["KAT", "2R-R8", "+-D", "-PB", "#", "STKPWHRAO*EUFRPBLGTSDZ"] {
            let bits = layout.parse_stroke(text).unwrap();
            assert_eq!(n.render_stroke(bits), layout.render_stroke(bits));
            assert_eq!(n.parse_stroke(text), layout.parse_stroke(text));
            let pieces: String = n
                .render_stroke_pieces(bits)
                .into_iter()
                .map(|p| p.text)
                .collect();
            assert_eq!(pieces, layout.render_stroke(bits));
        }
        assert_eq!(n.parse_strokes("/S"), layout.parse_strokes("/S"));
        assert_eq!(n.parse_stroke("NAEM"), None);
    }

    #[test]
    fn a_compound_is_drawn_as_the_groups_its_spelling_is_made_of() {
        let n = table(TABLE);
        let groups = |text: &str| -> Vec<(String, String)> {
            n.diagram_groups(raw(&n, text), None)
                .into_iter()
                .map(|(keys, label)| (n.layout().render_stroke(keys), label))
                .collect()
        };
        // flower: TPHR is written FL, and drawn as F (TP) and L (HR)
        assert_eq!(
            groups("TPHROUR"),
            [("TP".into(), "F".into()), ("HR".into(), "L".into())]
        );
        assert_eq!(
            groups("TPHAEPL"),
            [("TPH".into(), "N".into()), ("-PL".into(), "M".into())]
        );
        // A compound of a compound: -RBGT (RKT) is -RBG (RK) and -T, and -RBG is -R and -BG (K)
        let nested = table(r#"{"right": {"BG": "K", "RBG": "RK", "RBGT": "RKT"}}"#);
        let rkt = nested.diagram_groups(raw(&nested, "-RBGT"), None);
        assert_eq!(rkt, [(raw(&nested, "-BG"), "K".to_string())]);
        // A single key is no group; vowels never are
        assert_eq!(groups("KAT"), Vec::<(String, String)>::new());
        assert_eq!(
            plain().diagram_groups(raw(&n, "TPHAEPL"), None),
            Vec::<(u32, String)>::new()
        );
    }

    #[test]
    fn an_outline_written_its_own_way_says_how_its_strokes_split() {
        let n = table(
            r#"{"left": {"TPH": "N"}, "right": {"RB": "SH", "PB": "N"},
                "outlines": {"ORB": "ORB", "EPB/SHRAOEUPB": "EN/SHRAOEUN"}}"#,
        );
        let orb = raw(&n, "ORB");
        // The table alone writes -RB as SH; this outline is O, R and B
        assert_eq!(n.render_stroke(orb), "OSH");
        assert_eq!(n.outline_split(&[orb]), Some(&[Vec::new()][..]));
        assert_eq!(
            n.diagram_groups(orb, Some(&[])),
            Vec::<(u32, String)>::new()
        );
        assert_eq!(n.diagram_groups(orb, None).len(), 1);
        let shine = [raw(&n, "EPB"), raw(&n, "SHRAOEUPB")];
        let split = n.outline_split(&shine).unwrap();
        assert_eq!(split, [vec![raw(&n, "-PB")], vec![raw(&n, "-PB")]]);
        assert_eq!(n.outline_split(&[raw(&n, "KAT")]), None);
    }

    #[test]
    fn a_reported_split_comes_after_the_outlines_entry_and_before_the_table() {
        let n = table(r#"{"left": {"TPH": "N", "HR": "L"}, "outlines": {"TPHO": "TPHO"}}"#);
        let flow = raw(&n, "TPHROE");
        assert_eq!(n.render_strokes(&[flow]), "NROE");
        // A plugin read it as F + L
        let split = vec![vec![raw(&n, "TP"), raw(&n, "HR")]];
        assert_eq!(n.render_strokes_split(&[flow], Some(&split)), "TPLOE");
        assert_eq!(
            n.parse_strokes(&n.render_strokes_split(&[flow], Some(&split))),
            Some(vec![flow])
        );
        // The outlines entry wins over a report; a report for other strokes is not used
        let no = raw(&n, "TPHO");
        assert_eq!(
            n.render_strokes_split(&[no], Some(&[vec![raw(&n, "TP")]])),
            "TPHO"
        );
        assert_eq!(n.render_strokes_split(&[flow], Some(&[])), "NROE");
        assert_eq!(n.table_split(flow), [raw(&n, "TPH")]);
        // A group not all down, or across both hands, is not used
        assert_eq!(
            n.render_strokes_split(&[flow], Some(&[vec![raw(&n, "SKWR")]])),
            "TPHROE"
        );
    }

    #[test]
    fn the_source_is_kept_for_the_overlay_and_plain_has_none() {
        assert_eq!(table(TABLE).source().map(|s| &**s), Some(TABLE));
        assert!(plain().source().is_none());
    }

    #[test]
    fn the_file_is_found_beside_the_dictionaries() {
        let dir = crate::test_dir::TestDir::new("keymux-notation-load");
        std::fs::create_dir_all(dir.join("lyk")).unwrap();
        std::fs::write(dir.join("lyk").join(FILE_NAME), r#"{"left": {"TPH": "N"}}"#).unwrap();
        let n = Notation::load(Layout::plover(), &[dir.to_path_buf()], &dir.join("cache"));
        assert!(!n.is_plain());
        assert_eq!(
            n.render_stroke(Layout::plover().parse_stroke("TPHO").unwrap()),
            "NO"
        );
        std::fs::write(dir.join("lyk").join(FILE_NAME), "{ broken").unwrap();
        let n = Notation::load(Layout::plover(), &[dir.to_path_buf()], &dir.join("cache"));
        assert!(n.is_plain());
    }
}
