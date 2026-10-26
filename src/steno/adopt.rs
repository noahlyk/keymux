//! Works out how text that is already in the text box would have been written in steno.
//!
//! When the cursor lands in text steno didn't type (you clicked into a document, or switched
//! to another window), the tape is replaced with the strokes that would have written that
//! text, newest at the bottom. It is a reverse lookup, so it shows the shortest outline the
//! dictionary has for each word, not necessarily the one that was used.

use super::dict::Dictionary;

/// One word of the text, with the whitespace in front of it (or, at the end, only whitespace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Where it starts and ends in the text, in bytes
    pub start: usize,
    pub end: usize,
    /// Tape rows for it, as `(stroke, text)`. Every stroke of its outline has a row, and the
    /// text is on the last. Empty for whitespace.
    pub rows: Vec<(u32, String)>,
}

/// Characters that end a word without being part of it. They have strokes of their own.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];

/// Split `text` into its words and look each one up. With `truncated`, `text` starts partway
/// through a word, so the first word is left out: it can't be told what it was.
#[must_use]
pub fn segment(dict: &Dictionary, text: &str, truncated: bool) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < text.len() {
        let word_start = text[start..]
            .find(|ch: char| !ch.is_whitespace())
            .map_or(text.len(), |offset| start + offset);
        if word_start == text.len() {
            // Only whitespace is left
            segments.push(Segment {
                start,
                end: text.len(),
                rows: Vec::new(),
            });
            break;
        }
        let end = text[word_start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| word_start + offset);
        // A first word that is cut short stays in the buffer, but can't be undone or shown
        if !(first && truncated) {
            segments.push(Segment {
                start,
                end,
                rows: rows_for(dict, &text[word_start..end]),
            });
        }
        first = false;
        start = end;
    }
    segments
}

/// Tape rows for one word: its outline, then an outline for each punctuation mark after it.
fn rows_for(dict: &Dictionary, word: &str) -> Vec<(u32, String)> {
    let core = word.trim_end_matches(TRAILING);
    let (core, tail) = if core.is_empty() {
        (word, "")
    } else {
        (core, &word[core.len()..])
    };
    let Some(outline) = outline_for(dict, core) else {
        // Nothing types it: shown whole, with no strokes
        return vec![(0, format!("{word:?}"))];
    };
    let mut rows: Vec<(u32, String)> = outline
        .iter()
        .map(|&stroke| (stroke, String::new()))
        .collect();
    if let Some(last) = rows.last_mut() {
        last.1 = format!("{core:?}");
    }
    for mark in tail.chars() {
        if let Some(strokes) = outline_for(dict, mark.encode_utf8(&mut [0; 4])) {
            let last = strokes.len() - 1;
            rows.extend(strokes.into_iter().enumerate().map(|(i, stroke)| {
                (stroke, if i == last { format!("{mark:?}") } else { String::new() })
            }));
        }
    }
    rows
}

/// The fewest strokes that type `word`, trying a lowercase first letter if the dictionary has
/// no entry with the capital (a word that starts a sentence).
fn outline_for(dict: &Dictionary, word: &str) -> Option<Vec<u32>> {
    let best = |found: Vec<Vec<u32>>| {
        found.into_iter().min_by_key(|strokes| {
            let ones: u32 = strokes.iter().map(|stroke| stroke.count_ones()).sum();
            (strokes.len(), ones, strokes.clone())
        })
    };
    if let Some(outline) = best(dict.strokes_for(word)) {
        return Some(outline);
    }
    let mut chars = word.chars();
    let first = chars.next()?;
    if first.is_uppercase() {
        let lowered: String = first.to_lowercase().chain(chars).collect();
        return best(dict.strokes_for(&lowered));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn dict() -> Dictionary {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"HEL": "hello", "WORLD": "world", "PER/TPEBGT": "perfect", "-PL": "{.}", "KPA": "{,}", "KAT": "cat", "KA*T": "cat"}"#,
        )
        .unwrap();
        dict
    }

    fn stroke(text: &str) -> u32 {
        parse_stroke(text).unwrap()
    }

    #[test]
    fn each_word_is_a_segment_with_the_space_before_it() {
        let segments = segment(&dict(), "hello world", false);
        let spans: Vec<(usize, usize)> = segments.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, [(0, 5), (5, 11)]);
        assert_eq!(segments[1].rows, [(stroke("WORLD"), "\"world\"".to_string())]);
    }

    #[test]
    fn a_word_with_several_strokes_has_a_row_for_each_and_the_text_on_the_last() {
        let segments = segment(&dict(), "perfect", false);
        assert_eq!(
            segments[0].rows,
            [
                (stroke("PER"), String::new()),
                (stroke("TPEBGT"), "\"perfect\"".to_string())
            ]
        );
    }

    #[test]
    fn the_fewest_strokes_win_and_then_the_fewest_keys() {
        let segments = segment(&dict(), "cat", false);
        assert_eq!(segments[0].rows[0].0, stroke("KAT"));
    }

    #[test]
    fn punctuation_after_a_word_has_its_own_row() {
        let segments = segment(&dict(), "hello.", false);
        assert_eq!(
            segments[0].rows,
            [
                (stroke("HEL"), "\"hello\"".to_string()),
                (stroke("-PL"), "'.'".to_string())
            ]
        );
    }

    #[test]
    fn a_capital_is_looked_up_as_the_lowercase_word() {
        let segments = segment(&dict(), "Hello", false);
        assert_eq!(segments[0].rows, [(stroke("HEL"), "\"Hello\"".to_string())]);
    }

    #[test]
    fn a_word_the_dictionary_lacks_is_shown_without_strokes() {
        let segments = segment(&dict(), "zorg", false);
        assert_eq!(segments[0].rows, [(0, "\"zorg\"".to_string())]);
    }

    #[test]
    fn trailing_whitespace_is_a_segment_with_no_rows() {
        let segments = segment(&dict(), "hello ", false);
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[1].start, segments[1].end), (5, 6));
        assert!(segments[1].rows.is_empty());
    }

    #[test]
    fn a_first_word_cut_short_is_left_out() {
        let segments = segment(&dict(), "llo world", true);
        let spans: Vec<(usize, usize)> = segments.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, [(3, 9)]);
    }

    #[test]
    fn text_with_no_words_has_none() {
        assert!(segment(&dict(), "", false).is_empty());
    }

    #[test]
    fn newlines_and_extra_spaces_stay_with_the_word_after_them() {
        let segments = segment(&dict(), "hello\n\n  world", false);
        let spans: Vec<(usize, usize)> = segments.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, [(0, 5), (5, 14)]);
    }
}
