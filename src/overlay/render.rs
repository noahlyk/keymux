//! Draws a snapshot into pixels: the tape running down the screen and the steno keyboard at
//! the bottom, on a fully transparent background (unless `background` is set). A pure
//! function of (snapshot, config, scale, height), so it is testable without a compositor.
//!
//! Everything that depends only on size (key shapes, glyph masks, how many tape lines fit) is
//! built once in [`Renderer::new`]. A frame is a handful of fills and mask copies.

use super::geometry::{Keyboard, Rect};
use super::glyphs::{self, Glyph, GlyphCache};
use crate::config::OverlayConfig;
use crate::steno::overlay::{OverlaySnapshot, TapeEntry, TAPE_CAPACITY};
use anyhow::Result;
use std::borrow::Cow;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform};

/// Space between the content and the left/right edge of the overlay, logical px
const PAD_X: f32 = 8.0;
/// Space above the tape and below the keyboard, logical px
const PAD_Y: f32 = 12.0;
/// Space between the tape and the keyboard, logical px
const SECTION_GAP: f32 = 18.0;
const LABEL_PX: f32 = 13.0;
const TAPE_PX: f32 = 14.0;
/// Outline of an unlit key, logical px
const OUTLINE: f32 = 1.4;
/// Radius of a pad's top corners relative to its width. The bottom is fully round.
const PAD_TOP_ROUND: f32 = 0.2;
/// Slot of the `#` number bar
const NUMBER_SLOT: usize = 22;
/// Share of the tape width given to the steno columns; the translation gets the rest
const PAPER_SHARE: f32 = 0.62;
/// Oldest tape line's strength, so the tape fades out towards the top of the screen
const OLDEST_FADE: f32 = 0.18;
/// Plover paper tape order: `#` first, then the slots in stroke order
const PAPER_ORDER: [usize; 23] = [
    22, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
];

/// Colours. With no background, everything has to read over any wallpaper: unlit keys are
/// a faint dark glass pad with a light outline, and all text gets a soft drop shadow.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub panel: [u8; 3],
    pub key: [u8; 4],
    pub outline: [u8; 4],
    pub label: [u8; 4],
    pub label_lit: [u8; 4],
    pub consonant: [u8; 4],
    pub vowel: [u8; 4],
    pub tape: [u8; 4],
    pub translation: [u8; 4],
    pub unmatched: [u8; 4],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            panel: [10, 12, 16],
            key: [14, 18, 26, 120],
            outline: [214, 224, 238, 95],
            label: [214, 222, 236, 240],
            label_lit: [10, 14, 18, 255],
            consonant: [45, 212, 170, 255],
            vowel: [232, 226, 150, 255],
            tape: [226, 232, 242, 255],
            translation: [45, 212, 170, 255],
            unmatched: [240, 96, 96, 255],
        }
    }
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
    /// One pill per key, indexed by slot, built once
    key_shapes: Vec<Path>,
    /// Bottom of the tape area, line pitch and how many lines fit, physical px
    tape_bottom: f32,
    line_height: f32,
    tape_lines: usize,
    labels: GlyphCache,
    tape: GlyphCache,
}

impl Renderer {
    /// Lay the overlay out for `config` on a surface `height` logical pixels tall, at an
    /// integer output `scale`. The tape takes all the height the keyboard leaves.
    pub fn new(config: &OverlayConfig, scale: u32, height: u32) -> Result<Self> {
        let scale = scale.max(1) as f32;
        let font = config.font.as_deref();
        let labels = GlyphCache::new(glyphs::load_font(font, glyphs::BOLD)?, LABEL_PX * scale);
        let tape = GlyphCache::new(glyphs::load_font(font, glyphs::REGULAR)?, TAPE_PX * scale);

        let width = (config.width as f32 * scale).ceil();
        let height = (height.max(1) as f32 * scale).ceil();
        let (pad_x, pad_y) = (PAD_X * scale, PAD_Y * scale);
        let line_height = tape.line_height().ceil();

        // The keyboard sits at the bottom. Laying it out once at the origin gives its height.
        let keyboard = config.keyboard.then(|| {
            let keys_width = width - 2.0 * pad_x;
            let keys_height = Keyboard::new(0.0, 0.0, keys_width).size.1;
            Keyboard::new(pad_x, height - pad_y - keys_height, keys_width)
        });
        let tape_bottom = match &keyboard {
            Some(k) => k.keys[22].y - SECTION_GAP * scale,
            None => height - pad_y,
        };
        let fits = ((tape_bottom - pad_y) / line_height).floor().max(0.0) as usize;
        let tape_lines = if config.tape {
            fits.min(config.tape_lines.unwrap_or(usize::MAX))
                .min(TAPE_CAPACITY)
        } else {
            0
        };

        // Steno pads: a squarish top with slightly softened corners, and a fully round
        // bottom. The `#` bar is a plain pill. Radii are clamped to fit by `rounded_rect`.
        let key_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            k.keys
                .iter()
                .enumerate()
                .filter_map(|(slot, r)| {
                    if slot == NUMBER_SLOT {
                        rounded_rect(*r, f32::MAX, f32::MAX)
                    } else {
                        rounded_rect(*r, r.w * PAD_TOP_ROUND, f32::MAX)
                    }
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
            theme: Theme::default(),
            scale,
            size: (width as u32, height as u32),
            panel,
            keyboard,
            key_shapes,
            tape_bottom,
            line_height,
            tape_lines,
            labels,
            tape,
        })
    }

    /// Buffer size in physical pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// How many tape lines fit above the keyboard.
    #[must_use]
    pub fn tape_lines(&self) -> usize {
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
        if self.tape_lines > 0 {
            self.draw_tape(snapshot, pixmap);
        }
        if self.keyboard.is_some() {
            self.draw_keyboard(snapshot, pixmap);
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
        for (slot, shape) in self.key_shapes.iter().enumerate() {
            let bit = 1u32 << slot;
            let accent = if Keyboard::is_vowel(slot) {
                self.theme.vowel
            } else {
                self.theme.consonant
            };
            // Held keys full strength; keys already let go but still in the stroke dimmer,
            // so you can watch the stroke build up
            let label_color = if snapshot.held & bit != 0 {
                fill(pixmap, shape, accent);
                self.theme.label_lit
            } else if snapshot.chord & bit != 0 {
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
            let baseline = rect.y + (rect.h + self.labels.ascent() * 0.72) / 2.0;
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
        let pad = PAD_X * self.scale;
        let inner = self.size.0 as f32 - 2.0 * pad;
        let paper = inner * PAPER_SHARE;
        let column = paper / 23.0;
        let lines = self.tape_lines;
        let ascent = self.tape.ascent();

        // Newest at the bottom, just above the keyboard; older lines fade out upwards
        for (age, entry) in snapshot.tape.iter().rev().take(lines).enumerate() {
            let baseline = self.tape_bottom - (age + 1) as f32 * self.line_height + ascent;
            let fade = 1.0 - (1.0 - OLDEST_FADE) * age as f32 / lines as f32;
            self.draw_tape_line(pixmap, entry, (pad, baseline), (column, paper, inner), fade);
        }
    }

    fn draw_tape_line(
        &mut self,
        pixmap: &mut PixmapMut,
        entry: &TapeEntry,
        (pad, baseline): (f32, f32),
        (column, paper, inner): (f32, f32, f32),
        fade: f32,
    ) {
        // Steno columns: every slot has its own column, so lines stay aligned even though
        // Atkinson Hyperlegible is proportional
        for (column_index, &slot) in PAPER_ORDER.iter().enumerate() {
            if entry.stroke & (1 << slot) == 0 {
                continue;
            }
            let letter = Keyboard::label(slot);
            let x = pad + column * column_index as f32;
            let centred = x + (column - self.tape.get(letter).advance) / 2.0;
            let color = fade_color(self.theme.tape, fade);
            draw_char(
                pixmap,
                &mut self.tape,
                letter,
                centred,
                baseline,
                color,
                self.scale,
            );
        }

        let (text, color) = if entry.unmatched {
            (Cow::Borrowed("?"), self.theme.unmatched)
        } else {
            (display_text(&entry.text), self.theme.translation)
        };
        draw_text_clipped(
            pixmap,
            &mut self.tape,
            &text,
            pad + paper + 6.0 * self.scale,
            baseline,
            pad + inner,
            fade_color(color, fade),
            self.scale,
        );
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
#[allow(clippy::too_many_arguments)]
fn draw_text_clipped(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    text: &str,
    mut x: f32,
    baseline: f32,
    right: f32,
    color: [u8; 4],
    scale: f32,
) {
    let ellipsis = glyphs.get('…').advance;
    let fits = glyphs.measure(text) <= right - x;
    for c in text.chars() {
        let advance = glyphs.get(c).advance;
        if !fits && x + advance > right - ellipsis {
            draw_char(pixmap, glyphs, '…', x, baseline, color, scale);
            return;
        }
        draw_char(pixmap, glyphs, c, x, baseline, color, scale);
        x += advance;
    }
}

/// Draw one character with a soft drop shadow, so it reads over any wallpaper.
fn draw_char(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    c: char,
    x: f32,
    baseline: f32,
    color: [u8; 4],
    scale: f32,
) {
    let glyph = glyphs.get(c);
    let (x0, y0) = (
        x.round() as i32 + glyph.left,
        baseline.round() as i32 + glyph.top,
    );
    // Light text needs the shadow to stay readable on a light wallpaper. Dark text sits on a
    // lit key already, where a black shadow would only smudge it.
    let light = u32::from(color[0]) + u32::from(color[1]) + u32::from(color[2]) > 330;
    if light {
        let shift = scale.round().max(1.0) as i32;
        blend_glyph(
            pixmap,
            glyph,
            (x0 + shift, y0 + shift),
            [0, 0, 0, (u16::from(color[3]) * 7 / 10) as u8],
        );
    }
    blend_glyph(pixmap, glyph, (x0, y0), color);
}

/// Composite one glyph mask onto the pixmap (source-over, premultiplied RGBA).
fn blend_glyph(pixmap: &mut PixmapMut, glyph: &Glyph, (x0, y0): (i32, i32), [r, g, b, a]: [u8; 4]) {
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for gy in 0..glyph.height as i32 {
        let py = y0 + gy;
        if py < 0 || py >= height {
            continue;
        }
        for gx in 0..glyph.width as i32 {
            let px = x0 + gx;
            if px < 0 || px >= width {
                continue;
            }
            let coverage = u32::from(glyph.mask[(gy * glyph.width as i32 + gx) as usize]);
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
        }
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
        assert!(short.tape_lines() > 10);

        // Every line, and the keyboard below it, stays inside the surface
        let top = tall.tape_bottom - tall.tape_lines() as f32 * tall.line_height;
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
    fn keys_are_pads_square_on_top_and_round_at_the_bottom() {
        let mut renderer = Renderer::new(&OverlayConfig::default(), 1, 900).unwrap();
        let (w, h) = renderer.size();
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&OverlaySnapshot::default(), &mut pixmap.as_mut());
        let t = renderer.keyboard.as_ref().unwrap().keys[1];
        let alpha_at = |dx: f32, y: f32| pixmap.pixel((t.x + dx) as u32, y as u32).unwrap().alpha();

        // Just inside each corner of T-'s bounding box: the top is nearly square, so that
        // spot is part of the pad; the bottom is fully round, so it is empty
        let top_corner = alpha_at(2.0, t.y + 2.0);
        let bottom_corner = alpha_at(2.0, t.bottom() - 3.0);
        assert!(top_corner > 0, "top corner is only slightly rounded");
        assert_eq!(bottom_corner, 0, "bottom is a round end");
        assert!(alpha_at(2.0, t.y + t.h / 2.0) > 0, "body of the pad");
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
            chord: parse_stroke("TPHE").unwrap(),
            held: parse_stroke("PH").unwrap(),
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
        for (i, px) in pixmap.data().chunks_exact(4).enumerate() {
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

    #[test]
    fn translations_drop_the_tape_quotes() {
        assert_eq!(display_text("\"the\""), "the");
        assert_eq!(display_text("-3 \"the\""), "-3 the");
        assert_eq!(display_text("KC_A KC_B"), "KC_A KC_B");
    }
}
