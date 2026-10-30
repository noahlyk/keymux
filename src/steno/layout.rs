//! Steno stroke model: the keys a theory's strokes are built from, Plover's stroke notation
//! grammar, and the physical-key layout that maps QWERTY keys onto stroke keys.
//!
//! Every steno key - identity, order, even whether it exists at all - is theory-defined data
//! (see [`Layout`]), not something keymux hardcodes. [`Layout::plover`] is the one keymux ships
//! itself: Plover's own 23 keys, in Plover's own order, so a theory that configures nothing
//! reads and writes ordinary Plover-format dictionaries exactly as every other steno tool does.
//! A theory that wants something else - a different keyset, different group sizes, extra keys
//! beyond that shape entirely - says so in its own config; nothing here assumes Plover's
//! specific letters or counts.

use crate::config::KeyAction;
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::sync::OnceLock;

/// One stroke key: the character Plover-style notation writes it as, and the name a config
/// file gives it (`layout_overrides`, `overlay_overrides`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub char: char,
    pub name: String,
}

impl KeySpec {
    pub fn new(char: char, name: impl Into<String>) -> Self {
        Self {
            char,
            name: name.into(),
        }
    }
}

/// Whether `ch` is already stroke notation's, so no key - standard or theory-defined - may use
/// it as its own.
///
/// `-` marks the hand boundary, `/` separates strokes in a multi-stroke dictionary key, a
/// digit substitutes for whatever key carries it with the number key down, and whitespace has
/// no place inside a stroke at all.
#[must_use]
pub const fn reserved(ch: char) -> bool {
    matches!(ch, '-' | '/') || ch.is_ascii_digit() || ch.is_whitespace()
}

/// A theory's whole key layout: which keys exist, the grammar shape they sit in, and which
/// extra keys exist beyond that shape.
///
/// An extra key has no built-in meaning of its own - a theory gives one meaning through a
/// plugin (see [`super::plugin`]); keymux never special-cases a particular key's character or
/// name.
///
/// The grammar shape is Plover's: a left-hand cluster, then thumb keys before the star, the
/// star itself, thumb keys after it, a right-hand cluster, then the number key. Stroke notation
/// reads a key only once it is at or past the last one read, which is how the same letter
/// written on both hands (`R`, `T`, `S`...) tells them apart, and a `-` is only needed when
/// nothing from the middle group marks the hand boundary itself. Everything past that shape -
/// how many keys in each group, which letters, which (if any) double as digits, what else
/// exists beyond the 23 - is theory data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Layout {
    left: Vec<KeySpec>,
    pre_star: Vec<KeySpec>,
    star: KeySpec,
    post_star: Vec<KeySpec>,
    right: Vec<KeySpec>,
    number: KeySpec,
    /// Keys that write a digit with the number key down: (index into `flat()`, digit). In
    /// stroke order, so digits read in whatever order the theory's own number-bearing keys do.
    number_keys: Vec<(usize, char)>,
    extra: Vec<KeySpec>,
}

impl Layout {
    /// Plover's own 23 keys, in Plover's own order, with no extras - what every ordinary
    /// Plover-format dictionary assumes, and what a theory gets unless it configures otherwise.
    #[must_use]
    pub fn plover() -> Self {
        let k = KeySpec::new;
        Self {
            left: vec![
                k('S', "S-"),
                k('T', "T-"),
                k('K', "K-"),
                k('P', "P-"),
                k('W', "W-"),
                k('H', "H-"),
                k('R', "R-"),
            ],
            pre_star: vec![k('A', "A-"), k('O', "O-")],
            star: k('*', "*"),
            post_star: vec![k('E', "-E"), k('U', "-U")],
            right: vec![
                k('F', "-F"),
                k('R', "-R"),
                k('P', "-P"),
                k('B', "-B"),
                k('L', "-L"),
                k('G', "-G"),
                k('T', "-T"),
                k('S', "-S"),
                k('D', "-D"),
                k('Z', "-Z"),
            ],
            number: k('#', "#"),
            number_keys: vec![
                (0, '1'),
                (1, '2'),
                (3, '3'),
                (5, '4'),
                (7, '5'),
                (8, '0'),
                (12, '6'),
                (14, '7'),
                (16, '8'),
                (18, '9'),
            ],
            extra: Vec::new(),
        }
    }

    /// [`Self::plover`], plus a steno layer's own `extra_keys` (`config.ron`'s `StenoConfig`/
    /// `OverlayConfig` field of the same name) - the layout a real config actually builds,
    /// empty `extra_keys` behaving exactly like `Self::plover` alone.
    ///
    /// # Errors
    /// One message per problem found, not just the first (see [`Self::with_extra`]).
    pub fn plover_with(extra_keys: &[char]) -> Result<Self, Vec<String>> {
        let mut layout = Self::plover();
        let mut errors = Vec::new();
        for &ch in extra_keys {
            match layout.clone().with_extra(KeySpec::new(ch, ch.to_string())) {
                Ok(next) => layout = next,
                Err(mut key_errors) => errors.append(&mut key_errors),
            }
        }
        if errors.is_empty() {
            Ok(layout)
        } else {
            Err(errors)
        }
    }

    /// Build a layout from its pieces, validating that no key's character collides with
    /// stroke notation's own syntax or with another key in this same layout, and that no two
    /// keys share a config name either. `number_keys` names the keys that double as digits
    /// (in digit-reading order), by their config name - resolved against the keys given here.
    ///
    /// # Errors
    /// One message per problem found, not just the first.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        left: Vec<KeySpec>,
        pre_star: Vec<KeySpec>,
        star: KeySpec,
        post_star: Vec<KeySpec>,
        right: Vec<KeySpec>,
        number: KeySpec,
        number_keys: Vec<(String, char)>,
        extra: Vec<KeySpec>,
    ) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let all: Vec<&KeySpec> = left
            .iter()
            .chain(pre_star.iter())
            .chain(std::iter::once(&star))
            .chain(post_star.iter())
            .chain(right.iter())
            .chain(std::iter::once(&number))
            .chain(extra.iter())
            .collect();
        for key in &all {
            if reserved(key.char) {
                errors.push(format!(
                    "\"{}\" can't use {:?}: that character is part of stroke notation's own syntax",
                    key.name, key.char
                ));
            }
        }
        let mut seen_chars = std::collections::HashSet::new();
        let mut seen_names = std::collections::HashSet::new();
        for key in &all {
            if !seen_chars.insert(key.char) {
                errors.push(format!("{:?} is used by more than one key", key.char));
            }
            if !seen_names.insert(key.name.as_str()) {
                errors.push(format!("\"{}\" is used by more than one key", key.name));
            }
        }
        let flat_len = left.len() + pre_star.len() + 1 + post_star.len() + right.len() + 1;
        let mut resolved_number_keys = Vec::with_capacity(number_keys.len());
        for (name, digit) in number_keys {
            let index = left
                .iter()
                .chain(pre_star.iter())
                .chain(std::iter::once(&star))
                .chain(post_star.iter())
                .chain(right.iter())
                .position(|k| k.name == name);
            match index {
                Some(index) => resolved_number_keys.push((index, digit)),
                None => errors.push(format!(
                    "number key {digit:?} names an unknown steno key \"{name}\""
                )),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        debug_assert!(flat_len > 0);
        Ok(Self {
            left,
            pre_star,
            star,
            post_star,
            right,
            number,
            number_keys: resolved_number_keys,
            extra,
        })
    }

    /// Every standard-grammar key (not counting extras), in notation-parsing order.
    fn flat(&self) -> Vec<&KeySpec> {
        self.left
            .iter()
            .chain(self.pre_star.iter())
            .chain(std::iter::once(&self.star))
            .chain(self.post_star.iter())
            .chain(self.right.iter())
            .chain(std::iter::once(&self.number))
            .collect()
    }

    /// Index of the first slot past the left hand (the left hand itself is `0..left_end()`).
    #[must_use]
    pub const fn left_end(&self) -> usize {
        self.left.len()
    }

    /// Index of the star.
    #[must_use]
    pub const fn star_slot(&self) -> usize {
        self.left.len() + self.pre_star.len()
    }

    /// First slot a `-` allows: after it, the next key is the first past the star, never a
    /// thumb key before it or the star itself.
    #[must_use]
    pub const fn after_hyphen(&self) -> usize {
        self.star_slot() + 1
    }

    /// Index of the first right-hand key.
    #[must_use]
    pub const fn right_start(&self) -> usize {
        self.after_hyphen() + self.post_star.len()
    }

    /// Index of the number key.
    #[must_use]
    pub const fn number_slot(&self) -> usize {
        self.right_start() + self.right.len()
    }

    /// Index of the first extra key (beyond the standard grammar shape).
    #[must_use]
    pub const fn extra_start(&self) -> usize {
        self.number_slot() + 1
    }

    /// The key at stroke-bit index `slot` (`0..self.len()`), standard grammar keys first in
    /// parsing order, then extras - the same order a stroke's bits are read in.
    #[must_use]
    pub fn key_at(&self, slot: usize) -> Option<&KeySpec> {
        let left_end = self.left_end();
        let star_slot = self.star_slot();
        let after_hyphen = self.after_hyphen();
        let right_start = self.right_start();
        let number_slot = self.number_slot();
        let extra_start = self.extra_start();
        if slot < left_end {
            self.left.get(slot)
        } else if slot < star_slot {
            self.pre_star.get(slot - left_end)
        } else if slot == star_slot {
            Some(&self.star)
        } else if slot < right_start {
            self.post_star.get(slot - after_hyphen)
        } else if slot < number_slot {
            self.right.get(slot - right_start)
        } else if slot == number_slot {
            Some(&self.number)
        } else {
            self.extra.get(slot.checked_sub(extra_start)?)
        }
    }

    /// Paper-tape order: the number key first, then every other standard key in stroke order -
    /// how a paper tape machine prints a stroke, and how keymux's own tape reads too.
    #[must_use]
    pub fn paper_order(&self) -> Vec<usize> {
        let number_slot = self.number_slot();
        std::iter::once(number_slot).chain(0..number_slot).collect()
    }

    /// How many stroke keys this layout has in total, standard grammar keys plus extras.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.extra_start() + self.extra.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The extra keys beyond the standard grammar shape, in bit order from [`Self::extra_start`].
    #[must_use]
    pub fn extra_keys(&self) -> &[KeySpec] {
        &self.extra
    }

    /// Add one more extra key beyond the standard grammar shape, validated the same way
    /// [`Self::build`] validates every key: its character can't be stroke notation's own
    /// syntax, and neither its character nor its name can collide with a key this layout
    /// already has.
    ///
    /// # Errors
    /// One message per problem found, not just the first.
    pub fn with_extra(mut self, key: KeySpec) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        if reserved(key.char) {
            errors.push(format!(
                "\"{}\" can't use {:?}: that character is part of stroke notation's own syntax",
                key.name, key.char
            ));
        }
        let taken = self
            .flat()
            .into_iter()
            .chain(self.extra.iter())
            .any(|k| k.char == key.char);
        if taken {
            errors.push(format!("{:?} is used by more than one key", key.char));
        }
        let name_taken = self
            .flat()
            .into_iter()
            .chain(self.extra.iter())
            .any(|k| k.name == key.name);
        if name_taken {
            errors.push(format!("\"{}\" is used by more than one key", key.name));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        self.extra.push(key);
        Ok(self)
    }

    /// Look up a stroke slot by its config name (`"S-"`, `"*"`, `"-E"`, an extra key's own
    /// name, ...).
    #[must_use]
    pub fn slot_by_name(&self, name: &str) -> Option<usize> {
        self.flat()
            .iter()
            .position(|k| k.name == name)
            .or_else(|| {
                self.extra
                    .iter()
                    .position(|k| k.name == name)
                    .map(|i| self.extra_start() + i)
            })
    }

    /// Parse one stroke in Plover's notation: `KAT`, `TEFT`, `-PB`, `1-9`, `12K` or `#S`.
    ///
    /// Letters and digits must come in key order, a `-` separates the left hand from the right
    /// when no vowel or star does, and the number key may stand anywhere, once. A digit writes
    /// its key and implies the number key. Returns `None` for anything rejected, for a stroke
    /// with no keys, and for multi-stroke dictionary keys (those contain `/`).
    #[must_use]
    pub fn parse_stroke(&self, text: &str) -> Option<u32> {
        let bits = self.parse_stroke_bits(text)?;
        (bits != 0).then_some(bits)
    }

    fn parse_stroke_bits(&self, text: &str) -> Option<u32> {
        let flat = self.flat();
        let number_slot = self.number_slot();
        let after_hyphen = self.after_hyphen();
        let extra_start = self.extra_start();
        let mut bits = 0u32;
        let mut cursor = 0usize;
        let mut number = false;
        let mut hash_seen = false;
        let mut extras_seen = 0u32;
        for ch in text.chars() {
            // An extra key marks the stroke as a whole, wherever it falls in the text, and
            // does not move the cursor the standard keys parse against
            if let Some(i) = self.extra.iter().position(|k| k.char == ch) {
                if extras_seen & (1 << i) != 0 {
                    return None;
                }
                extras_seen |= 1 << i;
                bits |= 1 << (extra_start + i);
                continue;
            }
            if ch == self.number.char {
                if hash_seen {
                    return None;
                }
                hash_seen = true;
                number = true;
                continue;
            }
            // A hyphen only makes sense while the left hand and star are still to be written
            if ch == '-' {
                if cursor > after_hyphen + 1 {
                    return None;
                }
                cursor = cursor.max(after_hyphen);
                continue;
            }
            if let Some(&(slot, _)) = self.number_keys.iter().find(|(_, digit)| *digit == ch) {
                if slot < cursor {
                    return None;
                }
                bits |= 1 << slot;
                cursor = slot + 1;
                number = true;
                continue;
            }
            let offset = flat[cursor..number_slot]
                .iter()
                .position(|k| k.char == ch)?;
            let slot = cursor + offset;
            bits |= 1 << slot;
            cursor = slot + 1;
        }
        if number {
            bits |= 1 << number_slot;
        }
        Some(bits)
    }

    /// Parse one stroke, or several separated by `/` as in a multi-stroke dictionary key.
    ///
    /// Such a key is `"HRAOUP/AER"`. `None` if any part isn't a valid stroke, which includes
    /// empty parts such as a trailing `/`. A leading `/` is the empty stroke that marks a word
    /// boundary (`"/S"`), kept as the stroke `0`, and a lone `-` is that empty stroke written out.
    #[must_use]
    pub fn parse_strokes(&self, text: &str) -> Option<Vec<u32>> {
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
            strokes.push(self.parse_stroke_bits(part)?);
        }
        Some(strokes)
    }

    /// Like [`Self::parse_strokes`], for what people type: spaces around a slash are not part
    /// of a stroke.
    #[must_use]
    pub fn parse_strokes_trimmed(&self, text: &str) -> Option<Vec<u32>> {
        self.parse_strokes(&text.split('/').map(str::trim).collect::<Vec<_>>().join("/"))
    }

    /// Render a stroke bitmask in Plover's canonical notation.
    ///
    /// With the number key down, the keys that carry a digit are written as digits (`1`, `-9`,
    /// `12K`) and the number key's own character is dropped. With no such key, it is written.
    /// A `-` stands between the left hand and the right when no vowel or star does.
    #[must_use]
    pub fn render_stroke(&self, bits: u32) -> String {
        let flat = self.flat();
        let number_slot = self.number_slot();
        let left_end = self.left_end();
        let right_start = self.right_start();
        let down = |slot: usize| bits & (1 << slot) != 0;
        let number = down(number_slot);
        let digits = number && self.number_keys.iter().any(|(slot, _)| down(*slot));
        let key = |slot: usize| {
            if digits {
                if let Some(&(_, digit)) = self.number_keys.iter().find(|(s, _)| *s == slot) {
                    return digit;
                }
            }
            flat[slot].char
        };
        let mut out = String::new();
        for (i, k) in self.extra.iter().enumerate() {
            if down(self.extra_start() + i) {
                out.push(k.char);
            }
        }
        if number && !digits {
            out.push(self.number.char);
        }
        out.extend((0..left_end).filter(|&slot| down(slot)).map(key));
        // With no vowel or star in between, right-hand letters would read as left-hand ones,
        // so a dash marks the boundary.
        let middle = (left_end..right_start).any(down);
        let right = (right_start..number_slot).any(down);
        if !middle && right {
            out.push('-');
        }
        out.extend((left_end..number_slot).filter(|&slot| down(slot)).map(key));
        out
    }

    /// Several strokes, each in canonical notation, separated by `/`: a dictionary key.
    #[must_use]
    pub fn render_strokes(&self, strokes: &[u32]) -> String {
        strokes
            .iter()
            .map(|&bits| self.render_stroke(bits))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// A stroke as typed text when it has no translation, such as `STPHA-UBGS`.
    ///
    /// The left hand and its thumb come first, then a hyphen, then the star, the right thumb
    /// and the right hand. The hyphen is always there, so the two hands read the same whatever
    /// keys are down.
    #[must_use]
    pub fn render_raw_stroke(&self, bits: u32) -> String {
        let flat = self.flat();
        let star_slot = self.star_slot();
        let number_slot = self.number_slot();
        let letters = |range: std::ops::Range<usize>| -> String {
            range
                .filter(|i| bits & (1 << i) != 0)
                .map(|i| flat[i].char)
                .collect()
        };
        let extras: String = self
            .extra
            .iter()
            .enumerate()
            .filter(|(i, _)| bits & (1 << (self.extra_start() + i)) != 0)
            .map(|(_, k)| k.char)
            .collect();
        let number = (bits & (1 << number_slot) != 0).then_some(self.number.char);
        let star = (bits & (1 << star_slot) != 0).then_some(self.star.char);
        let left = letters(0..star_slot);
        let right = letters(star_slot + 1..number_slot);
        let mut out = extras;
        if let Some(c) = number {
            out.push(c);
        }
        // The hyphen marks where the left hand ends and the right begins; with nothing from
        // either hand (an extra key or the number on its own), there are no two hands for it
        // to mark.
        if left.is_empty() && star.is_none() && right.is_empty() {
            return out;
        }
        out.push_str(&left);
        out.push('-');
        if let Some(c) = star {
            out.push(c);
        }
        out.push_str(&right);
        out
    }

    /// Combine a preset with per-layer overrides into a physical-key → stroke-bit map.
    ///
    /// Returns every problem found, not just the first.
    pub fn build_key_map(
        &self,
        preset: &HashMap<String, KeyCode>,
        overrides: &HashMap<String, KeyAction>,
    ) -> Result<HashMap<KeyCode, u32>, Vec<String>> {
        let mut errors = Vec::new();
        let mut assigned: HashMap<String, KeyCode> = preset.clone();

        for (slot, action) in overrides {
            // A key's own name takes this literally, even one that is itself `+` (an extra
            // key may be named after its own stroke character) - only once that fails is `+`
            // read as joining several keys onto one physical key, e.g. "A-+O-" on V.
            let resolves = self.slot_by_name(slot).is_some()
                || slot.split('+').all(|part| self.slot_by_name(part).is_some());
            if !resolves {
                errors.push(format!("layout override for unknown steno key \"{slot}\""));
                continue;
            }
            match key_from_action(action) {
                Some(key) => {
                    assigned.insert(slot.clone(), key);
                }
                None => errors.push(format!(
                    "layout override for \"{slot}\" must be Key(KC_*), not a layer or macro action"
                )),
            }
        }

        let mut keys: HashMap<KeyCode, u32> = HashMap::new();
        let mut owners: HashMap<KeyCode, &str> = HashMap::new();
        let flat = self.flat();
        for (slot_index, k) in flat.iter().enumerate() {
            let Some(&key) = assigned.get(&k.name) else {
                errors.push(format!("steno key \"{}\" has no physical key", k.name));
                continue;
            };
            if let Some(other) = owners.insert(key, k.name.as_str()) {
                errors.push(format!(
                    "{key:?} is used by both \"{other}\" and \"{}\"",
                    k.name
                ));
                continue;
            }
            keys.insert(key, 1 << slot_index);
        }

        // An extra key beyond the standard grammar gets a physical key only if something gave
        // it one; no error if not, since most layouts have no use for it. Checked before the
        // preset's own "|" extras below, so a user override onto one of those keys takes it
        // over cleanly.
        for (i, k) in self.extra.iter().enumerate() {
            let Some(&key) = assigned.get(&k.name) else {
                continue;
            };
            if let Some(other) = owners.insert(key, k.name.as_str()) {
                errors.push(format!(
                    "{key:?} is used by both \"{other}\" and \"{}\"",
                    k.name
                ));
                continue;
            }
            keys.insert(key, 1 << (self.extra_start() + i));
        }

        // A combined key presses several stroke keys at once, e.g. "A-+O-" on V. Its bits are
        // the union of its parts - but only once `name` fails as a key's own literal name, the
        // same preference the override loop above already gives it.
        let mut combined: Vec<(&String, KeyCode)> = assigned
            .iter()
            .filter(|(name, _)| name.contains('+') && self.slot_by_name(name).is_none())
            .map(|(name, &key)| (name, key))
            .collect();
        combined.sort_by(|a, b| a.0.cmp(b.0));
        for (name, key) in combined {
            let Some(mask) = name
                .split('+')
                .map(|part| self.slot_by_name(part).map(|index| 1 << index))
                .try_fold(0, |mask, bit| bit.map(|bit| mask | bit))
            else {
                errors.push(format!(
                    "combined key \"{name}\" names an unknown steno key"
                ));
                continue;
            };
            if let Some(other) = owners.insert(key, name.as_str()) {
                errors.push(format!(
                    "{key:?} is used by both \"{other}\" and \"{name}\""
                ));
                continue;
            }
            keys.insert(key, mask);
        }

        // Extra keys come last and give way to anything already bound, so a user override
        // that moves a stroke key onto one of them keeps working.
        let mut extras: Vec<(&String, KeyCode)> = assigned
            .iter()
            .filter(|(name, _)| name.contains(EXTRA_KEY_SEPARATOR))
            .map(|(name, &key)| (name, key))
            .collect();
        extras.sort_by(|a, b| a.0.cmp(b.0));
        for (name, key) in extras {
            let Some(slot) = extra_key_slot(self, name) else {
                errors.push(format!("extra key \"{name}\" names an unknown steno key"));
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = owners.entry(key) {
                entry.insert(name.as_str());
                keys.insert(key, 1 << slot);
            }
        }

        if errors.is_empty() {
            Ok(keys)
        } else {
            Err(errors)
        }
    }
}

/// The layout keymux parses and renders with when nothing says otherwise: [`Layout::plover`],
/// built once.
fn default_layout() -> &'static Layout {
    static DEFAULT: OnceLock<Layout> = OnceLock::new();
    DEFAULT.get_or_init(Layout::plover)
}

/// [`Layout::parse_stroke`] against [`default_layout`].
#[must_use]
pub fn parse_stroke(text: &str) -> Option<u32> {
    default_layout().parse_stroke(text)
}

/// [`Layout::parse_strokes`] against [`default_layout`].
#[must_use]
pub fn parse_strokes(text: &str) -> Option<Vec<u32>> {
    default_layout().parse_strokes(text)
}

/// [`Layout::parse_strokes_trimmed`] against [`default_layout`].
#[must_use]
pub fn parse_strokes_trimmed(text: &str) -> Option<Vec<u32>> {
    default_layout().parse_strokes_trimmed(text)
}

/// [`Layout::render_stroke`] against [`default_layout`].
#[must_use]
pub fn render_stroke(bits: u32) -> String {
    default_layout().render_stroke(bits)
}

/// [`Layout::render_strokes`] against [`default_layout`].
#[must_use]
pub fn render_strokes(strokes: &[u32]) -> String {
    default_layout().render_strokes(strokes)
}

/// [`Layout::render_raw_stroke`] against [`default_layout`].
#[must_use]
pub fn render_raw_stroke(bits: u32) -> String {
    default_layout().render_raw_stroke(bits)
}

/// [`Layout::slot_by_name`] against [`default_layout`].
#[must_use]
pub fn slot_by_name(name: &str) -> Option<usize> {
    default_layout().slot_by_name(name)
}

/// [`Layout::build_key_map`] against [`default_layout`].
pub fn build_key_map(
    preset: &HashMap<String, KeyCode>,
    overrides: &HashMap<String, KeyAction>,
) -> Result<HashMap<KeyCode, u32>, Vec<String>> {
    default_layout().build_key_map(preset, overrides)
}

/// Load a built-in layout preset by name.
pub fn preset(name: &str) -> Result<HashMap<String, KeyCode>, String> {
    let text = match name {
        "qwerty" => include_str!("layouts/qwerty.ron"),
        other => return Err(format!("unknown steno layout preset \"{other}\"")),
    };
    let table: HashMap<String, KeyAction> =
        ron::from_str(text).map_err(|e| format!("built-in preset \"{name}\" is invalid: {e}"))?;
    table
        .into_iter()
        .map(|(slot, action)| {
            key_from_action(&action)
                .map(|key| (slot.clone(), key))
                .ok_or_else(|| format!("built-in preset \"{name}\": \"{slot}\" must be Key(KC_*)"))
        })
        .collect()
}

/// Extract the physical key from a layout entry. Only plain `Key(KC_*)` is allowed.
#[must_use]
pub const fn key_from_action(action: &KeyAction) -> Option<KeyCode> {
    match action {
        KeyAction::Key(key) => Some(*key),
        _ => None,
    }
}

/// Separates a stroke key from a label in a preset entry that gives it an extra
/// physical key: `"S-|Q"` makes Q press S- as well as the key `"S-"` names. An extra
/// key never replaces the main one, and is skipped if a user override already took it.
const EXTRA_KEY_SEPARATOR: char = '|';

/// The stroke key an extra-key entry such as `"S-|Q"` is for.
fn extra_key_slot(layout: &Layout, name: &str) -> Option<usize> {
    name.split_once(EXTRA_KEY_SEPARATOR)
        .and_then(|(slot, _)| layout.slot_by_name(slot))
}

/// The extra physical keys in a preset as (stroke slot, key), ordered by slot then name.
#[must_use]
pub fn extra_keys(preset: &HashMap<String, KeyCode>) -> Vec<(usize, KeyCode)> {
    let layout = default_layout();
    let mut named: Vec<(&String, usize, KeyCode)> = preset
        .iter()
        .filter_map(|(name, &key)| extra_key_slot(layout, name).map(|slot| (name, slot, key)))
        .collect();
    named.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    named
        .into_iter()
        .map(|(_, slot, key)| (slot, key))
        .collect()
}

/// A preset with only its main key for each stroke key, no extra keys. What the
/// `layout` and `keys` commands describe, so each stroke key has one answer.
#[must_use]
pub fn without_extra_keys(preset: HashMap<String, KeyCode>) -> HashMap<String, KeyCode> {
    preset
        .into_iter()
        .filter(|(name, _)| !name.contains(EXTRA_KEY_SEPARATOR))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test layout exactly like `Layout::plover`, but with a `+` "join" extra key - the
    /// shape every theory that wants one would build, not anything keymux treats specially.
    fn with_join() -> Layout {
        let mut layout = Layout::plover();
        layout.extra.push(KeySpec::new('+', "join"));
        layout
    }

    #[test]
    fn parses_known_strokes() {
        let layout = Layout::plover();
        // K + A + right T = "cat"
        assert_eq!(layout.parse_stroke("KAT"), Some((1 << 2) | (1 << 7) | (1 << 18)));
        // Right-only strokes need the dash
        assert_eq!(layout.parse_stroke("-PB"), Some((1 << 14) | (1 << 15)));
        // T E F T: the vowel separates the left T from the right F and T
        assert_eq!(
            layout.parse_stroke("TEFT"),
            Some((1 << 1) | (1 << 10) | (1 << 12) | (1 << 18))
        );
        // Numbers start with #
        assert_eq!(layout.parse_stroke("#T"), Some((1 << 22) | (1 << 1)));
    }

    #[test]
    fn rejects_non_strokes() {
        let layout = Layout::plover();
        assert_eq!(layout.parse_stroke("KPA/TKAOEU"), None);
        assert_eq!(layout.parse_stroke("xyz"), None);
        assert_eq!(layout.parse_stroke(""), None);
        // Out of order: a left-hand letter can't come after a vowel without a dash
        assert_eq!(layout.parse_stroke("AK"), None);
    }

    #[test]
    fn several_strokes_are_separated_by_a_slash() {
        let layout = Layout::plover();
        let one = layout.parse_stroke("HRAOUP").unwrap();
        let two = layout.parse_stroke("AER").unwrap();
        assert_eq!(layout.parse_strokes("HRAOUP"), Some(vec![one]));
        assert_eq!(layout.parse_strokes("HRAOUP/AER"), Some(vec![one, two]));
        assert_eq!(
            layout.parse_strokes("HRAOUP/AER/HRAOUP"),
            Some(vec![one, two, one])
        );
        // Spaces around a slash are not part of the stroke, though Plover itself would refuse them
        assert_eq!(
            layout.parse_strokes_trimmed("HRAOUP / AER"),
            Some(vec![one, two])
        );
        assert_eq!(layout.parse_strokes("HRAOUP / AER"), None);
        // Anything wrong anywhere makes the whole thing invalid, never a shorter trigger
        assert_eq!(layout.parse_strokes("HRAOUP/"), None);
        assert_eq!(layout.parse_strokes("HRAOUP//AER"), None);
        assert_eq!(layout.parse_strokes("HRAOUP/ZZZ"), None);
        assert_eq!(layout.parse_strokes(""), None);
        // A leading slash is the empty stroke that marks a word boundary, as in "/S"
        assert_eq!(layout.parse_strokes("/AER"), Some(vec![0, two]));
    }

    #[test]
    fn render_round_trips_known_strokes() {
        let layout = Layout::plover();
        for text in [
            "KAT",
            "-PB",
            "TEFT",
            "#K",
            "2",
            "1-9",
            "12K",
            "STKPWHR",
            "-FRPBLGTSDZ",
            "*",
            "TK-PB",
            "R-R",
        ] {
            let bits = layout
                .parse_stroke(text)
                .unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(layout.render_stroke(bits), text, "rendering {text}");
        }
    }

    #[test]
    fn a_raw_stroke_has_one_hyphen_between_the_hands_and_the_star_on_the_right() {
        let layout = with_join();
        for (text, raw) in [
            // Left hand and its thumb, a hyphen, then the right thumb and right hand
            ("STPWHAUBGSZ", "STPWHA-UBGSZ"),
            ("KAT", "KA-T"),
            ("-PB", "-PB"),
            ("TEFT", "T-EFT"),
            ("*", "-*"),
            ("STPHAO*EUFRPB", "STPHAO-*EUFRPB"),
            ("#T", "#T-"),
            ("#*", "#-*"),
            // Nothing from either hand: no hyphen to mark a boundary between them
            ("+", "+"),
        ] {
            let bits = layout
                .parse_stroke(text)
                .unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(layout.render_raw_stroke(bits), raw, "rendering {text}");
        }
    }

    /// Plover's own answers for how a set of keys is written and how a spelling is read. See
    /// `tools/plover-golden`.
    #[test]
    fn stroke_notation_matches_plovers_for_every_reference_case() {
        let layout = Layout::plover();
        let data: serde_json::Value =
            serde_json::from_str(include_str!("testdata/stroke_golden.json")).unwrap();
        for case in data["render"].as_array().unwrap() {
            let bits = case[0].as_array().unwrap().iter().fold(0u32, |bits, key| {
                bits | 1 << layout.slot_by_name(key.as_str().unwrap()).unwrap()
            });
            let steno = case[1].as_str().unwrap();
            assert_eq!(layout.render_stroke(bits), steno, "writing {:?}", case[0]);
            assert_eq!(
                layout.parse_strokes(steno),
                Some(vec![bits]),
                "reading {steno}"
            );
        }
        let mut mismatches = Vec::new();
        for case in data["parse"].as_array().unwrap() {
            let text = case[0].as_str().unwrap();
            // Plover reads "" as an outline with no strokes, which nothing can use
            if text.is_empty() {
                continue;
            }
            let expected: Option<Vec<String>> = case[1].as_array().map(|strokes| {
                strokes
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect()
            });
            let got = layout
                .parse_strokes(text)
                .map(|strokes| strokes.into_iter().map(|s| layout.render_stroke(s)).collect::<Vec<_>>());
            if got != expected {
                mismatches.push(format!("{text:?}: Plover {expected:?}, keymux {got:?}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} spellings read differently from Plover, such as:\n{}",
            mismatches.len(),
            mismatches
                .iter()
                .take(25)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn render_round_trips_sampled_masks() {
        let layout = Layout::plover();
        // Deterministic sample across the 23-bit space
        let mut state: u32 = 0x1234_5678;
        for _ in 0..20_000 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let bits = state & ((1 << 23) - 1);
            if bits == 0 {
                continue;
            }
            let text = layout.render_stroke(bits);
            assert_eq!(layout.parse_stroke(&text), Some(bits), "stroke {text}");
        }
    }

    #[test]
    fn qwerty_preset_covers_every_steno_key_once() {
        let layout = Layout::plover();
        let main_only = without_extra_keys(preset("qwerty").unwrap());
        let map = layout.build_key_map(&main_only, &HashMap::new()).unwrap();
        // One main physical key per stroke key; the default preset has no combined keys
        assert_eq!(map.len(), 23);
        let single = map
            .values()
            .filter(|bits| bits.count_ones() == 1)
            .fold(0, |all, bits| all | bits);
        assert_eq!(single.count_ones(), 23);
    }

    #[test]
    fn q_g_and_the_number_row_are_extra_keys_for_s_star_and_number() {
        let layout = Layout::plover();
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &HashMap::new())
            .unwrap();
        let bit = |name: &str| 1 << layout.slot_by_name(name).unwrap();
        // Q and A both press S-; T, G, H and Y all press *
        assert_eq!(map[&KeyCode::KC_Q], bit("S-"));
        assert_eq!(map[&KeyCode::KC_A], bit("S-"));
        assert_eq!(map[&KeyCode::KC_T], bit("*"));
        assert_eq!(map[&KeyCode::KC_G], bit("*"));
        assert_eq!(map[&KeyCode::KC_H], bit("*"));
        assert_eq!(map[&KeyCode::KC_Y], bit("*"));
        // 1 to 0 all press #
        for key in [
            KeyCode::KC_1,
            KeyCode::KC_2,
            KeyCode::KC_3,
            KeyCode::KC_4,
            KeyCode::KC_5,
            KeyCode::KC_6,
            KeyCode::KC_7,
            KeyCode::KC_8,
            KeyCode::KC_9,
            KeyCode::KC_0,
        ] {
            assert_eq!(map[&key], bit("#"), "{key:?}");
        }
        // 23 main keys plus Q, G, H, Y and 2 to 0
        assert_eq!(map.len(), 23 + 1 + 3 + 9);
        assert_eq!(extra_keys(&preset("qwerty").unwrap()).len(), 13);
    }

    #[test]
    fn an_override_onto_an_extra_key_wins_over_it() {
        // The documented `{ "S-": Key(KC_Q) }` override must keep working: Q is then S-'s
        // own key, and the extra Q entry quietly steps aside.
        let layout = Layout::plover();
        let mut overrides = HashMap::new();
        overrides.insert("S-".to_string(), KeyAction::Key(KeyCode::KC_Q));
        overrides.insert("W-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << 0)));
        assert_eq!(map.get(&KeyCode::KC_A), Some(&(1 << 4)));
    }

    #[test]
    fn combined_keys_press_both_of_their_parts() {
        // The default preset has no combined keys any more, but the mechanism is still
        // there: an override can still bind a combined key name to one physical key.
        let layout = Layout::plover();
        let mut overrides = HashMap::new();
        overrides.insert("A-+O-".to_string(), KeyAction::Key(KeyCode::KC_X));
        overrides.insert("-E+-U".to_string(), KeyAction::Key(KeyCode::KC_COMM));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        let bit = |name: &str| 1 << layout.slot_by_name(name).unwrap();
        assert_eq!(map[&KeyCode::KC_X], bit("A-") | bit("O-"));
        assert_eq!(map[&KeyCode::KC_COMM], bit("-E") | bit("-U"));
    }

    #[test]
    fn plover_with_builds_a_working_plus_join_key_end_to_end() {
        // The exact shape `config.ron`'s `extra_keys: ['+']` plus `layout_overrides: { "+":
        // Key(KC_Q) }` builds, start to finish.
        let layout = Layout::plover_with(&['+']).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("+".to_string(), KeyAction::Key(KeyCode::KC_Q));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << layout.extra_start())));
    }

    #[test]
    fn an_extra_key_literally_named_plus_overrides_as_itself_not_an_empty_combo() {
        // `extra_keys: ['+']` gives the extra key the name "+" too (its name is always its own
        // character) - a layout override for "+" must take that literally, not read it as
        // joining the two empty names either side of a `+` split.
        let layout = Layout::plover().with_extra(KeySpec::new('+', "+")).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("+".to_string(), KeyAction::Key(KeyCode::KC_Q));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << layout.extra_start())));
    }

    #[test]
    fn overrides_swap_keys() {
        // Swap S- (A) and W- (Q) in the preset
        let layout = Layout::plover();
        let mut overrides = HashMap::new();
        overrides.insert("S-".to_string(), KeyAction::Key(KeyCode::KC_Q));
        overrides.insert("W-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << 0)));
        assert_eq!(map.get(&KeyCode::KC_A), Some(&(1 << 4)));
    }

    #[test]
    fn reports_unknown_names_and_duplicate_keys() {
        let layout = Layout::plover();
        let mut overrides = HashMap::new();
        overrides.insert("NOPE".to_string(), KeyAction::Key(KeyCode::KC_Q));
        // Put T- on the same key as S-
        overrides.insert("T-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let errors = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("unknown steno key \"NOPE\"")));
        assert!(errors.iter().any(|e| e.contains("used by both")));
    }

    #[test]
    fn rejects_non_key_overrides() {
        let layout = Layout::plover();
        let mut overrides = HashMap::new();
        overrides.insert(
            "S-".to_string(),
            KeyAction::TO(crate::config::Layer("nav".to_string())),
        );
        let errors = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap_err();
        assert!(errors.iter().any(|e| e.contains("must be Key(KC_*)")));
    }

    #[test]
    fn an_extra_key_parses_and_renders_like_the_number_key_does() {
        let layout = with_join();
        let bit = |name: &str| 1 << layout.slot_by_name(name).unwrap();
        let extra_start = layout.extra_start();
        // Marks the whole stroke, wherever it falls in the text; round-trips the same way
        assert_eq!(
            layout.parse_stroke("+HR"),
            Some((1 << extra_start) | bit("H-") | bit("R-"))
        );
        assert_eq!(layout.render_stroke(layout.parse_stroke("+HR").unwrap()), "+HR");
        assert_eq!(
            layout.parse_stroke("+-G"),
            Some((1 << extra_start) | bit("-G"))
        );
        assert_eq!(layout.render_stroke(layout.parse_stroke("+-G").unwrap()), "+-G");
        // Multi-stroke dictionary keys carry it on whichever stroke has it
        assert_eq!(
            layout.parse_strokes("KAT/+-G"),
            Some(vec![
                layout.parse_stroke("KAT").unwrap(),
                layout.parse_stroke("+-G").unwrap()
            ])
        );
        // Twice in one stroke is not a stroke
        assert_eq!(layout.parse_stroke("+KAT+"), None);
    }

    #[test]
    fn an_extra_key_needs_no_physical_key_unless_something_gives_it_one() {
        let layout = with_join();
        // No layout anywhere assigns "+" a key: the default preset still builds, and the
        // 23 standard keys are exactly what's bound, as before this existed
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &HashMap::new())
            .unwrap();
        assert!(!map.values().any(|&bits| bits == 1 << layout.extra_start()));

        // An override gives it one, and it works like any other steno key
        let mut overrides = HashMap::new();
        overrides.insert("join".to_string(), KeyAction::Key(KeyCode::KC_Q));
        let map = layout
            .build_key_map(&preset("qwerty").unwrap(), &overrides)
            .unwrap();
        assert_eq!(map[&KeyCode::KC_Q], 1 << layout.extra_start());
        // Q was S-'s extra key; giving it to "join" instead means only A presses S- now
        assert_eq!(
            map.get(&KeyCode::KC_A),
            Some(&(1 << layout.slot_by_name("S-").unwrap()))
        );
        assert_eq!(
            map[&KeyCode::KC_Q],
            1 << layout.extra_start(),
            "not S- any more"
        );
        // Still the same number of physical keys bound; Q just presses something else now
        assert_eq!(map.len(), 23 + 1 + 3 + 9);
    }

    #[test]
    fn a_custom_key_cant_use_a_character_stroke_notation_already_needs() {
        let k = KeySpec::new;
        for bad in ['-', '/', '0', '5', ' ', '\t'] {
            let err = Layout::build(
                Layout::plover().left,
                Layout::plover().pre_star,
                Layout::plover().star,
                Layout::plover().post_star,
                Layout::plover().right,
                Layout::plover().number,
                Vec::new(),
                vec![k(bad, "join")],
            )
            .unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("join")),
                "{bad:?} should have been rejected: {err:?}"
            );
        }
    }

    #[test]
    fn two_keys_in_one_layout_cant_share_a_character_or_a_name() {
        let k = KeySpec::new;
        let dup_char = Layout::build(
            Layout::plover().left,
            Layout::plover().pre_star,
            Layout::plover().star,
            Layout::plover().post_star,
            Layout::plover().right,
            Layout::plover().number,
            Vec::new(),
            vec![k('S', "join")], // 'S' is already S-'s own character
        )
        .unwrap_err();
        assert!(dup_char.iter().any(|e| e.contains('S')));

        let dup_name = Layout::build(
            Layout::plover().left,
            Layout::plover().pre_star,
            Layout::plover().star,
            Layout::plover().post_star,
            Layout::plover().right,
            Layout::plover().number,
            Vec::new(),
            vec![k('+', "S-")], // "S-" is already taken
        )
        .unwrap_err();
        assert!(dup_name.iter().any(|e| e.contains("S-")));
    }

    #[test]
    fn a_custom_layout_can_resize_or_reorder_every_group() {
        // A small made-up theory: 2 left keys, no pre-star thumbs, a star, one post-star
        // thumb, 2 right keys, a number key, and one extra - nothing here is Plover's shape.
        let k = KeySpec::new;
        let layout = Layout::build(
            vec![k('X', "X-"), k('Y', "Y-")],
            Vec::new(),
            k('*', "*"),
            vec![k('I', "-I")],
            vec![k('Z', "-Z"), k('W', "-W")],
            k('#', "#"),
            vec![("X-".to_string(), '1')],
            vec![k('+', "join")],
        )
        .unwrap();
        assert_eq!(layout.len(), 2 + 1 + 1 + 2 + 1 + 1);
        let bits = layout.parse_stroke("XY*IZW").unwrap();
        assert_eq!(layout.render_stroke(bits), "XY*IZW");
        // The custom number key still substitutes correctly
        assert_eq!(layout.parse_stroke("1"), layout.parse_stroke("#X"));
    }
}
