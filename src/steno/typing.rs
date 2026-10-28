//! Following plain typing, so the history knows the words typed outside steno.
//!
//! Every key that reaches the application from a layer that isn't steno is looked at here, after
//! the remaps, which is the key the application gets. A printable key adds its character to the
//! history, Backspace takes one off, and anything that could move the caret or change the text
//! without us knowing (arrows, Delete, a shortcut with Ctrl held) makes the history forget what
//! was typed, because what is at the cursor is no longer known.

use super::journal::History;
use crate::keycode::{KeyCategory, KeyCode};
use std::collections::HashSet;

/// Which characters the physical keys type. The keys are positions, so this is the layout the
/// system uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypingLayout {
    #[default]
    Us,
    Dvorak,
    Colemak,
}

impl TypingLayout {
    /// The layout named in the config, in any letter case.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "us" | "qwerty" => Some(Self::Us),
            "dvorak" => Some(Self::Dvorak),
            "colemak" => Some(Self::Colemak),
            _ => None,
        }
    }

    /// The character `key` types, or `None` if it types none.
    #[must_use]
    pub fn character(self, key: KeyCode, shift: bool, caps: bool) -> Option<char> {
        let (plain, shifted) = us_pair(key)?;
        let (plain, shifted) = match self {
            Self::Us => (plain, shifted),
            Self::Dvorak => dvorak(plain, shifted),
            Self::Colemak => colemak(plain, shifted),
        };
        // Caps Lock flips the case of letters only
        let upper = shift != (caps && plain.is_alphabetic());
        Some(if upper { shifted } else { plain })
    }
}

/// The unshifted and shifted characters of a key on a US keyboard.
const fn us_pair(key: KeyCode) -> Option<(char, char)> {
    use KeyCode::*;
    Some(match key {
        KC_A => ('a', 'A'),
        KC_B => ('b', 'B'),
        KC_C => ('c', 'C'),
        KC_D => ('d', 'D'),
        KC_E => ('e', 'E'),
        KC_F => ('f', 'F'),
        KC_G => ('g', 'G'),
        KC_H => ('h', 'H'),
        KC_I => ('i', 'I'),
        KC_J => ('j', 'J'),
        KC_K => ('k', 'K'),
        KC_L => ('l', 'L'),
        KC_M => ('m', 'M'),
        KC_N => ('n', 'N'),
        KC_O => ('o', 'O'),
        KC_P => ('p', 'P'),
        KC_Q => ('q', 'Q'),
        KC_R => ('r', 'R'),
        KC_S => ('s', 'S'),
        KC_T => ('t', 'T'),
        KC_U => ('u', 'U'),
        KC_V => ('v', 'V'),
        KC_W => ('w', 'W'),
        KC_X => ('x', 'X'),
        KC_Y => ('y', 'Y'),
        KC_Z => ('z', 'Z'),
        KC_1 => ('1', '!'),
        KC_2 => ('2', '@'),
        KC_3 => ('3', '#'),
        KC_4 => ('4', '$'),
        KC_5 => ('5', '%'),
        KC_6 => ('6', '^'),
        KC_7 => ('7', '&'),
        KC_8 => ('8', '*'),
        KC_9 => ('9', '('),
        KC_0 => ('0', ')'),
        KC_SPC => (' ', ' '),
        KC_ENT => ('\n', '\n'),
        KC_TAB => ('\t', '\t'),
        KC_GRV => ('`', '~'),
        KC_MINS => ('-', '_'),
        KC_EQL => ('=', '+'),
        KC_LBRC => ('[', '{'),
        KC_RBRC => (']', '}'),
        KC_BSLS => ('\\', '|'),
        KC_SCLN => (';', ':'),
        KC_QUOT => ('\'', '"'),
        KC_COMM => (',', '<'),
        KC_DOT => ('.', '>'),
        KC_SLSH => ('/', '?'),
        _ => return None,
    })
}

/// What a Dvorak keyboard types at the position of the US key that types `us`. Keys it doesn't
/// move keep their characters.
const fn dvorak(us: char, shifted: char) -> (char, char) {
    let plain = match us {
        'q' => '\'',
        'w' => ',',
        'e' => '.',
        'r' => 'p',
        't' => 'y',
        'y' => 'f',
        'u' => 'g',
        'i' => 'c',
        'o' => 'r',
        'p' => 'l',
        '[' => '/',
        ']' => '=',
        's' => 'o',
        'd' => 'e',
        'f' => 'u',
        'g' => 'i',
        'h' => 'd',
        'j' => 'h',
        'k' => 't',
        'l' => 'n',
        ';' => 's',
        '\'' => '-',
        'z' => ';',
        'x' => 'q',
        'c' => 'j',
        'v' => 'k',
        'b' => 'x',
        'n' => 'b',
        ',' => 'w',
        '.' => 'v',
        '/' => 'z',
        '-' => '[',
        '=' => ']',
        _ => return (us, shifted),
    };
    let shifted = match plain {
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        ';' => ':',
        '-' => '_',
        letter => letter.to_ascii_uppercase(),
    };
    (plain, shifted)
}

/// What a Colemak keyboard types at the position of the US key that types `us`.
const fn colemak(us: char, shifted: char) -> (char, char) {
    let moved = match us {
        'e' => 'f',
        'r' => 'p',
        't' => 'g',
        'y' => 'j',
        'u' => 'l',
        'i' => 'u',
        'o' => 'y',
        'p' => ';',
        's' => 'r',
        'd' => 's',
        'f' => 't',
        'g' => 'd',
        'j' => 'n',
        'k' => 'e',
        'l' => 'i',
        ';' => 'o',
        'n' => 'k',
        _ => return (us, shifted),
    };
    if moved == ';' {
        (';', ':')
    } else {
        (moved, moved.to_ascii_uppercase())
    }
}

/// Watches the keys a keyboard sends to the application and keeps the history in step.
#[derive(Debug, Clone, Default)]
pub struct Typist {
    layout: TypingLayout,
    shift: HashSet<KeyCode>,
    /// Ctrl, Alt or Super held: the next key is a shortcut, not text
    shortcut: HashSet<KeyCode>,
    caps: bool,
}

impl Typist {
    #[must_use]
    pub fn new(layout: TypingLayout) -> Self {
        Self {
            layout,
            ..Self::default()
        }
    }

    /// A key reached the application (`pressed` is false for its release).
    pub fn key(&mut self, history: &mut History, key: KeyCode, pressed: bool) {
        if key.is_modifier() {
            let held = if matches!(key, KeyCode::KC_LSFT | KeyCode::KC_RSFT) {
                &mut self.shift
            } else {
                &mut self.shortcut
            };
            if pressed {
                held.insert(key);
            } else {
                held.remove(&key);
            }
            return;
        }
        if !pressed {
            return;
        }
        if key == KeyCode::KC_CAPS {
            self.caps = !self.caps;
            return;
        }
        if !self.shortcut.is_empty() {
            history.fence();
            return;
        }
        if key == KeyCode::KC_BSPC {
            history.backspace();
            return;
        }
        let shift = !self.shift.is_empty();
        if let Some(ch) = self.layout.character(key, shift, self.caps) {
            history.typed(ch);
        } else if !matches!(key.category(), KeyCategory::Lock | KeyCategory::Media) {
            // Arrows, Delete, Escape, function keys, keys we don't know: the caret may have moved
            history.fence();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::formatter::{Formatter, Options};
    use crate::steno::journal::Origin;

    fn history() -> History {
        History {
            formatter: Formatter::new(Options {
                start_attached: true,
                ..Options::default()
            }),
            ..History::default()
        }
    }

    fn press(typist: &mut Typist, history: &mut History, keys: &[KeyCode]) {
        for &key in keys {
            typist.key(history, key, true);
            typist.key(history, key, false);
        }
    }

    #[test]
    fn keys_type_their_characters_with_shift_and_caps_lock() {
        use KeyCode::*;
        let mut typist = Typist::default();
        let mut h = history();
        typist.key(&mut h, KC_LSFT, true);
        press(&mut typist, &mut h, &[KC_H, KC_1]);
        typist.key(&mut h, KC_LSFT, false);
        press(&mut typist, &mut h, &[KC_I, KC_COMM, KC_SPC]);
        press(&mut typist, &mut h, &[KC_CAPS, KC_A, KC_CAPS, KC_B]);
        assert_eq!(h.screen, "H!i, Ab");
    }

    #[test]
    fn dvorak_and_colemak_move_the_letters() {
        use KeyCode::*;
        for (layout, expected) in [
            (TypingLayout::Dvorak, "aoeu"),
            (TypingLayout::Colemak, "arst"),
        ] {
            let mut typist = Typist::new(layout);
            let mut h = history();
            press(&mut typist, &mut h, &[KC_A, KC_S, KC_D, KC_F]);
            assert_eq!(h.screen, expected);
        }
    }

    #[test]
    fn backspace_follows_what_was_typed() {
        use KeyCode::*;
        let mut typist = Typist::default();
        let mut h = history();
        press(&mut typist, &mut h, &[KC_C, KC_A, KC_T, KC_BSPC]);
        assert_eq!(h.screen, "ca");
        assert_eq!(
            h.state.translations.last().unwrap().origin,
            Origin::Typed("ca".into())
        );
    }

    #[test]
    fn a_shortcut_or_a_caret_key_makes_the_history_forget() {
        use KeyCode::*;
        for forget in [vec![KC_LEFT], vec![KC_DEL], vec![KC_ESC]] {
            let mut typist = Typist::default();
            let mut h = history();
            press(&mut typist, &mut h, &[KC_C, KC_A, KC_T]);
            press(&mut typist, &mut h, &forget);
            assert!(h.is_empty(), "{forget:?}");
        }
        let mut typist = Typist::default();
        let mut h = history();
        press(&mut typist, &mut h, &[KC_C, KC_A, KC_T]);
        typist.key(&mut h, KC_LCTL, true);
        press(&mut typist, &mut h, &[KC_A]);
        assert!(h.is_empty());
        typist.key(&mut h, KC_LCTL, false);
        // A volume key doesn't
        let mut h = history();
        press(&mut typist, &mut h, &[KC_C, KC_VOLU]);
        assert_eq!(h.screen, "c");
    }
}
