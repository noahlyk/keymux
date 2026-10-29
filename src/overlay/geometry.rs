//! Where each steno key sits on the panel. Computed once per panel size, then the drawing
//! code just indexes it by slot: no layout work per frame.

use crate::config::{Half, OverlayPlacement};
use crate::steno::layout::{slot_by_name, EXTRA_KEYS, SLOTS};
use std::collections::HashMap;

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
/// Key height relative to its width. Steno keys are tall pads, not squares.
const KEY_ASPECT: f32 = 1.75;
/// Height of the `#` bar relative to a key's width
const NUMBER_BAR: f32 = 0.5;
/// Gap between keys relative to a key's width
const GAP: f32 = 0.16;
/// Thumb key height relative to a key's width
const THUMB_ASPECT: f32 = 1.35;
/// Extra space above the thumb row relative to a key's width
const THUMB_DROP: f32 = 0.35;
/// Height of the row for keys beyond the standard 23, relative to a key's width: the same
/// scale as the `#` bar, since it is the same kind of marker row.
const EXTRA_ROW: f32 = 0.5;

/// The keyboard's key rectangles at one size.
#[derive(Debug, Clone)]
pub struct Keyboard {
    /// Indexed by stroke slot, the same bit order as strokes
    pub keys: [Rect; 23],
    /// One pad per [`EXTRA_KEYS`] entry, in that order. A key here has no physical slot of its
    /// own — a keyboard binds it to whatever key it has free (see `layout_overrides`) — so
    /// each gets its own badge in a row above everything else, rather than the panel guessing
    /// at a layout it has no way to know is right for a given keyboard.
    pub extra: Vec<Rect>,
    /// Width and height of the whole keyboard
    pub size: (f32, f32),
}

impl Keyboard {
    /// Lay the keys out to fill `width` pixels, with the top-left corner at `(x, y)`. An extra
    /// key not named in `overlay_overrides` draws in the default badge row; see
    /// [`OverlayPlacement`].
    #[must_use]
    pub fn new(
        x: f32,
        y: f32,
        width: f32,
        overlay_overrides: &HashMap<String, OverlayPlacement>,
    ) -> Self {
        let placement = |i: usize| -> &OverlayPlacement {
            EXTRA_KEYS
                .get(i)
                .and_then(|&(ch, _)| overlay_overrides.get(&ch.to_string()))
                .unwrap_or(&OverlayPlacement::Badge)
        };
        // The slot a `SplitKey` resolves to, or `None` for `Badge` and for a `SplitKey` naming
        // a slot that doesn't exist — which also draws as a badge, so it must count as needing
        // the row the same way, rather than falling back to a rect with no room reserved.
        let resolved_slot = |i: usize| -> Option<usize> {
            match placement(i) {
                OverlayPlacement::SplitKey { slot, .. } => {
                    slot_by_name(slot).filter(|&idx| idx < SLOTS.len())
                }
                OverlayPlacement::Badge => None,
            }
        };
        // The badge row is only there for an extra key that still wants it; one entirely
        // reassigned onto an existing key (`SplitKey`) needs no space of its own.
        let needs_badge_row = (0..EXTRA_KEYS.len()).any(|i| resolved_slot(i).is_none());

        let unit = width / GAP.mul_add(COLUMNS - 1.0, COLUMNS);
        let gap = unit * GAP;
        let step = unit + gap;
        let key_h = unit * KEY_ASPECT;
        let thumb_h = unit * THUMB_ASPECT;
        let extra_h = unit * EXTRA_ROW;
        let number_y = if needs_badge_row { y + extra_h + gap } else { y };
        let top = number_y + unit * NUMBER_BAR + gap;
        let bottom = top + key_h + gap;
        let thumb = bottom + key_h + gap + unit * THUMB_DROP;
        let mut keys: [Rect; 23] = std::array::from_fn(|slot| {
            let (column, row, rows) = PLACES[slot];
            let (row_y, height) = match row {
                Row::Number => {
                    return Rect {
                        x,
                        y: number_y,
                        w: width,
                        h: unit * NUMBER_BAR,
                    }
                }
                Row::Top => (top, key_h),
                Row::Bottom => (bottom, key_h),
                Row::Thumb => (thumb, thumb_h),
            };
            Rect {
                x: column.mul_add(step, x),
                y: row_y,
                w: unit,
                // A tall key covers both finger rows and the gap between them
                h: height.mul_add(rows, gap * (rows - 1.0)),
            }
        });
        let mut badge_slots = 0usize;
        let extra = (0..EXTRA_KEYS.len())
            .map(|i| {
                resolved_slot(i).map_or_else(
                    || {
                        let r = badge_rect(x, y, step, unit, extra_h, badge_slots);
                        badge_slots += 1;
                        r
                    },
                    |idx| {
                        let OverlayPlacement::SplitKey { half, .. } = placement(i) else {
                            unreachable!("resolved_slot only returns Some for SplitKey")
                        };
                        half_of(keys[idx], *half)
                    },
                )
            })
            .collect();
        // A `SplitKey` only halves the badge above: the named slot itself still drew full
        // height underneath it, so the two overlapped instead of stacking. Shrink the slot to
        // whichever half the badge didn't take, now that every badge has read its full rect.
        for i in 0..EXTRA_KEYS.len() {
            if let Some(idx) = resolved_slot(i) {
                let OverlayPlacement::SplitKey { half, .. } = placement(i) else {
                    unreachable!("resolved_slot only returns Some for SplitKey")
                };
                keys[idx] = half_of(keys[idx], half.opposite());
            }
        }
        let height = thumb + thumb_h - y;
        Self {
            keys,
            extra,
            size: (width, height),
        }
    }

    /// The letter printed on a slot's key.
    #[must_use]
    pub const fn label(slot: usize) -> char {
        SLOTS[slot].0
    }

    /// The character printed on an [`EXTRA_KEYS`] badge.
    #[must_use]
    pub const fn extra_label(i: usize) -> char {
        EXTRA_KEYS[i].0
    }

    /// The stroke bit an [`EXTRA_KEYS`] badge lights up for.
    #[must_use]
    pub const fn extra_bit(i: usize) -> u32 {
        1 << (SLOTS.len() + i)
    }

    /// Which part of the keyboard a slot is on, which decides its colour.
    #[must_use]
    pub const fn group(slot: usize) -> KeyGroup {
        match slot {
            0..=6 => KeyGroup::LeftHand,        // S T K P W H R
            7 | 8 | 10 | 11 => KeyGroup::Vowel, // A O E U
            9 | 22 => KeyGroup::Misc,           // * and #
            _ => KeyGroup::RightHand,           // F R P B L G T S D Z
        }
    }
}

/// An extra key's spot in the default badge row, the `badge_slots`-th one assigned so far —
/// not necessarily its own index among every extra key, since one reassigned onto an existing
/// key via `SplitKey` takes no spot in this row at all.
const fn badge_rect(x: f32, y: f32, step: f32, unit: f32, extra_h: f32, badge_slots: usize) -> Rect {
    Rect {
        x: (badge_slots as f32).mul_add(step, x),
        y,
        w: unit,
        h: extra_h,
    }
}

/// Half of `rect`, split along its height.
fn half_of(rect: Rect, half: Half) -> Rect {
    let h = rect.h / 2.0;
    Rect {
        y: match half {
            Half::Top => rect.y,
            Half::Bottom => rect.y + h,
        },
        h,
        ..rect
    }
}

/// A part of the steno keyboard, coloured as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyGroup {
    LeftHand,
    /// The thumb keys: `A O` and `E U`
    Vowel,
    RightHand,
    /// `*` and `#`
    Misc,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(x: f32, y: f32, width: f32) -> Keyboard {
        Keyboard::new(x, y, width, &HashMap::new())
    }

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn no_two_keys_overlap_and_all_fit_the_width() {
        let keyboard = kb(10.0, 20.0, 248.0);
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
    fn every_key_is_in_the_group_its_name_says() {
        for (slot, (_, name)) in SLOTS.iter().enumerate() {
            let expected = match *name {
                "A-" | "O-" | "-E" | "-U" => KeyGroup::Vowel,
                "*" | "#" => KeyGroup::Misc,
                name if name.ends_with('-') => KeyGroup::LeftHand,
                _ => KeyGroup::RightHand,
            };
            assert_eq!(Keyboard::group(slot), expected, "slot {slot} ({name})");
        }
    }

    #[test]
    fn keys_are_tall_pads() {
        let keyboard = kb(0.0, 0.0, 250.0);
        // T- is a single finger key: clearly taller than wide
        let t = keyboard.keys[1];
        assert!(t.h > t.w * 1.5, "{t:?}");
    }

    #[test]
    fn tall_keys_span_both_finger_rows() {
        let keyboard = kb(0.0, 0.0, 250.0);
        let (s, k) = (keyboard.keys[0], keyboard.keys[2]);
        assert!((s.bottom() - k.bottom()).abs() < 0.01);
        assert!(
            keyboard.keys[22].bottom() < keyboard.keys[1].y,
            "# bar sits above the keys"
        );
    }

    #[test]
    fn an_extra_key_with_no_override_draws_in_the_default_badge_row() {
        let keyboard = kb(0.0, 0.0, 250.0);
        let badge = keyboard.extra[0];
        // The badge row sits above the # bar and every other key
        assert!(badge.bottom() <= keyboard.keys[22].y + 0.01, "{badge:?}");
    }

    #[test]
    fn split_key_puts_an_extra_key_on_half_of_the_named_slot_and_shrinks_it_to_the_other_half() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "^".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &overrides);
        let s = keyboard.keys[0];
        let badge = keyboard.extra[0];
        // `*` is the other tall (2-row) key, untouched by the override, in this same keyboard
        // instance — the reference for what S-'s own full height would otherwise be.
        let star = keyboard.keys[9];
        assert!((badge.x - s.x).abs() < 0.01, "same column as S-");
        assert!((badge.w - s.w).abs() < 0.01, "same width as S-");
        assert!((badge.y - star.y).abs() < 0.01, "top half starts where a tall key starts");
        assert!((badge.h - star.h / 2.0).abs() < 0.01, "exactly half a tall key's height");
        // S- itself must have shrunk to the other half, not stayed full height underneath it.
        assert!((s.h - star.h / 2.0).abs() < 0.01, "S- shrank to half a tall key's height: {s:?}");
        assert!((s.y - badge.bottom()).abs() < 0.01, "S- starts right where the badge ends");
        assert!(!overlaps(&badge, &s), "badge and S- must stack, not overlap: {badge:?} {s:?}");
    }

    #[test]
    fn split_key_bottom_half_sits_below_the_top_half() {
        let mut top = HashMap::new();
        top.insert(
            "^".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let mut bottom = HashMap::new();
        bottom.insert(
            "^".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Bottom,
            },
        );
        let top_rect = Keyboard::new(0.0, 0.0, 250.0, &top).extra[0];
        let bottom_rect = Keyboard::new(0.0, 0.0, 250.0, &bottom).extra[0];
        assert!((bottom_rect.y - top_rect.bottom()).abs() < 0.01);
        assert!(!overlaps(&top_rect, &bottom_rect));
    }

    #[test]
    fn every_extra_key_split_onto_a_slot_leaves_no_badge_row() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "^".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let y = 0.0;
        let with_split = Keyboard::new(0.0, y, 250.0, &overrides);
        let with_badge_row = kb(0.0, y, 250.0);
        // With every extra key reassigned onto an existing slot, the # bar starts right where
        // the panel itself does: no row reserved above it, unlike the default badge-row case.
        assert!((with_split.keys[22].y - y).abs() < 0.01, "{:?}", with_split.keys[22].y);
        assert!(
            with_badge_row.keys[22].y > with_split.keys[22].y + 0.01,
            "the default badge row should still take real space: {:?} vs {:?}",
            with_badge_row.keys[22].y,
            with_split.keys[22].y
        );
    }

    #[test]
    fn an_unknown_split_key_slot_falls_back_to_the_badge_row() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "^".to_string(),
            OverlayPlacement::SplitKey {
                slot: "nonsense".to_string(),
                half: Half::Top,
            },
        );
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &overrides);
        let badge = keyboard.extra[0];
        assert!(badge.bottom() <= keyboard.keys[22].y + 0.01, "{badge:?}");
    }
}
