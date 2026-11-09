//! Merged keys: while every key of a configured group is in the stroke, the diagram draws them as
//! one bigger key (`T P H` as one `N`). This module is the pure part: which groups exist (the
//! built-in standard steno letter chords plus the config's), which of them win for a set of lit
//! keys, and the outline of the fused shape. Drawing is in [`super::render`].
//!
//! The shape is the union of the members' rectangles with the gaps between neighbours filled.
//! Each outer corner keeps the radius of the member key that owns it; the corners where members
//! meet are flat, and a corner the union folds inwards (an `L`) gets a small fillet. So a thumb
//! paddle (round end, radius half its width) merged with its neighbour loses the round end in the
//! middle and keeps a quarter circle at each outer bottom corner, with a straight bottom between.

// Plain coordinate arithmetic: rewriting it as fused multiply-adds and midpoints would hide it
#![allow(
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::suspicious_operation_groupings,
    clippy::needless_range_loop,
    clippy::too_long_first_doc_paragraph
)]

use super::geometry::{Keyboard, Rect};
use crate::config::{KeyMerge, OverlayConfig};
use crate::steno::layout::Layout;
use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use tiny_skia::{Path, PathBuilder};

/// The standard steno letter chords whose keys sit next to each other on the diagram, as key
/// names and the letter they spell. (`S R` = V and `K R` = C are left out: their keys are not
/// neighbours, a key lies between them.) Right-hand labels carry no hyphen, like the key labels.
pub const DEFAULTS: &[(&[&str], &str)] = &[
    (&["T-", "P-", "H-"], "N"),
    (&["P-", "H-"], "M"),
    (&["K-", "W-", "R-"], "Y"),
    (&["T-", "K-"], "D"),
    (&["P-", "W-"], "B"),
    (&["H-", "R-"], "L"),
    (&["T-", "P-"], "F"),
    (&["T-", "K-", "P-", "W-"], "G"),
    (&["S-", "K-", "W-", "R-"], "J"),
    (&["S-", "T-", "K-", "P-", "W-"], "Z"),
    (&["-P", "-B"], "N"),
    (&["-P", "-L"], "M"),
    (&["-B", "-G"], "K"),
    (&["-F", "-P"], "CH"),
    (&["-R", "-B"], "SH"),
    (&["-P", "-B", "-L", "-G"], "J"),
    (&["-G", "-S"], "SHUN"),
];

/// How far apart, in key widths, two keys may be and still be neighbours: the gap between
/// columns is 0.16, the thumbs' drop under the finger rows is over 0.5.
pub const NEIGHBOUR_GAP: f32 = 0.3;
/// Width the keyboard is laid out at to check groups, when there is no real size (`keymux check`)
const PROBE_WIDTH: f32 = 1000.0;
const EPS: f32 = 0.01;

/// A group of standard keys drawn as one while all are in the stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// The member slots, ascending
    pub slots: Vec<usize>,
    /// The members' stroke bits
    pub bits: u32,
    pub label: Option<String>,
}

/// One member key of a shape: where it is and the radii of its top and bottom corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Member {
    pub rect: Rect,
    pub top: f32,
    pub bottom: f32,
}

impl Member {
    /// The radii are clamped as a lone key's are: half the width at most, the two within the height.
    #[must_use]
    pub fn new(rect: Rect, top: f32, bottom: f32) -> Self {
        let top = top.min(rect.w / 2.0).min(rect.h / 2.0);
        let bottom = bottom.min(rect.w / 2.0).min(rect.h - top);
        Self { rect, top, bottom }
    }
}

/// One corner of a merged outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corner {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    /// Pointing out (a corner of the shape), as opposed to folding in (an `L`'s inner corner)
    pub convex: bool,
}

/// Whether two keys are neighbours: they overlap along one axis and the gap along the other is
/// within `tol`.
#[must_use]
pub fn adjacent(a: &Rect, b: &Rect, tol: f32) -> bool {
    let dx = (b.x - (a.x + a.w)).max(a.x - (b.x + b.w));
    let dy = (b.y - a.bottom()).max(a.y - b.bottom());
    (dx <= tol && dy < -EPS) || (dy <= tol && dx < -EPS)
}

/// The rectangle filling the gap between two neighbours, over the span they share.
fn bridge(a: &Rect, b: &Rect) -> Option<Rect> {
    let dx = (b.x - (a.x + a.w)).max(a.x - (b.x + b.w));
    let dy = (b.y - a.bottom()).max(a.y - b.bottom());
    if dx > EPS && dy < 0.0 {
        let y = a.y.max(b.y);
        Some(Rect {
            x: if b.x > a.x { a.x + a.w } else { b.x + b.w },
            y,
            w: dx,
            h: a.bottom().min(b.bottom()) - y,
        })
    } else if dy > EPS && dx < 0.0 {
        let x = a.x.max(b.x);
        Some(Rect {
            x,
            y: if b.y > a.y { a.bottom() } else { b.bottom() },
            w: (a.x + a.w).min(b.x + b.w) - x,
            h: dy,
        })
    } else {
        None
    }
}

fn coords(values: impl Iterator<Item = f32>) -> Vec<f32> {
    let mut v: Vec<f32> = values.collect();
    v.sort_by(f32::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < EPS);
    v
}

/// The outline of the union of the members and the gaps between neighbours, clockwise on screen
/// from the top-left-most corner. Convex corners take the radius of the member that owns them,
/// concave ones `concave_radius`. Collinear points are dropped, so a straight edge is one edge.
#[must_use]
pub fn outline(members: &[Member], tol: f32, concave_radius: f32) -> Vec<Corner> {
    let mut rects: Vec<Rect> = members.iter().map(|m| m.rect).collect();
    for (i, a) in members.iter().enumerate() {
        for b in &members[i + 1..] {
            if adjacent(&a.rect, &b.rect, tol) {
                rects.extend(bridge(&a.rect, &b.rect));
            }
        }
    }
    let xs = coords(rects.iter().flat_map(|r| [r.x, r.x + r.w]));
    let ys = coords(rects.iter().flat_map(|r| [r.y, r.bottom()]));
    if xs.len() < 2 || ys.len() < 2 {
        return Vec::new();
    }
    let (nx, ny) = (xs.len() - 1, ys.len() - 1);
    let mut filled = vec![vec![false; ny]; nx];
    for (i, column) in filled.iter_mut().enumerate() {
        for (j, cell) in column.iter_mut().enumerate() {
            let (cx, cy) = ((xs[i] + xs[i + 1]) / 2.0, (ys[j] + ys[j + 1]) / 2.0);
            *cell = rects
                .iter()
                .any(|r| cx > r.x && cx < r.x + r.w && cy > r.y && cy < r.bottom());
        }
    }
    // Whatever the outside cannot reach is inside: the hole at the middle of a 2x2 block
    let mut outside = vec![vec![false; ny]; nx];
    let mut queue = VecDeque::new();
    for i in 0..nx {
        for j in 0..ny {
            if (i == 0 || j == 0 || i == nx - 1 || j == ny - 1) && !filled[i][j] {
                outside[i][j] = true;
                queue.push_back((i, j));
            }
        }
    }
    while let Some((i, j)) = queue.pop_front() {
        let near = [
            (i.wrapping_sub(1), j),
            (i + 1, j),
            (i, j.wrapping_sub(1)),
            (i, j + 1),
        ];
        for (a, b) in near {
            if a < nx && b < ny && !filled[a][b] && !outside[a][b] {
                outside[a][b] = true;
                queue.push_back((a, b));
            }
        }
    }
    for i in 0..nx {
        for j in 0..ny {
            filled[i][j] = !outside[i][j];
        }
    }
    let is_filled = |i: isize, j: isize| {
        i >= 0 && j >= 0 && (i as usize) < nx && (j as usize) < ny && filled[i as usize][j as usize]
    };
    // Clockwise edges around every filled cell that face an empty one
    let mut next: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
    for i in 0..nx {
        for j in 0..ny {
            if !filled[i][j] {
                continue;
            }
            let (ii, jj) = (i as isize, j as isize);
            if !is_filled(ii, jj - 1) {
                next.insert((i, j), (i + 1, j));
            }
            if !is_filled(ii + 1, jj) {
                next.insert((i + 1, j), (i + 1, j + 1));
            }
            if !is_filled(ii, jj + 1) {
                next.insert((i + 1, j + 1), (i, j + 1));
            }
            if !is_filled(ii - 1, jj) {
                next.insert((i, j + 1), (i, j));
            }
        }
    }
    let Some(&start) = next.keys().min() else {
        return Vec::new();
    };
    let mut points = vec![start];
    let mut at = next[&start];
    while at != start && points.len() <= next.len() {
        points.push(at);
        at = match next.get(&at) {
            Some(&to) => to,
            None => break,
        };
    }
    let n = points.len();
    let mut corners = Vec::new();
    for k in 0..n {
        let (prev, cur, nxt) = (points[(k + n - 1) % n], points[k], points[(k + 1) % n]);
        let din = (cur.0 as i64 - prev.0 as i64, cur.1 as i64 - prev.1 as i64);
        let dout = (nxt.0 as i64 - cur.0 as i64, nxt.1 as i64 - cur.1 as i64);
        let cross = din.0 * dout.1 - din.1 * dout.0;
        if cross == 0 {
            continue;
        }
        let (x, y) = (xs[cur.0], ys[cur.1]);
        let convex = cross > 0;
        let radius = if convex {
            // Arriving rightwards and turning down is a top-right corner, and so on round
            members
                .iter()
                .filter_map(|m| {
                    let r = m.rect;
                    let (cx, cy, radius) = if din.0 > 0 {
                        (r.x + r.w, r.y, m.top)
                    } else if din.1 > 0 {
                        (r.x + r.w, r.bottom(), m.bottom)
                    } else if din.0 < 0 {
                        (r.x, r.bottom(), m.bottom)
                    } else {
                        (r.x, r.y, m.top)
                    };
                    ((cx - x).abs() < 0.05 && (cy - y).abs() < 0.05).then_some(radius)
                })
                .fold(0.0, f32::max)
        } else {
            concave_radius
        };
        corners.push(Corner {
            x,
            y,
            radius,
            convex,
        });
    }
    corners
}

/// The smooth path of an outline: radii are clamped to half the shorter of the two edges at a
/// corner, so neighbouring corners never cross (two quarter circles of half the width meet).
#[must_use]
pub fn path(corners: &[Corner]) -> Option<Path> {
    const K: f32 = 0.552_284_8;
    let n = corners.len();
    if n < 3 {
        return None;
    }
    let length = |a: Corner, b: Corner| (b.x - a.x).abs() + (b.y - a.y).abs();
    let radius = |k: usize| {
        let (prev, cur, nxt) = (corners[(k + n - 1) % n], corners[k], corners[(k + 1) % n]);
        cur.radius
            .min(length(prev, cur) / 2.0)
            .min(length(cur, nxt) / 2.0)
    };
    let toward = |from: Corner, to: Corner, d: f32| {
        let len = length(from, to);
        (
            from.x + (to.x - from.x) / len * d,
            from.y + (to.y - from.y) / len * d,
        )
    };
    let mut pb = PathBuilder::new();
    for k in 0..n {
        let (prev, cur, nxt) = (corners[(k + n - 1) % n], corners[k], corners[(k + 1) % n]);
        let r = radius(k);
        let a = toward(cur, prev, r);
        let b = toward(cur, nxt, r);
        if k == 0 {
            pb.move_to(a.0, a.1);
        } else {
            pb.line_to(a.0, a.1);
        }
        if r > 0.0 {
            pb.cubic_to(
                a.0 + (cur.x - a.0) * K,
                a.1 + (cur.y - a.1) * K,
                b.0 + (cur.x - b.0) * K,
                b.1 + (cur.y - b.1) * K,
                b.0,
                b.1,
            );
        } else {
            pb.line_to(cur.x, cur.y);
        }
    }
    pb.close();
    pb.finish()
}

/// The box around an outline.
#[must_use]
pub fn bounds(corners: &[Corner]) -> Rect {
    let min = |f: fn(&Corner) -> f32| corners.iter().map(f).fold(f32::MAX, f32::min);
    let max = |f: fn(&Corner) -> f32| corners.iter().map(f).fold(f32::MIN, f32::max);
    let (x, y) = (min(|c| c.x), min(|c| c.y));
    Rect {
        x,
        y,
        w: max(|c| c.x) - x,
        h: max(|c| c.y) - y,
    }
}

/// Where a label goes: the centre of area of the outline, which for an `L` lies on its bigger
/// arm instead of in the notch the bounding box would put it in. A rectangle's is its middle.
/// Falls back to the box's middle if that is not inside the outline.
#[must_use]
pub fn label_center(corners: &[Corner]) -> (f32, f32) {
    let b = bounds(corners);
    let middle = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let n = corners.len();
    let (mut twice_area, mut cx, mut cy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (p, q) = (corners[i], corners[(i + 1) % n]);
        let cross = p.x.mul_add(q.y, -(q.x * p.y));
        twice_area += cross;
        cx += (p.x + q.x) * cross;
        cy += (p.y + q.y) * cross;
    }
    if twice_area.abs() < EPS {
        return middle;
    }
    let at = (cx / (3.0 * twice_area), cy / (3.0 * twice_area));
    // Ray casting: an odd number of edges to the right means inside
    let inside = (0..n).fold(false, |inside, i| {
        let (p, q) = (corners[i], corners[(i + 1) % n]);
        if (p.y > at.1) != (q.y > at.1) && at.0 < (q.x - p.x) * (at.1 - p.y) / (q.y - p.y) + p.x {
            !inside
        } else {
            inside
        }
    });
    if inside { at } else { middle }
}

/// Which groups to draw merged for the lit keys: every group whose keys are all lit, the biggest
/// first (a tie goes to the earlier one, config entries ahead of built-in ones), and none that
/// shares a key with one already taken. Returns indices into `merges`.
#[must_use]
pub fn select<T: Borrow<Merge>>(merges: &[T], lit: u32) -> Vec<usize> {
    let bits = |i: usize| merges[i].borrow().bits;
    let mut order: Vec<usize> = (0..merges.len())
        .filter(|&i| bits(i) & lit == bits(i))
        .collect();
    order.sort_by_key(|&i| std::cmp::Reverse(merges[i].borrow().slots.len()));
    let mut taken = 0u32;
    let mut chosen = Vec::new();
    for i in order {
        if bits(i) & taken == 0 {
            taken |= bits(i);
            chosen.push(i);
        }
    }
    chosen
}

/// Check one group's keys against the layout and the keyboard; the slots, or why not.
fn slots_of(
    keys: &[String],
    layout: &Layout,
    keyboard: &Keyboard,
    connected: bool,
) -> Result<Vec<usize>, String> {
    let number = layout.number_slot();
    let mut slots = Vec::new();
    for name in keys {
        let slot = layout
            .slot_by_name(name)
            .ok_or_else(|| format!("merges: \"{name}\" is no steno key"))?;
        if slot >= number {
            return Err(format!(
                "merges: \"{name}\" is the number key or an extra key, which never merges"
            ));
        }
        if slots.contains(&slot) {
            return Err(format!("merges: \"{name}\" is listed twice in one group"));
        }
        slots.push(slot);
    }
    slots.sort_unstable();
    if connected {
        if slots.len() < 2 {
            return Err(format!("merges: {keys:?} has fewer than two keys"));
        }
        let tol = keyboard.keys[1].w * NEIGHBOUR_GAP;
        let mut reached = vec![slots[0]];
        let mut grew = true;
        while grew {
            grew = false;
            for &s in &slots {
                if !reached.contains(&s)
                    && reached
                        .iter()
                        .any(|&r| adjacent(&keyboard.keys[r], &keyboard.keys[s], tol))
                {
                    reached.push(s);
                    grew = true;
                }
            }
        }
        if reached.len() != slots.len() {
            return Err(format!(
                "merges: {keys:?} are not all next to each other on the diagram"
            ));
        }
    }
    Ok(slots)
}

fn bits(slots: &[usize]) -> u32 {
    slots.iter().fold(0, |b, &s| b | 1 << s)
}

/// The groups in force, config entries first, and what is wrong with the config's entries. A
/// built-in group that does not fit this layout is dropped without a word.
#[must_use]
pub fn resolve(
    config: &OverlayConfig,
    layout: &Layout,
    keyboard: &Keyboard,
) -> (Vec<Merge>, Vec<String>) {
    let mut problems = Vec::new();
    if !config.merge_keys {
        return (Vec::new(), problems);
    }
    let mut defaults: Vec<Merge> = Vec::new();
    if config.merge_defaults {
        for (names, label) in DEFAULTS {
            let keys: Vec<String> = names.iter().map(|&n| n.to_owned()).collect();
            if let Ok(slots) = slots_of(&keys, layout, keyboard, true) {
                defaults.push(Merge {
                    bits: bits(&slots),
                    slots,
                    label: Some((*label).to_owned()),
                });
            }
        }
    }
    let mut custom: Vec<Merge> = Vec::new();
    for KeyMerge {
        keys,
        label,
        enabled,
    } in &config.merges
    {
        match slots_of(keys, layout, keyboard, *enabled) {
            Err(problem) => problems.push(problem),
            Ok(slots) => {
                let bits = bits(&slots);
                defaults.retain(|d| d.bits != bits);
                if !*enabled {
                    custom.retain(|c| c.bits != bits);
                } else if custom.iter().any(|c| c.bits == bits) {
                    problems.push(format!("merges: {keys:?} is listed twice"));
                } else {
                    custom.push(Merge {
                        slots,
                        bits,
                        label: label.clone(),
                    });
                }
            }
        }
    }
    custom.extend(defaults);
    (custom, problems)
}

/// What is wrong with the config's groups, for `keymux check`.
#[must_use]
pub fn problems(config: &OverlayConfig, layout: &Layout) -> Vec<String> {
    let keyboard = Keyboard::new(0.0, 0.0, PROBE_WIDTH, layout, &config.overlay_overrides);
    resolve(config, layout, &keyboard).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    fn square(rect: Rect) -> Member {
        Member::new(rect, 0.0, 0.0)
    }

    fn plover() -> (Layout, Keyboard) {
        let layout = Layout::plover();
        let keyboard = Keyboard::new(0.0, 0.0, 1000.0, &layout, &HashMap::new());
        (layout, keyboard)
    }

    fn merges(config: &OverlayConfig) -> Vec<Merge> {
        let (layout, keyboard) = plover();
        let (merges, problems) = resolve(config, &layout, &keyboard);
        assert!(problems.is_empty(), "{problems:?}");
        merges
    }

    fn label_of(merges: &[Merge], keys: &str) -> Option<String> {
        let bits = parse_stroke(keys).unwrap();
        merges.iter().find(|m| m.bits == bits)?.label.clone()
    }

    fn area(corners: &[Corner]) -> f32 {
        let n = corners.len();
        (0..n)
            .map(|i| {
                let (a, b) = (corners[i], corners[(i + 1) % n]);
                a.x * b.y - b.x * a.y
            })
            .sum::<f32>()
            / 2.0
    }

    #[test]
    fn a_vertical_triple_is_one_tall_rectangle_with_the_gaps_filled() {
        let members = [
            square(r(0.0, 0.0, 10.0, 20.0)),
            square(r(0.0, 22.0, 10.0, 20.0)),
            square(r(0.0, 44.0, 10.0, 20.0)),
        ];
        let corners = outline(&members, 5.0, 1.0);
        assert_eq!(corners.len(), 4, "{corners:?}");
        assert_eq!(bounds(&corners), r(0.0, 0.0, 10.0, 64.0));
        assert!((area(&corners) - 640.0).abs() < 0.01, "gaps are filled");
    }

    #[test]
    fn a_horizontal_pair_is_one_wide_rectangle_and_keeps_the_outer_radii() {
        let members = [
            Member::new(r(0.0, 0.0, 10.0, 20.0), 3.0, 0.0),
            Member::new(r(12.0, 0.0, 10.0, 20.0), 3.0, 0.0),
        ];
        let corners = outline(&members, 5.0, 1.0);
        assert_eq!(corners.len(), 4);
        assert_eq!(bounds(&corners), r(0.0, 0.0, 22.0, 20.0));
        let radii: Vec<f32> = corners.iter().map(|c| c.radius).collect();
        // Clockwise from the top left: the top corners are soft, the bottom ones square
        assert_eq!(radii, [3.0, 3.0, 0.0, 0.0]);
    }

    #[test]
    fn an_l_shape_has_six_corners_and_a_filleted_inner_one() {
        // A tall key with a short one beside its top, as S- and T-
        let members = [
            square(r(0.0, 0.0, 10.0, 42.0)),
            square(r(12.0, 0.0, 10.0, 20.0)),
        ];
        let corners = outline(&members, 5.0, 2.0);
        assert_eq!(corners.len(), 6, "{corners:?}");
        let inner: Vec<&Corner> = corners.iter().filter(|c| !c.convex).collect();
        assert_eq!(inner.len(), 1);
        assert_eq!((inner[0].x, inner[0].y, inner[0].radius), (10.0, 20.0, 2.0));
        // The members plus the strip between them
        assert!((area(&corners) - (420.0 + 200.0 + 40.0)).abs() < 0.01);
    }

    #[test]
    fn a_label_sits_in_the_middle_of_a_rectangle_and_on_the_bigger_arm_of_an_l() {
        let rect = outline(
            &[
                square(r(0.0, 0.0, 10.0, 20.0)),
                square(r(12.0, 0.0, 10.0, 20.0)),
            ],
            5.0,
            1.0,
        );
        assert_eq!(label_center(&rect), (11.0, 10.0));
        // A tall key with a three key bar at its foot, as S- over K W R
        let members = [
            square(r(0.0, 0.0, 10.0, 42.0)),
            square(r(12.0, 22.0, 10.0, 20.0)),
            square(r(24.0, 22.0, 10.0, 20.0)),
            square(r(36.0, 22.0, 10.0, 20.0)),
        ];
        let corners = outline(&members, 5.0, 1.0);
        let (x, y) = label_center(&corners);
        assert!(
            x > 10.0 && y > 22.0,
            "on the bar, not in the notch above it: {x} {y}"
        );
    }

    #[test]
    fn a_two_by_two_block_has_no_hole_at_its_middle() {
        let members = [
            square(r(0.0, 0.0, 10.0, 20.0)),
            square(r(12.0, 0.0, 10.0, 20.0)),
            square(r(0.0, 22.0, 10.0, 20.0)),
            square(r(12.0, 22.0, 10.0, 20.0)),
        ];
        let corners = outline(&members, 5.0, 1.0);
        assert_eq!(corners.len(), 4);
        assert!((area(&corners) - 22.0 * 42.0).abs() < 0.01);
    }

    #[test]
    fn two_paddles_get_a_quarter_pipe_at_each_outer_bottom_corner_and_a_flat_bottom() {
        // Paddles: a soft top and a round end, radius half the width
        let a = Member::new(r(0.0, 0.0, 20.0, 27.0), 2.0, f32::MAX);
        let b = Member::new(r(23.0, 0.0, 20.0, 27.0), 2.0, f32::MAX);
        let corners = outline(&[a, b], 8.0, 1.0);
        assert_eq!(
            corners.len(),
            4,
            "no corner in the middle of the bottom: {corners:?}"
        );
        let bottom: Vec<&Corner> = corners.iter().filter(|c| c.y == 27.0).collect();
        assert_eq!(bottom.len(), 2);
        assert!(bottom.iter().all(|c| c.radius == 10.0 && c.convex));
        assert_eq!(bounds(&corners).w, 43.0);
        let path = path(&corners).unwrap();
        let bb = path.bounds();
        assert!((bb.width() - 43.0).abs() < 0.01 && (bb.height() - 27.0).abs() < 0.01);
    }

    #[test]
    fn the_path_covers_every_member_and_the_gaps_but_not_the_round_corners() {
        let a = Member::new(r(0.0, 0.0, 20.0, 27.0), 0.0, f32::MAX);
        let b = Member::new(r(23.0, 0.0, 20.0, 27.0), 0.0, f32::MAX);
        let corners = outline(&[a, b], 8.0, 1.0);
        let mut pixmap = tiny_skia::Pixmap::new(43, 27).unwrap();
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 255, 255, 255);
        pixmap.fill_path(
            &path(&corners).unwrap(),
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        let alpha = |x: u32, y: u32| pixmap.pixel(x, y).unwrap().alpha();
        assert!(alpha(21, 13) > 250, "the gap between the paddles is filled");
        assert!(alpha(21, 25) > 250, "...right down to the flat bottom");
        assert_eq!(alpha(0, 26), 0, "the outer bottom corner is rounded away");
        assert_eq!(alpha(42, 26), 0);
        assert!(alpha(10, 13) > 250);
    }

    #[test]
    fn the_standard_letter_chords_that_fit_this_layout_are_all_there() {
        let merges = merges(&OverlayConfig::default());
        for (keys, label) in [
            ("TPH", "N"),
            ("PH", "M"),
            ("KWR", "Y"),
            ("TK", "D"),
            ("PW", "B"),
            ("HR", "L"),
            ("TP", "F"),
            ("TKPW", "G"),
            ("SKWR", "J"),
            ("STKPW", "Z"),
            ("-PB", "N"),
            ("-PL", "M"),
            ("-BG", "K"),
            ("-FP", "CH"),
            ("-RB", "SH"),
            ("-PBLG", "J"),
            ("-GS", "SHUN"),
        ] {
            assert_eq!(label_of(&merges, keys).as_deref(), Some(label), "{keys}");
        }
        // Their keys are not next to each other
        assert_eq!(label_of(&merges, "SR"), None);
        assert_eq!(label_of(&merges, "KR"), None);
        assert_eq!(merges.len(), DEFAULTS.len());
    }

    #[test]
    fn the_biggest_group_fully_pressed_wins() {
        let merges = merges(&OverlayConfig::default());
        let chosen = |keys: &str| -> Vec<Option<String>> {
            select(&merges, parse_stroke(keys).unwrap())
                .into_iter()
                .map(|i| merges[i].label.clone())
                .collect()
        };
        // TPH holds TP and PH inside it: N alone
        assert_eq!(chosen("TPH"), [Some("N".to_owned())]);
        assert_eq!(chosen("TP"), [Some("F".to_owned())]);
        // STKPW holds TKPW (G), TK, PW, TP: Z alone
        assert_eq!(chosen("STKPW"), [Some("Z".to_owned())]);
        assert_eq!(chosen("TKPW"), [Some("G".to_owned())]);
        // TPH with a K below T (TK is D, sharing T): no bigger group, so the biggest fully pressed one and then
        // what is left over: N (3 keys), K stays alone
        assert_eq!(chosen("TKPH"), [Some("N".to_owned())]);
        // TP (F) and PW (B) share P and are the same size: the one listed first, B
        assert_eq!(chosen("TPW"), [Some("B".to_owned())]);
        assert!(chosen("T").is_empty());
        // Two groups on different keys both show: TK and the right hand -PB
        assert_eq!(chosen("TK-PB").len(), 2);
    }

    #[test]
    fn a_tie_goes_to_the_config_ahead_of_the_built_in_and_a_built_in_can_be_replaced_or_dropped() {
        let (layout, keyboard) = plover();
        let config = OverlayConfig {
            merges: vec![
                // Same keys as a built-in: a new label
                KeyMerge {
                    keys: vec!["P-".into(), "H-".into()],
                    label: Some("m".into()),
                    enabled: true,
                },
                // Dropped
                KeyMerge {
                    keys: vec!["T-".into(), "K-".into()],
                    label: None,
                    enabled: false,
                },
                // A paddle pair, new
                KeyMerge {
                    keys: vec!["A-".into(), "O-".into()],
                    label: Some("AO".into()),
                    enabled: true,
                },
            ],
            ..OverlayConfig::default()
        };
        let (merges, problems) = resolve(&config, &layout, &keyboard);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(label_of(&merges, "PH").as_deref(), Some("m"));
        assert_eq!(label_of(&merges, "TK"), None);
        assert_eq!(label_of(&merges, "AO").as_deref(), Some("AO"));
        assert_eq!(
            merges.len(),
            DEFAULTS.len() + 1 - 1 + 1 - 1,
            "one dropped, one added"
        );
        assert_eq!(
            merges[0].label.as_deref(),
            Some("m"),
            "config entries come first"
        );
        // Equal size, overlapping: the earlier (the config's) wins
        let tie = [
            Merge {
                slots: vec![1, 3],
                bits: 0b1010,
                label: Some("a".into()),
            },
            Merge {
                slots: vec![3, 5],
                bits: 0b101000,
                label: Some("b".into()),
            },
        ];
        assert_eq!(select(&tie, 0b101010), [0]);
    }

    #[test]
    fn the_switches_turn_the_built_in_groups_or_all_merging_off() {
        let (layout, keyboard) = plover();
        let none = OverlayConfig {
            merge_defaults: false,
            ..OverlayConfig::default()
        };
        assert!(resolve(&none, &layout, &keyboard).0.is_empty());
        let only_custom = OverlayConfig {
            merge_defaults: false,
            merges: vec![KeyMerge {
                keys: vec!["-E".into(), "-U".into()],
                label: None,
                enabled: true,
            }],
            ..OverlayConfig::default()
        };
        assert_eq!(resolve(&only_custom, &layout, &keyboard).0.len(), 1);
        let off = OverlayConfig {
            merge_keys: false,
            ..only_custom
        };
        assert!(resolve(&off, &layout, &keyboard).0.is_empty());
    }

    #[test]
    fn bad_groups_are_reported_in_plain_words() {
        let (layout, _) = plover();
        let bad = |keys: &[&str]| {
            let config = OverlayConfig {
                merges: vec![KeyMerge {
                    keys: keys.iter().map(|k| (*k).to_owned()).collect(),
                    label: None,
                    enabled: true,
                }],
                ..OverlayConfig::default()
            };
            problems(&config, &layout)
        };
        assert!(bad(&["Q-", "T-"])[0].contains("no steno key"));
        assert!(bad(&["T-"])[0].contains("fewer than two"));
        assert!(bad(&["T-", "T-"])[0].contains("twice"));
        assert!(bad(&["T-", "-T"])[0].contains("next to each other"));
        assert!(bad(&["T-", "#"])[0].contains("never merges"));
        // A thumb and a finger key are not neighbours either
        assert!(bad(&["W-", "A-"])[0].contains("next to each other"));
        // The same keys as a built-in group are fine: they replace it
        assert!(bad(&["T-", "P-"]).is_empty());
    }
}
