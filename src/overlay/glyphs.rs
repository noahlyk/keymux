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
    /// A blurred copy of the mask for a soft shadow: `HALO_PAD` pixels bigger on every side, so
    /// `(width + 2 * HALO_PAD) * (height + 2 * HALO_PAD)` values. Empty for blank glyphs.
    pub halo: Box<[u8]>,
}

/// How far the soft shadow reaches past the glyph, in pixels. One pass of a 3-wide blur spreads
/// coverage one pixel out: a tight shadow, so the letter itself stays crisp.
pub const HALO_PAD: u32 = 1;

/// The pixel size nearest `px` at which the height of `reference` is a whole number of pixels.
///
/// The text isn't hinted, so a letter's flat top and bottom (`E`, `T`, `x`) only land cleanly on
/// pixel rows when its height is a whole number of pixels. At an arbitrary size such as 12.6
/// they straddle two rows and each edge smears into a soft grey line. Use `H` for capitals and
/// `x` for lowercase. Draw at a whole-pixel baseline.
#[must_use]
pub fn crisp_size(font: &FontArc, px: f32, reference: char) -> f32 {
    // Measure at a large size, where the whole-pixel bounds the library reports are accurate
    // to a fraction of a percent
    const PROBE: f32 = 1000.0;
    let id = font.glyph_id(reference);
    let Some(outline) =
        font.outline_glyph(id.with_scale_and_position(PxScale::from(PROBE), point(0.0, 0.0)))
    else {
        return px;
    };
    let ratio = outline.px_bounds().height() / PROBE;
    if ratio <= 0.0 {
        return px;
    }
    let rows = (ratio * px).round().max(1.0);
    rows / ratio
}

/// How many horizontal sub-pixel positions each glyph is drawn at. Snapping every letter to a
/// whole pixel makes spacing visibly uneven at small sizes ("thi s"); quarter-pixel steps are
/// below what the eye sees.
const SUBPIXELS: usize = 4;

/// A font at one pixel size, with every character drawn so far at each sub-pixel offset.
pub struct GlyphCache {
    font: FontArc,
    scale: PxScale,
    /// ASCII by `bucket * 128 + char`: no hashing for the characters steno actually uses
    ascii: Vec<Option<Glyph>>,
    other: FxHashMap<(char, usize), Glyph>,
}

impl GlyphCache {
    #[must_use]
    pub fn new(font: FontArc, px: f32) -> Self {
        Self {
            font,
            scale: PxScale::from(px),
            ascii: (0..128 * SUBPIXELS).map(|_| None).collect(),
            other: FxHashMap::default(),
        }
    }

    /// The size this cache draws at, in pixels.
    #[must_use]
    pub fn pixel_size(&self) -> f32 {
        self.scale.y
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

    /// The glyph for `c` on a whole-pixel position, rasterizing it the first time. Use this to
    /// measure; to draw, use [`Self::place`].
    pub fn get(&mut self, c: char) -> &Glyph {
        self.bucket(c, 0)
    }

    /// The glyph for `c` drawn with its pen at horizontal position `x`, and the whole-pixel x
    /// to put its mask at (add `glyph.left`). The fractional part of `x` picks one of
    /// [`SUBPIXELS`] pre-drawn variants.
    pub fn place(&mut self, c: char, x: f32) -> (&Glyph, i32) {
        let whole = x.floor();
        let mut bucket = ((x - whole) * SUBPIXELS as f32).round() as usize;
        let mut whole = whole as i32;
        if bucket == SUBPIXELS {
            bucket = 0;
            whole += 1;
        }
        (self.bucket(c, bucket), whole)
    }

    fn bucket(&mut self, c: char, bucket: usize) -> &Glyph {
        let (font, scale) = (&self.font, self.scale);
        let shift = bucket as f32 / SUBPIXELS as f32;
        match usize::try_from(u32::from(c)).ok().filter(|&i| i < 128) {
            Some(i) => {
                self.ascii[bucket * 128 + i].get_or_insert_with(|| rasterize(font, scale, c, shift))
            }
            None => self
                .other
                .entry((c, bucket))
                .or_insert_with(|| rasterize(font, scale, c, shift)),
        }
    }

    /// Width of `text` in pixels.
    pub fn measure(&mut self, text: &str) -> f32 {
        text.chars().map(|c| self.get(c).advance).sum()
    }
}

/// Draw `c` with its pen `shift` pixels (0 to under 1) to the right of a whole-pixel position.
fn rasterize(font: &FontArc, scale: PxScale, c: char, shift: f32) -> Glyph {
    let id = font.glyph_id(c);
    let advance = font.as_scaled(scale).h_advance(id);
    let Some(outline) = font.outline_glyph(id.with_scale_and_position(scale, point(shift, 0.0)))
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
    let halo = blur(&mask, width, height);
    Glyph {
        width,
        height,
        left: bounds.min.x as i32,
        top: bounds.min.y as i32,
        advance,
        mask,
        halo,
    }
}

/// A soft shadow for a coverage mask: the mask padded by [`HALO_PAD`] on every side and blurred
/// with a single pass of a 3 x 3 box, then strengthened so the glow stays dark right next to
/// the letter. A hard shadow made of offset copies looks blocky; a blur reads as a gentle edge
/// on any background. It reaches only one pixel, because a wide dark haze makes small text look
/// fuzzy.
fn blur(mask: &[u8], width: u32, height: u32) -> Box<[u8]> {
    let pad = HALO_PAD as usize;
    let (w, h) = (width as usize + 2 * pad, height as usize + 2 * pad);
    let mut current = vec![0f32; w * h];
    for y in 0..height as usize {
        for x in 0..width as usize {
            current[(y + pad) * w + x + pad] = f32::from(mask[y * width as usize + x]);
        }
    }
    let mut next = vec![0f32; w * h];
    for _ in 0..1 {
        // Horizontal then vertical: a 3 x 3 box is the two 3-wide averages one after the other
        for y in 0..h {
            for x in 0..w {
                let at = |dx: isize| {
                    let nx = x as isize + dx;
                    if nx < 0 || nx >= w as isize {
                        0.0
                    } else {
                        current[y * w + nx as usize]
                    }
                };
                next[y * w + x] = (at(-1) + at(0) + at(1)) / 3.0;
            }
        }
        for y in 0..h {
            for x in 0..w {
                let at = |dy: isize| {
                    let ny = y as isize + dy;
                    if ny < 0 || ny >= h as isize {
                        0.0
                    } else {
                        next[ny as usize * w + x]
                    }
                };
                current[y * w + x] = (at(-1) + at(0) + at(1)) / 3.0;
            }
        }
    }
    current
        .iter()
        .map(|&v| (v * HALO_STRENGTH).min(255.0) as u8)
        .collect()
}

/// How much the blurred mask is boosted. A plain blur is too faint to separate text from a busy
/// background; this keeps it dark next to the glyph while still fading smoothly outward.
const HALO_STRENGTH: f32 = 1.7;

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

    #[test]
    fn the_halo_is_a_soft_blur_around_the_glyph() {
        let mut cache = GlyphCache::new(load_font(None, BOLD).unwrap(), 16.0);
        let glyph = cache.get('l');
        let pad = HALO_PAD as usize;
        let (w, h) = (
            glyph.width as usize + 2 * pad,
            glyph.height as usize + 2 * pad,
        );
        assert_eq!(glyph.halo.len(), w * h);

        // Soft, not a hard copy: the ring of pixels just outside the glyph is partly covered...
        let ring = glyph.halo[(h / 2) * w];
        assert!(ring > 0 && ring < 255, "{ring}");
        // ...strongest at the letter, fading outwards
        let at_stem = glyph.halo[(h / 2) * w + w / 2];
        assert!(at_stem > ring, "{at_stem} {ring}");
        // It reaches only one pixel: a wide dark haze makes small text look fuzzy
        assert_eq!(HALO_PAD, 1);
        // A blank glyph has no halo to draw
        assert!(cache.get(' ').halo.is_empty());
    }

    #[test]
    fn a_crisp_size_gives_the_reference_letter_a_whole_number_of_pixel_rows() {
        let font = load_font(None, BOLD).unwrap();
        for (reference, wanted) in [('H', 12.6), ('H', 15.2), ('x', 16.8), ('x', 13.1)] {
            let px = crisp_size(&font, wanted, reference);
            // Close to what was asked for: a fraction of a pixel of cap height at most
            assert!(
                (px - wanted).abs() < 1.2,
                "{reference}: {wanted} became {px}"
            );
            // The reference letter's height at that size is a whole number of pixels
            let id = font.glyph_id(reference);
            let outline = font
                .outline_glyph(id.with_scale_and_position(PxScale::from(1000.0), point(0.0, 0.0)))
                .unwrap();
            let rows = outline.px_bounds().height() / 1000.0 * px;
            assert!(
                (rows - rows.round()).abs() < 0.02,
                "{reference} at {px}: {rows} rows"
            );
        }
    }

    #[test]
    fn at_a_crisp_size_the_flat_edges_of_capitals_are_solid_pixel_rows() {
        let font = load_font(None, BOLD).unwrap();
        let mut cache = GlyphCache::new(font.clone(), crisp_size(&font, 15.0, 'H'));
        for letter in ['E', 'T', 'H'] {
            let glyph = cache.get(letter);
            let width = glyph.width as usize;
            let row_max = |row: usize| {
                *glyph.mask[row * width..(row + 1) * width]
                    .iter()
                    .max()
                    .unwrap()
            };
            let last = glyph.height as usize - 1;
            // The first and last rows of the flat letters are fully covered, not a faint
            // smear over two rows
            assert!(row_max(0) >= 240, "{letter}: top row {}", row_max(0));
            assert!(
                row_max(last) >= 240,
                "{letter}: bottom row {}",
                row_max(last)
            );
        }

        // Control: at an arbitrary size the same edges are smeared, so this test can fail
        let mut arbitrary = GlyphCache::new(font, 12.6);
        let smeared = ['E', 'T', 'H'].into_iter().any(|letter| {
            let glyph = arbitrary.get(letter);
            let width = glyph.width as usize;
            let last = glyph.height as usize - 1;
            let row_max = |row: usize| {
                *glyph.mask[row * width..(row + 1) * width]
                    .iter()
                    .max()
                    .unwrap()
            };
            row_max(0) < 240 || row_max(last) < 240
        });
        assert!(
            smeared,
            "12.6 px should not be crisp: the test would prove nothing"
        );
    }

    #[test]
    fn a_glyph_drawn_at_a_fractional_position_differs_from_the_whole_pixel_one() {
        let mut cache = GlyphCache::new(load_font(None, REGULAR).unwrap(), 16.0);
        let whole = cache.place('l', 10.0).0.mask.to_vec();
        let (half, x) = cache.place('l', 10.5);
        assert_eq!(x, 10, "the pen is still in pixel 10");
        assert_ne!(
            whole,
            half.mask.to_vec(),
            "half a pixel to the right looks different"
        );
    }

    #[test]
    fn rounding_up_to_the_next_pixel_carries_into_the_whole_part() {
        let mut cache = GlyphCache::new(load_font(None, REGULAR).unwrap(), 16.0);
        // 10.9 is closer to 11.0 than to 10.75, so it is pixel 11 with no sub-pixel offset
        assert_eq!(cache.place('l', 10.9).1, 11);
        assert_eq!(cache.place('l', 10.76).1, 10);
    }
}
