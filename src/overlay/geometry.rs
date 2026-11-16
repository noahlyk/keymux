//! Where each steno key sits on the panel. Computed once per panel size, then the drawing
//! code just indexes it by slot: no layout work per frame.

use crate::config::{Half, OverlayPlacement};
use crate::steno::layout::Layout;
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
/// Gap between the two keys of a column that go together (`T` over `K`), relative to the gap
/// between everything else: seven tenths of it, so the pair still reads as one piece.
const PAIR_GAP: f32 = 0.7;
/// ...and never under this many pixels, however small the overlay is drawn
const MIN_PAIR_GAP: f32 = 2.0;
/// Thumb key height relative to a key's width
const THUMB_ASPECT: f32 = 1.35;
/// Extra space above the thumb row relative to a key's width
const THUMB_DROP: f32 = 0.35;
/// Height of the row for keys beyond the standard 23, relative to a key's width: the same
/// scale as the `#` bar, since it is the same kind of marker row.
const EXTRA_ROW: f32 = 0.5;

/// The keyboard's key rectangles at one size.
///
/// The grid itself (`PLACES`) only knows Plover's own 23-key shape. `Keyboard::new` panics if
/// `layout` has a different number of standard-grammar keys than that - which no real config
/// can trigger today: `config.ron`'s `extra_keys` only ever appends keys past the standard 23,
/// never resizes the standard groups themselves.
#[derive(Debug, Clone)]
pub struct Keyboard {
    /// Indexed by stroke slot, the same bit order as strokes
    pub keys: [Rect; 23],
    /// One pad per [`Layout::extra_keys`] entry, in that order. A key here has no physical slot
    /// of its own — a keyboard binds it to whatever key it has free (see `layout_overrides`) —
    /// so each gets its own badge in a row above everything else, rather than the panel
    /// guessing at a layout it has no way to know is right for a given keyboard.
    pub extra: Vec<Rect>,
    /// For each of `extra`, the slot it shares and the half of it it took, or `None` for a badge.
    /// The drawing code squares the corners where the two halves meet.
    pub splits: Vec<Option<(usize, Half)>>,
    /// For each of `extra`, whether it is a tall key in a column of its own (`ColumnBefore`,
    /// `ColumnAfter`), drawn like `*`: soft on top, round at the bottom.
    pub columns: Vec<bool>,
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
        layout: &Layout,
        overlay_overrides: &HashMap<String, OverlayPlacement>,
    ) -> Self {
        assert_eq!(
            layout.number_slot(),
            PLACES.len() - 1,
            "the diagram only knows Plover's own 23-key shape so far"
        );
        let extra_keys = layout.extra_keys();
        let placement = |i: usize| -> &OverlayPlacement {
            extra_keys
                .get(i)
                .and_then(|k| overlay_overrides.get(&k.char.to_string()))
                .unwrap_or(&OverlayPlacement::Badge)
        };
        // The slot a `SplitKey` resolves to, or `None` for `Badge` and for a `SplitKey` naming
        // a slot that doesn't exist — which also draws as a badge, so it must count as needing
        // the row the same way, rather than falling back to a rect with no room reserved.
        let resolved_slot = |i: usize| -> Option<usize> {
            match placement(i) {
                OverlayPlacement::SplitKey { slot, .. } => {
                    layout.slot_by_name(slot).filter(|&idx| idx < 23)
                }
                _ => None,
            }
        };
        // The grid column an extra key's own column opens at, or `None` if it has none (not a
        // column placement, or one naming no finger or star slot, which draws as a badge).
        let column_edge = |i: usize| -> Option<f32> {
            let (name, after) = match placement(i) {
                OverlayPlacement::ColumnBefore(name) => (name, false),
                OverlayPlacement::ColumnAfter(name) => (name, true),
                _ => return None,
            };
            // Not the `#` bar (it spans every column), not another extra key
            let slot = layout.slot_by_name(name).filter(|&idx| idx < 22)?;
            let column = PLACES[slot].0.floor();
            Some(if after { column + 1.0 } else { column })
        };
        // Each column placement opens a new column at its edge (a whole grid column), in the
        // order the extra keys are declared: later ones sit further right at the same edge.
        // Every place at or right of the edge moves over by one.
        let inserts: Vec<(usize, f32)> = (0..extra_keys.len())
            .filter_map(|i| column_edge(i).map(|edge| (i, edge)))
            .collect();
        let shift = |column: f32| -> f32 {
            column + inserts.iter().filter(|(_, edge)| *edge <= column).count() as f32
        };
        let columns = COLUMNS + inserts.len() as f32;
        // The badge row is only there for an extra key that still wants it; one entirely
        // reassigned onto an existing key (`SplitKey`) or given a column needs no space of its own.
        let needs_badge_row =
            (0..extra_keys.len()).any(|i| resolved_slot(i).is_none() && column_edge(i).is_none());

        let unit = width / GAP.mul_add(columns - 1.0, columns);
        let gap = unit * GAP;
        let step = unit + gap;
        let key_h = unit * KEY_ASPECT;
        let thumb_h = unit * THUMB_ASPECT;
        let extra_h = unit * EXTRA_ROW;
        let number_y = if needs_badge_row {
            y + extra_h + gap
        } else {
            y
        };
        let top = number_y + unit * NUMBER_BAR + gap;
        let pair_gap = (gap * PAIR_GAP).max(MIN_PAIR_GAP);
        let bottom = top + key_h + pair_gap;
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
                x: shift(column).mul_add(step, x),
                y: row_y,
                w: unit,
                // A tall key covers both finger rows and the gap between them
                h: height.mul_add(rows, pair_gap * (rows - 1.0)),
            }
        });
        let mut badge_slots = 0usize;
        let extra = (0..extra_keys.len())
            .map(|i| {
                if let Some(edge) = column_edge(i) {
                    // Past every column opened at an earlier edge, and every one opened at the
                    // same edge before this key
                    let earlier = inserts
                        .iter()
                        .filter(|&&(j, e)| e < edge || (e == edge && j < i))
                        .count();
                    return Rect {
                        x: (edge + earlier as f32).mul_add(step, x),
                        y: top,
                        w: unit,
                        // A tall key covers both finger rows and the gap between them
                        h: key_h.mul_add(2.0, pair_gap),
                    };
                }
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
                        half_of(keys[idx], *half, pair_gap)
                    },
                )
            })
            .collect();
        // A `SplitKey` only halves the badge above: the named slot itself still drew full
        // height underneath it, so the two overlapped instead of stacking. Shrink the slot to
        // whichever half the badge didn't take, now that every badge has read its full rect.
        for i in 0..extra_keys.len() {
            if let Some(idx) = resolved_slot(i) {
                let OverlayPlacement::SplitKey { half, .. } = placement(i) else {
                    unreachable!("resolved_slot only returns Some for SplitKey")
                };
                keys[idx] = half_of(keys[idx], half.opposite(), pair_gap);
            }
        }
        let splits = (0..extra_keys.len())
            .map(|i| {
                let OverlayPlacement::SplitKey { half, .. } = placement(i) else {
                    return None;
                };
                resolved_slot(i).map(|idx| (idx, *half))
            })
            .collect();
        let tall = (0..extra_keys.len())
            .map(|i| column_edge(i).is_some())
            .collect();
        let height = thumb + thumb_h - y;
        Self {
            keys,
            extra,
            splits,
            columns: tall,
            size: (width, height),
        }
    }

    /// The letter printed on a slot's key.
    #[must_use]
    pub fn label(layout: &Layout, slot: usize) -> char {
        layout.key_at(slot).map_or('?', |k| k.char)
    }

    /// The character printed on a [`Layout::extra_keys`] badge.
    #[must_use]
    pub fn extra_label(layout: &Layout, i: usize) -> char {
        layout.extra_keys()[i].char
    }

    /// The stroke bit a [`Layout::extra_keys`] badge lights up for.
    #[must_use]
    pub const fn extra_bit(layout: &Layout, i: usize) -> u32 {
        1 << (layout.extra_start() + i)
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
const fn badge_rect(
    x: f32,
    y: f32,
    step: f32,
    unit: f32,
    extra_h: f32,
    badge_slots: usize,
) -> Rect {
    Rect {
        x: (badge_slots as f32).mul_add(step, x),
        y,
        w: unit,
        h: extra_h,
    }
}

/// Half of `rect`, split along its height, with `gap` left between the two halves — the gap
/// between the two keys of a column, so each half is as tall as a finger key and lines up with its row, so a split key's halves don't touch where
/// nothing else on the panel does.
fn half_of(rect: Rect, half: Half, gap: f32) -> Rect {
    let h = (rect.h - gap) / 2.0;
    Rect {
        y: match half {
            Half::Top => rect.y,
            Half::Bottom => rect.y + h + gap,
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
        Keyboard::new(x, y, width, &Layout::plover(), &HashMap::new())
    }

    /// A layout exactly like Plover's, plus a `+` "join" extra key - the shape any theory that
    /// wants an extra key beyond the standard 23 would build, not anything geometry.rs itself
    /// treats specially.
    fn layout_with_join() -> Layout {
        use crate::steno::layout::KeySpec;
        Layout::plover()
            .with_extra(KeySpec::new('+', "join"))
            .unwrap()
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
        let layout = Layout::plover();
        for slot in 0..23 {
            let name = &layout.key_at(slot).unwrap().name;
            let expected = match name.as_str() {
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
        let layout = layout_with_join();
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &HashMap::new());
        let badge = keyboard.extra[0];
        // The badge row sits above the # bar and every other key
        assert!(badge.bottom() <= keyboard.keys[22].y + 0.01, "{badge:?}");
    }

    fn column_before(slot: &str) -> HashMap<String, OverlayPlacement> {
        HashMap::from([(
            "+".to_string(),
            OverlayPlacement::ColumnBefore(slot.to_string()),
        )])
    }

    fn step_of(keyboard: &Keyboard) -> f32 {
        // One grid column to the next: T- (column 1) to P- (column 2)
        keyboard.keys[3].x - keyboard.keys[1].x
    }

    #[test]
    fn a_column_before_the_star_is_a_tall_key_between_the_left_hand_and_the_star() {
        let layout = layout_with_join();
        let plain = Keyboard::new(0.0, 0.0, 250.0, &layout, &HashMap::new());
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &column_before("*"));
        let join = keyboard.extra[0];
        let (star, h, r) = (keyboard.keys[9], keyboard.keys[5], keyboard.keys[6]);
        let step = step_of(&keyboard);
        // Just left of the star, right of the H- / R- column, a whole column each side
        assert!((star.x - join.x - step).abs() < 0.01, "{join:?} {star:?}");
        assert!((join.x - h.x - step).abs() < 0.01, "{h:?} {join:?}");
        // As tall as the star: both finger rows
        assert!((join.y - star.y).abs() < 0.01 && (join.h - star.h).abs() < 0.01);
        assert!((join.w - star.w).abs() < 0.01);
        assert!((join.y - h.y).abs() < 0.01 && (join.bottom() - r.bottom()).abs() < 0.01);
        // No badge row: the # bar is still at the top, the diagram as tall as before
        assert!((keyboard.keys[22].y - 0.0).abs() < 0.01);
        assert!(keyboard.columns == vec![true] && keyboard.splits == vec![None]);
        // One column more, so each column is narrower, but the keyboard still fills the width
        assert!(keyboard.keys[1].w < plain.keys[1].w);
        let right = keyboard.keys.iter().map(|k| k.x + k.w).fold(0.0, f32::max);
        assert!((right - 250.0).abs() < 0.01, "{right}");
        // The left hand does not move (in columns), the star and the right hand move over by one
        for slot in [0, 1, 2, 3, 4, 5, 6, 7, 8] {
            let before = plain.keys[slot].x / (plain.keys[1].w * (1.0 + GAP));
            let after = keyboard.keys[slot].x / (keyboard.keys[1].w * (1.0 + GAP));
            assert!((before - after).abs() < 0.01, "slot {slot}");
        }
        for slot in [9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21] {
            let before = plain.keys[slot].x / (plain.keys[1].w * (1.0 + GAP));
            let after = keyboard.keys[slot].x / (keyboard.keys[1].w * (1.0 + GAP));
            assert!((before + 1.0 - after).abs() < 0.01, "slot {slot}");
        }
    }

    #[test]
    fn a_column_never_overlaps_or_leaves_the_panel() {
        let layout = layout_with_join();
        for slot in ["S-", "T-", "H-", "*", "-F", "-Z", "A-", "-U", "-D"] {
            for after in [false, true] {
                let name = slot.to_string();
                let placement = if after {
                    OverlayPlacement::ColumnAfter(name)
                } else {
                    OverlayPlacement::ColumnBefore(name)
                };
                let overrides = HashMap::from([("+".to_string(), placement)]);
                let keyboard = Keyboard::new(10.0, 20.0, 248.0, &layout, &overrides);
                let join = keyboard.extra[0];
                assert!(join.x >= 10.0 - 0.01 && join.x + join.w <= 258.0 + 0.01);
                for (i, key) in keyboard.keys[..22].iter().enumerate() {
                    assert!(!overlaps(&join, key), "{slot} {after}: slot {i}");
                    assert!(key.x + key.w <= 258.0 + 0.01, "{slot} {after}: slot {i}");
                }
            }
        }
    }

    #[test]
    fn a_column_after_a_slot_sits_right_of_that_slots_column() {
        let layout = layout_with_join();
        let overrides = HashMap::from([(
            "+".to_string(),
            OverlayPlacement::ColumnAfter("*".to_string()),
        )]);
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &overrides);
        let step = step_of(&keyboard);
        let (star, join, f) = (keyboard.keys[9], keyboard.extra[0], keyboard.keys[12]);
        assert!((join.x - star.x - step).abs() < 0.01);
        assert!((f.x - join.x - step).abs() < 0.01);
        // Before the same slot is the mirror: the two placements put the key on opposite sides
        let before = Keyboard::new(0.0, 0.0, 250.0, &layout, &column_before("*"));
        assert!(before.extra[0].x < before.keys[9].x);
    }

    #[test]
    fn several_columns_at_one_edge_line_up_in_declaration_order() {
        use crate::steno::layout::KeySpec;
        let layout = layout_with_join()
            .with_extra(KeySpec::new('^', "caret"))
            .unwrap();
        let overrides = HashMap::from([
            ("+".to_string(), OverlayPlacement::ColumnBefore("*".to_string())),
            ("^".to_string(), OverlayPlacement::ColumnBefore("*".to_string())),
        ]);
        let keyboard = Keyboard::new(0.0, 0.0, 300.0, &layout, &overrides);
        let step = step_of(&keyboard);
        assert!((keyboard.extra[1].x - keyboard.extra[0].x - step).abs() < 0.01);
        assert!((keyboard.keys[9].x - keyboard.extra[1].x - step).abs() < 0.01);
    }

    #[test]
    fn a_column_naming_no_finger_or_star_slot_draws_as_a_badge() {
        let layout = layout_with_join();
        for slot in ["NOPE", "#", "+"] {
            let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &column_before(slot));
            let plain = Keyboard::new(0.0, 0.0, 250.0, &layout, &HashMap::new());
            assert_eq!(keyboard.extra, plain.extra, "{slot}");
            assert_eq!(keyboard.keys, plain.keys, "{slot}");
            assert_eq!(keyboard.columns, vec![false]);
        }
    }

    #[test]
    fn a_column_and_a_badge_can_share_one_keyboard() {
        use crate::steno::layout::KeySpec;
        let layout = layout_with_join()
            .with_extra(KeySpec::new('^', "caret"))
            .unwrap();
        let keyboard = Keyboard::new(0.0, 0.0, 300.0, &layout, &column_before("*"));
        // `^` is a badge, so the row above the # bar is back, and the column is not in it
        assert!(keyboard.extra[1].bottom() <= keyboard.keys[22].y + 0.01);
        assert!(keyboard.extra[0].y > keyboard.keys[22].y);
        assert_eq!(keyboard.columns, vec![true, false]);
    }

    #[test]
    fn no_column_leaves_the_keyboard_exactly_as_it_was() {
        let layout = layout_with_join();
        let a = Keyboard::new(3.0, 4.0, 250.0, &layout, &HashMap::new());
        let b = Keyboard::new(
            3.0,
            4.0,
            250.0,
            &layout,
            &HashMap::from([("+".to_string(), OverlayPlacement::Badge)]),
        );
        assert_eq!(a.keys, b.keys);
        assert_eq!(a.extra, b.extra);
        assert_eq!(a.size, b.size);
    }

    #[test]
    fn split_key_puts_an_extra_key_on_half_of_the_named_slot_and_shrinks_it_to_the_other_half() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let layout = layout_with_join();
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &overrides);
        let s = keyboard.keys[0];
        let badge = keyboard.extra[0];
        // `*` is the other tall (2-row) key, untouched by the override, in this same keyboard
        // instance — the reference for what S-'s own full height would otherwise be.
        let star = keyboard.keys[9];
        let unit = 250.0 / GAP.mul_add(COLUMNS - 1.0, COLUMNS);
        let gap = (unit * GAP * PAIR_GAP).max(MIN_PAIR_GAP);
        let half_height = (star.h - gap) / 2.0;
        assert!((badge.x - s.x).abs() < 0.01, "same column as S-");
        assert!((badge.w - s.w).abs() < 0.01, "same width as S-");
        assert!(
            (badge.y - star.y).abs() < 0.01,
            "top half starts where a tall key starts"
        );
        assert!(
            (badge.h - half_height).abs() < 0.01,
            "half a tall key's height, minus the gap"
        );
        // S- itself must have shrunk to the other half, not stayed full height underneath it.
        assert!(
            (s.h - half_height).abs() < 0.01,
            "S- shrank the same way: {s:?}"
        );
        assert!(
            (s.y - (badge.bottom() + gap)).abs() < 0.01,
            "S- starts a full gap below the badge, same as any other row: {s:?} {badge:?}"
        );
        assert!(
            !overlaps(&badge, &s),
            "badge and S- must stack, not overlap: {badge:?} {s:?}"
        );
    }

    #[test]
    fn split_key_bottom_half_sits_below_the_top_half() {
        let layout = layout_with_join();
        let mut top = HashMap::new();
        top.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let mut bottom = HashMap::new();
        bottom.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Bottom,
            },
        );
        let top_rect = Keyboard::new(0.0, 0.0, 250.0, &layout, &top).extra[0];
        let bottom_rect = Keyboard::new(0.0, 0.0, 250.0, &layout, &bottom).extra[0];
        let unit = 250.0 / GAP.mul_add(COLUMNS - 1.0, COLUMNS);
        let gap = (unit * GAP * PAIR_GAP).max(MIN_PAIR_GAP);
        assert!(
            (bottom_rect.y - (top_rect.bottom() + gap)).abs() < 0.01,
            "the column gap between the halves, like T over K: {top_rect:?} {bottom_rect:?}"
        );
        assert!(!overlaps(&top_rect, &bottom_rect));
    }

    #[test]
    fn every_extra_key_split_onto_a_slot_leaves_no_badge_row() {
        let layout = layout_with_join();
        let mut overrides = HashMap::new();
        overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let y = 0.0;
        let with_split = Keyboard::new(0.0, y, 250.0, &layout, &overrides);
        let with_badge_row = Keyboard::new(0.0, y, 250.0, &layout, &HashMap::new());
        // With every extra key reassigned onto an existing slot, the # bar starts right where
        // the panel itself does: no row reserved above it, unlike the default badge-row case.
        assert!(
            (with_split.keys[22].y - y).abs() < 0.01,
            "{:?}",
            with_split.keys[22].y
        );
        assert!(
            with_badge_row.keys[22].y > with_split.keys[22].y + 0.01,
            "the default badge row should still take real space: {:?} vs {:?}",
            with_badge_row.keys[22].y,
            with_split.keys[22].y
        );
    }

    #[test]
    fn an_unknown_split_key_slot_falls_back_to_the_badge_row() {
        let layout = layout_with_join();
        let mut overrides = HashMap::new();
        overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "nonsense".to_string(),
                half: Half::Top,
            },
        );
        let keyboard = Keyboard::new(0.0, 0.0, 250.0, &layout, &overrides);
        let badge = keyboard.extra[0];
        assert!(badge.bottom() <= keyboard.keys[22].y + 0.01, "{badge:?}");
    }
}
