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
/// Slots `0..LEFT_END` are the left hand, `LEFT_END..MIDDLE_END` the vowels and star.
const LEFT_END: usize = 7;
const MIDDLE_END: usize = 12;

/// Parse a Plover stroke string such as `KAT`, `TEFT` or `-PB` into a bitmask.
///
/// Returns `None` for anything that isn't a single stroke, including multi-stroke
/// dictionary keys (those contain `/`).
#[must_use]
pub fn parse_stroke(text: &str) -> Option<u32> {
    let mut bits = 0u32;
    let mut cursor = 0usize;
    for ch in text.chars() {
        match ch {
            '#' => bits |= 1 << NUMBER_SLOT,
            // A dash means the letters after it are not left-hand letters.
            '-' => cursor = cursor.max(LEFT_END),
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
    (bits != 0).then_some(bits)
}

/// Render a stroke bitmask in Plover's canonical notation.
#[must_use]
pub fn render_stroke(bits: u32) -> String {
    let letters = |range: std::ops::Range<usize>| -> String {
        range
            .filter(|i| bits & (1 << i) != 0)
            .map(|i| SLOTS[i].0)
            .collect()
    };

    let mut out = String::new();
    if bits & (1 << NUMBER_SLOT) != 0 {
        out.push('#');
    }
    let left = letters(0..LEFT_END);
    let middle = letters(LEFT_END..MIDDLE_END);
    let right = letters(MIDDLE_END..NUMBER_SLOT);

    out.push_str(&left);
    // With no vowel or star in between, right-hand letters would read as left-hand
    // ones, so Plover marks the boundary with a dash.
    if middle.is_empty() && !right.is_empty() {
        out.push('-');
    }
    out.push_str(&middle);
    out.push_str(&right);
    out
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
    SLOTS.iter().position(|(_, slot_name)| *slot_name == name)
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
            errors.push(format!("combined key \"{name}\" names an unknown steno key"));
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
    fn render_round_trips_known_strokes() {
        for text in [
            "KAT",
            "-PB",
            "TEFT",
            "#T",
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
        let map = build_key_map(&preset("qwerty").unwrap(), &HashMap::new()).unwrap();
        // One physical key per stroke key; the default preset has no combined keys
        assert_eq!(map.len(), SLOTS.len());
        let single = map
            .values()
            .filter(|bits| bits.count_ones() == 1)
            .fold(0, |all, bits| all | bits);
        assert_eq!(single.count_ones() as usize, SLOTS.len());
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
}
