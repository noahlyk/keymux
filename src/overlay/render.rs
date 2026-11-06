//! Draws a snapshot into pixels: the tape and the steno keyboard, on a transparent background.
//!
//! The tape runs down the screen with the keyboard at the bottom, on a fully transparent
//! background (unless `background` is set). It is a pure function of (snapshot, config, scale,
//! height), so it is testable without a compositor.
//!
//! Everything that depends only on size (key shapes, glyph masks, column positions, how many
//! tape lines fit) is built once in [`Renderer::new`]. A frame is a handful of fills and mask
//! copies.
//!
//! All text sizes are derived from the overlay's `width`, so one number scales the whole thing,
//! and `text_scale` nudges the text alone.

use super::anim;
use super::geometry::{KeyGroup, Keyboard, Rect};
use super::glyphs::{self, GlyphCache};
use crate::config::{Half, KeyColor, OverlayConfig};
use crate::steno::layout::Layout;
use crate::steno::overlay::{
    LookupMode, LookupRow, LookupView, OverlaySnapshot, TapeEntry, TAPE_CAPACITY,
};
use anyhow::Result;
use std::borrow::Cow;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform};

/// Space between the content and the left/right edge of the overlay, logical px
const PAD_X: f32 = 10.0;
/// Space between the tape and the keyboard, logical px
const SECTION_GAP: f32 = 20.0;
/// Outline of an unlit key, logical px
const OUTLINE: f32 = 1.5;
/// Radius of a pad's softened corners relative to its width: a small one, so a key reads as a
/// pad and not as a pill. The round ends (the bottom of a bottom-row key, a thumb or a tall key)
/// are not this; they are half the width. A corner that faces another key of the same column
/// is not rounded at all (see [`has_square_bottom`] and [`has_square_top`]).
const PAD_TOP_ROUND: f32 = 0.1;
/// Share of the width inside the padding given to the steno strokes; the words get the rest.
const PAPER_SHARE: f32 = 0.46;
/// How many keys a stroke can have and still be set at full size inside [`PAPER_SHARE`]. The
/// letters are sized from this, not from the 23 keys a tape grid would need: ten and a half covers nearly any
/// stroke people write in practice, so the letters read well above a grid's. A longer stroke is
/// set in the next smaller size of [`STROKE_TIERS`], and the smallest fits every key there is.
const REFERENCE_KEYS: f32 = 11.5;
/// The sizes a stroke is set in, as shares of the full size: a stroke is set in the largest one
/// it fits the paper in. The last one fits a stroke of every key (and an extra key) with no
/// overlap, so letters are never drawn on top of each other.
const STROKE_TIERS: [f32; 4] = [1.0, 0.8, 0.62, 0.42];
/// Space after each steno letter's own width, as a share of the letter: the letter spacing of
/// the strokes, and nothing else (the words are set normally). Small so strokes read as tight as
/// text.
const SLOT_GAP: f32 = 0.04;
/// The words are what you read, so they are at least as big as the strokes
const WORDS_RATIO: f32 = 1.04;
/// Line pitch relative to the words' own height: tight, but the pair underline under a stroke
/// still has room above the next line
const LINE_SPACING: f32 = 0.86;
/// A key's label is this share of the key's width
const LABEL_RATIO: f32 = 0.72;
/// Text size the stroke letters are measured at to size them from the paper; they scale linearly
const PROBE_PX: f32 = 32.0;
/// How far a word's outline may be squeezed (after the smallest of [`STROKE_TIERS`]) and still be kept on one
/// line: past this its strokes wrap onto further lines instead (see `wrap_strokes`)
const MIN_SQUEEZE: f32 = 0.75;
/// Space on each side of the thin rule that divides the strokes of one outline, relative to the
/// stroke letters' size
const RULE_SPACE: f32 = 0.3;
/// Thickness of that rule, relative to the stroke letters' size (never under one pixel)
const RULE_THICKNESS: f32 = 0.07;
/// Space between the outlines of two words of a phrase on one line, relative to the stroke
/// letters' size: wider than a rule's, so words read as separate and strokes as one word's
const GROUP_SPACE: f32 = 1.1;

/// Colours. With no background, everything has to read over any wallpaper: text is bright with
/// a dark halo, and unlit keys are a faint dark glass pad with a light outline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    pub panel: [u8; 3],
    pub key: [u8; 4],
    pub outline: [u8; 4],
    pub label: [u8; 4],
    pub label_lit: [u8; 4],
    /// Lit-key colours by part of the keyboard: left hand amber, thumb vowels blue, right hand
    /// purple, and `*`, `#` and the join key green (Lapwing Lab's palette)
    pub left: [u8; 4],
    pub vowel: [u8; 4],
    pub right: [u8; 4],
    pub misc: [u8; 4],
    /// The steno letters on the tape: secondary to the words
    pub strokes: [u8; 4],
    /// What each stroke typed: the main thing to read
    pub words: [u8; 4],
    pub unmatched: [u8; 4],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            panel: [10, 12, 16],
            key: [16, 20, 30, 46],
            outline: [140, 154, 190, 105],
            label: [226, 232, 242, 245],
            label_lit: [8, 12, 16, 255],
            left: [232, 162, 74, 255],
            vowel: [91, 155, 213, 255],
            right: [160, 122, 216, 255],
            misc: [111, 174, 126, 255],
            strokes: [192, 201, 216, 255],
            words: [248, 250, 253, 255],
            unmatched: [255, 120, 120, 255],
        }
    }
}

impl Theme {
    /// The tape's text at `opacity` (zero to one) of its full strength.
    fn with_text_opacity(mut self, opacity: f32) -> Self {
        let opacity = opacity.clamp(0.0, 1.0);
        for color in [&mut self.strokes, &mut self.words, &mut self.unmatched] {
            color[3] = (f32::from(color[3]) * opacity).round() as u8;
        }
        self
    }

    /// The colour a lit key in `group` is drawn in.
    const fn accent(&self, group: KeyGroup) -> [u8; 4] {
        match group {
            KeyGroup::LeftHand => self.left,
            KeyGroup::Vowel => self.vowel,
            KeyGroup::RightHand => self.right,
            KeyGroup::Misc => self.misc,
        }
    }
}

/// One key of a tape stroke, as drawn: its letter, its colour, and the pair it belongs to (keys one
/// finger plays together share a number and are underlined as one).
struct RunKey {
    ch: char,
    color: [u8; 4],
    pair: Option<u32>,
}

/// One stroke as drawn: its keys in order
type Run = Vec<RunKey>;
/// One word's outline: its strokes, divided by a thin rule when drawn
type Unit = Vec<Run>;

/// The group number given to an extra key drawn over a standard key that has no group of its
/// own, offset by that key's slot so each such key is a group of its own. The layout's own group
/// ids are the theory's data, so this is kept clear of anything a sane config would use.
const SPLIT_GROUP_BASE: u32 = 1_000_000;

pub struct Renderer {
    config: OverlayConfig,
    /// Which keys exist and what they're called. [`Layout::plover`] for now - threading a real
    /// per-theory layout in from the daemon is separate follow-on work.
    layout: Layout,
    theme: Theme,
    scale: f32,
    /// The steno letters' pixel size
    stroke_px: f32,
    /// Buffer size in physical pixels
    size: (u32, u32),
    /// Only when `background` is above zero
    panel: Option<Path>,
    keyboard: Option<Keyboard>,
    /// One pad per key, indexed by slot, built once
    key_shapes: Vec<Path>,
    /// One pad per [`super::geometry::Keyboard::extra`] badge, same order
    extra_shapes: Vec<Path>,
    /// Bottom of the tape area, line pitch and how many lines fit, physical px
    tape_bottom: f32,
    line_height: f32,
    tape_lines: usize,
    /// Which stroke of an animated lookup is lit right now (see [`anim`]); 0 for everything else
    step: usize,
    /// The paper the steno strokes are set in: its left edge, and the right edge they end at, flush
    /// against the words, which start at `words_left` (physical px)
    paper_left: f32,
    paper_right: f32,
    words_left: f32,
    /// The right edge words are clipped at
    words_right: f32,
    labels: GlyphCache,
    strokes: GlyphCache,
    /// The strokes' letters at each of [`STROKE_TIERS`], largest first
    stroke_tiers: Vec<GlyphCache>,
    words: GlyphCache,
}

/// A lookup row laid out: its outline in lines of units (empty when the outline is not steno and
/// is drawn as plain text instead), its words wrapped, and how many lines it takes
struct RowPlan {
    outline: Vec<Vec<Unit>>,
    /// The outline as plain wrapped text, only when it did not parse as steno
    plain: Vec<String>,
    text: Vec<String>,
    /// Whether the outline and the words are paired word for word (a phrase), so both have the
    /// same lines and neither is centred against the other
    paired: bool,
    lines: usize,
}

impl Renderer {
    /// The strokes a lookup walks through on the keyboard: all of its best outline's, or just the
    /// first (the `hint`) from a daemon that sent no more. Empty when there is nothing to light.
    #[must_use]
    pub fn lookup_strokes(view: &LookupView) -> Vec<u32> {
        if !view.strokes.is_empty() {
            view.strokes.clone()
        } else if view.hint != 0 {
            vec![view.hint]
        } else {
            Vec::new()
        }
    }

    /// Light stroke `step` of an animated lookup from now on (taken modulo how many there are).
    pub const fn set_step(&mut self, step: usize) {
        self.step = step;
    }

    #[must_use]
    pub const fn step(&self) -> usize {
        self.step
    }

    /// Lay the overlay out for `config` on a surface `height` logical pixels tall, at an
    /// integer output `scale`. The tape takes all the height the keyboard leaves.
    pub fn new(config: &OverlayConfig, scale: u32, height: u32) -> Result<Self> {
        Self::with_layout(config, scale, height, Layout::plover())
    }

    /// Like [`Self::new`], for a specific theory's own key layout instead of [`Layout::plover`].
    pub fn with_layout(
        config: &OverlayConfig,
        scale: u32,
        height: u32,
        mut layout: Layout,
    ) -> Result<Self> {
        // The config's group overrides apply over the layout's own; a bad name is dropped
        // (`keymux check` warns about it) rather than refusing to draw
        for (name, group) in &config.key_groups {
            if let Err(problem) = layout.set_key_group(name, *group) {
                tracing::warn!("overlay key_groups: {problem}");
            }
        }
        let scale = scale.max(1) as f32;
        let bold = glyphs::load_font(config.font.as_deref(), glyphs::BOLD)?;

        let number_slot = layout.number_slot();
        let paper_order = layout.paper_order();

        let width = (config.width as f32 * scale).ceil();
        let height = (height.max(1) as f32 * scale).ceil();
        let (pad_x, pad_y) = (PAD_X * scale, config.margin_y as f32 * scale);
        let inner = 2.0f32.mul_add(-pad_x, width);
        let text_scale = config.text_scale.clamp(0.5, 3.0);

        // The keyboard sits at the bottom. Laying it out once at the origin gives its height.
        let keys_height = Keyboard::new(0.0, 0.0, inner, &layout, &config.overlay_overrides)
            .size
            .1;
        let keyboard = config.keyboard.then(|| {
            Keyboard::new(
                pad_x,
                height - pad_y - keys_height,
                inner,
                &layout,
                &config.overlay_overrides,
            )
        });
        let key_width =
            Keyboard::new(0.0, 0.0, inner, &layout, &config.overlay_overrides).keys[1].w;
        // Every size is nudged to where the letters' flat tops and bottoms land on whole pixel
        // rows (`crisp_size`); at an arbitrary size they smear over two rows and the text looks
        // soft. Capitals (key labels, steno letters) snap on `H`, lowercase words on `x`.
        let label_px = glyphs::crisp_size(&bold, key_width * LABEL_RATIO * text_scale, 'H');
        let labels = GlyphCache::new(bold.clone(), label_px);

        // Size the steno letters so that `REFERENCE_KEYS` of them, each as wide as its own letter
        // plus `SLOT_GAP`, fill `PAPER_SHARE` of the width: a typical stroke fills the paper and
        // a longer one is set smaller (see `STROKE_TIERS`). Widths scale linearly with the
        // size, so measure the average letter once at a probe size.
        let mut probe = GlyphCache::new(bold.clone(), PROBE_PX);
        let extra_labels: Vec<char> = (0..layout.extra_keys().len())
            .map(|i| Keyboard::extra_label(&layout, i))
            .collect();
        let letters: Vec<char> = extra_labels
            .iter()
            .copied()
            .chain(
                paper_order
                    .iter()
                    .map(|&slot| Keyboard::label(&layout, slot)),
            )
            .collect();
        let probe_average: f32 = letters
            .iter()
            .map(|&ch| probe.get(ch).advance * (1.0 + SLOT_GAP))
            .sum::<f32>()
            / letters.len().max(1) as f32;
        let paper_width = PAPER_SHARE * inner;
        let stroke_px = glyphs::crisp_size(
            &bold,
            PROBE_PX * paper_width / (REFERENCE_KEYS * probe_average) * text_scale,
            'H',
        );
        let strokes = GlyphCache::new(bold.clone(), stroke_px);
        let stroke_tiers = STROKE_TIERS
            .iter()
            .map(|ratio| {
                GlyphCache::new(bold.clone(), glyphs::crisp_size(&bold, stroke_px * ratio, 'H'))
            })
            .collect();
        let words_px = glyphs::crisp_size(&bold, stroke_px * WORDS_RATIO, 'x');
        let words = GlyphCache::new(bold, words_px);

        let paper_right = pad_x + paper_width;
        let words_left = paper_right + stroke_px * 0.6;
        let words_right = width - pad_x;

        let natural_line = (words_px * LINE_SPACING).ceil();
        let tape_bottom = keyboard.as_ref().map_or(height - pad_y, |k| {
            SECTION_GAP.mul_add(-scale, k.keys[number_slot].y)
        });
        // The tape's top edge sits `pad_y` below the top of the surface, the same margin the
        // keyboard has below it, so the whole overlay is symmetric: nothing runs to the screen edge.
        let room = (tape_bottom - pad_y).max(0.0);
        let fits = (room / natural_line).floor() as usize;
        let tape_lines = if config.tape {
            fits.min(config.tape_lines.unwrap_or(usize::MAX))
                .min(TAPE_CAPACITY)
        } else {
            0
        };
        // A tape that fills the height spreads the leftover (under one line) over its lines, a
        // fraction of a pixel each, so its top edge is exactly the margin and not up to a line
        // below it. Glyphs snap to whole pixel rows when drawn, so the text stays crisp.
        let line_height = if tape_lines > 0 && tape_lines == fits {
            room / fits as f32
        } else {
            natural_line
        };

        // Steno pads: a squarish top with slightly softened corners and a fully round bottom.
        // Where two keys of a column go together (`T` over `K`) the corners that face each other
        // are square, so the pair reads as one piece with a seam: a top-row finger key has a
        // square bottom, a bottom-row one a square top. The `#` bar is a rounded rectangle with
        // the same soft corners as the pads. Radii are clamped to fit by `rounded_rect`.
        // A key split in two (`S-` with the join key on its top half) squares the corners where
        // the halves meet, like the two keys of any other column.
        let key_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            let soft = k.keys[0].w * PAD_TOP_ROUND;
            let taken = |slot: usize| {
                k.splits
                    .iter()
                    .flatten()
                    .find(|(idx, _)| *idx == slot)
                    .map(|(_, half)| *half)
            };
            k.keys
                .iter()
                .enumerate()
                .filter_map(|(slot, r)| {
                    if slot == number_slot {
                        return rounded_rect(*r, soft, soft);
                    }
                    // The extra key took one half, so this slot is the other one
                    let square_top = has_square_top(slot) || taken(slot) == Some(Half::Top);
                    let square_bottom = has_square_bottom(slot) || taken(slot) == Some(Half::Bottom);
                    rounded_rect(
                        *r,
                        if square_top { 0.0 } else { soft },
                        if square_bottom { 0.0 } else { f32::MAX },
                    )
                })
                .collect()
        });
        let extra_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            let soft = k.keys[0].w * PAD_TOP_ROUND;
            k.extra
                .iter()
                .zip(&k.splits)
                .filter_map(|(r, split)| match split {
                    // The top half of a key: soft on top, square where it meets the bottom half
                    Some((_, Half::Top)) => rounded_rect(*r, soft, 0.0),
                    // The bottom half keeps its slot's round end
                    Some((slot, Half::Bottom)) => rounded_rect(
                        *r,
                        0.0,
                        if has_square_bottom(*slot) { 0.0 } else { f32::MAX },
                    ),
                    None => rounded_rect(*r, soft, soft),
                })
                .collect()
        });
        let panel = (config.background > 0.0)
            .then(|| {
                rounded_rect(
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        w: width,
                        h: height,
                    },
                    0.0,
                    0.0,
                )
            })
            .flatten();

        Ok(Self {
            config: config.clone(),
            layout,
            theme: Theme::default().with_text_opacity(config.text_opacity),
            scale,
            stroke_px,
            size: (width as u32, height as u32),
            panel,
            keyboard,
            key_shapes,
            extra_shapes,
            tape_bottom,
            line_height,
            tape_lines,
            step: 0,
            paper_left: pad_x,
            paper_right,
            words_left,
            words_right,
            labels,
            strokes,
            stroke_tiers,
            words,
        })
    }

    /// Buffer size in physical pixels.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        self.size
    }

    /// How many tape lines fit above the keyboard.
    #[must_use]
    pub const fn tape_lines(&self) -> usize {
        self.tape_lines
    }

    /// Draw everything. `pixmap` must be [`Self::size`].
    pub fn draw(&mut self, snapshot: &OverlaySnapshot, pixmap: &mut PixmapMut) {
        pixmap.fill(Color::TRANSPARENT);
        if let Some(panel) = &self.panel {
            let [r, g, b] = self.theme.panel;
            let alpha = (self.config.background.clamp(0.0, 1.0) * 255.0) as u8;
            fill(pixmap, panel, [r, g, b, alpha]);
        }
        // An open lookup takes the place of the tape
        match &snapshot.lookup {
            Some(lookup) if self.tape_lines > 0 => self.draw_lookup(lookup, pixmap),
            None if self.tape_lines > 0 => self.draw_tape(snapshot, pixmap),
            _ => {}
        }
        if self.keyboard.is_some() {
            self.draw_keyboard(snapshot, pixmap);
        }
    }

    /// The live lookup, drawn where the tape is and bottom-up like it: the query being typed
    /// sits just above the keyboard, and the results above it, best one nearest. A short
    /// result list doesn't just hug the query line and leave the rest of the tape's height
    /// blank: it spreads out to use the whole area instead (see `row_pitch` below).
    fn draw_lookup(&mut self, view: &LookupView, pixmap: &mut PixmapMut) {
        let baseline_in_line =
            (self.line_height - self.words.line_height()) / 2.0 + self.words.ascent();
        let (bottom, line_height) = (self.tape_bottom, self.line_height);
        // The query always sits snug against the keyboard, one ordinary line up - only the
        // results above it get the extra spacing, so the query doesn't drift away from the
        // keys it's drawn in front of.
        let query_baseline = bottom - line_height + baseline_in_line;
        let paper_right = self.paper_right;
        // The query row: a label where the strokes go, the word with a caret where words go.
        // The label says what the query is actually doing instead of always the same generic
        // word: raw steno notation resolving forward, a whole phrase translating into one
        // outline, or (the default, a single word) the ordinary reverse lookup. Colours reuse
        // the keyboard's own accents, so they read as "this is what that key's colour means"
        // rather than new colours the keyboard doesn't otherwise use.
        let (label, accent) = match view.mode {
            LookupMode::Stroke => ("ENGINE RESOLVER", self.theme.right),
            LookupMode::Phrase => ("TRANSLATOR", self.theme.misc),
            LookupMode::Word => ("LOOKUP", self.theme.vowel),
        };
        draw_text_clipped(
            pixmap,
            &mut self.strokes,
            label,
            self.paper_left,
            query_baseline,
            paper_right,
            accent,
            self.scale,
        );
        let room = 4.0f32.mul_add(-self.scale, self.words_right - self.words_left);
        // Raw steno being typed is an outline like any other, so it is drawn as one while it
        // parses; a half-typed one that does not yet is plain text
        let typed = (view.mode == LookupMode::Stroke)
            .then(|| self.outline_units(view.query.trim()))
            .flatten()
            .filter(|units| units.iter().flatten().any(|run| !run.is_empty()));
        let query_end = if let Some(units) = typed {
            let line = [units.into_iter().flatten().collect::<Unit>()];
            let tier = self.fit_tier(&line, room);
            let width = self.line_width(&line, tier).min(room);
            self.draw_line(
                pixmap,
                &line,
                self.words_left + width,
                room,
                query_baseline,
                1.0,
                0,
                None,
            );
            self.words_left + width
        } else {
            let shown = tail_that_fits(&mut self.words, &view.query, room);
            draw_text_clipped(
                pixmap,
                &mut self.words,
                shown,
                self.words_left,
                query_baseline,
                self.words_right,
                self.theme.words,
                self.scale,
            );
            self.words_left + self.words.measure(shown)
        };
        let caret_x = 2.0f32.mul_add(self.scale, query_end);
        let caret_height = self.words.ascent() * 0.95;
        if let Some(caret) = rounded_rect(
            Rect {
                x: caret_x,
                y: query_baseline - caret_height,
                w: 2.0 * self.scale,
                h: self.words.ascent().mul_add(0.2, caret_height),
            },
            0.0,
            0.0,
        ) {
            fill(pixmap, &caret, accent);
        }

        // Results, best nearest the query. A message when there is nothing to list.
        let room_for = self.tape_lines.saturating_sub(1);
        if view.rows.is_empty() {
            let message = if view.query.trim().is_empty() {
                "Type a word. Esc to close."
            } else {
                "No stroke found."
            };
            draw_text_clipped(
                pixmap,
                &mut self.strokes,
                message,
                self.paper_left,
                query_baseline - line_height,
                self.words_right,
                with_alpha(self.theme.strokes, 170),
                self.scale,
            );
            return;
        }
        // How many lines each row needs - the wider of its outline and its word, since both
        // share the row - stopping once the panel's line budget is used up rather than ever
        // truncating a long outline or a long phrase with an ellipsis. A row that would only
        // partly fit is left off entirely, so the ones shown always get full text.
        let (outline_max, words_max) = (
            paper_right - self.paper_left,
            self.words_right - self.words_left,
        );
        let mut shown_rows = Vec::with_capacity(view.rows.len());
        let mut used = 0usize;
        for row in &view.rows {
            let plan = self.plan_row(row, outline_max, words_max);
            if used + plan.lines > room_for {
                break;
            }
            used += plan.lines;
            shown_rows.push((row, plan));
        }
        // A single short result is pushed up with some breathing room above the query instead
        // of sitting squeezed right against it, so it still makes use of the tape's height - but
        // only when there's just one row to place. With several results (plain word lookup's
        // usual case, one candidate per row), spreading that same leftover room across every
        // gap *between* them read as a wall of blank lines separating otherwise-related words;
        // those always stack at the plain `line_height`, one right after another, like a list.
        const MAX_GAP_RATIO: f32 = 0.6;
        let max_gap = MAX_GAP_RATIO * line_height;
        let gap_each = if shown_rows.len() == 1 {
            ((room_for as f32 - used as f32) * line_height).clamp(0.0, max_gap)
        } else {
            0.0
        };

        // The best result's outline is played stroke by stroke on the keyboard; its text shows
        // which one by keeping that stroke at full strength and dimming the rest
        let playing = Self::lookup_strokes(view).len() > 1;
        let step = self.step;
        let mut block_top = bottom - line_height; // the query's own block, already placed
        for (row, plan) in shown_rows {
            let row_height = plan.lines as f32 * line_height;
            let row_bottom = block_top - gap_each;
            let row_top = row_bottom - row_height;
            block_top = row_top;
            // The word as typed is full strength; words that merely start with it a bit less
            let strength = if row.text == view.query.trim() {
                1.0
            } else {
                0.88
            };
            let text_color = fade_color(self.theme.words, strength);
            // A phrase's two columns have the same lines; otherwise the shorter one is centred
            // within the row's height rather than pinned to its top, so it does not read as
            // orphaned above a tall gap
            let top = |col_lines: usize| {
                if plan.paired {
                    row_top + baseline_in_line
                } else {
                    ((plan.lines - col_lines) as f32 / 2.0).mul_add(line_height, row_top)
                        + baseline_in_line
                }
            };
            let lit = (playing && view.rows.first().is_some_and(|best| std::ptr::eq(row, best)))
                .then(|| step % Self::lookup_strokes(view).len());
            if plan.outline.is_empty() {
                // Not steno: drawn as plain text
                draw_lines(
                    pixmap,
                    &mut self.strokes,
                    &plan.plain,
                    self.paper_left,
                    top(plan.plain.len()),
                    line_height,
                    fade_color(self.theme.strokes, strength),
                    self.scale,
                );
            } else {
                // The strokes, coloured and underlined like the tape's, flush against the words
                let mut first = 0usize;
                let start = top(plan.outline.len());
                for (n, line) in plan.outline.iter().enumerate() {
                    self.draw_line(
                        pixmap,
                        line,
                        paper_right,
                        outline_max,
                        line_height.mul_add(n as f32, start),
                        strength,
                        first,
                        lit,
                    );
                    first += line.iter().map(Vec::len).sum::<usize>();
                }
            }
            draw_lines(
                pixmap,
                &mut self.words,
                &plan.text,
                self.words_left,
                top(plan.text.len()),
                line_height,
                text_color,
                self.scale,
            );
        }
    }

    fn draw_keyboard(&mut self, snapshot: &OverlaySnapshot, pixmap: &mut PixmapMut) {
        let Some(keyboard) = &self.keyboard else {
            return;
        };
        let outline = Stroke {
            width: OUTLINE * self.scale,
            ..Stroke::default()
        };
        // A lookup lights a stroke of its best result, dimly, like a stroke in progress: the first
        // for a single stroke, and for several the one whose turn it is (see `anim`)
        let hint = snapshot.lookup.as_ref().map_or(0, |lookup| {
            let strokes = Self::lookup_strokes(lookup);
            if strokes.is_empty() {
                0
            } else {
                strokes[self.step % strokes.len()]
            }
        });
        // A lit key glows: a soft halo of its own colour, painted for every lit key before any
        // key is drawn, so no glow ever lies over a neighbouring key
        let lit_strength = |bit: u32| {
            if snapshot.held & bit != 0 {
                Some(1.0)
            } else if (snapshot.chord | hint) & bit != 0 {
                Some(GLOW_DIM)
            } else {
                None
            }
        };
        for (slot, shape) in self.key_shapes.iter().enumerate() {
            if let Some(strength) = lit_strength(1u32 << slot) {
                let accent = self.theme.accent(Keyboard::group(slot));
                glow(pixmap, shape, accent, strength, self.scale);
            }
        }
        for (i, shape) in self.extra_shapes.iter().enumerate() {
            if let Some(strength) = lit_strength(Keyboard::extra_bit(&self.layout, i)) {
                let accent = self.theme.accent(self.extra_group(i));
                glow(pixmap, shape, accent, strength, self.scale);
            }
        }
        for (slot, shape) in self.key_shapes.iter().enumerate() {
            let bit = 1u32 << slot;
            let accent = self.theme.accent(Keyboard::group(slot));
            // Held keys full strength; keys already let go but still in the stroke dimmer,
            // so you can watch the stroke build up
            let label_color = if snapshot.held & bit != 0 {
                fill(pixmap, shape, accent);
                self.theme.label_lit
            } else if (snapshot.chord | hint) & bit != 0 {
                fill(pixmap, shape, with_alpha(accent, 150));
                self.theme.label_lit
            } else {
                fill(pixmap, shape, self.theme.key);
                stroke(pixmap, shape, self.theme.outline, &outline);
                self.theme.label
            };

            let rect = keyboard.keys[slot];
            let label = Keyboard::label(&self.layout, slot);
            let glyph_width = self.labels.get(label).advance;
            let baseline = rect.y + self.labels.ascent().mul_add(0.72, rect.h) / 2.0;
            draw_char(
                pixmap,
                &mut self.labels,
                label,
                rect.x + (rect.w - glyph_width) / 2.0,
                baseline,
                label_color,
                self.scale,
            );
        }

        // Keys beyond the standard 23 (currently just the join key): no physical slot of
        // their own, so each gets its own badge, lit the same way a key is.
        for (i, shape) in self.extra_shapes.iter().enumerate() {
            let accent = self.theme.accent(self.extra_group(i));
            let bit = Keyboard::extra_bit(&self.layout, i);
            let label_color = if snapshot.held & bit != 0 {
                fill(pixmap, shape, accent);
                self.theme.label_lit
            } else if (snapshot.chord | hint) & bit != 0 {
                fill(pixmap, shape, with_alpha(accent, 150));
                self.theme.label_lit
            } else {
                fill(pixmap, shape, self.theme.key);
                stroke(pixmap, shape, self.theme.outline, &outline);
                self.theme.label
            };

            let rect = keyboard.extra[i];
            let label = Keyboard::extra_label(&self.layout, i);
            let glyph_width = self.labels.get(label).advance;
            let baseline = rect.y + self.labels.ascent().mul_add(0.72, rect.h) / 2.0;
            draw_char(
                pixmap,
                &mut self.labels,
                label,
                rect.x + (rect.w - glyph_width) / 2.0,
                baseline,
                label_color,
                self.scale,
            );
        }
    }

    fn draw_tape(&mut self, snapshot: &OverlaySnapshot, pixmap: &mut PixmapMut) {
        let budget = self.tape_lines;
        // Centre the smaller stroke letters and the words on one baseline within the line
        let baseline_in_line =
            (self.line_height - self.words.line_height()) / 2.0 + self.words.ascent();

        // A stroke still being pressed draws as its own newest line, ahead of the finished ones,
        // so the keys appear on the tape as they go down instead of waiting for the stroke to
        // complete. It has no text yet: what it types isn't known until it's released.
        let live = (snapshot.chord != 0).then(|| TapeEntry {
            seq: 0,
            stroke: snapshot.chord,
            text: String::new(),
            unmatched: false,
            repeats: 0,
        });

        let room = self.words_right - self.words_left;
        // Newest first, bottom of the tape up: how many wrapped lines each entry's own text
        // needs, stopping once the panel's line budget is used up rather than ever truncating
        // a long entry's text with an ellipsis. An entry that would only partly fit is left off
        // entirely, so the ones that are shown always get their full text.
        let mut planned = Vec::new();
        let mut used = 0usize;
        for entry in live.iter().chain(snapshot.tape.iter().rev()) {
            let text = Self::tape_entry_text(entry);
            let text_lines = wrap_text(&mut self.words, &text, room).len().max(1);
            if used + text_lines > budget {
                break;
            }
            planned.push((entry, text_lines));
            used += text_lines;
        }

        let mut row_top = (used as f32).mul_add(-self.line_height, self.tape_bottom);
        // `planned` is newest first; reversed, this walks oldest (top of the drawn block) to
        // newest (bottom). The fade is by distance up from the newest line, over the tape's
        // whole length (`budget` lines), so it is one smooth ramp: a short tape sits at the
        // bottom and barely fades at all, a full one reaches the floor only at its very top.
        // An entry that wraps takes the strength of its bottom line.
        let mut lines_above = 0usize;
        for (entry, text_lines) in planned.iter().rev() {
            let from_bottom = used - lines_above - text_lines;
            let fade = line_fade(
                from_bottom,
                budget,
                self.config.tape_fade,
                self.config.tape_fade_lines,
            );
            self.draw_tape_line(pixmap, entry, row_top + baseline_in_line, fade);
            row_top = (*text_lines as f32).mul_add(self.line_height, row_top);
            lines_above += text_lines;
        }
    }

    /// The text a tape entry's own line shows, before any "held down x N" prefix: "?" for a
    /// stroke that matched nothing, its translated text otherwise.
    fn tape_entry_text(entry: &TapeEntry) -> Cow<'_, str> {
        let text = if entry.unmatched {
            Cow::Borrowed("?")
        } else {
            display_text(&entry.text)
        };
        // A stroke held down repeats: one line with a count, not a line for each
        if entry.repeats > 0 {
            Cow::Owned(format!("x{} {text}", entry.repeats + 1))
        } else {
            text
        }
    }

    /// The colour group of the `index`th extra key: what `extra_colors` says, `Misc` otherwise.
    fn extra_group(&self, index: usize) -> KeyGroup {
        let name = Keyboard::extra_label(&self.layout, index).to_string();
        match self.config.extra_colors.get(&name) {
            None | Some(KeyColor::Misc) => KeyGroup::Misc,
            Some(KeyColor::Left) => KeyGroup::LeftHand,
            Some(KeyColor::Vowel) => KeyGroup::Vowel,
            Some(KeyColor::Right) => KeyGroup::RightHand,
        }
    }

    /// The keys of a stroke as a run to read, not a grid: extra keys first, then the standard
    /// keys in stroke order, each in its part of the keyboard's colour, with a `-` only where the
    /// right hand could not otherwise be told from the left. Keys one finger plays together carry
    /// the same pair number so they can be underlined as one. This is how every stroke is drawn
    /// anywhere in the overlay: the tape and the lookup alike.
    fn stroke_run(&self, stroke: u32) -> Run {
        let mut run = Vec::new();
        let splits = self.keyboard.as_ref().map(|k| &k.splits);
        for index in 0..self.layout.extra_keys().len() {
            if stroke & Keyboard::extra_bit(&self.layout, index) == 0 {
                continue;
            }
            // An extra key's own group if the config gave it one; otherwise, one sharing a slot
            // with a standard key (the join key over `S-`) is in that slot's group
            let pair = self
                .layout
                .key_group(self.layout.extra_start() + index)
                .or_else(|| {
                    splits
                        .and_then(|s| s.get(index).copied().flatten())
                        .map(|(slot, _)| {
                            self.layout
                                .key_group(slot)
                                .unwrap_or(SPLIT_GROUP_BASE + slot as u32)
                        })
                });
            run.push(RunKey {
                ch: Keyboard::extra_label(&self.layout, index),
                color: self.theme.accent(self.extra_group(index)),
                pair,
            });
        }
        let number_slot = self.layout.number_slot();
        let present: Vec<usize> = (0..number_slot)
            .filter(|slot| stroke & (1 << slot) != 0)
            .collect();
        let dash = self.tape_needs_dash(&present);
        let mut dashed = false;
        for slot in std::iter::once(number_slot).chain(0..number_slot) {
            if stroke & (1 << slot) == 0 {
                continue;
            }
            if dash && !dashed && (12..number_slot).contains(&slot) {
                run.push(RunKey {
                    ch: '-',
                    color: self.theme.strokes,
                    pair: None,
                });
                dashed = true;
            }
            run.push(RunKey {
                ch: Keyboard::label(&self.layout, slot),
                color: self.theme.accent(Keyboard::group(slot)),
                pair: self.layout.key_group(slot),
            });
        }
        run
    }

    /// An outline written as text (`KAPBD/EUD`, or a phrase's `EU / WAUPBT / TO`) as units of
    /// strokes: one unit per word, split on the spaced slashes, its strokes on the bare ones.
    /// `None` when it is not steno at all (a raw row), which is then drawn as plain text.
    fn outline_units(&self, outline: &str) -> Option<Vec<Unit>> {
        outline
            .split(" / ")
            .map(|group| {
                self.layout
                    .parse_strokes_trimmed(group)
                    .map(|strokes| strokes.iter().map(|&s| self.stroke_run(s)).collect())
            })
            .collect()
    }

    /// The glyphs strokes are drawn with at size `tier` of [`STROKE_TIERS`] (0 is full size)
    fn stroke_glyphs(&mut self, tier: usize) -> &mut GlyphCache {
        &mut self.stroke_tiers[tier]
    }

    /// The largest size of letters `line` fits `max` wide in; the smallest fits a stroke of every key
    fn fit_tier(&mut self, line: &[Unit], max: f32) -> usize {
        let last = self.last_tier();
        (0..=last)
            .find(|&t| self.line_width(line, t) <= max)
            .unwrap_or(last)
    }

    /// The smallest size of the strokes' letters
    const fn last_tier(&self) -> usize {
        self.stroke_tiers.len() - 1
    }

    /// How wide a stroke is, in physical px
    fn run_width(&mut self, run: &[RunKey], tier: usize) -> f32 {
        let glyphs = self.stroke_glyphs(tier);
        run.iter()
            .map(|key| glyphs.get(key.ch).advance * (1.0 + SLOT_GAP))
            .sum()
    }

    /// Space on each side of the rule between two strokes of a word, and the rule's thickness
    fn rule_space(&self) -> f32 {
        (self.stroke_px * RULE_SPACE).round()
    }

    fn rule_thickness(&self) -> f32 {
        (self.stroke_px * RULE_THICKNESS).max(self.scale).round()
    }

    fn group_space(&self) -> f32 {
        (self.stroke_px * GROUP_SPACE).round()
    }

    /// How wide one word's outline is: its strokes with a rule between each pair
    fn unit_width(&mut self, unit: &[Run], tier: usize) -> f32 {
        let between = 2.0f32.mul_add(self.rule_space(), self.rule_thickness());
        let mut width = unit.len().saturating_sub(1) as f32 * between;
        for run in unit {
            width += self.run_width(run, tier);
        }
        width
    }

    /// How wide a line of outlines is: its words' outlines with a wider gap between words
    fn line_width(&mut self, line: &[Unit], tier: usize) -> f32 {
        let mut width = line.len().saturating_sub(1) as f32 * self.group_space();
        for unit in line {
            width += self.unit_width(unit, tier);
        }
        width
    }

    /// Draw one stroke with its left edge at `x` and return how far it advanced: coloured
    /// letters, and one line under each pair of keys one finger plays together.
    #[allow(clippy::too_many_arguments)]
    fn draw_run(
        &mut self,
        pixmap: &mut PixmapMut,
        run: &[RunKey],
        x: f32,
        baseline: f32,
        fade: f32,
        squeeze: f32,
        tier: usize,
    ) -> f32 {
        let gap = 1.0 + SLOT_GAP;
        let px = self.stroke_tiers[tier].pixel_size();
        let scale = self.scale;
        let thickness = (px * 0.09).max(scale).round();
        let underline_y = baseline + (px * 0.12).round();
        let mut at = x;
        let mut span_start: Option<f32> = None;
        for (i, key) in run.iter().enumerate() {
            let glyph_advance = self.stroke_glyphs(tier).get(key.ch).advance;
            let advance = glyph_advance * gap * squeeze;
            draw_char(
                pixmap,
                self.stroke_glyphs(tier),
                key.ch,
                at + glyph_advance.mul_add(-squeeze, advance) / 2.0,
                baseline,
                fade_color(key.color, fade),
                scale,
            );
            // Two neighbours with the same pair number are one finger's pair: one line under both
            let next_same = run.get(i + 1).and_then(|n| n.pair) == key.pair && key.pair.is_some();
            match (key.pair, span_start) {
                (Some(_), None) if next_same => span_start = Some(at),
                (Some(_), Some(from)) if !next_same => {
                    if let Some(rect) =
                        tiny_skia::Rect::from_xywh(from, underline_y, at + advance - from, thickness)
                    {
                        fill(pixmap, &PathBuilder::from_rect(rect), fade_color(key.color, fade));
                    }
                    span_start = None;
                }
                _ => {}
            }
            at += advance;
        }
        at - x
    }

    /// Draw a line of outlines with its right edge at `right`, fitting `max` wide: in the largest
    /// size of letters it fits in, and only then squeezed. `first` is the number of
    /// strokes of the row drawn on earlier lines and `current` the one stroke lit while an outline
    /// is playing (the rest dimmed to [`anim::DIM`]); `None` lights them all.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &mut self,
        pixmap: &mut PixmapMut,
        line: &[Unit],
        right: f32,
        max: f32,
        baseline: f32,
        fade: f32,
        first: usize,
        current: Option<usize>,
    ) {
        let tier = self.fit_tier(line, max);
        let natural = self.line_width(line, tier);
        // Whatever it takes: a stroke never runs past its paper into the words (only an outline
        // of several long strokes is squeezed at all, and `wrap_strokes` breaks those first)
        let squeeze = if natural > max && natural > 0.0 {
            max / natural
        } else {
            1.0
        };
        let (space, thickness) = (self.rule_space() * squeeze, self.rule_thickness());
        let group = self.group_space() * squeeze;
        let rule_height = (self.stroke_px * 0.7).round();
        let mut x = natural.mul_add(-squeeze, right);
        let mut index = first;
        let lit = |i: usize| current.is_none_or(|c| c == i);
        for (u, unit) in line.iter().enumerate() {
            if u > 0 {
                x += group;
            }
            for (k, run) in unit.iter().enumerate() {
                let strength = if lit(index) {
                    fade
                } else {
                    fade * anim::DIM
                };
                if k > 0 {
                    // the thin rule between two strokes of one word
                    x += space;
                    let rule_fade = if lit(index) || lit(index - 1) {
                        fade
                    } else {
                        fade * anim::DIM
                    };
                    if let Some(rect) = tiny_skia::Rect::from_xywh(
                        x,
                        baseline - rule_height,
                        thickness,
                        rule_height,
                    ) {
                        fill(
                            pixmap,
                            &PathBuilder::from_rect(rect),
                            fade_color(self.theme.strokes, rule_fade * 0.55),
                        );
                    }
                    x += thickness + space;
                }
                x += self.draw_run(pixmap, run, x, baseline, strength, squeeze, tier);
                index += 1;
            }
        }
    }

    /// Lay out one lookup row: its outline as lines of units and its words wrapped, paired word
    /// for word for a phrase. An outline that is not steno falls back to plain wrapped text.
    fn plan_row(&mut self, row: &LookupRow, outline_max: f32, words_max: f32) -> RowPlan {
        let paired = word_groups(&row.text, &row.outline);
        let Some(units) = self.outline_units(&row.outline) else {
            let plain = wrap_text(&mut self.strokes, &row.outline, outline_max);
            let text = wrap_text(&mut self.words, &row.text, words_max);
            let lines = plain.len().max(text.len()).max(1);
            return RowPlan {
                outline: Vec::new(),
                plain,
                text,
                paired: false,
                lines,
            };
        };
        if let Some((words, _)) = paired.filter(|(w, _)| w.len() == units.len()) {
            let (outline, text) = self.wrap_phrase(units, &words, outline_max, words_max);
            let lines = outline.len().max(1);
            return RowPlan {
                outline,
                plain: Vec::new(),
                text,
                paired: true,
                lines,
            };
        }
        // One word (or a raw row's strokes): its strokes wrap on their own; the words wrap on theirs
        let strokes: Vec<Run> = units.into_iter().flatten().collect();
        let outline = self.wrap_strokes(strokes, outline_max);
        let text = wrap_text(&mut self.words, &row.text, words_max);
        let lines = outline.len().max(text.len()).max(1);
        RowPlan {
            outline,
            plain: Vec::new(),
            text,
            paired: false,
            lines,
        }
    }

    /// Break one word's strokes over lines of at most `max`: a stroke is never split. A line is
    /// packed as far as the smaller letters and the least squeeze can make it fit (that is what
    /// [`Self::draw_line`] will draw it with), so `KAPBD/EUD` stays on one line and only an
    /// outline that really cannot fit breaks.
    fn wrap_strokes(&mut self, strokes: Vec<Run>, max: f32) -> Vec<Vec<Unit>> {
        let max = max / MIN_SQUEEZE;
        let between = 2.0f32.mul_add(self.rule_space(), self.rule_thickness());
        let mut lines: Vec<Vec<Unit>> = Vec::new();
        let mut line: Unit = Vec::new();
        let mut width = 0.0f32;
        for run in strokes {
            let last = self.last_tier();
            let w = self.run_width(&run, last);
            if !line.is_empty() && width + between + w > max {
                lines.push(vec![std::mem::take(&mut line)]);
                width = 0.0;
            }
            width += if line.is_empty() { w } else { between + w };
            line.push(run);
        }
        if !line.is_empty() || lines.is_empty() {
            lines.push(vec![line]);
        }
        lines
    }

    /// Break a phrase's words and their outlines over shared lines, word by word: a word only
    /// moves to a new line together with its own outline when either would overflow, so both
    /// columns always show the same words on the same line (see [`wrap_paired`], the text-only
    /// version of this).
    fn wrap_phrase(
        &mut self,
        units: Vec<Unit>,
        words: &[&str],
        outline_max: f32,
        words_max: f32,
    ) -> (Vec<Vec<Unit>>, Vec<String>) {
        let sep_words = self.words.measure(" ");
        let group = self.group_space();
        let mut outline_lines: Vec<Vec<Unit>> = Vec::new();
        let mut text_lines: Vec<String> = Vec::new();
        let mut outline_line: Vec<Unit> = Vec::new();
        let mut text_line = String::new();
        let (mut outline_width, mut text_width) = (0.0f32, 0.0f32);
        for (unit, &word) in units.into_iter().zip(words) {
            let unit_width = self.unit_width(&unit, 0);
            let word_width = self.words.measure(word);
            let next_outline = if outline_line.is_empty() {
                unit_width
            } else {
                outline_width + group + unit_width
            };
            let next_text = if text_line.is_empty() {
                word_width
            } else {
                text_width + sep_words + word_width
            };
            if !outline_line.is_empty() && (next_outline > outline_max || next_text > words_max) {
                outline_lines.push(std::mem::take(&mut outline_line));
                text_lines.push(std::mem::take(&mut text_line));
                outline_width = unit_width;
                text_width = word_width;
                outline_line.push(unit);
                text_line.push_str(word);
                continue;
            }
            if !text_line.is_empty() {
                text_line.push(' ');
            }
            text_line.push_str(word);
            outline_line.push(unit);
            outline_width = next_outline;
            text_width = next_text;
        }
        if !outline_line.is_empty() || outline_lines.is_empty() {
            outline_lines.push(outline_line);
            text_lines.push(text_line);
        }
        (outline_lines, text_lines)
    }

    /// Whether the stroke needs a `-`: no vowel or star, some right-hand key, and the bare letters
    /// in steno order would read as other keys (`-T` is a left `T`, `-PB` a left `P` then `B`;
    /// `-D` and `-F` are only ever right-hand keys).
    fn tape_needs_dash(&self, present: &[usize]) -> bool {
        let number_slot = self.layout.number_slot();
        if present.iter().any(|slot| (7..=11).contains(slot)) {
            return false;
        }
        if !present.iter().any(|slot| (12..number_slot).contains(slot)) {
            return false;
        }
        let mut next = 0;
        for &slot in present {
            let letter = Keyboard::label(&self.layout, slot);
            let Some(read) = (next..number_slot)
                .find(|&candidate| Keyboard::label(&self.layout, candidate) == letter)
            else {
                return true;
            };
            if read != slot {
                return true;
            }
            next = read + 1;
        }
        false
    }

    fn draw_tape_line(
        &mut self,
        pixmap: &mut PixmapMut,
        entry: &TapeEntry,
        baseline: f32,
        fade: f32,
    ) {
        // The stroke is a run of coloured letters, set flush against the words it spelt so the
        // eye goes straight from one to the other; a long stroke is set smaller and then
        // fitted to the paper (the same drawing the lookup uses for its outlines).
        let run = self.stroke_run(entry.stroke);
        if !run.is_empty() {
            let (right, max) = (self.paper_right, self.paper_right - self.paper_left);
            self.draw_line(pixmap, &[vec![run]], right, max, baseline, fade, 0, None);
        }

        let color = if entry.unmatched {
            self.theme.unmatched
        } else {
            self.theme.words
        };
        let text = Self::tape_entry_text(entry);
        draw_text_wrapped(
            pixmap,
            &mut self.words,
            &text,
            self.words_left,
            baseline,
            self.words_right,
            self.line_height,
            fade_color(color, fade),
            self.scale,
        );
    }
}

/// The end of `text` that fits in `max` pixels, so the caret stays visible while a long query
/// is being typed. Drops characters from the front.
fn tail_that_fits<'a>(glyphs: &mut GlyphCache, text: &'a str, max: f32) -> &'a str {
    let mut rest = text;
    loop {
        if glyphs.measure(rest) <= max {
            return rest;
        }
        let mut chars = rest.chars();
        if chars.next().is_none() {
            return rest;
        }
        rest = chars.as_str();
    }
}

/// How strong a tape line is, by how many lines it is above the newest one (`0` is the newest)
/// on a tape of `lines` lines: full strength everywhere except the last `fading` lines at the
/// top, which fall in a straight line to `floor` at the topmost. The line just below those is
/// still full strength, so the fade is only a soft end to the tape and nothing else changes.
/// `floor` is clamped to 0..=1; 1.0, or `fading` of 0, turns the fade off.
fn line_fade(from_bottom: usize, lines: usize, floor: f32, fading: usize) -> f32 {
    let fading = fading.min(lines.saturating_sub(1));
    if fading == 0 {
        return 1.0;
    }
    let floor = floor.clamp(0.0, 1.0);
    // lines above the last full-strength one; `fading` of them is the top line
    let past = (from_bottom + 1 + fading).saturating_sub(lines);
    let t = (past as f32 / fading as f32).min(1.0);
    (1.0 - floor).mul_add(-t, 1.0)
}

/// The tape file writes translations as Rust string literals (`"the"`, or `-3 "the"` when a
/// stroke took back three characters first); show them without the quotes.
fn display_text(text: &str) -> Cow<'_, str> {
    let unquote = |t: &str| {
        t.strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .map(str::to_owned)
    };
    if let Some((back, rest)) = text.split_once(' ') {
        let is_count = back
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let (true, Some(typed)) = (is_count, unquote(rest)) {
            return Cow::Owned(format!("{back} {typed}"));
        }
    }
    unquote(text).map_or(Cow::Borrowed(text), Cow::Owned)
}

fn with_alpha([r, g, b, a]: [u8; 4], alpha: u8) -> [u8; 4] {
    [r, g, b, ((u16::from(a) * u16::from(alpha)) / 255) as u8]
}

fn fade_color(color: [u8; 4], fade: f32) -> [u8; 4] {
    with_alpha(color, (fade.clamp(0.0, 1.0) * 255.0) as u8)
}

fn paint([r, g, b, a]: [u8; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    paint
}

fn fill(pixmap: &mut PixmapMut, path: &Path, color: [u8; 4]) {
    pixmap.fill_path(
        path,
        &paint(color),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// The glow of a lit key: the key's own outline stroked several times, each wider and fainter
/// than the one before, which stacks up to a soft falloff reaching about [`GLOW_REACH`] logical
/// pixels out from the key. `strength` scales it (a key already let go glows less).
fn glow(pixmap: &mut PixmapMut, path: &Path, accent: [u8; 4], strength: f32, scale: f32) {
    for (reach, alpha) in GLOW_LAYERS {
        let style = Stroke {
            width: reach * 2.0 * scale,
            line_join: tiny_skia::LineJoin::Round,
            ..Stroke::default()
        };
        let a = (alpha * strength).clamp(0.0, 1.0);
        stroke(pixmap, path, with_alpha(accent, (a * 255.0) as u8), &style);
    }
}

/// How far a lit key's glow reaches past its edge, logical px
const GLOW_REACH: f32 = 6.0;
/// The layers of the glow, outermost first: how far each reaches past the key's edge (logical
/// px) and its alpha
const GLOW_LAYERS: [(f32, f32); 5] = [
    (GLOW_REACH, 0.05),
    (GLOW_REACH * 0.75, 0.08),
    (GLOW_REACH * 0.5, 0.12),
    (GLOW_REACH * 0.3, 0.17),
    (GLOW_REACH * 0.15, 0.24),
];
/// How much of the full glow a key shows that is in the stroke but no longer held
const GLOW_DIM: f32 = 0.5;

fn stroke(pixmap: &mut PixmapMut, path: &Path, color: [u8; 4], style: &Stroke) {
    pixmap.stroke_path(path, &paint(color), style, Transform::identity(), None);
}

/// Whether a key's bottom is square instead of a round end: the top-row finger keys, `TPH` and
/// `FPLTD`, which sit on the bottom-row key of their column.
const fn has_square_bottom(slot: usize) -> bool {
    matches!(slot, 1 | 3 | 5 | 12 | 14 | 16 | 18 | 20)
}

/// Whether a key's top is square instead of softened: the bottom-row finger keys, `KWR` and
/// `RBGSZ`, which sit under the top-row key of their column.
const fn has_square_top(slot: usize) -> bool {
    matches!(slot, 2 | 4 | 6 | 13 | 15 | 17 | 19 | 21)
}

/// A rectangle whose top corners have radius `top_radius` and bottom corners `bottom_radius`
/// (cubic approximation of quarter circles). Radii are clamped so they fit: half the width at
/// most, and the two together within the height. A radius of half the width on both ends
/// makes a pill.
fn rounded_rect(r: Rect, top_radius: f32, bottom_radius: f32) -> Option<Path> {
    const K: f32 = 0.552_284_8; // control point distance for a quarter circle
    let rt = top_radius.min(r.w / 2.0).min(r.h / 2.0);
    let rb = bottom_radius.min(r.w / 2.0).min(r.h - rt);
    let (ct, cb) = (rt * (1.0 - K), rb * (1.0 - K));
    let (left, top, right, bottom) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let mut pb = PathBuilder::new();
    pb.move_to(left + rt, top);
    pb.line_to(right - rt, top);
    pb.cubic_to(right - ct, top, right, top + ct, right, top + rt);
    pb.line_to(right, bottom - rb);
    pb.cubic_to(right, bottom - cb, right - cb, bottom, right - rb, bottom);
    pb.line_to(left + rb, bottom);
    pb.cubic_to(left + cb, bottom, left, bottom - cb, left, bottom - rb);
    pb.line_to(left, top + rt);
    pb.cubic_to(left, top + ct, left + ct, top, left + rt, top);
    pb.close();
    pb.finish()
}

/// Draw `text` from `x`, stopping with an ellipsis before it would pass `right`.
///
/// Every letter lands on a whole pixel, and the pen moves by whole pixels (each advance
/// rounded), the way a hinted bitmap font is set. Letters at fractional positions put their
/// vertical stems across two pixel columns and look soft; whole-pixel positions keep them
/// sharp, at the cost of letter gaps that are off by up to half a pixel.
#[allow(clippy::too_many_arguments)]
fn draw_text_clipped(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    text: &str,
    x: f32,
    baseline: f32,
    right: f32,
    color: [u8; 4],
    scale: f32,
) {
    let mut x = x.round();
    let ellipsis = glyphs.get('…').advance.round();
    let width: f32 = text.chars().map(|c| glyphs.get(c).advance.round()).sum();
    let fits = width <= right - x;
    for c in text.chars() {
        let advance = glyphs.get(c).advance.round();
        if !fits && x + advance > right - ellipsis {
            draw_char(pixmap, glyphs, '…', x, baseline, color, scale);
            return;
        }
        draw_char(pixmap, glyphs, c, x, baseline, color, scale);
        x += advance;
    }
}

/// Split `text` into lines that each fit within `max_width`: broken at a space or a `/` (a
/// stroke outline's own separator) when one is available on the line so far, and mid-character
/// otherwise, for a single word or outline longer than `max_width` on its own
/// ("supercalifragilisticexpialidocious", a long multi-key stroke). Never truncates; the last
/// line holds whatever is left over, however wide.
fn wrap_text(glyphs: &mut GlyphCache, text: &str, max_width: f32) -> Vec<String> {
    // Pieces end right after a break opportunity, so they can be packed onto a line without
    // re-scanning for where it's safe to break.
    let mut pieces = Vec::new();
    let mut piece = String::new();
    for c in text.chars() {
        piece.push(c);
        if c == ' ' || c == '/' {
            pieces.push(std::mem::take(&mut piece));
        }
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }

    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0.0f32;
    for piece in pieces {
        let piece_width = glyphs.measure(&piece);
        if width > 0.0 && width + piece_width > max_width {
            lines.push(std::mem::take(&mut line));
            width = 0.0;
        }
        if piece_width > max_width {
            // Doesn't fit on a line by itself either: the only break left is mid-character.
            for c in piece.chars() {
                let advance = glyphs.get(c).advance;
                if width > 0.0 && width + advance > max_width {
                    lines.push(std::mem::take(&mut line));
                    width = 0.0;
                }
                line.push(c);
                width += advance;
            }
            continue;
        }
        line.push_str(&piece);
        width += piece_width;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Like [`draw_text_clipped`], but wraps instead of truncating: `text` starts at `baseline`,
/// continuing on further lines (each `pitch` further down) as needed to fit within `right`
/// rather than ever being cut short with an ellipsis. Returns how many lines it drew, so the
/// caller can budget the vertical space whatever comes after it needs.
#[allow(clippy::too_many_arguments)]
fn draw_text_wrapped(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    text: &str,
    x: f32,
    baseline: f32,
    right: f32,
    pitch: f32,
    color: [u8; 4],
    scale: f32,
) -> usize {
    let lines = wrap_text(glyphs, text, (right - x).max(0.0));
    draw_lines(pixmap, glyphs, &lines, x, baseline, pitch, color, scale);
    lines.len()
}

/// Draw already-wrapped `lines`, one below the other, `pitch` apart - the drawing half of
/// [`draw_text_wrapped`], split out so lines wrapped some other way (such as
/// [`wrap_paired`]'s, broken on the same word boundaries as a companion column rather than on
/// this column's own width) can be drawn without wrapping the text over again, which would just
/// undo that pairing.
#[allow(clippy::too_many_arguments)]
fn draw_lines(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    lines: &[String],
    x: f32,
    baseline: f32,
    pitch: f32,
    color: [u8; 4],
    scale: f32,
) {
    for (i, line) in lines.iter().enumerate() {
        let mut cx = x.round();
        let line_baseline = pitch.mul_add(i as f32, baseline);
        for c in line.chars() {
            draw_char(pixmap, glyphs, c, cx, line_baseline, color, scale);
            cx += glyphs.get(c).advance.round();
        }
    }
}

/// Recover a phrase row's per-word correspondence between its outline and its text, if there is
/// one. [`crate::steno::reverse::display`] joins one stroke-group per word with `" / "`
/// (spaced), while a multi-stroke word's own strokes join with a bare `"/"` (no spaces) - so
/// splitting on `" / "` here can never land in the middle of one word's own outline, and lines
/// back up with `text`'s words split on a single space. `None` when the two don't actually line
/// up word for word: a single-word row (nothing to sync) or a raw-steno "what does this type"
/// row, whose text is the resulting phrase, not one word per stroke group.
fn word_groups<'a>(text: &'a str, outline: &'a str) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    let words: Vec<&str> = text.split(' ').collect();
    let strokes: Vec<&str> = outline.split(" / ").collect();
    (words.len() > 1 && words.len() == strokes.len()).then_some((words, strokes))
}

/// Draw one character, with its pen at `x` (rounded to a whole pixel, so it is sharp), with a
/// dark outline on light text so it reads over any wallpaper, a white window included.
fn draw_char(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    c: char,
    x: f32,
    baseline: f32,
    color: [u8; 4],
    scale: f32,
) {
    let (glyph, pen) = glyphs.place(c, x.round());
    let (x0, y0) = (pen + glyph.left, baseline.round() as i32 + glyph.top);
    // Light text gets an outline: the glyph grown a pixel and softened, drawn once underneath. A
    // faint shadow alone vanishes over a white window; a solid ring does not, and over a dark
    // window it is invisible. Dark text, which is only ever on a lit key, gets none: the key is
    // its own background, and an outline there would only make it softer.
    let light = u32::from(color[0]) + u32::from(color[1]) + u32::from(color[2]) > 330;
    if light && !glyph.halo.is_empty() {
        let pad = glyphs::HALO_PAD as i32;
        let drop = scale.round().max(1.0) as i32;
        let shadow = [SHADOW_TINT[0], SHADOW_TINT[1], SHADOW_TINT[2], (u16::from(color[3]) * SHADOW_ALPHA / 255) as u8];
        blend_mask(
            pixmap,
            &glyph.halo,
            (
                glyph.width + 2 * glyphs::HALO_PAD,
                glyph.height + 2 * glyphs::HALO_PAD,
            ),
            (x0 - pad, y0 - pad + drop),
            shadow,
        );
    }
    blend_mask(
        pixmap,
        &glyph.mask,
        (glyph.width, glyph.height),
        (x0, y0),
        color,
    );
}

/// How dark the outline under light text is, out of 255, right next to the letter: nearly opaque,
/// so the letter keeps its edge over white. The mask already fades out from there.
const SHADOW_ALPHA: u16 = 175;
/// The shadow's colour: a very dark blue-grey, softer than pure black
const SHADOW_TINT: [u8; 3] = [4, 8, 20];

/// Composite a coverage mask `size` pixels big onto the pixmap at `(x0, y0)` (source-over,
/// premultiplied RGBA).
fn blend_mask(
    pixmap: &mut PixmapMut,
    mask: &[u8],
    (mask_width, mask_height): (u32, u32),
    (x0, y0): (i32, i32),
    [r, g, b, a]: [u8; 4],
) {
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for gy in 0..mask_height as i32 {
        let py = y0 + gy;
        if py < 0 || py >= height {
            continue;
        }
        for gx in 0..mask_width as i32 {
            let px = x0 + gx;
            if px < 0 || px >= width {
                continue;
            }
            let coverage = u32::from(mask[(gy * mask_width as i32 + gx) as usize]);
            if coverage == 0 {
                continue;
            }
            let src_a = u32::from(a) * coverage / 255;
            let i = ((py * width + px) * 4) as usize;
            let inv = 255 - src_a;
            for (channel, value) in [r, g, b].into_iter().enumerate() {
                let src = u32::from(value) * src_a / 255;
                data[i + channel] = (src + u32::from(data[i + channel]) * inv / 255) as u8;
            }
            data[i + 3] = (src_a + u32::from(data[i + 3]) * inv / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Pixmap;

    /// A layout exactly like Plover's, plus a `+` "join" extra key - the shape any theory that
    /// wants one would build, not anything render.rs itself treats specially.
    fn test_layout_with_join() -> Layout {
        use crate::steno::layout::KeySpec;
        Layout::plover()
            .with_extra(KeySpec::new('+', "join"))
            .unwrap()
    }

    fn tape_entry(seq: u64, stroke: &str, text: &str) -> TapeEntry {
        TapeEntry {
            seq,
            stroke: crate::steno::layout::parse_stroke(stroke).unwrap(),
            text: text.into(),
            unmatched: text.is_empty(),
            repeats: 0,
        }
    }

    #[test]
    fn long_text_wraps_onto_further_lines_instead_of_an_ellipsis() {
        let font = glyphs::load_font(None, glyphs::BOLD).unwrap();
        let mut glyphs = GlyphCache::new(font, 16.0);
        let width = glyphs.measure("supercalifragilistic");

        // A single unbroken word longer than the width is split mid-word, never truncated: it
        // all comes back out, across however many lines it takes, with no "…" anywhere.
        let lines = wrap_text(&mut glyphs, "supercalifragilisticexpialidocious", width);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(lines.concat(), "supercalifragilisticexpialidocious");
        assert!(lines.iter().all(|l| !l.contains('…')), "{lines:?}");

        // A phrase breaks at its spaces when a break is available there
        let lines = wrap_text(&mut glyphs, "tell me how to do this", width);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(
            lines
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "tell me how to do this"
        );

        // A stroke outline breaks at its own "/" separators the same way
        let lines = wrap_text(&mut glyphs, "KAPBD/TKEU/HRAOEUT/SRAOEUT", width);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(lines.concat(), "KAPBD/TKEU/HRAOEUT/SRAOEUT");
    }

    #[test]
    fn word_groups_pairs_up_a_phrase_but_not_a_single_word_or_raw_steno() {
        // A phrase: one stroke-group per word, lining up 1:1.
        assert_eq!(
            word_groups("I want to go", "EU / WAUPBT / TO / TKPWOE"),
            Some((
                vec!["I", "want", "to", "go"],
                vec!["EU", "WAUPBT", "TO", "TKPWOE"]
            ))
        );
        // A single word has nothing to sync against another word, even with a multi-stroke
        // outline (whose own strokes join with a bare "/", never " / ").
        assert_eq!(word_groups("candy", "KAPBD/TKEU"), None);
        // Raw steno resolving forward: the text is the resulting phrase, not one word per
        // stroke, so splitting it on spaces doesn't mean the same thing as the outline's groups.
        assert_eq!(word_groups("\u{2192} \"I want\"", "EU/WAUPBT"), None);
    }

    #[test]
    fn wrap_phrase_breaks_both_columns_on_the_same_words() {
        // A phrase long enough to wrap, where the outline column (short stroke groups) could fit
        // far more per line than the word column (real English words): independently wrapping
        // each on its own width put a different number of words on "line 1" of each column.
        // Pairing them must always agree on the split.
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let words = [
            "I", "want", "to", "go", "to", "the", "store", "and", "buy", "cookies",
        ];
        let outline = "EU / WAUPBT / TO / TKPWOE / TO / -T / STOR / APBD / PWEU / KAOBG/KEUS";
        let units = renderer.outline_units(outline).unwrap();
        assert_eq!(units.len(), words.len());

        // Wide enough for every outline on one line; only wide enough for about 4 words.
        let outline_max = renderer.line_width(&units, 0) + 1.0;
        let words_max = 3.0f32.mul_add(
            renderer.words.measure(" "),
            4.0 * renderer.words.measure("store"),
        );
        let (outline_lines, text_lines) =
            renderer.wrap_phrase(units, &words, outline_max, words_max);

        assert_eq!(
            outline_lines.len(),
            text_lines.len(),
            "paired columns must always break into the same number of lines: {text_lines:?}"
        );
        assert!(text_lines.len() > 1, "the words should have wrapped");
        for (outline_line, text_line) in outline_lines.iter().zip(&text_lines) {
            assert_eq!(
                outline_line.len(),
                text_line.split(' ').count(),
                "line {text_line:?} has a different number of words than its outline"
            );
        }
        // Every word (and every outline) still shows up exactly once, in order.
        assert_eq!(text_lines.join(" "), words.join(" "));
        assert_eq!(outline_lines.iter().map(Vec::len).sum::<usize>(), words.len());
    }

    #[test]
    fn a_long_word_on_the_tape_wraps_instead_of_showing_an_ellipsis() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();

        let words_left = renderer.words_left;
        let words_right = renderer.words_right;
        let line_height = renderer.line_height;
        let tape_bottom = renderer.tape_bottom;

        let long_word = "supercalifragilisticexpialidocious";
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            ..OverlaySnapshot::default()
        };
        snapshot.tape.push_back(tape_entry(1, "KAT", long_word));
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        // The newest (only) entry's text, were it a single clipped line, would draw somewhere
        // on the bottom-most tape row and nowhere else; wrapped, it also lights pixels on the
        // row above that one.
        let lit_in_row = |pixmap: &Pixmap, row_top: f32| {
            let y0 = (row_top).max(0.0) as u32;
            let y1 = ((row_top + line_height).ceil() as u32).min(h);
            let x0 = words_left.max(0.0) as u32;
            let x1 = (words_right.ceil() as u32).min(w);
            (y0..y1)
                .flat_map(|py| (x0..x1).map(move |px| (px, py)))
                .filter_map(|(px, py)| pixmap.pixel(px, py))
                .any(|p| p.alpha() > 0)
        };
        let newest_row_top = tape_bottom - line_height;
        let row_above_top = newest_row_top - line_height;
        assert!(
            lit_in_row(&pixmap, newest_row_top),
            "the first line of the long word should draw on the newest tape row"
        );
        assert!(
            lit_in_row(&pixmap, row_above_top),
            "a word too long for one line should continue onto the row above, not be cut off"
        );
    }

    #[test]
    fn a_frame_draws_without_wayland_and_lights_held_keys() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        let (w, h) = renderer.size();
        assert_eq!((w, h), (config.width, 900));
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            held: 1 << 1, // T-
            chord: 1 << 1,
            ..OverlaySnapshot::default()
        };
        snapshot.tape.push_back(tape_entry(1, "KAT", "\"cat\""));
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        // T- is the consonant accent: sampled at its left edge, mid-height, clear of the
        // round ends and the centred label
        let t = renderer.keyboard.as_ref().unwrap().keys[1];
        let pixel = pixmap
            .pixel((t.x + 2.0) as u32, (t.y + t.h / 2.0) as u32)
            .unwrap()
            .demultiply();
        assert_eq!([pixel.red(), pixel.green(), pixel.blue()], [232, 162, 74]);
    }

    #[test]
    fn a_key_beyond_the_standard_23_lights_up_in_its_own_badge() {
        let config = OverlayConfig::default();
        let layout = test_layout_with_join();
        let mut renderer = Renderer::with_layout(&config, 1, 900, layout.clone()).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let bit = Keyboard::extra_bit(&layout, 0);
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            held: bit,
            chord: bit,
            ..OverlaySnapshot::default()
        };
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let badge = renderer.keyboard.as_ref().unwrap().extra[0];
        let pixel = pixmap
            .pixel((badge.x + 2.0) as u32, (badge.y + badge.h / 2.0) as u32)
            .unwrap()
            .demultiply();
        assert_ne!(
            [pixel.red(), pixel.green(), pixel.blue()],
            [0, 0, 0],
            "the badge should be filled with its lit colour, not left empty"
        );

        // A standard key is untouched by the extra key's own bit
        let s = renderer.keyboard.as_ref().unwrap().keys[0];
        let unlit = pixmap
            .pixel((s.x + 2.0) as u32, (s.y + 2.0) as u32)
            .unwrap()
            .demultiply();
        assert_ne!(
            [pixel.red(), pixel.green(), pixel.blue()],
            [unlit.red(), unlit.green(), unlit.blue()],
            "lighting the extra key must not also light an unrelated standard key"
        );
    }

    #[test]
    fn there_is_no_background_unless_asked_for() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        // The bottom-right corner, below the keyboard and far from any content, is fully transparent
        assert_eq!(pixmap.pixel(w - 1, h - 1).unwrap().alpha(), 0);

        let config = OverlayConfig {
            background: 0.5,
            ..OverlayConfig::default()
        };
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let alpha = pixmap.pixel(w - 1, h - 1).unwrap().alpha();
        assert!((126..=129).contains(&alpha), "{alpha}");
    }

    #[test]
    fn the_tape_runs_the_screen_height_above_the_keyboard() {
        let config = OverlayConfig::default();
        let config = OverlayConfig {
            tape_lines: Some(99),
            ..config
        };
        let short = Renderer::new(&config, 1, 600).unwrap();
        let tall = Renderer::new(&config, 1, 1900).unwrap();
        assert!(tall.tape_lines() > short.tape_lines() * 2);
        assert!(short.tape_lines() > 5);

        // Every line, and the keyboard below it, stays inside the surface
        let top = (tall.tape_lines() as f32).mul_add(-tall.line_height, tall.tape_bottom);
        assert!(top >= 0.0, "tape starts at {top}");
        let keys = tall.keyboard.as_ref().unwrap();
        assert!(keys.keys.iter().map(Rect::bottom).fold(0.0, f32::max) <= 1900.0);
        assert!(
            keys.keys[22].y > tall.tape_bottom,
            "keyboard is below the tape"
        );

        // Left alone, the tape fills the screen: the same as asking for far more than fits
        let default = Renderer::new(&OverlayConfig::default(), 1, 1900).unwrap();
        assert_eq!(default.tape_lines(), tall.tape_lines());
        let top = (default.tape_lines() as f32).mul_add(-default.line_height, default.tape_bottom);
        assert!((top - 14.0).abs() < 0.01, "the tape starts at the margin: {top}");
        // `tape_lines` caps it; zero is allowed
        let capped = OverlayConfig {
            tape_lines: Some(5),
            ..OverlayConfig::default()
        };
        assert_eq!(Renderer::new(&capped, 1, 1900).unwrap().tape_lines(), 5);
        let off = OverlayConfig {
            tape: false,
            ..OverlayConfig::default()
        };
        assert_eq!(Renderer::new(&off, 1, 1900).unwrap().tape_lines(), 0);
    }

    #[test]
    fn the_fade_is_only_the_last_few_lines_at_the_top() {
        for lines in [6, 8, 40, 400] {
            let ramp: Vec<f32> = (0..lines).map(|row| line_fade(row, lines, 0.2, 5)).collect();
            // everything but the last five lines is exactly full strength
            assert!(ramp[..lines - 5].iter().all(|&f| f == 1.0), "{lines}: {ramp:?}");
            // the last five fall in even steps to the floor at the very top
            assert!(ramp[lines - 5..].iter().all(|&f| f < 1.0), "{lines}: {ramp:?}");
            assert!((ramp[lines - 1] - 0.2).abs() < 1e-6, "{lines}: top is the floor");
            let tail = &ramp[lines - 6..];
            assert!(tail.windows(2).all(|pair| pair[0] > pair[1]), "monotonic: {tail:?}");
            let steps: Vec<f32> = tail.windows(2).map(|p| p[0] - p[1]).collect();
            assert!(steps.iter().all(|s| (s - 0.16).abs() < 1e-5), "even steps: {steps:?}");
        }
        // the number of fading lines is the knob
        let ramp: Vec<f32> = (0..20).map(|row| line_fade(row, 20, 0.0, 3)).collect();
        assert_eq!(ramp.iter().filter(|&&f| f < 1.0).count(), 3);
        // past the top stays at the floor; a floor of 1 or no fading lines turns it off;
        // out-of-range floors are clamped; a tape too short for the fade is never faded away whole
        assert!((line_fade(99, 8, 0.25, 5) - 0.25).abs() < 1e-6);
        assert!((0..8).all(|row| line_fade(row, 8, 1.0, 5) == 1.0));
        assert!((0..8).all(|row| line_fade(row, 8, 0.0, 0) == 1.0));
        assert!((line_fade(7, 8, -3.0, 5)).abs() < 1e-6);
        assert_eq!(line_fade(7, 8, 9.0, 5), 1.0);
        assert_eq!(line_fade(0, 1, 0.0, 5), 1.0);
        assert_eq!(line_fade(0, 0, 0.0, 5), 1.0);
        assert_eq!(line_fade(0, 3, 0.0, 5), 1.0, "the newest line never fades");
    }

    #[test]
    fn the_tape_margin_above_is_the_margin_below_the_keyboard() {
        for height in [1080, 1440, 2160] {
            for scale in [1, 2] {
                let r = Renderer::new(&OverlayConfig::default(), scale, height).unwrap();
                let (_, h) = r.size();
                let keys = r.keyboard.as_ref().unwrap();
                let below = h as f32 - keys.keys.iter().map(Rect::bottom).fold(0.0, f32::max);
                let top = (r.tape_lines() as f32).mul_add(-r.line_height, r.tape_bottom);
                assert!(
                    (top - below).abs() < 0.01,
                    "{height}@{scale}: {top} above the tape, {below} below the keyboard"
                );
            }
        }
        // the margin is one setting for both ends
        for margin_y in [0, 30, 60] {
            let config = OverlayConfig { margin_y, ..OverlayConfig::default() };
            let r = Renderer::new(&config, 1, 1080).unwrap();
            let keys = r.keyboard.as_ref().unwrap();
            let below = 1080.0 - keys.keys.iter().map(Rect::bottom).fold(0.0, f32::max);
            let top = (r.tape_lines() as f32).mul_add(-r.line_height, r.tape_bottom);
            assert!((below - margin_y as f32).abs() < 0.01 && (top - below).abs() < 0.01);
        }
        // capped: the tape is not stretched and still never rises above the margin
        let capped = OverlayConfig { tape_lines: Some(5), ..OverlayConfig::default() };
        let r = Renderer::new(&capped, 1, 1080).unwrap();
        assert!((5.0f32).mul_add(-r.line_height, r.tape_bottom) > 14.0);
    }

    #[test]
    fn lines_fitting_the_screen_follow_its_height() {
        let fits = |h| Renderer::new(&OverlayConfig::default(), 1, h).unwrap().tape_lines();
        let (a, b, c) = (fits(1080), fits(1440), fits(2160));
        assert!(a > 30, "1080 shows {a} lines");
        assert!(a < b && b < c, "{a} {b} {c}");
        // 360 more pixels of height add about 360 / line pitch lines
        let line = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap().line_height;
        assert!(((b - a) as f32 - 360.0 / line).abs() <= 1.0);
        // never more than the entries the overlay is sent
        let huge = OverlayConfig { text_scale: 0.3, ..OverlayConfig::default() };
        assert!(Renderer::new(&huge, 1, 4000).unwrap().tape_lines() <= TAPE_CAPACITY);
    }

    fn row_alpha(pixmap: &Pixmap, renderer: &Renderer, h: u32, row_top: f32) -> u8 {
        let words_x = renderer.words_left as u32;
        let y0 = row_top.max(0.0) as u32;
        let y1 = ((row_top + renderer.line_height).ceil() as u32).min(h);
        (y0..y1)
            .flat_map(|y| (words_x..(words_x + 20).min(pixmap.width())).map(move |x| (x, y)))
            .filter_map(|(x, y)| pixmap.pixel(x, y))
            .map(|p| p.alpha())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn the_tape_actually_fades_toward_the_top_not_the_bottom() {
        // `line_fade` alone can't catch a caller passing it the wrong row; this draws a full
        // tape and checks which end of the screen comes out dim, and that it is gradual.
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let lines = renderer.tape_lines();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            ..OverlaySnapshot::default()
        };
        // Oldest (seq 1) first, newest (seq `lines`) last - a VecDeque's back is the newest
        for seq in 1..=lines as u64 {
            snapshot
                .tape
                .push_back(tape_entry(seq, "KAT", &format!("\"w{seq}\"")));
        }
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let first_top = (lines as f32).mul_add(-renderer.line_height, renderer.tape_bottom);
        let alphas: Vec<u8> = (0..lines)
            .map(|row| {
                let top = (row as f32).mul_add(renderer.line_height, first_top);
                row_alpha(&pixmap, &renderer, h, top)
            })
            .collect();
        // row 0 is the oldest, at the top
        assert!(alphas[0] < alphas[lines - 1], "{alphas:?}");
        assert!(alphas[0] > 20, "the oldest line is faint but still there: {alphas:?}");
        // Only the last five lines fade, getting dimmer going up; every other line is the same
        // full strength as the newest
        let full = alphas[lines - 1];
        assert!(full >= 150, "the newest is strong: {alphas:?}");
        assert!(
            alphas[5..].iter().all(|&a| a.abs_diff(full) <= 4),
            "below the fade every line is at full strength: {alphas:?}"
        );
        assert!(alphas[..5].iter().all(|&a| a < full - 8), "the last five fade: {alphas:?}");
        assert!(alphas[..5].windows(2).all(|p| p[0] < p[1]), "dimmer going up: {alphas:?}");
    }

    #[test]
    fn a_tape_nowhere_near_full_does_not_fade_its_one_line() {
        // A single line sits snug against the keyboard with the tape's height empty above it:
        // it is the newest line, so it is full strength, however tall the tape could be.
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            ..OverlaySnapshot::default()
        };
        snapshot.tape.push_back(tape_entry(1, "KAT", "\"cat\""));
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let only_row = renderer.tape_bottom - renderer.line_height;
        // Full strength is the text opacity plus the shadow under it
        assert!(
            row_alpha(&pixmap, &renderer, h, only_row) >= 150,
            "the tape's one and only line should be at full strength, not faded"
        );
    }

    #[test]
    fn steno_letters_never_overlap_their_neighbours() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        // A stroke is as wide as its letters each advance plus the small letter gap, so a
        // letter never runs into the next one, and a wide letter gets more room than a narrow one
        let kat = renderer.stroke_run(crate::steno::layout::parse_stroke("KAT").unwrap());
        let letters: Vec<char> = kat.iter().map(|key| key.ch).collect();
        let expected: f32 = letters
            .iter()
            .map(|&c| renderer.strokes.get(c).advance * (1.0 + SLOT_GAP))
            .sum();
        assert!((renderer.run_width(&kat, 0) - expected).abs() < 0.01);
        let (t, w) = (
            Keyboard::label(&renderer.layout, 1),
            Keyboard::label(&renderer.layout, 4),
        );
        assert_eq!((t, w), ('T', 'W'));
        assert!(renderer.strokes.get(w).advance > renderer.strokes.get(t).advance);
    }

    #[test]
    fn the_text_is_big_enough_to_read_and_the_words_are_the_biggest_thing() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (strokes, words, label) = (
            renderer.strokes.pixel_size(),
            renderer.words.pixel_size(),
            renderer.labels.pixel_size(),
        );
        // The default panel is 340 px wide: readable, not huge. The strokes are about the size of
        // the keys' own labels, and the words at least as big
        assert!((20.0..=30.0).contains(&strokes), "strokes are {strokes} px");
        assert!(strokes >= label, "strokes ({strokes}) are smaller than key labels ({label})");
        assert!(words >= strokes, "the words must be the biggest text");
        assert!(words <= 32.0, "words are {words} px, too big");
        // Lines are compact: a line is about as tall as the words' text size, with no gap
        // between one entry and the next beyond the underline's room
        assert!(renderer.line_height >= words * 0.8);
        assert!(renderer.line_height <= words * 1.05, "{}", renderer.line_height);
        // A typical eight-key stroke takes well over half the paper, and the words get the larger share
        let mut renderer = renderer;
        let eight = renderer.stroke_run(crate::steno::layout::parse_stroke("STKPWHRA").unwrap());
        let width = renderer.run_width(&eight, 0);
        let paper = renderer.paper_right - renderer.paper_left;
        assert!(width <= paper * 1.15, "eight keys ({width}) overflow the paper ({paper})");
        assert!(width >= paper * 0.5, "eight keys ({width}) leave the paper empty ({paper})");
        assert!(
            renderer.words_right - renderer.words_left > paper,
            "the words get more room than the strokes"
        );
    }

    #[test]
    fn a_stroke_too_long_for_the_paper_is_set_smaller_and_still_fits() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let long = renderer.stroke_run(
            crate::steno::layout::parse_stroke("STKPWHRAOEUFRPBLGTSDZ").unwrap(),
        );
        let paper = renderer.paper_right - renderer.paper_left;
        let last = renderer.last_tier();
        let full = renderer.run_width(&long, 0);
        let smallest = renderer.run_width(&long, last);
        assert!(full > paper, "this stroke should not fit at full size");
        assert!(smallest < full, "the smaller letters are narrower");
        assert!(
            smallest <= paper,
            "a stroke of every key fits the paper in the smallest letters, with no overlap: {smallest} > {paper}"
        );
        assert!(
            renderer.stroke_tiers[last].pixel_size() < renderer.strokes.pixel_size(),
            "the last tier is smaller than full size"
        );
        // The tiers step down: the first one that fits is the one used
        let again = renderer
            .stroke_run(crate::steno::layout::parse_stroke("STKPWHRAOEUFRPBLGTSDZ").unwrap());
        assert_eq!(renderer.fit_tier(&[vec![again]], paper), last);
        let short = renderer.stroke_run(crate::steno::layout::parse_stroke("KAT").unwrap());
        assert_eq!(renderer.fit_tier(&[vec![short]], paper), 0);
        // Drawn, it stays inside the paper (nothing inked left of it)
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let line = [vec![long]];
        let baseline = renderer.tape_bottom - renderer.line_height;
        let (right, max) = (renderer.paper_right, paper);
        renderer.draw_line(&mut pixmap.as_mut(), &line, right, max, baseline, 1.0, 0, None);
        let left_of_paper = (0..h)
            .flat_map(|y| (0..renderer.paper_left as u32 - 1).map(move |x| (x, y)))
            .filter_map(|(x, y)| pixmap.pixel(x, y))
            .any(|p| p.alpha() > 0);
        assert!(!left_of_paper, "a long stroke spilled off the left of the paper");
    }

    #[test]
    fn a_lookup_outline_has_the_tapes_colours_and_underlines() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let view = playing("KAPBD");
        let pixmap = drawn(&mut renderer, &view);
        let paper = (renderer.paper_left as u32, renderer.paper_right as u32);
        let above = (renderer.tape_bottom - renderer.line_height) as u32;
        let pixels: Vec<_> = (0..above)
            .flat_map(|y| (paper.0..paper.1).map(move |x| (x, y)))
            .filter_map(|(x, y)| pixmap.pixel(x, y))
            .filter(|p| p.alpha() > 220)
            .collect();
        // K is a left-hand key (amber), A a vowel (blue), P B D the right hand (purple)
        let amber = pixels.iter().any(|p| p.red() > 180 && p.green() > 110 && p.blue() < 120);
        let blue = pixels.iter().any(|p| p.blue() > 180 && p.red() < 140);
        let purple = pixels.iter().any(|p| p.blue() > 180 && p.red() > 130 && p.green() < 150);
        assert!(amber && blue && purple, "amber {amber}, blue {blue}, purple {purple}");
    }

    #[test]
    fn a_lookup_reads_an_outline_with_the_join_key_as_strokes() {
        let renderer =
            Renderer::with_layout(&OverlayConfig::default(), 1, 900, test_layout_with_join())
                .unwrap();
        // `ED/+-D`: one word of two strokes, the second led by the join key in the misc colour
        let units = renderer.outline_units("ED/+-D").unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].len(), 2);
        assert_eq!(units[0][1][0].ch, '+');
        assert_eq!(units[0][1][0].color, renderer.theme.accent(KeyGroup::Misc));
        // A phrase is one unit per word; text that is not steno is not drawn as strokes
        assert_eq!(renderer.outline_units("KAT / TKOG").unwrap().len(), 2);
        assert!(renderer.outline_units("what does this type?").is_none());
    }

    #[test]
    fn a_wrapped_outline_never_splits_a_stroke() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let units = renderer.outline_units("KAPBD/TKAEUT/STKPWHR/PHAPB").unwrap();
        let strokes: Vec<Run> = units.into_iter().flatten().collect();
        // wrapping packs a line as far as the smallest letters allow, so measure in those
        let last = renderer.last_tier();
        let one = renderer.run_width(&strokes[0], last);
        // Room for about two strokes: they wrap onto further lines, none cut, all in order
        let lines = renderer.wrap_strokes(strokes, one * 2.5);
        assert!(lines.len() > 1, "four strokes should not fit two to a line");
        assert_eq!(lines.iter().flatten().map(Vec::len).sum::<usize>(), 4);
    }

    #[test]
    fn an_extra_key_on_a_stroke_is_drawn_ahead_of_the_standard_keys() {
        let layout = test_layout_with_join();
        let mut renderer =
            Renderer::with_layout(&OverlayConfig::default(), 1, 900, layout.clone()).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let kat = crate::steno::layout::parse_stroke("KAT").unwrap();
        let entry = TapeEntry {
            seq: 1,
            stroke: Keyboard::extra_bit(&layout, 0) | kat,
            text: "\"cat\"".into(),
            unmatched: false,
            repeats: 0,
        };
        let baseline = 100.0;
        renderer.draw_tape_line(&mut pixmap.as_mut(), &entry, baseline, 1.0);

        // The stroke is a run set flush against the words: scan the paper for any ink at all. The
        // exact glyph shape of `^` isn't worth pinning down pixel-by-pixel, but something must be
        // drawn there.
        let x0 = renderer.paper_left.max(0.0) as u32;
        let x1 = (renderer.words_left.ceil() as u32).min(w);
        let y0 = (baseline - renderer.strokes.pixel_size()).max(0.0) as u32;
        let y1 = (baseline as u32 + 2).min(h);
        let lit = (y0..y1)
            .flat_map(|py| (x0..x1).map(move |px| (px, py)))
            .filter_map(|(px, py)| pixmap.pixel(px, py))
            .any(|p| p.alpha() > 0);
        assert!(
            lit,
            "the extra key's letter should be drawn on the paper"
        );
    }

    #[test]
    fn a_stroke_in_progress_draws_on_the_tape_before_it_is_released() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let t_bit = 1 << renderer.layout.paper_order()[0];

        // Geometry of the tape's newest (bottom-most) line, copied out so sampling doesn't
        // need to borrow `renderer` across the mutable `draw` calls below.
        let paper_left = renderer.paper_left;
        let words_left = renderer.words_left;
        let pixel_size = renderer.strokes.pixel_size();
        let baseline_in_line =
            (renderer.line_height - renderer.words.line_height()) / 2.0 + renderer.words.ascent();
        let baseline = (renderer.tape_bottom - renderer.line_height) + baseline_in_line;
        let sample = |pixmap: &Pixmap| {
            let x0 = paper_left.max(0.0) as u32;
            let x1 = (words_left.ceil() as u32).min(w);
            let y0 = (baseline - pixel_size).max(0.0) as u32;
            let y1 = (baseline as u32 + 2).min(h);
            (y0..y1)
                .flat_map(|py| (x0..x1).map(move |px| (px, py)))
                .filter_map(|(px, py)| pixmap.pixel(px, py))
                .any(|p| p.alpha() > 0)
        };

        // Nothing pressed, nothing on the tape yet: the newest line is blank
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        assert!(
            !sample(&pixmap),
            "nothing held: the newest line should be blank"
        );

        // T- held, the stroke not released yet, so it has no dictionary match or text: its
        // letter still appears on the newest line right away, without waiting for a release
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            held: t_bit,
            chord: t_bit,
            ..OverlaySnapshot::default()
        };
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        assert!(
            sample(&pixmap),
            "a key held mid-stroke should draw on the tape's newest line"
        );
    }

    /// The ink colours of a drawn tape line: how many opaque-ish pixels sit near each colour.
    fn tape_ink(stroke_keys: &[usize], fade: f32) -> [usize; 4] {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let entry = TapeEntry {
            seq: 1,
            stroke: stroke_keys.iter().fold(0, |bits, slot| bits | (1 << slot)),
            text: "\"x\"".into(),
            unmatched: false,
            repeats: 0,
        };
        renderer.draw_tape_line(&mut pixmap.as_mut(), &entry, 100.0, fade);
        let theme = renderer.theme;
        let near = |p: tiny_skia::PremultipliedColorU8, c: [u8; 4]| {
            let p = p.demultiply();
            p.alpha() > 200
                && p.red().abs_diff(c[0]) < 40
                && p.green().abs_diff(c[1]) < 40
                && p.blue().abs_diff(c[2]) < 40
        };
        let mut counts = [0; 4];
        for py in 0..h {
            for px in 0..w {
                let Some(p) = pixmap.pixel(px, py) else { continue };
                for (i, c) in [theme.left, theme.vowel, theme.right, theme.misc]
                    .into_iter()
                    .enumerate()
                {
                    if near(p, c) {
                        counts[i] += 1;
                    }
                }
            }
        }
        counts
    }

    #[test]
    fn tape_letters_are_coloured_by_hand() {
        // K- (left), A- (vowel), -T (right), and `*` (misc)
        let [left, vowel, right, misc] = tape_ink(&[2, 7, 18], 1.0);
        assert!(left > 10, "the left-hand letter should be amber");
        assert!(vowel > 10, "the vowel should be blue");
        assert!(right > 10, "the right-hand letter should be purple");
        // (bigger letters have more anti-aliased edge pixels that land near any colour, so the
        // green is judged against the real colours rather than against a fixed few pixels)
        assert!(
            misc * 8 < left.min(vowel).min(right),
            "no star on this stroke, so next to no green: {misc} against {left}/{vowel}/{right}"
        );
        let [_, _, _, star] = tape_ink(&[9], 1.0);
        assert!(star > 0, "the star should be green");
    }

    #[test]
    fn a_tape_dash_appears_only_where_steno_order_needs_it() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        // -T alone, -S alone, -PB: a bare letter would be read as the left hand
        assert!(renderer.tape_needs_dash(&[18]));
        assert!(renderer.tape_needs_dash(&[19]));
        assert!(renderer.tape_needs_dash(&[14, 15]));
        // -D, -F, -G are only ever the right hand; KPWR-BG's letters fall in steno order
        assert!(!renderer.tape_needs_dash(&[20]));
        assert!(!renderer.tape_needs_dash(&[12]));
        assert!(!renderer.tape_needs_dash(&[2, 3, 4, 6, 15, 17]));
        // a vowel or the star separates the hands, and no right hand needs no dash
        assert!(!renderer.tape_needs_dash(&[1, 7, 18]));
        assert!(!renderer.tape_needs_dash(&[9, 18]));
        assert!(!renderer.tape_needs_dash(&[1, 2]));
    }

    #[test]
    fn keys_one_finger_plays_are_underlined_on_the_tape() {
        // Ink below the baseline is only ever an underline (these letters have no descenders)
        let below = |keys: &[usize]| {
            let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
            let (w, h) = renderer.size();
            let mut pixmap = Pixmap::new(w, h).unwrap();
            let entry = TapeEntry {
                seq: 1,
                stroke: keys.iter().fold(0, |bits, slot| bits | (1 << slot)),
                text: "\"x\"".into(),
                unmatched: false,
                repeats: 0,
            };
            let baseline = 100.0;
            renderer.draw_tape_line(&mut pixmap.as_mut(), &entry, baseline, 1.0);
            let from = baseline as u32 + 2;
            (from..from + 6)
                .flat_map(|py| (0..renderer.words_left as u32).map(move |px| (px, py)))
                .filter_map(|(px, py)| pixmap.pixel(px, py))
                // the letters' dark shadow reaches below the baseline too; only ink is bright
                .filter(|p| u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue()) > 120)
                .count()
        };
        assert!(below(&[1, 2]) > 0, "T over K go together: underlined");
        assert!(below(&[1, 3]) == 0, "T and P are different fingers: no underline");
    }


    #[test]
    fn each_part_of_the_keyboard_lights_in_its_own_colour() {
        let theme = Theme::default();
        // One key from each part: T- (left hand), A- (vowel), -F (right hand), `*`
        let cases = [
            (1, theme.left),
            (7, theme.vowel),
            (12, theme.right),
            (9, theme.misc),
        ];
        // They are four different colours, so the check below can tell them apart
        for (i, (_, a)) in cases.iter().enumerate() {
            for (_, b) in &cases[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        for (slot, color) in cases {
            let mut pixmap = Pixmap::new(w, h).unwrap();
            let snapshot = OverlaySnapshot {
                held: 1 << slot,
                chord: 1 << slot,
                ..OverlaySnapshot::default()
            };
            renderer.draw(&snapshot, &mut pixmap.as_mut());
            // The key's left edge, mid-height: clear of the round end and the label
            let key = renderer.keyboard.as_ref().unwrap().keys[slot];
            let pixel = pixmap
                .pixel((key.x + 2.0) as u32, (key.y + key.h / 2.0) as u32)
                .unwrap()
                .demultiply();
            assert_eq!(
                [pixel.red(), pixel.green(), pixel.blue()],
                [color[0], color[1], color[2]],
                "slot {slot}"
            );
        }
    }

    #[test]
    fn the_words_are_not_letter_spaced() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        // The words advance by each glyph's own advance, with no letter gap added
        let mut words = GlyphCache::new(
            glyphs::load_font(None, glyphs::BOLD).unwrap(),
            renderer.words.pixel_size(),
        );
        let expected: f32 = "steno".chars().map(|c| words.get(c).advance).sum();
        assert_eq!(words.measure("steno"), expected);
    }

    /// Whether any pixel outside the letter itself got drawn on (a shadow), for `H` in `color`.
    fn shadow_drawn_around_h(color: [u8; 4]) -> bool {
        let mut glyphs = GlyphCache::new(glyphs::load_font(None, glyphs::BOLD).unwrap(), 16.0);
        let mut pixmap = Pixmap::new(40, 40).unwrap();
        draw_char(
            &mut pixmap.as_mut(),
            &mut glyphs,
            'H',
            10.0,
            28.0,
            color,
            1.0,
        );
        let glyph = glyphs.get('H');
        // Where the letter's own pixels are, in pixmap coordinates
        let (x0, y0) = (10 + glyph.left, 28 + glyph.top);
        let covered = |x: i32, y: i32| {
            let (gx, gy) = (x - x0, y - y0);
            gx >= 0
                && gy >= 0
                && (gx as u32) < glyph.width
                && (gy as u32) < glyph.height
                && glyph.mask[(gy as u32 * glyph.width + gx as u32) as usize] > 0
        };
        (0..40).any(|y| {
            (0..40).any(|x| pixmap.pixel(x as u32, y as u32).unwrap().alpha() > 0 && !covered(x, y))
        })
    }

    #[test]
    fn text_on_a_lit_key_has_no_shadow_and_text_on_the_tape_does() {
        // Dark text, which only ever sits on a lit key: just the letter, so it is as sharp
        // as it can be
        assert!(!shadow_drawn_around_h(Theme::default().label_lit));
        // Light text over the wallpaper gets its soft shadow
        assert!(shadow_drawn_around_h(Theme::default().words));
        assert!(shadow_drawn_around_h(Theme::default().label));
    }

    #[test]
    fn the_words_are_at_least_as_big_as_the_strokes() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        assert!(renderer.words.pixel_size() >= renderer.strokes.pixel_size());
    }

    #[test]
    fn text_scale_scales_the_text_and_keeps_the_width() {
        let tall = OverlayConfig {
            tape_lines: Some(99),
            ..OverlayConfig::default()
        };
        let normal = Renderer::new(&tall, 1, 900).unwrap();
        let big = Renderer::new(
            &OverlayConfig {
                text_scale: 1.25,
                ..tall
            },
            1,
            900,
        )
        .unwrap();
        assert!(big.strokes.pixel_size() > normal.strokes.pixel_size() * 1.2);
        assert_eq!(big.size().0, normal.size().0);
        // Bigger text means fewer lines fit
        assert!(big.tape_lines() < normal.tape_lines());
    }

    #[test]
    fn keys_are_pads_square_on_top_and_round_at_the_bottom() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        // K-, a bottom-row key: the top-row finger keys are squarer at the bottom (see below)
        let t = renderer.keyboard.as_ref().unwrap().keys[2];
        let alpha_at = |dx: f32, y: f32| pixmap.pixel((t.x + dx) as u32, y as u32).unwrap().alpha();

        // Just inside each corner of K-'s bounding box: the top is nearly square, so that
        // spot is part of the pad; the bottom is fully round, so it is empty
        let top_corner = alpha_at(2.0, t.y + 2.0);
        let bottom_corner = alpha_at(2.0, t.bottom() - 3.0);
        assert!(top_corner > 0, "top corner faces the key above, so it is square");
        assert_eq!(bottom_corner, 0, "bottom is a round end");
        assert!(alpha_at(2.0, t.y + t.h / 2.0) > 0, "body of the pad");
    }

    #[test]
    fn the_top_row_finger_keys_have_the_same_corners_at_the_bottom_as_the_top() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let keys = renderer.keyboard.as_ref().unwrap().keys;
        let alpha_at = |slot: usize, y: f32| {
            let k = keys[slot];
            pixmap.pixel((k.x + 2.0) as u32, y as u32).unwrap().alpha()
        };
        // T P H and F P L T D: the bottom corner faces the key below, so it is square
        for slot in [1, 3, 5, 12, 14, 16, 18, 20] {
            assert!(alpha_at(slot, keys[slot].bottom() - 1.0) > 0, "slot {slot}");
        }
        // The rest keep their round ends: K W R, -R -B -G -S -Z, and the tall S and *
        for slot in [0, 2, 4, 6, 9, 13, 15, 17, 19, 21] {
            assert_eq!(alpha_at(slot, keys[slot].bottom() - 3.0), 0, "slot {slot}");
        }
    }

    #[test]
    fn corners_that_face_another_key_of_the_column_are_square() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let keys = renderer.keyboard.as_ref().unwrap().keys;
        let alpha_at = |slot: usize, y: f32| {
            let k = keys[slot];
            pixmap.pixel((k.x + 1.0) as u32, y as u32).unwrap().alpha()
        };
        // K W R and -R -B -G -S -Z: the top corner is square, right up to the pad's own edge
        for slot in [2, 4, 6, 13, 15, 17, 19, 21] {
            assert!(alpha_at(slot, keys[slot].y + 1.0) > 0, "top of slot {slot}");
        }
        // The top of T, S and the star is still softened, so the corner pixel is mostly empty
        for slot in [0, 1, 9] {
            assert!(alpha_at(slot, keys[slot].y) < 128, "top of slot {slot}");
        }
    }

    #[test]
    fn a_split_key_squares_the_corners_where_its_two_halves_meet() {
        use crate::config::OverlayPlacement;
        let mut config = OverlayConfig::default();
        config.overlay_overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let mut renderer =
            Renderer::with_layout(&config, 1, 900, test_layout_with_join()).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let kb = renderer.keyboard.as_ref().unwrap();
        let (join, s) = (kb.extra[0], kb.keys[0]);
        let alpha_at = |x: f32, y: f32| pixmap.pixel(x as u32, y as u32).unwrap().alpha();
        // The join key's bottom and S-'s top face each other, so both are square there
        assert!(alpha_at(join.x + 1.0, join.bottom() - 1.0) > 0, "join bottom");
        assert!(alpha_at(s.x + 1.0, s.y + 1.0) > 0, "S- top");
        // ...while the join key's top and S-'s bottom keep their own shapes
        assert!(alpha_at(join.x + 1.0, join.y) < 128, "join top is softened");
        assert_eq!(alpha_at(s.x + 1.0, s.bottom() - 3.0), 0, "S- bottom is a round end");
    }

    #[test]
    fn the_number_bar_is_a_rounded_rectangle_not_a_pill() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let bar = renderer.keyboard.as_ref().unwrap().keys[22];
        let alpha_at = |dx: f32, y: f32| pixmap.pixel((bar.x + dx) as u32, y as u32).unwrap().alpha();
        // A pill would have no pad at the left edge a few pixels in from the top and bottom
        assert!(alpha_at(1.0, bar.y + bar.h / 2.0) > 0, "middle of the end");
        assert!(alpha_at(3.0, bar.y + 3.0) > 0, "the corner is only softened");
    }

    #[test]
    fn the_pad_path_has_the_radii_asked_for() {
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 20.0,
            h: 40.0,
        };
        // A pill: both ends are half the width
        let pill = rounded_rect(r, f32::MAX, f32::MAX).unwrap();
        assert_eq!(pill.bounds().width(), 20.0);
        // Radii that don't fit are clamped instead of producing a bad path
        assert!(rounded_rect(r, 500.0, 500.0).is_some());
        assert!(rounded_rect(r, 3.0, f32::MAX).is_some());
    }

    /// Writes a sample frame as a PPM over a backdrop, to look at the design:
    /// `OVERLAY_PREVIEW=/tmp/overlay.ppm cargo test preview -- --ignored`
    /// Writes the tape and the lookup over white, black and mid-grey to `$OVERLAY_PREVIEW_DIR`:
    /// `OVERLAY_PREVIEW_DIR=/tmp/p cargo test --release preview_backdrops -- --ignored`
    #[test]
    #[ignore = "writes images for eyeballing"]
    fn preview_backdrops() {
        use crate::steno::layout::parse_stroke;
        let dir = std::env::var("OVERLAY_PREVIEW_DIR").expect("set OVERLAY_PREVIEW_DIR");
        std::fs::create_dir_all(&dir).unwrap();
        let words = [
            ("TH", "\"this\""),
            ("S", "\"is\""),
            ("TEFT", "\"test\""),
            ("-T", "\"the\""),
            ("STEPB", "\"steno\""),
            ("STKPWHR", ""),
            ("STKPWHRAOEUFRPBLGTSDZ", "\"everything\""),
            ("PWEUG", "\"big\""),
            ("TKPWAEUPL", "\"game\""),
        ];
        // `OVERLAY_PREVIEW_HEIGHT` renders a taller surface (1080, 1440): the tape fills it
        let surface_height = std::env::var("OVERLAY_PREVIEW_HEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        for back_name in ["white", "black", "grey", "photo", "code"] {
            // `OVERLAY_PREVIEW_OPACITY` tries another `text_opacity` than the default
            let mut config = OverlayConfig::default();
            if let Some(opacity) = std::env::var("OVERLAY_PREVIEW_OPACITY")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                config.text_opacity = opacity;
            }
            let mut renderer = Renderer::new(&config, 1, surface_height).unwrap();
            let (w, h) = renderer.size();
            let mut snapshot = OverlaySnapshot {
                layer: Some("steno".into()),
                chord: parse_stroke("STKA*EUFPLT").unwrap(),
                held: parse_stroke("TKA*UPL").unwrap(),
                ..OverlaySnapshot::default()
            };
            for i in 0..renderer.tape_lines() {
                let (stroke, text) = words[i % words.len()];
                snapshot.tape.push_back(tape_entry(i as u64 + 1, stroke, text));
            }
            let write = |name: &str, pixmap: &Pixmap| {
                let mut out = format!("P6 {w} {h} 255\n").into_bytes();
                for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
                    let back = preview_backdrop(back_name, (i as u32 % w, i as u32 / w), h);
                    for (c, b) in px[..3].iter().zip(back) {
                        out.push(c + (b * (255 - u32::from(px[3])) / 255) as u8);
                    }
                }
                std::fs::write(format!("{dir}/{back_name}-{name}.ppm"), out).unwrap();
            };
            let mut pixmap = Pixmap::new(w, h).unwrap();
            renderer.draw(&snapshot, &mut pixmap.as_mut());
            write("tape", &pixmap);
            // and the lookup, which takes the tape's place
            // more results than the area holds: the best-ranked fill it
            let mut view = lookup_view();
            for i in 0..400 {
                let (outline, text) = words[i % words.len()];
                view.rows.push(crate::steno::overlay::LookupRow {
                    text: format!("cand{}", text.trim_matches('"')),
                    outline: outline.into(),
                });
            }
            snapshot.lookup = Some(view);
            let mut pixmap = Pixmap::new(w, h).unwrap();
            renderer.draw(&snapshot, &mut pixmap.as_mut());
            write("lookup", &pixmap);
        }
    }

    /// A backdrop pixel for the previews: flat colours, a noisy photo-like gradient, or a mock
    /// window of dark code over light text bands.
    #[allow(clippy::suboptimal_flops)]
    fn preview_backdrop(name: &str, (x, y): (u32, u32), height: u32) -> [u32; 3] {
        let hash = |a: u32, b: u32| {
            let mut v = a.wrapping_mul(374_761_393) ^ b.wrapping_mul(668_265_263);
            v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
            (v ^ (v >> 16)) % 256
        };
        match name {
            "white" => [255, 255, 255],
            "black" => [0, 0, 0],
            "grey" => [150, 150, 155],
            "photo" => {
                let (fx, fy) = (x as f32 / 340.0, y as f32 / height as f32);
                let base = [
                    90.0 + 140.0 * (fx * 6.0 + fy * 9.0).sin().abs(),
                    120.0 + 100.0 * (fy * 7.0 - fx * 3.0).cos(),
                    70.0 + 150.0 * fx * (1.0 - fy) + 30.0 * (fy * 20.0).sin(),
                ];
                let noise = hash(x, y) as f32 - 128.0;
                base.map(|c| (c + noise * 0.25).clamp(0.0, 255.0) as u32)
            }
            _ => {
                let light = (y / 150) % 2 == 1;
                let ink = hash(x / 7, y / 14) < 110 && y % 14 < 8;
                match (light, ink) {
                    (false, false) => [30, 34, 44],
                    (false, true) => [210 - hash(y / 14, 1) / 2, 180, 120],
                    (true, false) => [246, 246, 244],
                    (true, true) => [40, 60, 110],
                }
            }
        }
    }

    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview() {
        use crate::steno::layout::parse_stroke;
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            // Keys from every part of the keyboard, so the colours show: some held, some
            // let go of but still in the stroke
            chord: parse_stroke("#STKA*EUFPLT").unwrap(),
            held: parse_stroke("#TKA*UPL").unwrap(),
            ..OverlaySnapshot::default()
        };
        let words = [
            ("TH", "\"this\""),
            ("S", "\"is\""),
            ("A", "\"a\""),
            ("TEFT", "\"test\""),
            ("OF", "\"of\""),
            ("-T", "\"the\""),
            ("STEPB", "\"steno\""),
            ("OEFR", "\"over\""),
            ("STKPWHR", ""),
            ("STKPWHRAOEUFRPBLGTSDZ", "\"everything\""),
            ("PWEUG", "\"big\""),
            ("TKPWAEUPL", "\"game\""),
            ("KWRAOU", "\"you\""),
        ];
        for i in 0..renderer.tape_lines() {
            let (stroke, text) = words[i % words.len()];
            snapshot
                .tape
                .push_back(tape_entry(i as u64 + 1, stroke, text));
        }
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
            // Premultiplied over a wallpaper-like gradient
            let row = i as u32 / w;
            let col = i as u32 % w;
            let backdrop = [70 + row * 50 / h, 60 + col * 40 / w, 100 + row * 60 / h];
            for (c, back) in px[..3].iter().zip(backdrop) {
                out.push(c + (back * (255 - u32::from(px[3])) / 255) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    /// Writes a tape and a lookup as they look with the lyk layout (the join key on top of `S-`),
    /// at a width from `OVERLAY_WIDTH` (default 340): `OVERLAY_PREVIEW=/tmp/lyk cargo test
    /// preview_lyk -- --ignored` writes `/tmp/lyk_tape.ppm` and `/tmp/lyk_lookup.ppm`.
    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview_lyk() {
        use crate::config::OverlayPlacement;
        let base = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut config = OverlayConfig::default();
        if let Ok(width) = std::env::var("OVERLAY_WIDTH") {
            config.width = width.parse().unwrap();
        }
        config.overlay_overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let layout = test_layout_with_join();
        let mut renderer = Renderer::with_layout(&config, 1, 1080, layout.clone()).unwrap();
        let (w, h) = renderer.size();
        let stroke = |text: &str| layout.parse_stroke(text).unwrap();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            ..OverlaySnapshot::default()
        };
        let lines = [
            ("ED", "\"end\""),
            ("+-D", "\"ed\""),
            ("TKOG", "\"dog\""),
            ("+-Z", "\"s\""),
            ("WAUBG", "\"walk\""),
            ("+-G", "\"ing\""),
            ("-T", "\"the\""),
            ("TPAEUPBLG", "\"fangled\""),
            ("+STKPWHRAOEUFRPBLGTSDZ", ""),
            ("KAT", "\"cat\""),
        ];
        for (i, (strokes, text)) in lines.iter().enumerate() {
            snapshot.tape.push_back(TapeEntry {
                seq: i as u64 + 1,
                stroke: stroke(strokes),
                text: (*text).into(),
                unmatched: text.is_empty(),
                repeats: 0,
            });
        }
        let write = |name: &str, renderer: &mut Renderer, snapshot: &OverlaySnapshot| {
            let mut pixmap = Pixmap::new(w, h).unwrap();
            renderer.draw(snapshot, &mut pixmap.as_mut());
            let mut out = format!("P6 {w} {h} 255\n").into_bytes();
            for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
                let (row, col) = (i as u32 / w, i as u32 % w);
                let backdrop = [70 + row * 50 / h, 60 + col * 40 / w, 100 + row * 60 / h];
                for (c, back) in px[..3].iter().zip(backdrop) {
                    out.push(c + (back * (255 - u32::from(px[3])) / 255) as u8);
                }
            }
            std::fs::write(format!("{base}_{name}.ppm"), out).unwrap();
        };
        write("tape", &mut renderer, &snapshot);

        let row = |text: &str, outline: &str| crate::steno::overlay::LookupRow {
            text: text.into(),
            outline: outline.into(),
        };
        let best = layout.parse_strokes_trimmed("ED/+-D").unwrap();
        snapshot.lookup = Some(LookupView {
            query: "ended".into(),
            rows: vec![
                row("ended", "ED/+-D"),
                row("ending", "ED/+-G"),
                row("end", "ED"),
                row("endo", "EPBD/O"),
            ],
            hint: best[0],
            strokes: best,
            mode: LookupMode::Word,
        });
        renderer.set_step(1);
        write("lookup", &mut renderer, &snapshot);
    }

    fn lookup_view() -> LookupView {
        use crate::steno::overlay::LookupRow;
        LookupView {
            query: "cand".into(),
            rows: vec![
                LookupRow {
                    text: "candy".into(),
                    outline: "KAPBD".into(),
                },
                LookupRow {
                    text: "candle".into(),
                    outline: "KAPBL".into(),
                },
            ],
            hint: crate::steno::layout::parse_stroke("T").unwrap(),
            strokes: Vec::new(),
            mode: LookupMode::Word,
        }
    }

    /// A lookup whose best outline is `outline` (strokes parsed from it), for the animation tests
    fn playing(outline: &str) -> LookupView {
        use crate::steno::overlay::LookupRow;
        let strokes = crate::steno::layout::parse_strokes_trimmed(outline).unwrap();
        LookupView {
            query: "candy".into(),
            rows: vec![LookupRow {
                text: "candy".into(),
                outline: outline.into(),
            }],
            hint: strokes[0],
            strokes,
            mode: LookupMode::Word,
        }
    }

    fn drawn(renderer: &mut Renderer, view: &LookupView) -> Pixmap {
        let (w, h) = renderer.size();
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(view.clone()),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        pixmap
    }

    /// Whether the key for `slot` is lit (a lit key is filled; an unlit one is nearly clear)
    fn key_lit(renderer: &Renderer, pixmap: &Pixmap, slot: usize) -> bool {
        let key = renderer.keyboard.as_ref().unwrap().keys[slot];
        let pixel = pixmap
            .pixel((key.x + 2.0) as u32, (key.y + key.h / 2.0) as u32)
            .unwrap();
        pixel.alpha() > 100
    }

    #[test]
    fn a_multi_stroke_lookup_lights_each_stroke_of_its_outline_in_turn() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let view = playing("T/-D");
        // T is the left-hand T- (slot 1); -D is slot 20
        let (t, d) = (1, 20);
        let first = drawn(&mut renderer, &view);
        assert!(key_lit(&renderer, &first, t), "the first stroke lights first");
        assert!(!key_lit(&renderer, &first, d), "and not the second yet");

        renderer.set_step(1);
        let second = drawn(&mut renderer, &view);
        assert!(key_lit(&renderer, &second, d), "then the second");
        assert!(!key_lit(&renderer, &second, t), "and the first goes dark");

        // It loops: a step past the end is the first again
        renderer.set_step(2);
        let again = drawn(&mut renderer, &view);
        assert_eq!(first.data(), again.data());
    }

    #[test]
    fn a_lookup_with_the_join_key_lights_it_on_the_continuation_stroke_only() {
        use crate::steno::layout::Layout;
        use crate::config::OverlayPlacement;
        // The join key as the lyk theory has it: the top half of S-
        let mut config = OverlayConfig::default();
        config.overlay_overrides.insert(
            "+".to_string(),
            OverlayPlacement::SplitKey {
                slot: "S-".to_string(),
                half: Half::Top,
            },
        );
        let layout = Layout::plover_with(&['+']).unwrap();
        let mut renderer = Renderer::with_layout(&config, 1, 900, layout.clone()).unwrap();
        let strokes = layout.parse_strokes_trimmed("ED/+-D").unwrap();
        let view = LookupView {
            query: "ended".into(),
            rows: vec![crate::steno::overlay::LookupRow {
                text: "ended".into(),
                outline: "ED/+-D".into(),
            }],
            hint: strokes[0],
            strokes,
            mode: LookupMode::Word,
        };
        assert_eq!(Renderer::lookup_strokes(&view).len(), 2);
        let join_lit = |renderer: &Renderer, pixmap: &Pixmap| {
            let rect = renderer.keyboard.as_ref().unwrap().extra[0];
            let pixel = pixmap
                .pixel((rect.x + 2.0) as u32, (rect.y + rect.h / 2.0) as u32)
                .unwrap();
            pixel.alpha() > 100
        };
        let e = crate::steno::layout::slot_by_name("-E").unwrap();
        let first = drawn(&mut renderer, &view);
        assert!(key_lit(&renderer, &first, e), "ED starts with E");
        assert!(!join_lit(&renderer, &first), "no join key on the first stroke");
        renderer.set_step(1);
        let second = drawn(&mut renderer, &view);
        assert!(join_lit(&renderer, &second), "+-D lights the join key");
        assert!(!key_lit(&renderer, &second, e), "and E goes dark");
        renderer.set_step(2);
        assert_eq!(first.data(), drawn(&mut renderer, &view).data(), "it loops");
    }

    #[test]
    fn a_single_stroke_lookup_is_static_whatever_the_step() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let view = playing("KAPBD");
        let at_zero = drawn(&mut renderer, &view);
        renderer.set_step(1);
        let at_one = drawn(&mut renderer, &view);
        renderer.set_step(7);
        let at_seven = drawn(&mut renderer, &view);
        assert_eq!(at_zero.data(), at_one.data());
        assert_eq!(at_zero.data(), at_seven.data());
        assert_eq!(Renderer::lookup_strokes(&view).len(), 1);
    }

    #[test]
    fn a_daemon_that_sent_only_a_hint_still_lights_it() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let mut view = playing("T");
        view.strokes.clear();
        assert_eq!(Renderer::lookup_strokes(&view), [view.hint]);
        let pixmap = drawn(&mut renderer, &view);
        assert!(key_lit(&renderer, &pixmap, 1));
        view.hint = 0;
        assert!(Renderer::lookup_strokes(&view).is_empty());
    }

    /// The brightest ink drawn in the columns `x0..x1`, above the query row (so only the results).
    /// Brightness, not alpha: a glyph's shadow is black, so it adds alpha but no light.
    fn ink(renderer: &Renderer, pixmap: &Pixmap, x0: f32, x1: f32) -> u8 {
        let above = (renderer.tape_bottom - renderer.line_height) as u32;
        (0..above)
            .flat_map(|y| (x0 as u32..x1 as u32).map(move |x| (x, y)))
            .filter_map(|(x, y)| pixmap.pixel(x, y))
            .map(|p| p.red().max(p.green()).max(p.blue()))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn the_outline_text_dims_the_strokes_not_being_shown_instead_of_bracketing_one() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let view = playing("KAPBD/EUD");
        // the first stroke is "KAPBD" and the second "EUD", with a rule between them; the
        // strokes end at the paper's right edge, flush against the words
        let word = renderer.outline_units("KAPBD/EUD").unwrap().remove(0);
        // set the way `draw_line` sets it: smaller letters if it does not fit the paper at full size
        let paper = renderer.paper_right - renderer.paper_left;
        let tier = renderer.fit_tier(std::slice::from_ref(&word), paper);
        let (w1, w2) = (
            renderer.run_width(&word[0], tier),
            renderer.run_width(&word[1], tier),
        );
        let between = 2.0f32.mul_add(renderer.rule_space(), renderer.rule_thickness());
        let left = renderer.paper_right - (w1 + between + w2);
        let first = (left, left + w1);
        let second_x = left + w1 + between;
        let second = (second_x, second_x + w2);

        let at_zero = drawn(&mut renderer, &view);
        let (a, b) = (
            ink(&renderer, &at_zero, first.0, first.1),
            ink(&renderer, &at_zero, second.0, second.1),
        );
        assert!(a > 150, "the stroke being shown is full strength: {a}");
        assert!(
            u32::from(b) * 10 < u32::from(a) * 6,
            "the other is dimmed to about half: {b} against {a}"
        );

        renderer.set_step(1);
        let at_one = drawn(&mut renderer, &view);
        let (a, b) = (
            ink(&renderer, &at_one, first.0, first.1),
            ink(&renderer, &at_one, second.0, second.1),
        );
        assert!(b > 150, "now the second is full strength: {b}");
        assert!(
            u32::from(a) * 10 < u32::from(b) * 6,
            "and the first is dimmed: {a} against {b}"
        );
    }

    #[test]
    fn an_open_lookup_replaces_the_tape_and_lights_the_hinted_key() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut tape_only = OverlaySnapshot::default();
        tape_only.tape.push_back(tape_entry(1, "KAT", "\"cat\""));
        let mut with_lookup = OverlaySnapshot {
            lookup: Some(lookup_view()),
            ..tape_only.clone()
        };

        let mut tape_pixels = Pixmap::new(w, h).unwrap();
        renderer.draw(&tape_only, &mut tape_pixels.as_mut());
        let mut lookup_pixels = Pixmap::new(w, h).unwrap();
        renderer.draw(&with_lookup, &mut lookup_pixels.as_mut());
        assert_ne!(
            tape_pixels.data(),
            lookup_pixels.data(),
            "the lookup is drawn"
        );

        // The newest tape line sits just above the keyboard; with a lookup open that spot holds
        // the query row instead, so the tape's own text ("cat") is gone from its usual place
        let row = (renderer.tape_bottom - renderer.line_height) as u32;
        let words_x = renderer.words_left as u32;
        let region = |pixmap: &Pixmap| -> Vec<u8> {
            (words_x..words_x + 80)
                .flat_map(|x| {
                    // mid-line, where the letters are (the bigger text starts further down)
                    let p = pixmap.pixel(x, row + (renderer.line_height / 2.0) as u32).unwrap();
                    [p.red(), p.green(), p.blue(), p.alpha()]
                })
                .collect()
        };
        assert_ne!(region(&tape_pixels), region(&lookup_pixels));

        // The hinted key (T-) is lit in the accent, dimmer than a held key
        let t = renderer.keyboard.as_ref().unwrap().keys[1];
        let pixel = lookup_pixels
            .pixel((t.x + 2.0) as u32, (t.y + t.h / 2.0) as u32)
            .unwrap();
        assert!(
            pixel.alpha() > 100 && pixel.alpha() < 200,
            "{}",
            pixel.alpha()
        );
        assert!(
            pixel.red() > pixel.blue(),
            "the left hand's amber accent, not the unlit key"
        );

        // Closing the lookup brings the tape back exactly as it was
        with_lookup.lookup = None;
        let mut after = Pixmap::new(w, h).unwrap();
        renderer.draw(&with_lookup, &mut after.as_mut());
        assert_eq!(tape_pixels.data(), after.data());
    }

    #[test]
    fn a_long_result_list_fills_the_whole_tape_area_up_to_the_top_margin() {
        use crate::steno::overlay::LookupRow;
        for height in [1080, 1440] {
            let mut renderer = Renderer::new(&OverlayConfig::default(), 1, height).unwrap();
            let (w, h) = renderer.size();
            let snapshot = OverlaySnapshot {
                layer: Some("steno".into()),
                lookup: Some(LookupView {
                    query: "ca".into(),
                    rows: (0..400)
                        .map(|i| LookupRow {
                            text: format!("cand{i}"),
                            outline: "KAPBD".into(),
                        })
                        .collect(),
                    hint: 0,
                    strokes: Vec::new(),
                    mode: LookupMode::Word,
                }),
                ..OverlaySnapshot::default()
            };
            let mut pixmap = Pixmap::new(w, h).unwrap();
            renderer.draw(&snapshot, &mut pixmap.as_mut());
            // Every line of the tape area but the query's own holds a result, the topmost one
            // starting at the margin; there is no ink above that margin
            let lit = |y0: u32, y1: u32| {
                (y0..y1).any(|y| (0..w).any(|x| pixmap.pixel(x, y).unwrap().alpha() > 0))
            };
            let top = (renderer.tape_lines() as f32)
                .mul_add(-renderer.line_height, renderer.tape_bottom);
            assert!((top - 14.0).abs() < 0.01);
            for line in 0..renderer.tape_lines() {
                let y = (line as f32).mul_add(renderer.line_height, top);
                assert!(
                    lit(y as u32, (y + renderer.line_height) as u32),
                    "{height}: line {line} of {} holds a result",
                    renderer.tape_lines()
                );
            }
            assert!(!lit(0, (top - 6.0) as u32), "{height}: ink above the top margin");
        }
    }

    #[test]
    fn several_single_word_results_stack_without_a_gap_between_them() {
        use crate::steno::overlay::LookupRow;
        // Multiple one-line candidates - plain word lookup's usual shape - used to have the
        // tape's leftover room spread out as extra gap *between every row*, reading as a wall
        // of blank lines separating otherwise related words. They should stack tight, one row
        // right after another, like a list; only a single result gets pushed up for breathing
        // room (covered by the preview tests, since there's no "gap between rows" to check).
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let row = |text: &str, outline: &str| LookupRow {
            text: text.into(),
            outline: outline.into(),
        };
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![
                    row("candy", "KAPBD"),
                    row("candle", "KAPBL"),
                    row("candid", "KAPBD/EUD"),
                    row("candidate", "KAPBD/TKAEUT"),
                    row("cane", "KAEUPB"),
                ],
                hint: 0,
                strokes: Vec::new(),
                mode: LookupMode::Word,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let lit_in_row = |row_top: f32| {
            let y0 = row_top.max(0.0) as u32;
            let y1 = ((row_top + renderer.line_height).ceil() as u32).min(h);
            let x0 = renderer.words_left.max(0.0) as u32;
            let x1 = (renderer.words_right.ceil() as u32).min(w);
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter_map(|(x, y)| pixmap.pixel(x, y))
                .any(|p| p.alpha() > 0)
        };

        // Five one-line results, stacked tight: the whole block is exactly 5 lines tall, right
        // above the query row. The row snug against the query is lit, as expected either way -
        // the real test is that nothing spills into the *sixth* line's worth of space: with the
        // old bug, gaps between the five rows push the block's top further up, lighting pixels
        // a full extra line above where a tightly-packed block of 5 would ever reach.
        let query_row_top = renderer.tape_bottom - renderer.line_height;
        assert!(
            lit_in_row(query_row_top - renderer.line_height),
            "the result closest to the query should be lit"
        );
        assert!(
            !lit_in_row(6.0f32.mul_add(-renderer.line_height, query_row_top)),
            "5 one-line results stacked with no gap take up exactly 5 lines - anything lit a 6th \
             line above the query means extra gap crept in between them"
        );
    }

    #[test]
    fn a_result_does_not_draw_on_the_query_row() {
        use crate::steno::overlay::LookupRow;
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();

        // Same query, one with a result below it and one without: the query row itself (the
        // "LOOKUP" label, the typed text and its caret) should look identical either way. If a
        // result is drawn on top of that row instead of above it, this differs.
        let no_rows = OverlaySnapshot {
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![],
                hint: 0,
                strokes: Vec::new(),
                mode: LookupMode::Word,
            }),
            ..OverlaySnapshot::default()
        };
        let one_row = OverlaySnapshot {
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![LookupRow {
                    text: "candy".into(),
                    outline: "KAPBD".into(),
                }],
                hint: 0,
                strokes: Vec::new(),
                mode: LookupMode::Word,
            }),
            ..OverlaySnapshot::default()
        };

        let mut without = Pixmap::new(w, h).unwrap();
        renderer.draw(&no_rows, &mut without.as_mut());
        let mut with_one = Pixmap::new(w, h).unwrap();
        renderer.draw(&one_row, &mut with_one.as_mut());

        let query_row_top = (renderer.tape_bottom - renderer.line_height).max(0.0) as u32;
        let query_row_bottom = (renderer.tape_bottom.ceil() as u32).min(h);
        let row = |pixmap: &Pixmap| -> Vec<u8> {
            (query_row_top..query_row_bottom)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .flat_map(|(x, y)| {
                    let p = pixmap.pixel(x, y).unwrap();
                    // Only the ink counts: a result's soft dark shadow may reach into this row
                    if u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue()) > 300 {
                        [p.red(), p.green(), p.blue(), p.alpha()]
                    } else {
                        [0; 4]
                    }
                })
                .collect()
        };
        assert_eq!(
            row(&without),
            row(&with_one),
            "the query row should be the same whether or not a result sits above it"
        );
    }

    #[test]
    fn a_wrapped_result_extends_upward_not_onto_the_row_below() {
        use crate::steno::overlay::LookupRow;
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();

        // An outline too wide for one line, paired with a short word so only the strokes
        // column wraps. The wrapped second line has to land above the first, inside this row's
        // own budget - never downward onto the query row or the keyboard beneath it.
        let snapshot = OverlaySnapshot {
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![LookupRow {
                    text: "candy".into(),
                    outline: "KAPBD/TKEU/HRAOEUT/SRAOEUT/KAPBD/TKEU/HRAOEUT/SRAOEUT".into(),
                }],
                hint: 0,
                strokes: Vec::new(),
                mode: LookupMode::Word,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let lit_in_row = |row_top: f32| {
            let y0 = row_top.max(0.0) as u32;
            let y1 = ((row_top + renderer.line_height).ceil() as u32).min(h);
            let x0 = renderer.paper_left.max(0.0) as u32;
            let x1 = w;
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter_map(|(x, y)| pixmap.pixel(x, y))
                .any(|p| p.alpha() > 0)
        };

        let query_row_top = renderer.tape_bottom - renderer.line_height;
        let result_row_top = query_row_top - renderer.line_height;
        let wrap_row_top = result_row_top - renderer.line_height;
        assert!(
            lit_in_row(result_row_top),
            "the wrapped outline's first line should draw directly above the query"
        );
        assert!(
            lit_in_row(wrap_row_top),
            "a wrapped outline should continue upward, not be dropped"
        );
    }

    #[test]
    fn a_long_phrase_breaks_its_outline_and_words_on_the_same_lines() {
        use crate::steno::overlay::LookupRow;
        // The user's reported case: "I want to go to the store and buy cookies" wraps the word
        // column onto several lines; the outline column, being much narrower per word, used to
        // wrap independently and cram more words' worth of strokes onto its own "line 1" than
        // the word column showed there - 6 outlines over 4 words, confusing to read. Every line
        // in one column must show the same words as the matching line in the other.
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "i want to go to the store and buy cookies".into(),
                rows: vec![LookupRow {
                    text: "I want to go to the store and buy cookies".into(),
                    outline: "EU / WAUPBT / TO / TKPWOE / TO / -T / STOR / APBD / PWEU / KAOBG/KEUS"
                        .into(),
                }],
                hint: 0,
                strokes: Vec::new(),
                mode: LookupMode::Phrase,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let lit = |x0: u32, x1: u32, y0: f32, y1: f32| {
            (y0.max(0.0) as u32..(y1.ceil() as u32).min(h))
                .flat_map(|y| (x0..x1.min(w)).map(move |x| (x, y)))
                .filter_map(|(x, y)| pixmap.pixel(x, y))
                .any(|p| p.alpha() > 0)
        };
        let paper_left = renderer.paper_left as u32;
        let paper_right = renderer.paper_right as u32;
        let words_left = renderer.words_left as u32;
        let words_right = renderer.words_right as u32;
        let line_height = renderer.line_height;

        // Walk upward from the query row, one line at a time: as soon as both columns have gone
        // blank, the phrase's block is done. Any line in between where one column still has
        // something lit and the other doesn't is exactly the desync this guards against.
        let mut row_top = 2.0f32.mul_add(-line_height, renderer.tape_bottom); // above the query row
        let mut synced_lines = 0;
        for _ in 0..8 {
            let outline_lit = lit(paper_left, paper_right, row_top, row_top + line_height);
            let text_lit = lit(words_left, words_right, row_top, row_top + line_height);
            if !outline_lit && !text_lit {
                break;
            }
            assert_eq!(
                outline_lit, text_lit,
                "row at y={row_top} has outline_lit={outline_lit} but text_lit={text_lit} - \
                 the two columns disagree about which words are on this line"
            );
            synced_lines += 1;
            row_top -= line_height;
        }
        assert!(
            synced_lines > 1,
            "expected the phrase to wrap onto more than one line"
        );
    }

    #[test]
    fn a_long_query_keeps_its_end_visible() {
        let mut glyphs = GlyphCache::new(glyphs::load_font(None, glyphs::BOLD).unwrap(), 20.0);
        assert_eq!(tail_that_fits(&mut glyphs, "short", 500.0), "short");
        let long = "supercalifragilisticexpialidocious";
        let tail = tail_that_fits(&mut glyphs, long, 120.0);
        assert!(long.ends_with(tail) && tail.len() < long.len() && !tail.is_empty());
        assert!(glyphs.measure(tail) <= 120.0);
        // Nothing fits: no panic, and no endless loop
        assert_eq!(tail_that_fits(&mut glyphs, "wide", 0.0), "");
    }

    /// Writes a frame with a lookup open, to look at it:
    /// `OVERLAY_PREVIEW=/tmp/lookup.ppm cargo test preview_lookup -- --ignored`
    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview_lookup() {
        use crate::steno::overlay::LookupRow;
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let row = |text: &str, outline: &str| LookupRow {
            text: text.into(),
            outline: outline.into(),
        };
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![
                    row("candy", "KAPBD"),
                    row("candle", "KAPBL"),
                    row("candid", "KAPBD/EUD"),
                    row("candidate", "KAPBD/TKAEUT"),
                    row("cane", "KAEUPB"),
                ],
                hint: crate::steno::layout::parse_stroke("KAPBD").unwrap(),
                strokes: Vec::new(),
                mode: LookupMode::Word,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
            let (row, col) = (i as u32 / w, i as u32 % w);
            let backdrop = [70 + row * 50 / h, 60 + col * 40 / w, 100 + row * 60 / h];
            for (c, back) in px[..3].iter().zip(backdrop) {
                out.push(c + (back * (255 - u32::from(px[3])) / 255) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    /// Scratch preview with a long translation whose word side wraps far more than its
    /// outline side, to eyeball the per-row gap and the tight within-row spacing by hand.
    /// `OVERLAY_PREVIEW=/tmp/lookup3.ppm cargo test preview_lookup_long -- --ignored`
    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview_lookup_long() {
        use crate::steno::overlay::LookupRow;
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "cand".into(),
                rows: vec![
                    LookupRow {
                        text: "candidate".into(),
                        outline: "KAPBD/TKAEUT".into(),
                    },
                    LookupRow {
                        text: "I hope you dont store this translation per key".into(),
                        outline: "KAPBD".into(),
                    },
                    LookupRow {
                        text: "I want to go to the store and buy cookies".into(),
                        outline:
                            "EU / WAUPBT / TO / TKPWOE / TO / -T / STOR / APBD / PWEU / KAOBG/KEUS"
                                .into(),
                    },
                    LookupRow {
                        text: "candy".into(),
                        outline: "KAPBD".into(),
                    },
                ],
                hint: crate::steno::layout::parse_stroke("KAPBD").unwrap(),
                strokes: Vec::new(),
                mode: LookupMode::Phrase,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
            let (row, col) = (i as u32 / w, i as u32 % w);
            let backdrop = [70 + row * 50 / h, 60 + col * 40 / w, 100 + row * 60 / h];
            for (c, back) in px[..3].iter().zip(backdrop) {
                out.push(c + (back * (255 - u32::from(px[3])) / 255) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    /// Scratch preview of the "ENGINE RESOLVER" label, the longest of the three, to check it
    /// fits the strokes column. `OVERLAY_PREVIEW=/tmp/lookup6.ppm cargo test preview_lookup_stroke -- --ignored`
    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview_lookup_stroke() {
        use crate::steno::overlay::LookupRow;
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 1080).unwrap();
        let (w, h) = renderer.size();
        let row = |text: &str, outline: &str| LookupRow {
            text: text.into(),
            outline: outline.into(),
        };
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "KAPBD/TKEU".into(),
                rows: vec![
                    row("\u{2192} \"candy\"", "KAPBD/TKEU"),
                    row("\u{2192} \"candidate\"", "KAPBD/TKAEUT"),
                    row("\u{2192} \"candle\"", "KAPBL/TKEU"),
                    row("\u{2192} \"can\"", "KAPB/TKEU"),
                ],
                hint: crate::steno::layout::parse_stroke("KAPBD").unwrap(),
                strokes: Vec::new(),
                mode: LookupMode::Stroke,
            }),
            ..OverlaySnapshot::default()
        };
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for (i, px) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
            let (row, col) = (i as u32 / w, i as u32 % w);
            let backdrop = [70 + row * 50 / h, 60 + col * 40 / w, 100 + row * 60 / h];
            for (c, back) in px[..3].iter().zip(backdrop) {
                out.push(c + (back * (255 - u32::from(px[3])) / 255) as u8);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn translations_drop_the_tape_quotes() {
        assert_eq!(display_text("\"the\""), "the");
        assert_eq!(display_text("-3 \"the\""), "-3 the");
        assert_eq!(display_text("KC_A KC_B"), "KC_A KC_B");
    }

    /// The group number of each key in `stroke`'s run, with `-` and ungrouped keys as `None`
    fn groups_of(renderer: &Renderer, stroke: &str) -> Vec<(char, Option<u32>)> {
        let bits = renderer.layout.parse_stroke(stroke).unwrap();
        renderer
            .stroke_run(bits)
            .iter()
            .map(|k| (k.ch, k.pair))
            .collect()
    }

    #[test]
    fn the_default_layout_groups_keys_in_todays_pairs() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let g = groups_of(&renderer, "TKPWAO*EUFRDZ");
        let group = |ch: char, n: usize| g.iter().filter(|(c, _)| *c == ch).nth(n).unwrap().1;
        assert_eq!(group('T', 0), group('K', 0));
        assert_eq!(group('P', 0), group('W', 0));
        assert_eq!(group('A', 0), group('O', 0));
        assert_eq!(group('F', 0), group('R', 0));
        assert_eq!(group('D', 0), group('Z', 0));
        assert_ne!(group('T', 0), group('P', 0));
        assert_eq!(group('*', 0), None);
    }

    #[test]
    fn config_can_group_keys_as_a_trio() {
        let mut config = OverlayConfig::default();
        for name in ["S-", "T-", "K-"] {
            config.key_groups.insert(name.into(), 50);
        }
        let renderer = Renderer::with_layout(&config, 1, 900, Layout::plover()).unwrap();
        let g = groups_of(&renderer, "STKPW");
        let groups: Vec<_> = g.iter().map(|(_, p)| *p).collect();
        assert_eq!(&groups[..3], &[Some(50), Some(50), Some(50)]);
        // `P W` is untouched
        assert_eq!(groups[3], groups[4]);
        assert_ne!(groups[3], Some(50));
        // A key name that does not exist is ignored, not fatal
        config.key_groups.insert("nonsense".into(), 1);
        assert!(Renderer::with_layout(&config, 1, 900, Layout::plover()).is_ok());
    }

    #[test]
    fn an_extra_key_inherits_its_slots_group_unless_config_overrides_it() {
        use crate::config::OverlayPlacement;
        let mut config = OverlayConfig::default();
        config.overlay_overrides.insert(
            "+".into(),
            OverlayPlacement::SplitKey {
                slot: "T-".into(),
                half: Half::Top,
            },
        );
        let renderer =
            Renderer::with_layout(&config, 1, 900, test_layout_with_join()).unwrap();
        let g = groups_of(&renderer, "+TK");
        assert_eq!(g[0].1, g[1].1);
        assert_eq!(g[1].1, g[2].1);
        assert!(g[0].1.is_some());

        config.key_groups.insert("join".into(), 77);
        let renderer =
            Renderer::with_layout(&config, 1, 900, test_layout_with_join()).unwrap();
        let g = groups_of(&renderer, "+TK");
        assert_eq!(g[0].1, Some(77));
        assert_ne!(g[0].1, g[1].1);
    }

    #[test]
    fn an_extra_keys_colour_group_comes_from_config_and_defaults_to_misc() {
        let layout = || test_layout_with_join();
        let plain = Renderer::with_layout(&OverlayConfig::default(), 1, 900, layout()).unwrap();
        let bits = plain.layout.parse_stroke("+K").unwrap();
        assert_eq!(plain.stroke_run(bits)[0].color, plain.theme.accent(KeyGroup::Misc));

        for (color, group) in [
            (KeyColor::Left, KeyGroup::LeftHand),
            (KeyColor::Vowel, KeyGroup::Vowel),
            (KeyColor::Right, KeyGroup::RightHand),
        ] {
            let mut config = OverlayConfig::default();
            config.extra_colors.insert("+".into(), color);
            let renderer = Renderer::with_layout(&config, 1, 900, layout()).unwrap();
            assert_eq!(renderer.stroke_run(bits)[0].color, renderer.theme.accent(group));
        }
    }
}
