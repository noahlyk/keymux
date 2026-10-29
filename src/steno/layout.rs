//! Steno stroke model: the 23 stroke keys, Plover stroke notation, and the
//! physical-key layout that maps QWERTY keys onto stroke keys.

use crate::config::KeyAction;
use crate::keycode::KeyCode;
use std::collections::HashMap;

/// Stroke slots in Plover's canonical order. Bit `i` of a stroke is slot `i`.
/// The letter is how the slot appears in Plover's stroke strings, and the name
/// is how it's written in config files.
pub const SLOTS: [(char, &str); 23] = [
    ('S', "S-"),
    ('T', "T-"),
    ('K', "K-"),
    ('P', "P-"),
    ('W', "W-"),
    ('H', "H-"),
    ('R', "R-"),
    ('A', "A-"),
    ('O', "O-"),
    ('*', "*"),
    ('E', "-E"),
    ('U', "-U"),
    ('F', "-F"),
    ('R', "-R"),
    ('P', "-P"),
    ('B', "-B"),
    ('L', "-L"),
    ('G', "-G"),
    ('T', "-T"),
    ('S', "-S"),
    ('D', "-D"),
    ('Z', "-Z"),
    ('#', "#"),
];

/// Slot index of the number key `#`.
const NUMBER_SLOT: usize = 22;

/// Extra stroke keys beyond the standard 23, for a theory that needs one.
///
/// Each is written before the regular letters, independent of hand and steno order, the same
/// way `#` is: a mark on the stroke as a whole, not a sound at some position in it. Bit
/// `EXTRA_START + i` of a stroke is entry `i`.
///
/// Unlike the 23 slots above, these need no physical key: a layout with none configured never
/// sees a difference. A theory asks for one with an ordinary `layout_overrides` entry, such as
/// `"^": Key(KC_Q)`; nothing else changes it, and no key is required to have one.
///
/// This is the extension point for a theory key that doesn't fit Plover's own 23: a join key
/// that marks a stroke as continuing the word before it, for instance, rather than a sound.
/// Adding a key to this list does not touch the 23 above or their steno-order rules at all.
pub const EXTRA_KEYS: &[(char, &str)] = &[('^', "^")];

/// Slot index of the first extra key.
const EXTRA_START: usize = SLOTS.len();

/// The join key's bit.
///
/// A stroke marks itself as continuing the one before it, rather than starting a word of its
/// own, by setting this. The translator reads it before trying to match anything, so a
/// dictionary that uses it never needs the engine to guess how many strokes make up a word; it
/// is told outright, stroke by stroke, as they come in.
///
/// This is the first extra key, `"^"`. A theory earns the behaviour by giving a physical key to
/// that name in its layout; nothing changes for one that does not.
pub const JOIN_BIT: u32 = 1 << EXTRA_START;
/// Slot index of the star, which sits between the left and right hands.
const STAR_SLOT: usize = 9;
/// Slots `0..LEFT_END` are the left hand; the vowels, star and right thumb follow, then the right hand.
const LEFT_END: usize = 7;
/// First slot of the right hand (`-F`).
const RIGHT_START: usize = 12;
/// First slot a `-` allows: after it the next key is `-E` or further right, never `A`, `O` or `*`.
const AFTER_HYPHEN: usize = 10;
/// The keys that write a digit when the number key is down: slot and digit. In stroke order, so
/// digits read `1 2 3 4 5 0 6 7 8 9` across a stroke.
const NUMBER_KEYS: [(usize, char); 10] = [
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
];

/// Parse one stroke in Plover's notation: `KAT`, `TEFT`, `-PB`, `1-9`, `12K` or `#S`.
///
/// Letters and digits must come in key order, a `-` separates the left hand from the right when
/// no vowel or star does, and the number key `#` may stand anywhere, once. A digit writes its
/// key and implies the number key. Returns `None` for anything Plover rejects, for a stroke with
/// no keys, and for multi-stroke dictionary keys (those contain `/`).
#[must_use]
pub fn parse_stroke(text: &str) -> Option<u32> {
    let bits = parse_stroke_bits(text)?;
    (bits != 0).then_some(bits)
}

fn parse_stroke_bits(text: &str) -> Option<u32> {
    let mut bits = 0u32;
    let mut cursor = 0usize;
    let mut number = false;
    let mut hash_seen = false;
    let mut extras_seen = 0u32;
    for ch in text.chars() {
        // An extra key marks the stroke as a whole, wherever it falls in the text, and does
        // not move the cursor that the 23 standard keys parse against
        if let Some(i) = EXTRA_KEYS.iter().position(|&(c, _)| c == ch) {
            if extras_seen & (1 << i) != 0 {
                return None;
            }
            extras_seen |= 1 << i;
            bits |= 1 << (EXTRA_START + i);
            continue;
        }
        match ch {
            '#' => {
                if hash_seen {
                    return None;
                }
                hash_seen = true;
                number = true;
            }
            // A hyphen only makes sense while the left hand and star are still to be written
            '-' => {
                if cursor > AFTER_HYPHEN + 1 {
                    return None;
                }
                cursor = cursor.max(AFTER_HYPHEN);
            }
            '0'..='9' => {
                let (slot, _) = *NUMBER_KEYS.iter().find(|(_, digit)| *digit == ch)?;
                if slot < cursor {
                    return None;
                }
                bits |= 1 << slot;
                cursor = slot + 1;
                number = true;
            }
            _ => {
                let offset = SLOTS[cursor..NUMBER_SLOT]
                    .iter()
                    .position(|(letter, _)| *letter == ch)?;
                let slot = cursor + offset;
                bits |= 1 << slot;
                cursor = slot + 1;
            }
        }
    }
    if number {
        bits |= 1 << NUMBER_SLOT;
    }
    Some(bits)
}

/// Parse one stroke, or several separated by `/` as in a multi-stroke dictionary key.
///
/// Such a key is `"HRAOUP/AER"`. `None` if any part isn't a valid stroke, which includes empty
/// parts such as a trailing `/`. A leading `/` is the empty stroke that marks a word boundary
/// (`"/S"`), kept as the stroke `0`, and a lone `-` is that empty stroke written out.
#[must_use]
pub fn parse_strokes(text: &str) -> Option<Vec<u32>> {
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
        strokes.push(parse_stroke_bits(part)?);
    }
    Some(strokes)
}

/// Like `parse_strokes`, for what people type: spaces around a slash are not part of a stroke.
#[must_use]
pub fn parse_strokes_trimmed(text: &str) -> Option<Vec<u32>> {
    parse_strokes(&text.split('/').map(str::trim).collect::<Vec<_>>().join("/"))
}

/// Render a stroke bitmask in Plover's canonical notation.
///
/// With the number key down, the keys that carry a digit are written as digits (`1`, `-9`, `12K`)
/// and the `#` itself is dropped. With no such key, `#` is written. A `-` stands between the left
/// hand and the right when no vowel or star does, so `-S` and `S-S` can't be mistaken for each other.
#[must_use]
pub fn render_stroke(bits: u32) -> String {
    let down = |slot: usize| bits & (1 << slot) != 0;
    let number = down(NUMBER_SLOT);
    let digits = number && NUMBER_KEYS.iter().any(|(slot, _)| down(*slot));
    let key = |slot: usize| {
        if digits {
            if let Some((_, digit)) = NUMBER_KEYS.iter().find(|(s, _)| *s == slot) {
                return *digit;
            }
        }
        SLOTS[slot].0
    };
    let mut out = String::new();
    for (i, &(ch, _)) in EXTRA_KEYS.iter().enumerate() {
        if down(EXTRA_START + i) {
            out.push(ch);
        }
    }
    if number && !digits {
        out.push('#');
    }
    out.extend((0..LEFT_END).filter(|&slot| down(slot)).map(key));
    // With no vowel or star in between, right-hand letters would read as left-hand
    // ones, so Plover marks the boundary with a dash.
    let middle = (LEFT_END..RIGHT_START).any(down);
    let right = (RIGHT_START..NUMBER_SLOT).any(down);
    if !middle && right {
        out.push('-');
    }
    out.extend((LEFT_END..NUMBER_SLOT).filter(|&slot| down(slot)).map(key));
    out
}

/// Several strokes, each in Plover's canonical notation, separated by `/`: a dictionary key.
#[must_use]
pub fn render_strokes(strokes: &[u32]) -> String {
    strokes
        .iter()
        .map(|&bits| render_stroke(bits))
        .collect::<Vec<_>>()
        .join("/")
}

/// A stroke as typed text when it has no translation, such as `STPHA-UBGS`.
///
/// The left hand and its thumb come first, then a hyphen, then the star, the right thumb and
/// the right hand. The hyphen is always there, so the two hands read the same whatever keys
/// are down.
#[must_use]
pub fn render_raw_stroke(bits: u32) -> String {
    let letters = |range: std::ops::Range<usize>| -> String {
        range
            .filter(|i| bits & (1 << i) != 0)
            .map(|i| SLOTS[i].0)
            .collect()
    };
    let extras: String = EXTRA_KEYS
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << (EXTRA_START + i)) != 0)
        .map(|(_, &(ch, _))| ch)
        .collect();
    let number = if bits & (1 << NUMBER_SLOT) != 0 {
        "#"
    } else {
        ""
    };
    // The star is written first in the right hand, where it sits between the thumbs
    let star = if bits & (1 << STAR_SLOT) != 0 {
        "*"
    } else {
        ""
    };
    let left = letters(0..STAR_SLOT);
    let right = letters(STAR_SLOT + 1..NUMBER_SLOT);
    // The hyphen marks where the left hand ends and the right begins; with nothing from either
    // hand (an extra key or the number bar on its own), there are no two hands for it to mark.
    if left.is_empty() && star.is_empty() && right.is_empty() {
        format!("{extras}{number}")
    } else {
        format!("{extras}{number}{left}-{star}{right}")
    }
}

/// The sound each stroke key stands for, in the same order as [`SLOTS`]. This is the
/// reason a key is in a stroke: `KWEUBG` is K (k), W (w), E (e), U (u), B and G (ck).
pub const SLOT_SOUNDS: [&str; 23] = [
    "s", "t", "k", "p", "w", "h", "r", "a", "o", "*", "e", "u", "f", "r", "p", "b", "l", "g", "t",
    "s", "d", "z", "#",
];

/// Look up a stroke slot by its config name (`"S-"`, `"*"`, `"-E"`, ...).
#[must_use]
pub fn slot_by_name(name: &str) -> Option<usize> {
    SLOTS
        .iter()
        .position(|(_, slot_name)| *slot_name == name)
        .or_else(|| {
            EXTRA_KEYS
                .iter()
                .position(|(_, slot_name)| *slot_name == name)
                .map(|i| EXTRA_START + i)
        })
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
fn extra_key_slot(name: &str) -> Option<usize> {
    name.split_once(EXTRA_KEY_SEPARATOR)
        .and_then(|(slot, _)| slot_by_name(slot))
}

/// The extra physical keys in a preset as (stroke slot, key), ordered by slot then name.
#[must_use]
pub fn extra_keys(preset: &HashMap<String, KeyCode>) -> Vec<(usize, KeyCode)> {
    let mut named: Vec<(&String, usize, KeyCode)> = preset
        .iter()
        .filter_map(|(name, &key)| extra_key_slot(name).map(|slot| (name, slot, key)))
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

/// Combine a preset with per-layer overrides into a physical-key → stroke-bit map.
///
/// Returns every problem found, not just the first.
pub fn build_key_map(
    preset: &HashMap<String, KeyCode>,
    overrides: &HashMap<String, KeyAction>,
) -> Result<HashMap<KeyCode, u32>, Vec<String>> {
    let mut errors = Vec::new();
    let mut assigned: HashMap<String, KeyCode> = preset.clone();

    for (slot, action) in overrides {
        if !slot.split('+').all(|part| slot_by_name(part).is_some()) {
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
    for (slot_index, &(_, name)) in SLOTS.iter().enumerate() {
        let Some(&key) = assigned.get(name) else {
            errors.push(format!("steno key \"{name}\" has no physical key"));
            continue;
        };
        if let Some(other) = owners.insert(key, name) {
            errors.push(format!(
                "{key:?} is used by both \"{other}\" and \"{name}\""
            ));
            continue;
        }
        keys.insert(key, 1 << slot_index);
    }

    // An extra key beyond the standard 23 gets a physical key only if something gave it one;
    // no error if not, since most layouts have no use for it. Checked before the preset's own
    // "|" extras below, so a user override onto one of those keys takes it over cleanly.
    for (i, &(_, name)) in EXTRA_KEYS.iter().enumerate() {
        let Some(&key) = assigned.get(name) else {
            continue;
        };
        if let Some(other) = owners.insert(key, name) {
            errors.push(format!(
                "{key:?} is used by both \"{other}\" and \"{name}\""
            ));
            continue;
        }
        keys.insert(key, 1 << (EXTRA_START + i));
    }

    // A combined key presses several stroke keys at once, e.g. "A-+O-" on V.
    // Its bits are the union of its parts.
    let mut combined: Vec<(&String, KeyCode)> = assigned
        .iter()
        .filter(|(name, _)| name.contains('+'))
        .map(|(name, &key)| (name, key))
        .collect();
    combined.sort_by(|a, b| a.0.cmp(b.0));
    for (name, key) in combined {
        let Some(mask) = name
            .split('+')
            .map(|part| slot_by_name(part).map(|index| 1 << index))
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
        let Some(slot) = extra_key_slot(name) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_strokes() {
        // K + A + right T = "cat"
        assert_eq!(parse_stroke("KAT"), Some((1 << 2) | (1 << 7) | (1 << 18)));
        // Right-only strokes need the dash
        assert_eq!(parse_stroke("-PB"), Some((1 << 14) | (1 << 15)));
        // T E F T: the vowel separates the left T from the right F and T
        assert_eq!(
            parse_stroke("TEFT"),
            Some((1 << 1) | (1 << 10) | (1 << 12) | (1 << 18))
        );
        // Numbers start with #
        assert_eq!(parse_stroke("#T"), Some((1 << 22) | (1 << 1)));
    }

    #[test]
    fn rejects_non_strokes() {
        assert_eq!(parse_stroke("KPA/TKAOEU"), None);
        assert_eq!(parse_stroke("xyz"), None);
        assert_eq!(parse_stroke(""), None);
        // Out of order: a left-hand letter can't come after a vowel without a dash
        assert_eq!(parse_stroke("AK"), None);
    }

    #[test]
    fn several_strokes_are_separated_by_a_slash() {
        let one = parse_stroke("HRAOUP").unwrap();
        let two = parse_stroke("AER").unwrap();
        assert_eq!(parse_strokes("HRAOUP"), Some(vec![one]));
        assert_eq!(parse_strokes("HRAOUP/AER"), Some(vec![one, two]));
        assert_eq!(
            parse_strokes("HRAOUP/AER/HRAOUP"),
            Some(vec![one, two, one])
        );
        // Spaces around a slash are not part of the stroke, though Plover itself would refuse them
        assert_eq!(parse_strokes_trimmed("HRAOUP / AER"), Some(vec![one, two]));
        assert_eq!(parse_strokes("HRAOUP / AER"), None);
        // Anything wrong anywhere makes the whole thing invalid, never a shorter trigger
        assert_eq!(parse_strokes("HRAOUP/"), None);
        assert_eq!(parse_strokes("HRAOUP//AER"), None);
        assert_eq!(parse_strokes("HRAOUP/ZZZ"), None);
        assert_eq!(parse_strokes(""), None);
        // A leading slash is the empty stroke that marks a word boundary, as in "/S"
        assert_eq!(parse_strokes("/AER"), Some(vec![0, two]));
    }

    #[test]
    fn render_round_trips_known_strokes() {
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
            let bits = parse_stroke(text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(render_stroke(bits), text, "rendering {text}");
        }
    }

    #[test]
    fn a_raw_stroke_has_one_hyphen_between_the_hands_and_the_star_on_the_right() {
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
            ("^", "^"),
        ] {
            let bits = parse_stroke(text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(render_raw_stroke(bits), raw, "rendering {text}");
        }
    }

    /// Plover's own answers for how a set of keys is written and how a spelling is read. See
    /// `tools/plover-golden`.
    #[test]
    fn stroke_notation_matches_plovers_for_every_reference_case() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("testdata/stroke_golden.json")).unwrap();
        for case in data["render"].as_array().unwrap() {
            let bits = case[0].as_array().unwrap().iter().fold(0u32, |bits, key| {
                bits | 1 << slot_by_name(key.as_str().unwrap()).unwrap()
            });
            let steno = case[1].as_str().unwrap();
            assert_eq!(render_stroke(bits), steno, "writing {:?}", case[0]);
            assert_eq!(parse_strokes(steno), Some(vec![bits]), "reading {steno}");
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
            let got = parse_strokes(text)
                .map(|strokes| strokes.into_iter().map(render_stroke).collect::<Vec<_>>());
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
        // Deterministic sample across the 23-bit space
        let mut state: u32 = 0x1234_5678;
        for _ in 0..20_000 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let bits = state & ((1 << 23) - 1);
            if bits == 0 {
                continue;
            }
            let text = render_stroke(bits);
            assert_eq!(parse_stroke(&text), Some(bits), "stroke {text}");
        }
    }

    #[test]
    fn qwerty_preset_covers_every_steno_key_once() {
        let main_only = without_extra_keys(preset("qwerty").unwrap());
        let map = build_key_map(&main_only, &HashMap::new()).unwrap();
        // One main physical key per stroke key; the default preset has no combined keys
        assert_eq!(map.len(), SLOTS.len());
        let single = map
            .values()
            .filter(|bits| bits.count_ones() == 1)
            .fold(0, |all, bits| all | bits);
        assert_eq!(single.count_ones() as usize, SLOTS.len());
    }

    #[test]
    fn q_g_and_the_number_row_are_extra_keys_for_s_star_and_number() {
        let map = build_key_map(&preset("qwerty").unwrap(), &HashMap::new()).unwrap();
        let bit = |name: &str| 1 << slot_by_name(name).unwrap();
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
        assert_eq!(map.len(), SLOTS.len() + 1 + 3 + 9);
        assert_eq!(extra_keys(&preset("qwerty").unwrap()).len(), 13);
    }

    #[test]
    fn an_override_onto_an_extra_key_wins_over_it() {
        // The documented `{ "S-": Key(KC_Q) }` override must keep working: Q is then S-'s
        // own key, and the extra Q entry quietly steps aside.
        let mut overrides = HashMap::new();
        overrides.insert("S-".to_string(), KeyAction::Key(KeyCode::KC_Q));
        overrides.insert("W-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let map = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << 0)));
        assert_eq!(map.get(&KeyCode::KC_A), Some(&(1 << 4)));
    }

    #[test]
    fn combined_keys_press_both_of_their_parts() {
        // The default preset has no combined keys any more, but the mechanism is still
        // there: an override can still bind a combined key name to one physical key.
        let mut overrides = HashMap::new();
        overrides.insert("A-+O-".to_string(), KeyAction::Key(KeyCode::KC_X));
        overrides.insert("-E+-U".to_string(), KeyAction::Key(KeyCode::KC_COMM));
        let map = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap();
        let bit = |name: &str| 1 << slot_by_name(name).unwrap();
        assert_eq!(map[&KeyCode::KC_X], bit("A-") | bit("O-"));
        assert_eq!(map[&KeyCode::KC_COMM], bit("-E") | bit("-U"));
    }

    #[test]
    fn overrides_swap_keys() {
        // Swap S- (A) and W- (Q) in the preset
        let mut overrides = HashMap::new();
        overrides.insert("S-".to_string(), KeyAction::Key(KeyCode::KC_Q));
        overrides.insert("W-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let map = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap();
        assert_eq!(map.get(&KeyCode::KC_Q), Some(&(1 << 0)));
        assert_eq!(map.get(&KeyCode::KC_A), Some(&(1 << 4)));
    }

    #[test]
    fn reports_unknown_names_and_duplicate_keys() {
        let mut overrides = HashMap::new();
        overrides.insert("NOPE".to_string(), KeyAction::Key(KeyCode::KC_Q));
        // Put T- on the same key as S-
        overrides.insert("T-".to_string(), KeyAction::Key(KeyCode::KC_A));
        let errors = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("unknown steno key \"NOPE\"")));
        assert!(errors.iter().any(|e| e.contains("used by both")));
    }

    #[test]
    fn rejects_non_key_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "S-".to_string(),
            KeyAction::TO(crate::config::Layer("nav".to_string())),
        );
        let errors = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("must be Key(KC_*)")));
    }

    #[test]
    fn an_extra_key_parses_and_renders_like_the_number_key_does() {
        let bit = |name: &str| 1 << slot_by_name(name).unwrap();
        // Marks the whole stroke, wherever it falls in the text; round-trips the same way
        assert_eq!(
            parse_stroke("^HR"),
            Some((1 << EXTRA_START) | bit("H-") | bit("R-"))
        );
        assert_eq!(render_stroke(parse_stroke("^HR").unwrap()), "^HR");
        assert_eq!(parse_stroke("^-G"), Some((1 << EXTRA_START) | bit("-G")));
        assert_eq!(render_stroke(parse_stroke("^-G").unwrap()), "^-G");
        // Multi-stroke dictionary keys carry it on whichever stroke has it
        assert_eq!(
            parse_strokes("KAT/^-G"),
            Some(vec![
                parse_stroke("KAT").unwrap(),
                parse_stroke("^-G").unwrap()
            ])
        );
        // Twice in one stroke is not a stroke
        assert_eq!(parse_stroke("^KAT^"), None);
    }

    #[test]
    fn an_extra_key_needs_no_physical_key_unless_something_gives_it_one() {
        // No layout anywhere assigns "^" a key: the default preset still builds, and the
        // 23 standard keys are exactly what's bound, as before this existed
        let map = build_key_map(&preset("qwerty").unwrap(), &HashMap::new()).unwrap();
        assert!(!map.values().any(|&bits| bits == 1 << EXTRA_START));

        // An override gives it one, and it works like any other steno key
        let mut overrides = HashMap::new();
        overrides.insert("^".to_string(), KeyAction::Key(KeyCode::KC_Q));
        let map = build_key_map(&preset("qwerty").unwrap(), &overrides).unwrap();
        assert_eq!(map[&KeyCode::KC_Q], 1 << EXTRA_START);
        // Q was S-'s extra key; giving it to "^" instead means only A presses S- now
        assert_eq!(
            map.get(&KeyCode::KC_A),
            Some(&(1 << slot_by_name("S-").unwrap()))
        );
        assert_eq!(map[&KeyCode::KC_Q], 1 << EXTRA_START, "not S- any more");
        // Still the same number of physical keys bound; Q just presses something else now
        assert_eq!(map.len(), SLOTS.len() + 1 + 3 + 9);
    }
}
