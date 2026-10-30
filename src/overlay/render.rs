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

use super::geometry::{KeyGroup, Keyboard, Rect};
use super::glyphs::{self, GlyphCache};
use crate::config::OverlayConfig;
use crate::steno::layout::EXTRA_KEYS;
use crate::steno::overlay::{LookupMode, LookupView, OverlaySnapshot, TapeEntry, TAPE_CAPACITY};
use anyhow::Result;
use std::borrow::Cow;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform};

/// Space between the content and the left/right edge of the overlay, logical px
const PAD_X: f32 = 10.0;
/// Space above the tape and below the keyboard, logical px
const PAD_Y: f32 = 14.0;
/// Space between the tape and the keyboard, logical px
const SECTION_GAP: f32 = 20.0;
/// Outline of an unlit key, logical px
const OUTLINE: f32 = 1.0;
/// Radius of a pad's top corners relative to its width. The bottom is fully round, except on
/// the top-row finger keys (see [`has_square_bottom`]), which are the same as the top.
const PAD_TOP_ROUND: f32 = 0.2;
/// Slot of the `#` number bar
const NUMBER_SLOT: usize = 22;
/// Share of the width inside the padding given to the steno letters; the words get the rest.
/// The letters are sized to fill this, so a bigger share means bigger steno letters.
const PAPER_SHARE: f32 = 0.68;
/// Space after each steno letter's own width, as a share of the letter: the letter spacing of
/// the tape's steno letters, and nothing else (the words on the right are set normally). Small
/// so strokes read as tight as text; each column is as wide as its own letter, so `W` gets
/// more room than `T` and nothing ever overlaps.
const SLOT_GAP: f32 = 0.04;
/// The words are what you read, so they are at least as big as the strokes
const WORDS_RATIO: f32 = 1.06;
/// Line pitch relative to the words' own height
const LINE_SPACING: f32 = 1.12;
/// A key's label is this share of the key's width
const LABEL_RATIO: f32 = 0.56;
/// Text size the column widths are measured at; they scale linearly from here
const PROBE_PX: f32 = 32.0;
/// The oldest tape lines, at the top of the screen, fade out: this many of them, the topmost
/// faintest. Every other line is at full strength.
const FADE_LINES: usize = 3;
/// Plover paper tape order: `#` first, then the slots in stroke order
const PAPER_ORDER: [usize; 23] = [
    22, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
];

/// Colours. With no background, everything has to read over any wallpaper: text is bright with
/// a dark halo, and unlit keys are a faint dark glass pad with a light outline.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub panel: [u8; 3],
    pub key: [u8; 4],
    pub outline: [u8; 4],
    pub label: [u8; 4],
    pub label_lit: [u8; 4],
    /// Lit-key colours by part of the keyboard: left hand green, thumb vowels yellow, right
    /// hand red, and `*` and `#` light blue
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
            key: [14, 18, 26, 38],
            outline: [214, 224, 238, 80],
            label: [226, 232, 242, 245],
            label_lit: [8, 12, 16, 255],
            left: [45, 212, 170, 255],
            vowel: [232, 226, 150, 255],
            right: [244, 118, 124, 255],
            misc: [136, 196, 255, 255],
            strokes: [192, 201, 216, 255],
            words: [248, 250, 253, 255],
            unmatched: [255, 120, 120, 255],
        }
    }
}

impl Theme {
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

/// Where one steno letter sits on a tape line, relative to the left edge of the letters.
#[derive(Debug, Clone, Copy)]
struct Column {
    x: f32,
    width: f32,
}

pub struct Renderer {
    config: OverlayConfig,
    theme: Theme,
    scale: f32,
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
    /// One per [`EXTRA_KEYS`] entry, same order, drawn before the standard columns below —
    /// a stroke's extra keys read first on the tape, the same way they get their own badge
    /// row above the keyboard rather than a guessed spot among the 23 standard keys.
    extra_columns: Vec<Column>,
    /// One per tape slot in [`PAPER_ORDER`] order
    columns: Vec<Column>,
    /// Left edge of the steno letters, and where the words start, physical px
    paper_left: f32,
    words_left: f32,
    /// The right edge words are clipped at
    words_right: f32,
    labels: GlyphCache,
    strokes: GlyphCache,
    words: GlyphCache,
}

impl Renderer {
    /// Lay the overlay out for `config` on a surface `height` logical pixels tall, at an
    /// integer output `scale`. The tape takes all the height the keyboard leaves.
    pub fn new(config: &OverlayConfig, scale: u32, height: u32) -> Result<Self> {
        let scale = scale.max(1) as f32;
        let bold = glyphs::load_font(config.font.as_deref(), glyphs::BOLD)?;

        let width = (config.width as f32 * scale).ceil();
        let height = (height.max(1) as f32 * scale).ceil();
        let (pad_x, pad_y) = (PAD_X * scale, PAD_Y * scale);
        let inner = 2.0f32.mul_add(-pad_x, width);
        let text_scale = config.text_scale.clamp(0.5, 3.0);

        // The keyboard sits at the bottom. Laying it out once at the origin gives its height.
        let keys_height = Keyboard::new(0.0, 0.0, inner, &config.overlay_overrides).size.1;
        let keyboard = config.keyboard.then(|| {
            Keyboard::new(
                pad_x,
                height - pad_y - keys_height,
                inner,
                &config.overlay_overrides,
            )
        });
        let key_width = Keyboard::new(0.0, 0.0, inner, &config.overlay_overrides).keys[1].w;
        // Every size is nudged to where the letters' flat tops and bottoms land on whole pixel
        // rows (`crisp_size`); at an arbitrary size they smear over two rows and the text looks
        // soft. Capitals (key labels, steno letters) snap on `H`, lowercase words on `x`.
        let label_px = glyphs::crisp_size(&bold, key_width * LABEL_RATIO * text_scale, 'H');
        let labels = GlyphCache::new(bold.clone(), label_px);

        // Size the steno letters so that every column — any extra keys first, then all 23
        // standard slots — each as wide as its own letter plus `SLOT_GAP`, fill `PAPER_SHARE`
        // of the width. Column widths scale linearly with the size, so measure once at a
        // probe size.
        let mut probe = GlyphCache::new(bold.clone(), PROBE_PX);
        let extra_labels: Vec<char> = (0..EXTRA_KEYS.len()).map(Keyboard::extra_label).collect();
        let probe_total: f32 = extra_labels
            .iter()
            .copied()
            .chain(PAPER_ORDER.iter().map(|&slot| Keyboard::label(slot)))
            .map(|ch| probe.get(ch).advance * (1.0 + SLOT_GAP))
            .sum();
        let stroke_px = glyphs::crisp_size(
            &bold,
            PROBE_PX * (PAPER_SHARE * inner) / probe_total * text_scale,
            'H',
        );
        let mut strokes = GlyphCache::new(bold.clone(), stroke_px);
        let words_px = glyphs::crisp_size(&bold, stroke_px * WORDS_RATIO, 'x');
        let words = GlyphCache::new(bold, words_px);

        let mut x = 0.0;
        let mut extra_columns = Vec::with_capacity(extra_labels.len());
        for &ch in &extra_labels {
            let width = strokes.get(ch).advance * (1.0 + SLOT_GAP);
            extra_columns.push(Column { x, width });
            x += width;
        }
        let mut columns = Vec::with_capacity(PAPER_ORDER.len());
        for &slot in &PAPER_ORDER {
            let width = strokes.get(Keyboard::label(slot)).advance * (1.0 + SLOT_GAP);
            columns.push(Column { x, width });
            x += width;
        }
        let words_left = pad_x + x + stroke_px * 0.6;
        let words_right = width - pad_x;

        let line_height = (words.line_height() * LINE_SPACING).ceil();
        let tape_bottom = keyboard.as_ref().map_or(height - pad_y, |k| {
            SECTION_GAP.mul_add(-scale, k.keys[NUMBER_SLOT].y)
        });
        let fits = ((tape_bottom - pad_y) / line_height).floor().max(0.0) as usize;
        let tape_lines = if config.tape {
            fits.min(config.tape_lines.unwrap_or(usize::MAX))
                .min(TAPE_CAPACITY)
        } else {
            0
        };

        // Steno pads: a squarish top with slightly softened corners, and a fully round
        // bottom, except on the top-row finger keys, whose bottom matches their top. The `#`
        // bar is a plain pill. Radii are clamped to fit by `rounded_rect`.
        let key_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            k.keys
                .iter()
                .enumerate()
                .filter_map(|(slot, r)| {
                    if slot == NUMBER_SLOT {
                        rounded_rect(*r, f32::MAX, f32::MAX)
                    } else if has_square_bottom(slot) {
                        rounded_rect(*r, r.w * PAD_TOP_ROUND, r.w * PAD_TOP_ROUND)
                    } else {
                        rounded_rect(*r, r.w * PAD_TOP_ROUND, f32::MAX)
                    }
                })
                .collect()
        });
        let extra_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            k.extra
                .iter()
                .filter_map(|r| rounded_rect(*r, r.w * PAD_TOP_ROUND, r.w * PAD_TOP_ROUND))
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
            theme: Theme::default(),
            scale,
            size: (width as u32, height as u32),
            panel,
            keyboard,
            key_shapes,
            extra_shapes,
            tape_bottom,
            line_height,
            tape_lines,
            extra_columns,
            columns,
            paper_left: pad_x,
            words_left,
            words_right,
            labels,
            strokes,
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
        let paper_right = {
            let last = self.columns[self.columns.len() - 1];
            self.paper_left + last.x + last.width
        };

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
        let caret_x = 2.0f32.mul_add(self.scale, self.words_left + self.words.measure(shown));
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
        // How many lines each row's own text needs - the wider of its outline and its word,
        // since both share the row - stopping once the panel's line budget is used up rather
        // than ever truncating a long outline or a long phrase with an ellipsis. A row that
        // would only partly fit is left off entirely, so the ones shown always get full text.
        let mut shown_rows = Vec::with_capacity(view.rows.len());
        let mut used = 0usize;
        for row in &view.rows {
            let outline_lines = wrap_text(&mut self.strokes, &row.outline, paper_right - self.paper_left).len();
            let text_lines = wrap_text(&mut self.words, &row.text, self.words_right - self.words_left).len();
            let row_lines = outline_lines.max(text_lines).max(1);
            if used + row_lines > room_for {
                break;
            }
            shown_rows.push((row, outline_lines, text_lines, row_lines));
            used += row_lines;
        }
        // Spread the rows that fit across the whole budget instead of only the room their own
        // text needs, so a handful of short results still fill the tape rather than leaving
        // most of it blank - but only as *extra gap between rows*. A row's own wrapped lines
        // (its outline or its word breaking onto a further line) always stay at the normal,
        // tight `line_height`: stretching those too made every wrapped line look like it had
        // a spare blank line under it, which is wrong far more often than it's helpful, since
        // a word usually wraps more than its outline does (English words are long; outlines
        // are a handful of letters), making the two columns look unsynced inside one row.
        const MAX_GAP_RATIO: f32 = 0.6;
        let max_gap = MAX_GAP_RATIO * line_height;
        let gaps = shown_rows.len() as f32;
        let extra = (room_for as f32 - used as f32) * line_height;
        let gap_each = if gaps > 0.0 {
            (extra / gaps).clamp(0.0, max_gap)
        } else {
            0.0
        };

        let mut block_top = bottom - line_height; // the query's own block, already placed
        for (row, outline_lines, text_lines, row_lines) in shown_rows {
            let row_height = row_lines as f32 * line_height;
            let row_bottom = block_top - gap_each;
            let row_top = row_bottom - row_height;
            block_top = row_top;
            // A word usually needs more lines to wrap than its outline does, so the shorter
            // column is centred within the row's full height rather than pinned to its top -
            // otherwise it reads as orphaned above a tall gap instead of belonging to the row.
            let center = |col_lines: usize| {
                ((row_lines - col_lines) as f32 / 2.0).mul_add(line_height, row_top) + baseline_in_line
            };
            // The word as typed is full strength; words that merely start with it a bit less
            let strength = if row.text == view.query.trim() {
                1.0
            } else {
                0.8
            };
            draw_text_wrapped(
                pixmap,
                &mut self.strokes,
                &row.outline,
                self.paper_left,
                center(outline_lines),
                paper_right,
                line_height,
                fade_color(self.theme.strokes, strength),
                self.scale,
            );
            draw_text_wrapped(
                pixmap,
                &mut self.words,
                &row.text,
                self.words_left,
                center(text_lines),
                self.words_right,
                line_height,
                fade_color(self.theme.words, strength),
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
        // A lookup lights the first stroke of its best result, dimly, like a stroke in progress
        let hint = snapshot.lookup.as_ref().map_or(0, |lookup| lookup.hint);
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
            let label = Keyboard::label(slot);
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
        let accent = self.theme.accent(KeyGroup::Misc);
        for (i, shape) in self.extra_shapes.iter().enumerate() {
            let bit = Keyboard::extra_bit(i);
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
            let label = Keyboard::extra_label(i);
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
        // `planned` is newest first; reversed, this walks oldest (top of the tape) to newest
        // (bottom), so `row_from_top` already is what `line_fade` wants - only the oldest few
        // rows, at the top, should ever come out faint.
        for (row_from_top, (entry, text_lines)) in planned.iter().rev().enumerate() {
            let fade = line_fade(row_from_top);
            self.draw_tape_line(pixmap, entry, row_top + baseline_in_line, fade);
            row_top = (*text_lines as f32).mul_add(self.line_height, row_top);
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

    fn draw_tape_line(
        &mut self,
        pixmap: &mut PixmapMut,
        entry: &TapeEntry,
        baseline: f32,
        fade: f32,
    ) {
        // Every slot has its own column, as wide as its letter, so strokes line up from line
        // to line and nothing overlaps. Extra keys (the join key among them) read first,
        // ahead of the 23 standard slots.
        for (index, column) in self.extra_columns.iter().enumerate() {
            if entry.stroke & Keyboard::extra_bit(index) == 0 {
                continue;
            }
            let letter = Keyboard::extra_label(index);
            let advance = self.strokes.get(letter).advance;
            draw_char(
                pixmap,
                &mut self.strokes,
                letter,
                self.paper_left + column.x + (column.width - advance) / 2.0,
                baseline,
                fade_color(self.theme.strokes, fade),
                self.scale,
            );
        }
        for (index, &slot) in PAPER_ORDER.iter().enumerate() {
            if entry.stroke & (1 << slot) == 0 {
                continue;
            }
            let letter = Keyboard::label(slot);
            let column = self.columns[index];
            let advance = self.strokes.get(letter).advance;
            draw_char(
                pixmap,
                &mut self.strokes,
                letter,
                self.paper_left + column.x + (column.width - advance) / 2.0,
                baseline,
                fade_color(self.theme.strokes, fade),
                self.scale,
            );
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

/// How strong a tape line is, by how far it is from the top of the tape (`0` is the topmost).
/// Only the top few lines fade; every other line is at full strength, so the tape stays easy
/// to read all the way up.
fn line_fade(row_from_top: usize) -> f32 {
    if row_from_top >= FADE_LINES {
        1.0
    } else {
        (row_from_top + 1) as f32 / (FADE_LINES + 1) as f32
    }
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

fn stroke(pixmap: &mut PixmapMut, path: &Path, color: [u8; 4], style: &Stroke) {
    pixmap.stroke_path(path, &paint(color), style, Transform::identity(), None);
}

/// Whether a key's bottom is softened like its top instead of fully round: the top-row finger
/// keys, `TPH` and `FPLTD`.
const fn has_square_bottom(slot: usize) -> bool {
    matches!(slot, 1 | 3 | 5 | 12 | 14 | 16 | 18 | 20)
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
    for (i, line) in lines.iter().enumerate() {
        let mut cx = x.round();
        let line_baseline = pitch.mul_add(i as f32, baseline);
        for c in line.chars() {
            draw_char(pixmap, glyphs, c, cx, line_baseline, color, scale);
            cx += glyphs.get(c).advance.round();
        }
    }
    lines.len()
}

/// Draw one character, with its pen at `x` (rounded to a whole pixel, so it is sharp), with a
/// soft shadow on light text so it reads over any wallpaper.
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
    // Light text gets a soft shadow: the glyph blurred, drawn once underneath. That reads as a
    // gentle edge on light and busy backgrounds; copies of the glyph stamped at whole-pixel
    // offsets looked blocky. Dark text, which is only ever on a lit key, gets none: the key is
    // its own background, and a shadow there would only make it softer.
    let light = u32::from(color[0]) + u32::from(color[1]) + u32::from(color[2]) > 330;
    if light && !glyph.halo.is_empty() {
        let pad = glyphs::HALO_PAD as i32;
        let drop = scale.round().max(1.0) as i32 / 2;
        let shadow = [0, 0, 0, (u16::from(color[3]) * SHADOW_ALPHA / 255) as u8];
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

/// How dark the soft shadow under light text is, out of 255. The blurred mask already fades
/// out, so this is the strength right next to the letter.
const SHADOW_ALPHA: u16 = 200;

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
        assert_eq!(lines.join(" ").split_whitespace().collect::<Vec<_>>().join(" "), "tell me how to do this");

        // A stroke outline breaks at its own "/" separators the same way
        let lines = wrap_text(&mut glyphs, "KAPBD/TKEU/HRAOEUT/SRAOEUT", width);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(lines.concat(), "KAPBD/TKEU/HRAOEUT/SRAOEUT");
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
        snapshot
            .tape
            .push_back(tape_entry(1, "KAT", long_word));
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
        assert_eq!([pixel.red(), pixel.green(), pixel.blue()], [45, 212, 170]);
    }

    #[test]
    fn a_key_beyond_the_standard_23_lights_up_in_its_own_badge() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let bit = Keyboard::extra_bit(0);
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
        // The top-right corner, far from any content, is fully transparent
        assert_eq!(pixmap.pixel(w - 1, 0).unwrap().alpha(), 0);

        let config = OverlayConfig {
            background: 0.5,
            ..OverlayConfig::default()
        };
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let alpha = pixmap.pixel(w - 1, 0).unwrap().alpha();
        assert!((126..=129).contains(&alpha), "{alpha}");
    }

    #[test]
    fn the_tape_runs_the_screen_height_above_the_keyboard() {
        let config = OverlayConfig::default();
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
    fn only_the_top_few_lines_fade_and_every_other_line_is_full_strength() {
        // Rows 0.. are counted from the top of the tape
        let faded: Vec<f32> = (0..FADE_LINES).map(line_fade).collect();
        assert!(faded.windows(2).all(|pair| pair[0] < pair[1]), "{faded:?}");
        assert!(faded[0] > 0.0 && faded[FADE_LINES - 1] < 1.0, "{faded:?}");
        for row in FADE_LINES..200 {
            assert_eq!(line_fade(row), 1.0, "row {row}");
        }
    }

    #[test]
    fn the_tape_actually_fades_the_oldest_lines_at_the_top_not_the_newest_at_the_bottom() {
        // `line_fade` alone (above) can't catch a caller passing it the wrong row index; this
        // draws real tape lines and checks which end of the screen actually comes out dim.
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            ..OverlaySnapshot::default()
        };
        // More than FADE_LINES, oldest (seq 1) first, newest (seq 5) last - a VecDeque's back
        for seq in 1..=5 {
            snapshot
                .tape
                .push_back(tape_entry(seq, "KAT", &format!("\"w{seq}\"")));
        }
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        let words_x = renderer.words_left as u32;
        let row_alpha = |row_top: f32| {
            let y0 = row_top.max(0.0) as u32;
            let y1 = ((row_top + renderer.line_height).ceil() as u32).min(h);
            (y0..y1)
                .flat_map(|y| (words_x..(words_x + 20).min(w)).map(move |x| (x, y)))
                .filter_map(|(x, y)| pixmap.pixel(x, y))
                .map(|p| p.alpha())
                .max()
                .unwrap_or(0)
        };
        let top_row = 5.0f32.mul_add(-renderer.line_height, renderer.tape_bottom); // oldest, seq 1
        let bottom_row = renderer.tape_bottom - renderer.line_height; // newest, seq 5
        assert!(
            row_alpha(top_row) < row_alpha(bottom_row),
            "the oldest line (top, alpha {}) should be dimmer than the newest (bottom, alpha {})",
            row_alpha(top_row),
            row_alpha(bottom_row)
        );
    }

    #[test]
    fn steno_letters_never_overlap_their_neighbours() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let mut strokes = GlyphCache::new(
            glyphs::load_font(None, glyphs::BOLD).unwrap(),
            renderer.strokes.pixel_size(),
        );
        for (pair, columns) in PAPER_ORDER.windows(2).zip(renderer.columns.windows(2)) {
            // Each column starts where the previous one ends
            assert!((columns[0].x + columns[0].width - columns[1].x).abs() < 0.01);
            // And the letter fits inside its own column
            let letter = Keyboard::label(pair[0]);
            assert!(
                strokes.get(letter).advance <= columns[0].width,
                "{letter:?} is wider than its column"
            );
        }
        // W needs more room than T: the columns are not a uniform grid
        let (t, w) = (Keyboard::label(1), Keyboard::label(4));
        assert_eq!((t, w), ('T', 'W'));
        assert!(renderer.columns[5].width > renderer.columns[2].width);
    }

    #[test]
    fn an_extra_key_has_its_own_tape_column_ahead_of_the_standard_slots() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        assert_eq!(renderer.extra_columns.len(), EXTRA_KEYS.len());
        let last_extra = renderer.extra_columns[renderer.extra_columns.len() - 1];
        // Extra columns come first, so the standard slots start right where they end
        assert!((last_extra.x + last_extra.width - renderer.columns[0].x).abs() < 0.01);
        let mut strokes = GlyphCache::new(
            glyphs::load_font(None, glyphs::BOLD).unwrap(),
            renderer.strokes.pixel_size(),
        );
        for (i, column) in renderer.extra_columns.iter().enumerate() {
            let letter = Keyboard::extra_label(i);
            assert!(
                strokes.get(letter).advance <= column.width,
                "{letter:?} is wider than its column"
            );
        }
    }

    #[test]
    fn an_extra_key_on_a_stroke_is_drawn_on_its_own_tape_column() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let kat = crate::steno::layout::parse_stroke("KAT").unwrap();
        let entry = TapeEntry {
            seq: 1,
            stroke: Keyboard::extra_bit(0) | kat,
            text: "\"cat\"".into(),
            unmatched: false,
            repeats: 0,
        };
        let baseline = 100.0;
        renderer.draw_tape_line(&mut pixmap.as_mut(), &entry, baseline, 1.0);

        // Scan the extra key's whole column for any ink at all: the exact glyph shape of `^`
        // isn't worth pinning down pixel-by-pixel, but something must be drawn there.
        let column = renderer.extra_columns[0];
        let x0 = (renderer.paper_left + column.x).max(0.0) as u32;
        let x1 = ((renderer.paper_left + column.x + column.width).ceil() as u32).min(w);
        let y0 = (baseline - renderer.strokes.pixel_size()).max(0.0) as u32;
        let y1 = (baseline as u32 + 2).min(h);
        let lit = (y0..y1)
            .flat_map(|py| (x0..x1).map(move |px| (px, py)))
            .filter_map(|(px, py)| pixmap.pixel(px, py))
            .any(|p| p.alpha() > 0);
        assert!(lit, "the extra key's letter should be drawn somewhere in its tape column");
    }

    #[test]
    fn a_stroke_in_progress_draws_on_the_tape_before_it_is_released() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let t_bit = 1 << PAPER_ORDER[0];

        // Geometry of the tape's newest (bottom-most) line, copied out so sampling doesn't
        // need to borrow `renderer` across the mutable `draw` calls below.
        let paper_left = renderer.paper_left;
        let column = renderer.columns[0];
        let pixel_size = renderer.strokes.pixel_size();
        let baseline_in_line =
            (renderer.line_height - renderer.words.line_height()) / 2.0 + renderer.words.ascent();
        let baseline = (renderer.tape_bottom - renderer.line_height) + baseline_in_line;
        let sample = |pixmap: &Pixmap| {
            let x0 = (paper_left + column.x).max(0.0) as u32;
            let x1 = ((paper_left + column.x + column.width).ceil() as u32).min(w);
            let y0 = (baseline - pixel_size).max(0.0) as u32;
            let y1 = (baseline as u32 + 2).min(h);
            (y0..y1)
                .flat_map(|py| (x0..x1).map(move |px| (px, py)))
                .filter_map(|(px, py)| pixmap.pixel(px, py))
                .any(|p| p.alpha() > 0)
        };

        // Nothing pressed, nothing on the tape yet: the newest line is blank
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        assert!(!sample(&pixmap), "nothing held: the newest line should be blank");

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
    fn tight_steno_letters_are_not_smaller_and_the_words_are_untouched() {
        let renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        // Each column is its letter's width plus only a small gap...
        let mut strokes = GlyphCache::new(
            glyphs::load_font(None, glyphs::BOLD).unwrap(),
            renderer.strokes.pixel_size(),
        );
        for (&slot, column) in PAPER_ORDER.iter().zip(&renderer.columns) {
            let advance = strokes.get(Keyboard::label(slot)).advance;
            let gap = column.width - advance;
            assert!(gap > 0.0, "{slot}: columns must not touch");
            assert!(gap < advance * 0.1, "{slot}: gap {gap} is not tight");
        }
        // ...and the letters are big enough to read, with the paper filling about its share of
        // the width (a little under or over, since the size snaps to whole pixel rows)
        assert!(
            renderer.strokes.pixel_size() >= 14.0,
            "steno letters are {} px, too small",
            renderer.strokes.pixel_size()
        );
        let paper: f32 = renderer.columns.iter().map(|column| column.width).sum();
        let inner = 2.0f32.mul_add(-PAD_X, renderer.size().0 as f32);
        assert!(
            (paper / (PAPER_SHARE * inner) - 1.0).abs() < 0.1,
            "paper is {paper}, wanted about {}",
            PAPER_SHARE * inner
        );
        // The words are not letter-spaced: they advance by each glyph's own advance
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
        let normal = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let big = Renderer::new(
            &OverlayConfig {
                text_scale: 1.25,
                ..OverlayConfig::default()
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
        assert!(top_corner > 0, "top corner is only slightly rounded");
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
        // T P H and F P L T D: the bottom corner is part of the pad, as the top corner is
        for slot in [1, 3, 5, 12, 14, 16, 18, 20] {
            assert!(alpha_at(slot, keys[slot].bottom() - 3.0) > 0, "slot {slot}");
        }
        // The rest keep their round ends: K W R, -R -B -G -S -Z, and the tall S and *
        for slot in [0, 2, 4, 6, 9, 13, 15, 17, 19, 21] {
            assert_eq!(alpha_at(slot, keys[slot].bottom() - 3.0), 0, "slot {slot}");
        }
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
            mode: LookupMode::Word,
        }
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
            (words_x..words_x + 40)
                .flat_map(|x| {
                    let p = pixmap.pixel(x, row + 6).unwrap();
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
            pixel.green() > pixel.red(),
            "accent colour, not the unlit key"
        );

        // Closing the lookup brings the tape back exactly as it was
        with_lookup.lookup = None;
        let mut after = Pixmap::new(w, h).unwrap();
        renderer.draw(&with_lookup, &mut after.as_mut());
        assert_eq!(tape_pixels.data(), after.data());
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
                    [p.red(), p.green(), p.blue(), p.alpha()]
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
                        text: "candy".into(),
                        outline: "KAPBD".into(),
                    },
                ],
                hint: crate::steno::layout::parse_stroke("KAPBD").unwrap(),
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
        let snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            lookup: Some(LookupView {
                query: "KAPBD/TKEU".into(),
                rows: vec![LookupRow {
                    text: "\u{2192} \"candy\"".into(),
                    outline: "KAPBD/TKEU".into(),
                }],
                hint: crate::steno::layout::parse_stroke("KAPBD").unwrap(),
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
}
