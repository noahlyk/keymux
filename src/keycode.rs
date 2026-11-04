use serde::{Deserialize, Serialize};

/// Key category for classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCategory {
    /// Modifier key (Ctrl, Shift, Alt, GUI)
    Modifier,
    /// Letter key (A-Z)
    Letter,
    /// Number key (0-9)
    Number,
    /// Function key (F1-F24)
    Function,
    /// Special key (Enter, Space, Esc, etc.)
    Special,
    /// Navigation key (Arrows, Home, End, etc.)
    Navigation,
    /// Numpad key
    Numpad,
    /// Media key (Volume, Mute, etc.)
    Media,
    /// International/Language key
    International,
    /// Lock key (Caps Lock, Num Lock, etc.)
    Lock,
    /// General key (fallback)
    General,
}

/// Macro for defining keycodes with optional metadata
///
/// Syntax: `KC_NAME = code, category`
///
/// Categories: modifier, letter, number, function, special, navigation, numpad, media, international, lock, general
///
/// Example:
/// ```ignore
/// KC_LCTL = 29, modifier,
/// KC_A = 30, letter,
/// KC_1 = 2, number,
/// ```
macro_rules! define_keycodes {
    // Main entry point - requires trailing commas on each entry
    (
        $(
            $variant:ident = $code:expr, $category:ident,
        )*
    ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[repr(u16)]
        #[allow(non_camel_case_types)]
        pub enum KeyCode {
            $(
                $variant = $code,
            )*
        }

        impl KeyCode {
            /// Create a KeyCode from an evdev numeric code value
            /// Returns None if the code is not supported/mapped
            #[must_use]
            pub const fn from_evdev_code(code: u16) -> Option<Self> {
                match code {
                    $(
                        $code => Some(Self::$variant),
                    )*
                    _ => None,
                }
            }

            /// Get the evdev numeric code value for this KeyCode
            #[must_use]
            pub const fn code(self) -> u16 {
                self as u16
            }

            /// Check if this key is a modifier (Ctrl, Shift, Alt, GUI)
            #[must_use]
            pub const fn is_modifier(self) -> bool {
                matches!(self.category(), KeyCategory::Modifier)
            }

            /// Get the category of this key
            #[must_use]
            pub const fn category(self) -> KeyCategory {
                match self {
                    $(
                        Self::$variant => define_keycodes!(@@category $category),
                    )*
                }
            }

            /// Get the name of this key (e.g., "KC_A")
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(
                        Self::$variant => stringify!($variant),
                    )*
                }
            }
        }
    };

    // Helper: get category from identifier
    (@@category modifier) => { KeyCategory::Modifier };
    (@@category letter) => { KeyCategory::Letter };
    (@@category number) => { KeyCategory::Number };
    (@@category function) => { KeyCategory::Function };
    (@@category special) => { KeyCategory::Special };
    (@@category navigation) => { KeyCategory::Navigation };
    (@@category numpad) => { KeyCategory::Numpad };
    (@@category media) => { KeyCategory::Media };
    (@@category international) => { KeyCategory::International };
    (@@category lock) => { KeyCategory::Lock };
    (@@category general) => { KeyCategory::General };
}

// Generate the KeyCode enum with all keycodes
define_keycodes! {
    // Letters
    KC_A = 30, letter,
    KC_B = 48, letter,
    KC_C = 46, letter,
    KC_D = 32, letter,
    KC_E = 18, letter,
    KC_F = 33, letter,
    KC_G = 34, letter,
    KC_H = 35, letter,
    KC_I = 23, letter,
    KC_J = 36, letter,
    KC_K = 37, letter,
    KC_L = 38, letter,
    KC_M = 50, letter,
    KC_N = 49, letter,
    KC_O = 24, letter,
    KC_P = 25, letter,
    KC_Q = 16, letter,
    KC_R = 19, letter,
    KC_S = 31, letter,
    KC_T = 20, letter,
    KC_U = 22, letter,
    KC_V = 47, letter,
    KC_W = 17, letter,
    KC_X = 45, letter,
    KC_Y = 21, letter,
    KC_Z = 44, letter,

    // Numbers
    KC_1 = 2, number,
    KC_2 = 3, number,
    KC_3 = 4, number,
    KC_4 = 5, number,
    KC_5 = 6, number,
    KC_6 = 7, number,
    KC_7 = 8, number,
    KC_8 = 9, number,
    KC_9 = 10, number,
    KC_0 = 11, number,

    // Modifiers
    KC_LCTL = 29, modifier,
    KC_LSFT = 42, modifier,
    KC_LALT = 56, modifier,
    KC_LGUI = 125, modifier,
    KC_RCTL = 97, modifier,
    KC_RSFT = 54, modifier,
    KC_RALT = 100, modifier,
    KC_RGUI = 126, modifier,

    // Special keys
    KC_NO = 0, general,
    KC_ESC = 1, special,
    KC_CAPS = 58, lock,
    KC_TAB = 15, special,
    KC_SPC = 57, special,
    KC_ENT = 28, special,
    KC_BSPC = 14, special,
    KC_DEL = 111, special,
    KC_GRV = 41, special,
    KC_MINS = 12, special,
    KC_EQL = 13, special,
    KC_LBRC = 26, special,
    KC_RBRC = 27, special,
    KC_BSLS = 43, special,
    KC_SCLN = 39, special,
    KC_QUOT = 40, special,
    KC_COMM = 51, special,
    KC_DOT = 52, special,
    KC_SLSH = 53, special,

    // Print Screen / System keys
    KC_PSCR = 99, special,
    KC_BRK = 101, special,

    // Arrow keys
    KC_LEFT = 105, navigation,
    KC_DOWN = 108, navigation,
    KC_UP = 103, navigation,
    KC_RGHT = 106, navigation,

    // Function keys
    KC_F1 = 59, function,
    KC_F2 = 60, function,
    KC_F3 = 61, function,
    KC_F4 = 62, function,
    KC_F5 = 63, function,
    KC_F6 = 64, function,
    KC_F7 = 65, function,
    KC_F8 = 66, function,
    KC_F9 = 67, function,
    KC_F10 = 68, function,
    KC_F11 = 69, function,
    KC_F12 = 70, function,

    // Special function keys
    KC_F13 = 183, function,
    KC_F14 = 184, function,
    KC_F15 = 185, function,
    KC_F16 = 186, function,
    KC_F17 = 187, function,
    KC_F18 = 188, function,
    KC_F19 = 189, function,
    KC_F20 = 190, function,
    KC_F21 = 191, function,
    KC_F22 = 192, function,
    KC_F23 = 193, function,
    KC_F24 = 194, function,

    // Lock keys
    KC_SLCK = 214, lock,
    KC_NLCK = 215, lock,
    KC_PAUS = 216, special,

    // Navigation
    KC_INS = 110, navigation,
    KC_HOME = 102, navigation,
    KC_PGUP = 104, navigation,
    KC_END = 107, navigation,
    KC_PGDN = 109, navigation,

    // Numpad
    KC_NUBS = 86, numpad,
    KC_PSLS = 200, numpad,
    KC_PAST = 201, numpad,
    KC_PMNS = 82, numpad,
    KC_PPLS = 87, numpad,
    KC_PENT = 202, numpad,
    KC_P1 = 203, numpad,
    KC_P2 = 204, numpad,
    KC_P3 = 205, numpad,
    KC_P4 = 206, numpad,
    KC_P5 = 207, numpad,
    KC_P6 = 208, numpad,
    KC_P7 = 209, numpad,
    KC_P8 = 210, numpad,
    KC_P9 = 211, numpad,
    KC_P0 = 212, numpad,
    KC_PDOT = 213, numpad,

    // Media keys
    KC_MUTE = 217, media,
    KC_VOLD = 218, media,
    KC_VOLU = 219, media,

    // Application keys
    KC_APP = 220, special,
    KC_HELP = 221, special,
    KC_SCRL = 222, lock,
    KC_ASST = 226, special,

    // Power management
    KC_PWR = 223, special,
    KC_SLEP = 224, special,
    KC_WAKE = 225, special,

    // International keys (Japanese)
    KC_INT1 = 121, international,
    KC_INT2 = 122, international,
    KC_INT3 = 123, international,
    KC_INT4 = 124, international,
    KC_INT5 = 128, international,

    // Language keys
    KC_LANG1 = 131, international,
    KC_LANG2 = 132, international,
    KC_LANG3 = 133, international,
    KC_LANG4 = 134, international,
    KC_LANG5 = 135, international,
    KC_LANG6 = 136, international,
    KC_LANG7 = 137, international,
    KC_LANG8 = 138, international,
    KC_LANG9 = 139, international,

    // Korean keys
    KC_HAEN = 140, international,
    KC_HANJ = 141, international,
}
