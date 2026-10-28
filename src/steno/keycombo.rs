//! Key combinations, the `{#...}` of Plover's translation language: `{#Return}`, `{#Left}`,
//! `{#Control_L(a)}`, `{#control(z shift(z))}`.
//!
//! A combo is key names separated by spaces, and a name followed by `(` holds that key while the
//! keys inside are pressed until the matching `)`. Names are case-insensitive. The parser is
//! Plover's `plover/key_combo.py`; the names it knows are those of a US keyboard, with the
//! names Plover takes from X11 for shifted symbols (`plus` presses the `=` key, without Shift,
//! as Plover's documentation says).

use crate::keycode::KeyCode;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Parse a combo into key events, `(key, true)` to press and `(key, false)` to release, with
/// `key_of` turning a lowercase name into a key. Plover's parser, with its errors as text.
pub fn parse_key_combo<K: PartialEq + Clone>(
    combo: &str,
    key_of: impl Fn(&str) -> Option<K>,
) -> Result<Vec<(K, bool)>, String> {
    let chars: Vec<char> = combo.chars().collect();
    let mut events = Vec::new();
    let mut down: Vec<K> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if is_word(c) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect::<String>().to_lowercase();
            // A `(`, possibly after spaces, means hold the key
            let mut after = i;
            while after < chars.len() && chars[after].is_whitespace() {
                after += 1;
            }
            let hold = chars.get(after) == Some(&'(');
            if hold {
                i = after + 1;
            }
            let Some(key) = key_of(&name) else {
                return Err(format!("unknown key \"{name}\" in \"{combo}\""));
            };
            if down.contains(&key) {
                return Err(format!("key \"{name}\" already pressed in \"{combo}\""));
            }
            events.push((key.clone(), true));
            if hold {
                down.push(key);
            } else {
                events.push((key, false));
            }
        } else if c == ')' {
            let Some(key) = down.pop() else {
                return Err(format!("unbalanced \")\" in \"{combo}\""));
            };
            events.push((key, false));
            i += 1;
        } else {
            return Err(format!("invalid character \"{c}\" in \"{combo}\""));
        }
    }
    if down.is_empty() {
        Ok(events)
    } else {
        Err(format!("unbalanced \"(\" in \"{combo}\""))
    }
}

/// The key a name stands for on a US keyboard, if keymux can press it.
#[must_use]
pub fn key_for_name(name: &str) -> Option<KeyCode> {
    use KeyCode as K;
    let letters = [
        K::KC_A,
        K::KC_B,
        K::KC_C,
        K::KC_D,
        K::KC_E,
        K::KC_F,
        K::KC_G,
        K::KC_H,
        K::KC_I,
        K::KC_J,
        K::KC_K,
        K::KC_L,
        K::KC_M,
        K::KC_N,
        K::KC_O,
        K::KC_P,
        K::KC_Q,
        K::KC_R,
        K::KC_S,
        K::KC_T,
        K::KC_U,
        K::KC_V,
        K::KC_W,
        K::KC_X,
        K::KC_Y,
        K::KC_Z,
    ];
    let digits = [
        K::KC_0,
        K::KC_1,
        K::KC_2,
        K::KC_3,
        K::KC_4,
        K::KC_5,
        K::KC_6,
        K::KC_7,
        K::KC_8,
        K::KC_9,
    ];
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        if c.is_ascii_lowercase() {
            return letters.get((c as u8 - b'a') as usize).copied();
        }
        if c.is_ascii_digit() {
            return digits.get((c as u8 - b'0') as usize).copied();
        }
    }
    if let Some(number) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return [
            K::KC_F1,
            K::KC_F2,
            K::KC_F3,
            K::KC_F4,
            K::KC_F5,
            K::KC_F6,
            K::KC_F7,
            K::KC_F8,
            K::KC_F9,
            K::KC_F10,
            K::KC_F11,
            K::KC_F12,
            K::KC_F13,
            K::KC_F14,
            K::KC_F15,
            K::KC_F16,
            K::KC_F17,
            K::KC_F18,
            K::KC_F19,
            K::KC_F20,
            K::KC_F21,
            K::KC_F22,
            K::KC_F23,
            K::KC_F24,
        ]
        .get(usize::from(number).checked_sub(1)?)
        .copied();
    }
    Some(match name {
        "return" | "linefeed" => K::KC_ENT,
        "space" => K::KC_SPC,
        "tab" => K::KC_TAB,
        "backspace" => K::KC_BSPC,
        "delete" => K::KC_DEL,
        "escape" => K::KC_ESC,
        "insert" => K::KC_INS,
        "home" => K::KC_HOME,
        "end" => K::KC_END,
        "page_up" | "prior" => K::KC_PGUP,
        "page_down" | "next" => K::KC_PGDN,
        "left" => K::KC_LEFT,
        "right" => K::KC_RGHT,
        "up" => K::KC_UP,
        "down" => K::KC_DOWN,
        "caps_lock" => K::KC_CAPS,
        "num_lock" => K::KC_NLCK,
        "scroll_lock" => K::KC_SLCK,
        "pause" | "break" => K::KC_PAUS,
        "print" | "sys_req" => K::KC_PSCR,
        "menu" => K::KC_APP,
        // Modifiers, with the aliases Plover adds
        "shift" | "shift_l" => K::KC_LSFT,
        "shift_r" => K::KC_RSFT,
        "control" | "control_l" | "ctrl" | "ctrl_l" => K::KC_LCTL,
        "control_r" | "ctrl_r" => K::KC_RCTL,
        "alt" | "alt_l" | "option" => K::KC_LALT,
        "alt_r" => K::KC_RALT,
        "super" | "super_l" | "windows" | "command" => K::KC_LGUI,
        "super_r" => K::KC_RGUI,
        // Symbols: the key, never Shift, as Plover's documentation says
        "minus" | "underscore" => K::KC_MINS,
        "equal" | "plus" => K::KC_EQL,
        "bracketleft" | "braceleft" => K::KC_LBRC,
        "bracketright" | "braceright" => K::KC_RBRC,
        "backslash" | "bar" => K::KC_BSLS,
        "semicolon" | "colon" => K::KC_SCLN,
        "apostrophe" | "quotedbl" | "quoteright" => K::KC_QUOT,
        "grave" | "asciitilde" | "quoteleft" => K::KC_GRV,
        "comma" | "less" => K::KC_COMM,
        "period" | "greater" => K::KC_DOT,
        "slash" | "question" => K::KC_SLSH,
        "exclam" => K::KC_1,
        "at" => K::KC_2,
        "numbersign" => K::KC_3,
        "dollar" => K::KC_4,
        "percent" => K::KC_5,
        "asciicircum" => K::KC_6,
        "ampersand" => K::KC_7,
        "asterisk" => K::KC_8,
        "parenleft" => K::KC_9,
        "parenright" => K::KC_0,
        // Media keys
        "audioraisevolume" | "xf86audioraisevolume" => K::KC_VOLU,
        "audiolowervolume" | "xf86audiolowervolume" => K::KC_VOLD,
        "audiomute" | "xf86audiomute" => K::KC_MUTE,
        _ => return None,
    })
}

/// The key events for `combo` as keymux can send them, or why it can't.
pub fn key_events(combo: &str) -> Result<Vec<(KeyCode, bool)>, String> {
    parse_key_combo(combo, key_for_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(combo: &str) -> Result<Vec<(String, bool)>, String> {
        parse_key_combo(combo, |name| Some(name.to_string()))
    }

    #[test]
    fn a_name_is_pressed_and_released() {
        assert_eq!(
            names("Return").unwrap(),
            [("return".into(), true), ("return".into(), false)]
        );
    }

    #[test]
    fn a_modifier_holds_while_the_keys_inside_are_pressed() {
        let events = names("control(z shift(z))").unwrap();
        let shown: Vec<String> = events
            .iter()
            .map(|(name, down)| format!("{}{name}", if *down { "+" } else { "-" }))
            .collect();
        assert_eq!(
            shown,
            ["+control", "+z", "-z", "+shift", "+z", "-z", "-shift", "-control"]
        );
    }

    #[test]
    fn keys_in_a_row_are_pressed_one_after_the_other() {
        assert_eq!(names("a b").unwrap().len(), 4);
        assert_eq!(names("shift (a)").unwrap(), names("shift(a)").unwrap());
    }

    #[test]
    fn mistakes_are_refused() {
        assert!(names("shift(a").is_err());
        assert!(names("a)").is_err());
        assert!(names("a+b").is_err());
        assert!(names("shift(shift(a))").is_err());
    }

    #[test]
    fn names_become_the_keys_of_a_us_keyboard() {
        assert_eq!(
            key_events("Control_L(BackSpace)").unwrap(),
            [
                (KeyCode::KC_LCTL, true),
                (KeyCode::KC_BSPC, true),
                (KeyCode::KC_BSPC, false),
                (KeyCode::KC_LCTL, false),
            ]
        );
        // `plus` is the `=` key, with no Shift added, as Plover documents
        assert_eq!(key_events("plus").unwrap()[0].0, KeyCode::KC_EQL);
        assert_eq!(key_events("F5").unwrap()[0].0, KeyCode::KC_F5);
        assert_eq!(key_events("super(l)").unwrap()[0].0, KeyCode::KC_LGUI);
        assert!(key_events("eacute").is_err());
        assert!(key_events("shift_l(shift(a))").is_err());
    }
}
