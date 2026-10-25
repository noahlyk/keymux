//! Text for the overlay. Atkinson Hyperlegible is embedded, the font the steno-lessons site
//! uses for keys: it was designed so letters that are easy to mix up stay distinct.
//!
//! Each glyph is rasterized once per size into an alpha mask and kept. The overlay only ever
//! draws a few dozen distinct characters, so after the first frames text is a mask copy.

use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;

pub const REGULAR: &[u8] = include_bytes!("../../assets/fonts/AtkinsonHyperlegible-Regular.ttf");
pub const BOLD: &[u8] = include_bytes!("../../assets/fonts/AtkinsonHyperlegible-Bold.ttf");

/// The embedded regular face, or the user's font file from `overlay: (font: Some(..))`.
pub fn load_font(path: Option<&str>, embedded: &'static [u8]) -> Result<FontArc> {
    match path {
        Some(path) => {
            let bytes = std::fs::read(path).with_context(|| format!("overlay font {path}"))?;
            FontArc::try_from_vec(bytes).with_context(|| format!("overlay font {path}"))
        }
        None => Ok(FontArc::try_from_slice(embedded)?),
    }
}

/// One rasterized character: an alpha mask and where to put it relative to the pen.
#[derive(Debug, Default)]
pub struct Glyph {
    pub width: u32,
    pub height: u32,
    /// Offset from the pen position to the mask's top-left corner
    pub left: i32,
    pub top: i32,
    /// How far the pen moves after this character
    pub advance: f32,
    /// `width * height` coverage values, row by row
    pub mask: Box<[u8]>,
}

/// A font at one pixel size, with every character drawn so far.
pub struct GlyphCache {
    font: FontArc,
    scale: PxScale,
    /// ASCII by index: no hashing for the characters steno actually uses
    ascii: [Option<Glyph>; 128],
    other: FxHashMap<char, Glyph>,
}

impl GlyphCache {
    #[must_use]
    pub fn new(font: FontArc, px: f32) -> Self {
        Self {
            font,
            scale: PxScale::from(px),
            ascii: std::array::from_fn(|_| None),
            other: FxHashMap::default(),
        }
    }

    /// Distance from the top of a line to its baseline.
    #[must_use]
    pub fn ascent(&self) -> f32 {
        self.font.as_scaled(self.scale).ascent()
    }

    /// Height of a line, baseline to baseline.
    #[must_use]
    pub fn line_height(&self) -> f32 {
        let scaled = self.font.as_scaled(self.scale);
        scaled.height() + scaled.line_gap()
    }

    /// The glyph for `c`, rasterizing it the first time.
    pub fn get(&mut self, c: char) -> &Glyph {
        let (font, scale) = (&self.font, self.scale);
        match usize::try_from(u32::from(c)).ok().filter(|&i| i < 128) {
            Some(i) => self.ascii[i].get_or_insert_with(|| rasterize(font, scale, c)),
            None => self
                .other
                .entry(c)
                .or_insert_with(|| rasterize(font, scale, c)),
        }
    }

    /// Width of `text` in pixels.
    pub fn measure(&mut self, text: &str) -> f32 {
        text.chars().map(|c| self.get(c).advance).sum()
    }
}

fn rasterize(font: &FontArc, scale: PxScale, c: char) -> Glyph {
    let id = font.glyph_id(c);
    let advance = font.as_scaled(scale).h_advance(id);
    let Some(outline) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, 0.0)))
    else {
        // Space and other blank characters: nothing to draw, only the advance
        return Glyph {
            advance,
            ..Glyph::default()
        };
    };
    let bounds = outline.px_bounds();
    let (width, height) = (bounds.width() as u32, bounds.height() as u32);
    let mut mask = vec![0u8; (width * height) as usize].into_boxed_slice();
    outline.draw(|x, y, coverage| {
        if x < width && y < height {
            mask[(y * width + x) as usize] = (coverage.clamp(0.0, 1.0) * 255.0) as u8;
        }
    });
    Glyph {
        width,
        height,
        left: bounds.min.x as i32,
        top: bounds.min.y as i32,
        advance,
        mask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_fonts_load_and_draw_steno_letters() {
        for face in [REGULAR, BOLD] {
            let mut cache = GlyphCache::new(load_font(None, face).unwrap(), 16.0);
            for c in "#STKPWHRAO*EUFRPBLGTSDZ".chars() {
                let glyph = cache.get(c);
                assert!(
                    glyph.width > 0 && glyph.mask.iter().any(|&a| a > 0),
                    "{c:?} is blank"
                );
            }
            assert_eq!(cache.get(' ').width, 0);
            assert!(cache.measure("STKPW") > 0.0);
        }
    }
}
