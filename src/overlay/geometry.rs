//! Where each steno key sits on the panel. Computed once per panel size, then the drawing
//! code just indexes it by slot: no layout work per frame.

use crate::steno::layout::SLOTS;

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Grid rows: the `#` bar, the two finger rows, the thumbs.
#[derive(Clone, Copy)]
enum Row {
    Number,
    Top,
    Bottom,
    Thumb,
}

/// Where each slot goes, in key units: (column, row, height in rows). Columns 0–3 are the
/// left hand, 4 the star, 5–9 the right hand. Thumbs sit at half columns, under the inner
/// fingers, as on the steno-lessons keyboard.
const PLACES: [(f32, Row, f32); 23] = [
    (0.0, Row::Top, 2.0),    // S-  (tall: top and bottom row are one key)
    (1.0, Row::Top, 1.0),    // T-
    (1.0, Row::Bottom, 1.0), // K-
    (2.0, Row::Top, 1.0),    // P-
    (2.0, Row::Bottom, 1.0), // W-
    (3.0, Row::Top, 1.0),    // H-
    (3.0, Row::Bottom, 1.0), // R-
    (2.5, Row::Thumb, 1.0),  // A-
    (3.5, Row::Thumb, 1.0),  // O-
    (4.0, Row::Top, 2.0),    // *   (tall)
    (5.5, Row::Thumb, 1.0),  // -E
    (6.5, Row::Thumb, 1.0),  // -U
    (5.0, Row::Top, 1.0),    // -F
    (5.0, Row::Bottom, 1.0), // -R
    (6.0, Row::Top, 1.0),    // -P
    (6.0, Row::Bottom, 1.0), // -B
    (7.0, Row::Top, 1.0),    // -L
    (7.0, Row::Bottom, 1.0), // -G
    (8.0, Row::Top, 1.0),    // -T
    (8.0, Row::Bottom, 1.0), // -S
    (9.0, Row::Top, 1.0),    // -D
    (9.0, Row::Bottom, 1.0), // -Z
    (0.0, Row::Number, 1.0), // #   (spans every column, see `NUMBER_BAR`)
];

const COLUMNS: f32 = 10.0;
/// Height of the `#` bar relative to a key
const NUMBER_BAR: f32 = 0.42;
/// Gap between keys relative to a key
const GAP: f32 = 0.14;
/// Extra space above the thumb row relative to a key
const THUMB_DROP: f32 = 0.3;

/// The keyboard's key rectangles at one size.
#[derive(Debug, Clone)]
pub struct Keyboard {
    /// Indexed by stroke slot, the same bit order as strokes
    pub keys: [Rect; 23],
    /// Width and height of the whole keyboard
    pub size: (f32, f32),
}

impl Keyboard {
    /// Lay the keys out to fill `width` pixels, with the top-left corner at `(x, y)`.
    #[must_use]
    pub fn new(x: f32, y: f32, width: f32) -> Self {
        let unit = width / (COLUMNS + GAP * (COLUMNS - 1.0));
        let gap = unit * GAP;
        let step = unit + gap;
        let row_y = |row: Row| match row {
            Row::Number => y,
            Row::Top => y + unit * NUMBER_BAR + gap,
            Row::Bottom => y + unit * NUMBER_BAR + gap + step,
            Row::Thumb => y + unit * NUMBER_BAR + gap + 2.0 * step + unit * THUMB_DROP,
        };
        let keys = std::array::from_fn(|slot| {
            let (column, row, rows) = PLACES[slot];
            if let Row::Number = row {
                return Rect {
                    x,
                    y,
                    w: width,
                    h: unit * NUMBER_BAR,
                };
            }
            Rect {
                x: x + column * step,
                y: row_y(row),
                w: unit,
                h: unit * rows + gap * (rows - 1.0),
            }
        });
        let height = row_y(Row::Thumb) + unit - y;
        Self {
            keys,
            size: (width, height),
        }
    }

    /// The letter printed on a slot's key.
    #[must_use]
    pub fn label(slot: usize) -> char {
        SLOTS[slot].0
    }

    /// Vowel keys and `*` get the vowel accent, like the mockup's yellow thumbs.
    #[must_use]
    pub fn is_vowel(slot: usize) -> bool {
        (7..=11).contains(&slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn no_two_keys_overlap_and_all_fit_the_width() {
        let keyboard = Keyboard::new(10.0, 20.0, 248.0);
        for (i, a) in keyboard.keys.iter().enumerate() {
            assert!(
                a.x >= 10.0 - 0.01 && a.x + a.w <= 258.0 + 0.01,
                "slot {i} out of bounds"
            );
            for (j, b) in keyboard.keys.iter().enumerate().skip(i + 1) {
                assert!(!overlaps(a, b), "slots {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn tall_keys_span_both_finger_rows() {
        let keyboard = Keyboard::new(0.0, 0.0, 250.0);
        let (s, k) = (keyboard.keys[0], keyboard.keys[2]);
        assert!((s.bottom() - k.bottom()).abs() < 0.01);
        assert!(
            keyboard.keys[22].bottom() < keyboard.keys[1].y,
            "# bar sits above the keys"
        );
    }
}
