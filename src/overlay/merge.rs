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

/// The standard steno letter and vowel chords, as key names and the letter or sound they spell.
/// Only chords every steno typist reads as one unit: single letters, `CH`, `SH`, `SHUN`, `NCH`
/// and the two thumb pairs `A O` and `E U` (labelled with their keys); no consonant clusters (`ST`, `FT`, `-PT`...) and nothing a
/// theory or dictionary decides, and no vowel across the thumb gap. Keys with a key between them
/// (`S R` = V, `K R` = C) are drawn as separate pieces joined by a thin connector (see
/// [`links`]). The order breaks ties between groups. Right-hand labels carry no hyphen.
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
    (&["K-", "W-"], "Q"),
    (&["-B", "-G", "-S"], "X"),
    (&["-F", "-R", "-P", "-B"], "NCH"),
    (&["A-", "O-"], "AO"),
    (&["-E", "-U"], "EU"),
    (&["S-", "R-"], "V"),
    (&["K-", "R-"], "C"),
];

/// How far apart, in key widths, two keys may be and still be neighbours: the gap between
/// columns is 0.16, the thumbs' drop under the finger rows is over 0.5.
pub const NEIGHBOUR_GAP: f32 = 0.3;
/// How far apart, in key widths, two pieces of a bridged group may be: the span from `S-` to
/// `R-` is 2.5, the longest on the diagram, `A-` to `-U`, 3.6.
pub const BRIDGE_REACH: f32 = 4.0;
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
    /// Pieces that are not next to each other are joined by a thin connector
    pub bridge: bool,
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

/// For a group whose keys fall into several pieces (each piece connected through neighbours), the
/// thin connectors that join them side by side, left to right: a third of a key wide, centred on
/// the height the two pieces share, running through whatever unpressed keys lie between. `None`
/// if they cannot be joined (no shared height, too far apart, or a member in the way); empty if
/// the group is one piece.
#[must_use]
pub fn links(rects: &[Rect], tol: f32) -> Option<Vec<Rect>> {
    let n = rects.len();
    let mut piece: Vec<usize> = (0..n).collect();
    loop {
        let mut changed = false;
        for i in 0..n {
            for j in 0..n {
                if piece[i] < piece[j] && adjacent(&rects[i], &rects[j], tol) {
                    piece[j] = piece[i];
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut ids: Vec<usize> = piece.clone();
    ids.sort_unstable();
    ids.dedup();
    let left = |id: usize| {
        (0..n)
            .filter(|&i| piece[i] == id)
            .map(|i| rects[i].x)
            .fold(f32::MAX, f32::min)
    };
    ids.sort_by(|&a, &b| left(a).total_cmp(&left(b)));
    let mut out: Vec<Rect> = Vec::new();
    for pair in ids.windows(2) {
        let mut best: Option<(f32, f32, Rect)> = None;
        for a in (0..n).filter(|&i| piece[i] == pair[0]) {
            for b in (0..n).filter(|&i| piece[i] == pair[1]) {
                let (ra, rb) = (rects[a], rects[b]);
                let thick = ra.w.min(rb.w) / 3.0;
                let dx = rb.x - (ra.x + ra.w);
                let (lo, hi) = (ra.y.max(rb.y), ra.bottom().min(rb.bottom()));
                if dx <= EPS || dx > BRIDGE_REACH * ra.w || hi - lo < thick {
                    continue;
                }
                let rect = Rect {
                    x: ra.x + ra.w,
                    y: (lo + hi) / 2.0 - thick / 2.0,
                    w: dx,
                    h: thick,
                };
                let overlap = hi - lo;
                if best.is_none_or(|(d, o, _)| dx < d - EPS || (dx < d + EPS && overlap > o)) {
                    best = Some((dx, overlap, rect));
                }
            }
        }
        let (_, _, rect) = best?;
        let hits = rects.iter().any(|r| {
            rect.x < r.x + r.w - EPS
                && rect.x + rect.w > r.x + EPS
                && rect.y < r.bottom() - EPS
                && rect.bottom() > r.y + EPS
        });
        if hits {
            return None;
        }
        out.push(rect);
    }
    Some(out)
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
    let plain: Vec<Rect> = rects.clone();
    rects.extend(links(&plain, tol).unwrap_or_default());
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
    if inside {
        at
    } else {
        middle
    }
}

/// Where a bridged group's label goes: on its biggest key, since the connector is too thin for a
/// letter; the first such key on a tie. A group in one piece uses [`label_center`].
#[must_use]
pub fn label_spot(members: &[Member], corners: &[Corner], tol: f32) -> (f32, f32) {
    let rects: Vec<Rect> = members.iter().map(|m| m.rect).collect();
    if links(&rects, tol).is_none_or(|l| l.is_empty()) {
        return label_center(corners);
    }
    let big = rects.iter().fold(rects[0], |best, r| {
        if r.w * r.h > best.w * best.h + EPS {
            *r
        } else {
            best
        }
    });
    (big.x + big.w / 2.0, big.y + big.h / 2.0)
}

/// Most search steps `select` takes before settling for the best cover found so far
const SELECT_BUDGET: usize = 20_000;

type Score = (
    u32,
    usize,
    std::cmp::Reverse<usize>,
    std::cmp::Reverse<Vec<usize>>,
);

struct Search<'a> {
    bits: &'a [u32],
    sizes: &'a [usize],
    cands: &'a [usize],
    steps: usize,
    best: Option<Score>,
}

fn search(s: &mut Search, from: usize, taken: u32, chosen: &mut Vec<usize>) {
    if !chosen.is_empty() {
        let key = (
            taken.count_ones(),
            chosen.iter().map(|&i| s.sizes[i]).max().unwrap_or(0),
            std::cmp::Reverse(chosen.len()),
            std::cmp::Reverse(chosen.clone()),
        );
        if s.best.as_ref().is_none_or(|b| key > *b) {
            s.best = Some(key);
        }
    }
    for k in from..s.cands.len() {
        s.steps += 1;
        if s.steps > SELECT_BUDGET {
            return;
        }
        let i = s.cands[k];
        if s.bits[i] & taken == 0 {
            chosen.push(i);
            search(s, k + 1, taken | s.bits[i], chosen);
            chosen.pop();
        }
    }
}

/// Which groups to draw merged for the lit keys: a set of groups that are fully lit and share no
/// key, chosen by, in this order, (1) the most lit keys covered, (2) the biggest single group,
/// (3) the fewest groups, (4) the earliest groups (config entries ahead of built-in ones). So
/// holding `S T K P W H R` is `Z` + `L` (7 keys, a 5-key group), not `J` + `N` (7 keys, a 4-key
/// group); a small group never takes keys a bigger cover needs; lit keys in no chosen group light
/// on their own. Deterministic; the search is capped, so a huge config only gets a good cover, not
/// the best. Returns indices into `merges`, ascending.
#[must_use]
pub fn select<T: Borrow<Merge>>(merges: &[T], lit: u32) -> Vec<usize> {
    let bits: Vec<u32> = merges.iter().map(|m| m.borrow().bits).collect();
    let sizes: Vec<usize> = merges.iter().map(|m| m.borrow().slots.len()).collect();
    let cands: Vec<usize> = (0..merges.len())
        .filter(|&i| bits[i] != 0 && bits[i] & lit == bits[i])
        .collect();
    let mut state = Search {
        bits: &bits,
        sizes: &sizes,
        cands: &cands,
        steps: 0,
        best: None,
    };
    search(&mut state, 0, 0, &mut Vec::new());
    state.best.map_or_else(Vec::new, |b| b.3 .0)
}

/// Check one group's keys against the layout and the keyboard; the slots, or why not.
fn slots_of(
    keys: &[String],
    layout: &Layout,
    keyboard: &Keyboard,
    connected: bool,
    bridge: bool,
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
        let rects: Vec<Rect> = slots.iter().map(|&s| keyboard.keys[s]).collect();
        match links(&rects, tol) {
            Some(l) if l.is_empty() || bridge => {}
            Some(_) | None if !bridge => {
                return Err(format!(
                    "merges: {keys:?} are not all next to each other on the diagram (set bridge: true to join them with a connector)"
                ));
            }
            _ => {
                return Err(format!(
                    "merges: {keys:?} cannot be joined: their pieces share no height, are over {BRIDGE_REACH} keys apart or have a member between"
                ));
            }
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
            if let Ok(slots) = slots_of(&keys, layout, keyboard, true, true) {
                defaults.push(Merge {
                    bits: bits(&slots),
                    slots,
                    label: Some((*label).to_owned()),
                    bridge: true,
                });
            }
        }
    }
    let mut custom: Vec<Merge> = Vec::new();
    for KeyMerge {
        keys,
        label,
        enabled,
        bridge,
    } in &config.merges
    {
        match slots_of(keys, layout, keyboard, *enabled, *bridge) {
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
                        bridge: *bridge,
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

    #[test]
    fn a_column_of_its_own_changes_no_group_and_is_in_none() {
        use crate::config::OverlayPlacement;
        use crate::steno::layout::KeySpec;
        let layout = Layout::plover().with_extra(KeySpec::new('+', "join")).unwrap();
        let config = OverlayConfig::default();
        let mut with_column = config.clone();
        with_column.overlay_overrides.insert(
            "+".to_string(),
            OverlayPlacement::ColumnBefore("*".to_string()),
        );
        let plain = Keyboard::new(0.0, 0.0, 1000.0, &layout, &HashMap::new());
        let widened = Keyboard::new(0.0, 0.0, 1000.0, &layout, &with_column.overlay_overrides);
        let (without, problems) = resolve(&config, &layout, &plain);
        assert!(problems.is_empty(), "{problems:?}");
        let (with, problems) = resolve(&with_column, &layout, &widened);
        assert!(problems.is_empty(), "{problems:?}");
        // The same groups, none holding the star (nor the extra key)
        assert_eq!(without, with);
        assert!(with.iter().all(|m| !m.slots.contains(&9)));
        // The star is no neighbour of the left hand once a column sits between them, the
        // way a column shows no key next to it merging
        let tol = widened.keys[1].w * NEIGHBOUR_GAP;
        assert!(adjacent(&plain.keys[5], &plain.keys[9], plain.keys[1].w * NEIGHBOUR_GAP));
        assert!(!adjacent(&widened.keys[5], &widened.keys[9], tol));
        assert!(!adjacent(&widened.keys[6], &widened.keys[9], tol));
        // A group of the user's own naming the star beside the left hand is then an error
        let mut asks_for_it = with_column.clone();
        asks_for_it.merges.push(KeyMerge {
            keys: vec!["R-".into(), "*".into()],
            label: None,
            enabled: true,
            bridge: false,
        });
        let (_, problems) = resolve(&asks_for_it, &layout, &widened);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let (_, problems) = resolve(&asks_for_it, &layout, &plain);
        assert!(problems.is_empty(), "{problems:?}");
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
            ("KW", "Q"),
            ("-BGS", "X"),
            ("-FRPB", "NCH"),
            ("AO", "AO"),
            ("-EU", "EU"),
            ("SR", "V"),
            ("KR", "C"),
        ] {
            assert_eq!(label_of(&merges, keys).as_deref(), Some(label), "{keys}");
        }
        // Not reliable as one unit, or no connector can join them: never built in
        for keys in [
            "KP", "ST", "-FT", "-PT", "-LT", "SH", "TH", "AE", "OE", "OU", "AU", "AOEU",
        ] {
            assert_eq!(label_of(&merges, keys), None, "{keys}");
        }
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
        // T K P H: N (T P H) leaves K alone, 3 keys; D (T K) + M (P H) cover all 4
        assert_eq!(chosen("TKPH"), [Some("M".to_owned()), Some("D".to_owned())]);
        // TP (F) and PW (B) share P and are the same size: the one listed first, B
        assert_eq!(chosen("TPW"), [Some("B".to_owned())]);
        assert!(chosen("T").is_empty(), "expected none");
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
                    bridge: false,
                },
                // Dropped
                KeyMerge {
                    keys: vec!["T-".into(), "K-".into()],
                    label: None,
                    enabled: false,
                    bridge: false,
                },
                // A paddle pair, new
                KeyMerge {
                    keys: vec!["S-".into(), "T-".into()],
                    label: Some("ST".into()),
                    enabled: true,
                    bridge: false,
                },
            ],
            ..OverlayConfig::default()
        };
        let (merges, problems) = resolve(&config, &layout, &keyboard);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(label_of(&merges, "PH").as_deref(), Some("m"));
        assert_eq!(label_of(&merges, "TK"), None);
        assert_eq!(label_of(&merges, "ST").as_deref(), Some("ST"));
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
                bridge: false,
            },
            Merge {
                slots: vec![3, 5],
                bits: 0b101000,
                label: Some("b".into()),
                bridge: false,
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
        assert!(resolve(&none, &layout, &keyboard).0.is_empty(), "expected none");
        let only_custom = OverlayConfig {
            merge_defaults: false,
            merges: vec![KeyMerge {
                keys: vec!["-E".into(), "-U".into()],
                label: None,
                enabled: true,
                bridge: false,
            }],
            ..OverlayConfig::default()
        };
        assert_eq!(resolve(&only_custom, &layout, &keyboard).0.len(), 1);
        let off = OverlayConfig {
            merge_keys: false,
            ..only_custom
        };
        assert!(resolve(&off, &layout, &keyboard).0.is_empty(), "expected none");
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
                    bridge: false,
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
        assert!(bad(&["T-", "P-"]).is_empty(), "expected none");
    }

    fn labels(merges: &[Merge], keys: &str) -> Vec<String> {
        select(merges, parse_stroke(keys).unwrap())
            .into_iter()
            .map(|i| merges[i].label.clone().unwrap())
            .collect()
    }

    #[test]
    fn the_cover_of_s_t_k_p_w_h_r_is_z_and_l_not_j_and_n() {
        let merges = merges(&OverlayConfig::default());
        // Z (5) + L (2) and J (4) + N (3) both cover all 7 keys with 2 groups: the bigger single
        // group decides
        assert_eq!(labels(&merges, "STKPWHR"), ["L", "Z"]);
    }

    #[test]
    fn covering_more_keys_beats_a_bigger_group_and_a_smaller_group_never_steals_keys() {
        let merges = merges(&OverlayConfig::default());
        // N (T P H) alone covers 3 of T K P H; D + M cover 4
        assert_eq!(labels(&merges, "TKPH"), ["M", "D"]);
        // S T K P W: Z covers 5, TK + PW only 4
        assert_eq!(labels(&merges, "STKPW"), ["Z"]);
        // T P H plus the right hand's -P -B: two Ns, nothing inside N gets its own letter
        assert_eq!(labels(&merges, "TPH-PB"), ["N", "N"]);
        // -F -R -P -B: NCH and CH + SH (-FP, -RB) both cover 4, one group beats two
        assert_eq!(labels(&merges, "-FRPB"), ["NCH"]);
        // -P -B -L -G: J alone
        assert_eq!(labels(&merges, "-PBLG"), ["J"]);
        // -B -G -S: X (3) beats K (-BG) and SHUN (-GS), which share G
        assert_eq!(labels(&merges, "-BGS"), ["X"]);
    }

    #[test]
    fn a_partial_press_leaves_keys_unmerged_and_ties_go_to_the_earlier_group() {
        let merges = merges(&OverlayConfig::default());
        // Only part of every group: nothing merges
        assert!(labels(&merges, "S").is_empty(), "expected none");
        assert!(labels(&merges, "TW").is_empty(), "expected none");
        assert!(labels(&merges, "-FB").is_empty(), "expected none");
        // S T K P with W missing: T K is D, S and P light on their own
        assert_eq!(labels(&merges, "STKP"), ["D"]);
        // S K R: SR (V) and KR (C) share R and are the same size: the earlier in the list, V
        assert_eq!(labels(&merges, "SKR"), ["V"]);
        assert_eq!(labels(&merges, "KWR"), ["Y"]);
    }

    #[test]
    fn the_search_is_capped_and_still_answers_with_many_groups() {
        // Every pair of 20 keys is a group: the cap stops the search, the answer is still a cover
        let merges: Vec<Merge> = (0..20)
            .flat_map(|a| (a + 1..20).map(move |b| (a, b)))
            .map(|(a, b)| Merge {
                slots: vec![a, b],
                bits: 1 << a | 1 << b,
                label: None,
                bridge: false,
            })
            .collect();
        let chosen = select(&merges, (1 << 20) - 1);
        assert!(!chosen.is_empty(), "nothing chosen");
        let mut seen = 0u32;
        for i in chosen {
            assert_eq!(seen & merges[i].bits, 0, "no shared key");
            seen |= merges[i].bits;
        }
    }

    #[test]
    fn s_r_and_k_r_are_joined_by_a_connector_through_the_unpressed_keys() {
        let (layout, keyboard) = plover();
        let rects = |names: &[&str]| -> Vec<Rect> {
            names
                .iter()
                .map(|n| keyboard.keys[layout.slot_by_name(n).unwrap()])
                .collect()
        };
        let tol = keyboard.keys[1].w * NEIGHBOUR_GAP;
        let w = keyboard.keys[1].w;
        // K R: W lies between them; one connector, a third of a key thick, at the keys' mid height
        let kr = rects(&["K-", "R-"]);
        let l = links(&kr, tol).unwrap();
        assert_eq!(l.len(), 1);
        assert!((l[0].h - w / 3.0).abs() < 0.01);
        assert!((l[0].x - (kr[0].x + kr[0].w)).abs() < 0.01);
        assert!((l[0].x + l[0].w - kr[1].x).abs() < 0.01);
        assert!((l[0].y + l[0].h / 2.0 - (kr[0].y + kr[0].h / 2.0)).abs() < 0.01);
        // S R: the connector runs at R's height, out of the tall S's right edge
        let sr = rects(&["S-", "R-"]);
        let l = links(&sr, tol).unwrap();
        assert_eq!(l.len(), 1);
        assert!((l[0].y + l[0].h / 2.0 - (sr[1].y + sr[1].h / 2.0)).abs() < 0.01);
        // One piece: no connector. A thumb and a finger key share no height: cannot be joined
        assert_eq!(links(&rects(&["T-", "P-"]), tol), Some(vec![]));
        assert_eq!(links(&rects(&["W-", "A-"]), tol), None);
        // The outline of K R is one shape with the connector in it
        let members: Vec<Member> = kr.iter().map(|&r| square(r)).collect();
        let corners = outline(&members, tol, 1.0);
        assert!((bounds(&corners).w - (kr[1].x + kr[1].w - kr[0].x)).abs() < 0.01);
        // Label on a key, not on the thin connector
        let (x, _) = label_spot(&members, &corners, tol);
        assert!((x - (kr[0].x + kr[0].w / 2.0)).abs() < 0.01);
    }

    #[test]
    fn a_config_group_needs_bridge_true_to_span_a_gap() {
        let (layout, _) = plover();
        let with = |bridge: bool, keys: &[&str]| {
            let config = OverlayConfig {
                merges: vec![KeyMerge {
                    keys: keys.iter().map(|k| (*k).to_owned()).collect(),
                    label: Some("x".into()),
                    enabled: true,
                    bridge,
                }],
                ..OverlayConfig::default()
            };
            problems(&config, &layout)
        };
        assert!(with(false, &["T-", "H-"])[0].contains("bridge: true"));
        assert!(with(true, &["T-", "H-"]).is_empty(), "expected none");
        assert!(with(true, &["W-", "A-"])[0].contains("cannot be joined"));
        // Dropping a built-in bridged group works like any other
        let config = OverlayConfig {
            merges: vec![KeyMerge {
                keys: vec!["S-".into(), "R-".into()],
                label: None,
                enabled: false,
                bridge: false,
            }],
            ..OverlayConfig::default()
        };
        let (layout, keyboard) = plover();
        let (merges, problems) = resolve(&config, &layout, &keyboard);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(label_of(&merges, "SR"), None);
        assert_eq!(label_of(&merges, "KR").as_deref(), Some("C"));
    }
}
