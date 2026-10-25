//! Draws a snapshot into pixels: a translucent rounded panel with the tape on top and the
//! steno keyboard below it. A pure function of (snapshot, config, scale), so it is testable
//! without a compositor.
//!
//! Everything that depends only on size (key shapes, the panel shape, glyph masks) is built
//! once in [`Renderer::new`]. A frame is a handful of fills and mask copies.

use super::geometry::{Keyboard, Rect};
use super::glyphs::{self, GlyphCache};
use crate::config::OverlayConfig;
use crate::steno::overlay::{OverlaySnapshot, TapeEntry};
use anyhow::Result;
use std::borrow::Cow;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, PixmapMut, Transform};

/// Space inside the panel edge, logical px
const PADDING: f32 = 14.0;
/// Space between the tape and the keyboard, logical px
const SECTION_GAP: f32 = 14.0;
const PANEL_RADIUS: f32 = 14.0;
const KEY_RADIUS: f32 = 6.0;
const LABEL_PX: f32 = 13.0;
const TAPE_PX: f32 = 14.0;
/// Share of the tape width given to the steno columns; the translation gets the rest
const PAPER_SHARE: f32 = 0.62;
/// Plover paper tape order: `#` first, then the slots in stroke order
const PAPER_ORDER: [usize; 23] = [
    22, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
];

/// Colours, as in the mockup: near-black glass, slate keys, teal consonants, pale-yellow vowels.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub panel: [u8; 3],
    pub key: [u8; 4],
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
            key: [38, 44, 56, 235],
            label: [150, 160, 176, 255],
            label_lit: [12, 16, 20, 255],
            consonant: [45, 212, 170, 255],
            vowel: [232, 226, 150, 255],
            tape: [205, 212, 222, 255],
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
    panel: Path,
    keyboard: Option<Keyboard>,
    /// One shape per key, indexed by slot, built once
    key_shapes: Vec<Path>,
    /// Top of the tape area and its line pitch, physical px
    tape_top: f32,
    line_height: f32,
    labels: GlyphCache,
    tape: GlyphCache,
}

impl Renderer {
    /// Lay out the panel for `config` at an integer output `scale`.
    pub fn new(config: &OverlayConfig, scale: u32) -> Result<Self> {
        let scale = scale.max(1) as f32;
        let font = config.font.as_deref();
        let labels = GlyphCache::new(glyphs::load_font(font, glyphs::BOLD)?, LABEL_PX * scale);
        let tape = GlyphCache::new(glyphs::load_font(font, glyphs::REGULAR)?, TAPE_PX * scale);

        let width = config.width as f32 * scale;
        let pad = PADDING * scale;
        let line_height = tape.line_height().ceil();
        let tape_height = if config.tape {
            line_height * config.tape_lines as f32
        } else {
            0.0
        };
        let tape_top = pad;
        let keyboard = config.keyboard.then(|| {
            let gap = if config.tape {
                SECTION_GAP * scale
            } else {
                0.0
            };
            Keyboard::new(pad, tape_top + tape_height + gap, width - 2.0 * pad)
        });
        let content_bottom = keyboard.as_ref().map_or(tape_top + tape_height, |k| {
            k.keys.iter().map(Rect::bottom).fold(0.0, f32::max)
        });
        let height = (content_bottom + pad).ceil();

        let key_shapes = keyboard.as_ref().map_or_else(Vec::new, |k| {
            k.keys
                .iter()
                .filter_map(|r| rounded_rect(*r, KEY_RADIUS * scale))
                .collect()
        });
        let panel = rounded_rect(
            Rect {
                x: 0.0,
                y: 0.0,
                w: width,
                h: height,
            },
            PANEL_RADIUS * scale,
        )
        .expect("panel has a size");

        Ok(Self {
            config: config.clone(),
            theme: Theme::default(),
            scale,
            size: (width.ceil() as u32, height as u32),
            panel,
            keyboard,
            key_shapes,
            tape_top,
            line_height,
            labels,
            tape,
        })
    }

    /// Buffer size in physical pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Surface size in logical pixels, for `layer_surface.set_size`.
    #[must_use]
    pub fn logical_size(&self) -> (u32, u32) {
        let (w, h) = self.size;
        (
            (w as f32 / self.scale) as u32,
            (h as f32 / self.scale) as u32,
        )
    }

    /// Draw the whole panel. `pixmap` must be [`Self::size`].
    ///
    /// TODO(overlay): partial redraw. Compare with the last drawn snapshot and repaint only
    /// the keyboard when only keys changed (the common case while writing a stroke), and
    /// return the rect to `damage_buffer`.
    pub fn draw(&mut self, snapshot: &OverlaySnapshot, pixmap: &mut PixmapMut) {
        pixmap.fill(Color::TRANSPARENT);
        let [r, g, b] = self.theme.panel;
        let alpha = (self.config.opacity.clamp(0.0, 1.0) * 255.0) as u8;
        fill(pixmap, &self.panel, [r, g, b, alpha]);

        if self.config.tape {
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
        for (slot, shape) in self.key_shapes.iter().enumerate() {
            let bit = 1u32 << slot;
            let accent = if Keyboard::is_vowel(slot) {
                self.theme.vowel
            } else {
                self.theme.consonant
            };
            // Held keys full strength; keys already let go but still in the stroke dimmer,
            // so you can watch the stroke build up
            let (fill_color, label_color) = if snapshot.held & bit != 0 {
                (accent, self.theme.label_lit)
            } else if snapshot.chord & bit != 0 {
                (with_alpha(accent, 150), self.theme.label_lit)
            } else {
                (self.theme.key, self.theme.label)
            };
            fill(pixmap, shape, fill_color);

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
            );
        }
    }

    fn draw_tape(&mut self, snapshot: &OverlaySnapshot, pixmap: &mut PixmapMut) {
        let pad = PADDING * self.scale;
        let inner = self.size.0 as f32 - 2.0 * pad;
        let paper = inner * PAPER_SHARE;
        let column = paper / 23.0;
        let lines = self.config.tape_lines;
        let shown: Vec<&TapeEntry> = snapshot.tape.iter().rev().take(lines).collect();
        let ascent = self.tape.ascent();

        // Newest at the bottom; older lines fade out towards the top
        for (age, entry) in shown.iter().enumerate() {
            let row = lines - 1 - age;
            let baseline = self.tape_top + row as f32 * self.line_height + ascent;
            let fade = 1.0 - age as f32 / lines as f32 * 0.75;

            // Steno columns: every slot has its own column, so lines stay aligned even
            // though Atkinson Hyperlegible is proportional
            for (column_index, &slot) in PAPER_ORDER.iter().enumerate() {
                if entry.stroke & (1 << slot) == 0 {
                    continue;
                }
                let letter = Keyboard::label(slot);
                let x = pad + column * column_index as f32;
                let centred = x + (column - self.tape.get(letter).advance) / 2.0;
                let color = fade_color(self.theme.tape, fade);
                draw_char(pixmap, &mut self.tape, letter, centred, baseline, color);
            }

            let (text, color) = if entry.unmatched {
                (Cow::Borrowed("?"), self.theme.unmatched)
            } else {
                (display_text(&entry.text), self.theme.translation)
            };
            let x = pad + paper + 6.0 * self.scale;
            draw_text_clipped(
                pixmap,
                &mut self.tape,
                &text,
                x,
                baseline,
                pad + inner,
                fade_color(color, fade),
            );
        }
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

fn fill(pixmap: &mut PixmapMut, path: &Path, [r, g, b, a]: [u8; 4]) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// A rectangle with circular corners (cubic approximation).
fn rounded_rect(r: Rect, radius: f32) -> Option<Path> {
    const K: f32 = 0.552_284_8; // control point distance for a quarter circle
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0);
    let c = rad * (1.0 - K);
    let (left, top, right, bottom) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let mut pb = PathBuilder::new();
    pb.move_to(left + rad, top);
    pb.line_to(right - rad, top);
    pb.cubic_to(right - c, top, right, top + c, right, top + rad);
    pb.line_to(right, bottom - rad);
    pb.cubic_to(right, bottom - c, right - c, bottom, right - rad, bottom);
    pb.line_to(left + rad, bottom);
    pb.cubic_to(left + c, bottom, left, bottom - c, left, bottom - rad);
    pb.line_to(left, top + rad);
    pb.cubic_to(left, top + c, left + c, top, left + rad, top);
    pb.close();
    pb.finish()
}

/// Draw `text` from `x`, stopping with an ellipsis before it would pass `right`.
fn draw_text_clipped(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    text: &str,
    mut x: f32,
    baseline: f32,
    right: f32,
    color: [u8; 4],
) {
    let ellipsis = glyphs.get('…').advance;
    let fits = glyphs.measure(text) <= right - x;
    for c in text.chars() {
        let advance = glyphs.get(c).advance;
        if !fits && x + advance > right - ellipsis {
            draw_char(pixmap, glyphs, '…', x, baseline, color);
            return;
        }
        draw_char(pixmap, glyphs, c, x, baseline, color);
        x += advance;
    }
}

/// Composite one glyph mask onto the pixmap (source-over, premultiplied RGBA).
fn draw_char(
    pixmap: &mut PixmapMut,
    glyphs: &mut GlyphCache,
    c: char,
    x: f32,
    baseline: f32,
    [r, g, b, a]: [u8; 4],
) {
    let glyph = glyphs.get(c);
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let x0 = x.round() as i32 + glyph.left;
    let y0 = baseline.round() as i32 + glyph.top;
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

    #[test]
    fn a_frame_draws_without_wayland_and_lights_held_keys() {
        let config = OverlayConfig::default();
        let mut renderer = Renderer::new(&config, 1).unwrap();
        let (w, h) = renderer.size();
        assert_eq!(w, config.width);
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            held: 1 << 1, // T-
            chord: 1 << 1,
            ..OverlaySnapshot::default()
        };
        snapshot.tape.push_back(TapeEntry {
            seq: 1,
            stroke: 0b11,
            text: "\"test\"".into(),
            unmatched: false,
        });
        renderer.draw(&snapshot, &mut pixmap.as_mut());

        // T- is the consonant accent: sampled at its left edge, mid-height, clear of the
        // rounded corners and the centred label
        let t = renderer.keyboard.as_ref().unwrap().keys[1];
        let pixel = pixmap
            .pixel((t.x + 2.0) as u32, (t.y + t.h / 2.0) as u32)
            .unwrap()
            .demultiply();
        assert_eq!([pixel.red(), pixel.green(), pixel.blue()], [45, 212, 170]);
    }

    /// Writes a sample frame as a PPM over a mid-grey backdrop, to look at the design:
    /// `OVERLAY_PREVIEW=/tmp/overlay.ppm cargo test preview -- --ignored`
    #[test]
    #[ignore = "writes an image for eyeballing"]
    fn preview() {
        use crate::steno::layout::parse_stroke;
        let path = std::env::var("OVERLAY_PREVIEW").expect("set OVERLAY_PREVIEW");
        let mut renderer = Renderer::new(&OverlayConfig::default(), 2).unwrap();
        let (w, h) = renderer.size();
        let mut snapshot = OverlaySnapshot {
            layer: Some("steno".into()),
            chord: parse_stroke("TPHE").unwrap(),
            held: parse_stroke("PH").unwrap(),
            ..OverlaySnapshot::default()
        };
        let lines = [
            ("TH", "\"this\""),
            ("S", "\"is\""),
            ("A", "\"a\""),
            ("TEFT", "\"test\""),
            ("OF", "\"of\""),
            ("-T", "\"the\""),
            ("STEPB", "\"steno\""),
            ("OEFR", "\"over\""),
            ("HRAEU", "\"lay\""),
            ("STKPWHR", ""),
            ("PWEUG", "\"big\""),
            ("TKPWAEUPL", "\"game\""),
            ("KWRAOU", "\"you\""),
            ("KAPB", "\"can\""),
            ("SAOE", "\"see\""),
            ("-PBT", "\"in\""),
            ("TPAOEL", "\"feel\""),
            ("TKPWAOD", "\"good\""),
        ];
        for (i, (stroke, text)) in lines.iter().enumerate() {
            snapshot.tape.push_back(TapeEntry {
                seq: i as u64 + 1,
                stroke: parse_stroke(stroke).unwrap(),
                text: (*text).into(),
                unmatched: text.is_empty(),
            });
        }
        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.draw(&snapshot, &mut pixmap.as_mut());
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for (i, px) in pixmap.data().chunks_exact(4).enumerate() {
            // Premultiplied over a backdrop gradient, like a wallpaper behind the glass
            let shade = 70 + ((i as u32 / w) * 60 / h) as u8;
            for c in &px[..3] {
                out.push(c + (u32::from(shade) * (255 - u32::from(px[3])) / 255) as u8);
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
