//! An [`Answer`] as lines of text: the one view `keymux steno lookup`, `keys` and `stroke` print,
//! and the interactive session redraws on every key.
//!
//! The columns follow `quote` in the lyk-steno-theory generator (`word`, then the outline
//! column at least 12 wide), one outline per row. Each outline is set in colour where the
//! terminal has it, a colour per part of the keyboard as on the overlay's tape, and keys one
//! finger plays together underlined as one.

use super::answer::{Answer, Mark, Row};
use super::notation::Notation;
use crate::config::KeyColor;
use crate::keycode::KeyCode;
use console::{measure_text_width, style, Color, Style};
use std::collections::HashMap;

/// The narrowest the outline column is set, as in lyk's `quote`.
const OUTLINE_WIDTH: usize = 12;
/// The narrowest the word column is set, as in lyk's `quote`.
const WORD_WIDTH: usize = 4;

/// Everything a view needs besides the answer.
pub struct View<'a> {
    /// How outlines are written: Plover's notation, or the theory's (see [`super::notation`])
    pub notation: &'a Notation,
    /// The physical key for each stroke key, for the keys column
    pub keys_by_bit: &'a HashMap<u32, KeyCode>,
    /// The overlay's colour for each extra key
    pub extra_colors: &'a HashMap<String, KeyColor>,
    pub show_keys: bool,
    /// Whether to colour and underline
    pub color: bool,
}

impl View<'_> {
    /// Every line to print for `answer`: the header and rows, a note under each word that was
    /// composed or spelled out, and what the best outlines add up to.
    #[must_use]
    pub fn lines(&self, answer: &Answer) -> Vec<String> {
        if let Some(problem) = &answer.problem {
            return vec![self.paint(style(problem.clone()).red())];
        }
        if answer.rows.is_empty() {
            return Vec::new();
        }
        let cells: Vec<(String, usize)> = answer
            .rows
            .iter()
            .map(|row| self.outline_cell(row))
            .collect();
        let word_width = answer
            .rows
            .iter()
            .map(|row| {
                measure_text_width(&row.word) + usize::from(row.mark == Mark::Fingerspelled) * 2
            })
            .max()
            .unwrap_or(0)
            .max(WORD_WIDTH);
        let outline_width = cells
            .iter()
            .map(|(_, w)| *w)
            .max()
            .unwrap_or(0)
            .max(OUTLINE_WIDTH);

        let mut header = format!(
            "{}  {}",
            self.paint(style(format!("{:<word_width$}", "word")).bold()),
            self.paint(style(format!("{:<outline_width$}", "outline")).bold()),
        );
        if self.show_keys {
            header.push_str(&format!("  {}", self.paint(style("keys").bold())));
        }
        let mut lines = vec![header.trim_end().to_string()];

        for (row, (outline, width)) in answer.rows.iter().zip(&cells) {
            let word = if row.mark == Mark::Fingerspelled {
                format!("\"{}\"", row.word)
            } else {
                row.word.clone()
            };
            let word = format!(
                "{word}{}",
                " ".repeat(word_width.saturating_sub(measure_text_width(&word)))
            );
            let word = match row.mark {
                Mark::Missing => self.paint(style(word).dim()),
                Mark::Completion | Mark::Nearby => self.paint(style(word).dim()),
                _ => self.paint(style(word).bold()),
            };
            let mut line = format!("{word}  {outline}");
            if self.show_keys && !row.strokes.is_empty() {
                line.push_str(&" ".repeat(outline_width - width));
                line.push_str("  ");
                line.push_str(&self.paint(style(self.keys_text(&row.strokes)).green()));
            }
            lines.push(line.trim_end().to_string());
        }

        for row in &answer.rows {
            match &row.mark {
                Mark::Composed(pieces) => lines.push(format!(
                    "  {} was not a whole entry - composed ({})",
                    row.word,
                    pieces.join(" + ")
                )),
                Mark::Fingerspelled => lines.push(format!(
                    "  {} has no dictionary entry - shown fingerspelled instead (in quotes)",
                    row.word
                )),
                _ => {}
            }
        }
        let (strokes, keys) = answer.totals();
        if strokes > 0 {
            lines.push(String::new());
            lines.push(
                self.paint(
                    style(format!(
                        "keys (best outline of each word): {keys} in {strokes} stroke{}",
                        if strokes == 1 { "" } else { "s" }
                    ))
                    .dim(),
                ),
            );
        }
        lines
    }

    fn paint<D: std::fmt::Display>(&self, styled: console::StyledObject<D>) -> String {
        styled.force_styling(self.color).to_string()
    }

    /// The outline column of a row and its width on screen.
    fn outline_cell(&self, row: &Row) -> (String, usize) {
        if row.mark == Mark::Missing {
            let text = "no stroke";
            return (self.paint(style(text).red().dim()), text.len());
        }
        let plain = self.notation.render_strokes(&row.strokes);
        let width = measure_text_width(&plain);
        // An outline the theory writes its own way as a whole has no pieces to colour
        if !self.color || self.notation.outline_override(&row.strokes).is_some() {
            return (plain, width);
        }
        // Each stroke grouped as the outline's known split says, else key by key (or by the table,
        // for a theory that lists no known splits)
        let strokes: Vec<String> = self
            .notation
            .render_strokes_pieces(&row.strokes)
            .map_or_else(
                || {
                    row.strokes
                        .iter()
                        .map(|&bits| self.stroke_text(bits))
                        .collect()
                },
                |pieces| pieces.into_iter().map(|p| self.pieces_text(p)).collect(),
            );
        (
            strokes.join(&style("/").dim().force_styling(true).to_string()),
            width,
        )
    }

    /// One stroke, a colour a key by part of the keyboard, keys one finger plays together
    /// underlined as one run. A piece of the theory's notation that stands for several keys
    /// (`N` for `TPH`) is coloured by its first key and underlined as one.
    #[must_use]
    pub fn stroke_text(&self, bits: u32) -> String {
        self.pieces_text(self.notation.render_stroke_pieces(bits))
    }

    /// One stroke's written pieces, coloured as [`Self::stroke_text`] says.
    fn pieces_text(&self, pieces: Vec<super::notation::Piece>) -> String {
        let layout = self.notation.layout();
        let parts: Vec<(String, Option<usize>, bool)> = pieces
            .into_iter()
            .map(|piece| {
                let slot = (piece.keys != 0).then(|| piece.keys.trailing_zeros() as usize);
                (piece.text, slot, piece.keys.count_ones() > 1)
            })
            .collect();
        let group = |slot: Option<usize>| slot.and_then(|slot| layout.key_group(slot));
        let mut out = String::new();
        for (i, (text, slot, several)) in parts.iter().enumerate() {
            let this = group(*slot);
            let joined = *several
                || this.is_some()
                    && (i > 0 && !parts[i - 1].2 && group(parts[i - 1].1) == this
                        || parts
                            .get(i + 1)
                            .is_some_and(|next| !next.2 && group(next.1) == this));
            let mut part = Style::new().force_styling(true);
            if let Some(color) = self.color_of(*slot) {
                part = part.fg(color);
            } else {
                part = part.dim();
            }
            if joined {
                part = part.underlined();
            }
            out.push_str(&part.apply_to(text).to_string());
        }
        out
    }

    /// Green for the left hand, yellow for the vowels, red for the right hand, blue for the star
    /// and number key: the overlay's colours. An extra key is blue unless `extra_colors` says
    /// otherwise.
    fn color_of(&self, slot: Option<usize>) -> Option<Color> {
        let slot = slot?;
        let layout = self.notation.layout();
        let named = |color: KeyColor| match color {
            KeyColor::Left => Color::Green,
            KeyColor::Vowel => Color::Yellow,
            KeyColor::Right => Color::Red,
            KeyColor::Misc => Color::Cyan,
        };
        Some(if slot >= layout.extra_start() {
            let name = layout
                .key_at(slot)
                .map(|k| k.name.clone())
                .unwrap_or_default();
            named(
                self.extra_colors
                    .get(&name)
                    .copied()
                    .unwrap_or(KeyColor::Misc),
            )
        } else if slot == layout.number_slot() || slot == layout.star_slot() {
            Color::Cyan
        } else if slot < layout.left_end() {
            Color::Green
        } else if slot < layout.right_start() {
            Color::Yellow
        } else {
            Color::Red
        })
    }

    /// The physical keys that press each stroke, strokes split by `/`.
    #[must_use]
    pub fn keys_text(&self, strokes: &[u32]) -> String {
        super::commands::keys_text(strokes, self.keys_by_bit)
    }
}
