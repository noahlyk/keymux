//! One reverse-lookup engine for "how do I type this", shared by the CLI and the live overlay
//! lookup, so there is a single answer instead of three slightly different ones.
//!
//! A query is either a stroke to resolve forward (what does `KAT/-D` type?) or text to find the
//! stroke for — a single word or a whole phrase, word by word, falling back to fingerspelling a
//! word with no dictionary entry at all and marking that it did, since spelling a word out one
//! letter at a time is a materially worse answer than a real outline, not just a different-
//! looking one, and a caller showing the answer to someone has reason to say which it got.

use super::commands::{exact_strokes, ranked_strokes, word_spans};
use super::dict::Dictionary;
use super::layout::{parse_strokes_trimmed, render_strokes};
use super::shortest::shortest_outline_labeled;
use super::translate::Translator;

/// What a query, as typed, means.
///
/// Case is the signal, same as a learner already writes stroke notation in caps and everything
/// else in lowercase: text with no lowercase letter that parses as steno notation is a stroke to
/// resolve forward; anything else is text to find the stroke(s) for. A real word that happens to
/// be valid notation too (`AND`, `SO`) reads as a stroke when typed in caps, since that's what
/// typing it in caps almost always means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Stroke(Vec<u32>),
    Text(String),
}

#[must_use]
pub fn classify(input: &str) -> Query {
    let trimmed = input.trim();
    let looks_like_a_stroke = !trimmed.is_empty() && !trimmed.chars().any(|c| c.is_ascii_lowercase());
    if looks_like_a_stroke {
        if let Some(strokes) = parse_strokes_trimmed(trimmed) {
            return Query::Stroke(strokes);
        }
    }
    Query::Text(trimmed.to_string())
}

/// One word of a reverse-lookup result, or a run of letters fingerspelled in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub strokes: Vec<u32>,
    pub fingerspelled: bool,
}

/// `phrase`'s reverse lookup, one segment per word in order.
///
/// `None` for a word that is neither in the dictionary (alone or as part of a phrase entry
/// covering it and its neighbors) nor fingerspellable (a letter with no stroke of its own). A run
/// of words covered by a single phrase dictionary entry (Plover-style phrase briefs: "I am" as
/// one outline) is searched for and preferred over one stroke per word, the same way `keymux
/// steno keys` already does — this reuses that exact search, not a reimplementation of it, so the
/// two can never answer differently for the same phrase.
#[must_use]
pub fn reverse_phrase(base: &Translator, dict: &Dictionary, phrase: &str) -> Vec<Option<Segment>> {
    let spans = word_spans(phrase);
    let words: Vec<&str> = spans.iter().map(|&(s, e)| &phrase[s..e]).collect();
    let candidates: Vec<Vec<Vec<u32>>> = words.iter().map(|&w| ranked_strokes(dict, w)).collect();

    let mut chosen: Vec<Option<Segment>> = Vec::with_capacity(words.len());
    let mut start = 0;
    while start < words.len() {
        if candidates[start].is_empty() {
            chosen.push(
                shortest_outline_labeled(dict, words[start]).map(|found| Segment {
                    text: words[start].to_string(),
                    strokes: found.strokes,
                    fingerspelled: found.fingerspelled,
                }),
            );
            start += 1;
            continue;
        }
        let end = (start..words.len())
            .find(|&i| candidates[i].is_empty())
            .unwrap_or(words.len());
        let run_start = spans[start].0;
        let wants: Vec<String> = spans[start..end]
            .iter()
            .map(|&(_, word_end)| {
                phrase[run_start..word_end]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let as_segment = |word: &str, strokes: Vec<u32>| Segment {
            text: word.to_string(),
            strokes,
            fingerspelled: false,
        };
        match exact_strokes(base, &wants, &candidates[start..end]) {
            Some(exact) => chosen.extend(
                words[start..end]
                    .iter()
                    .zip(exact)
                    .map(|(&w, s)| Some(as_segment(w, s))),
            ),
            None => chosen.extend(
                words[start..end]
                    .iter()
                    .zip(&candidates[start..end])
                    .map(|(&w, opts)| opts.first().cloned().map(|s| as_segment(w, s))),
            ),
        }
        start = end;
    }
    chosen
}

/// `segments` rendered for a human.
///
/// The text (a fingerspelled run wrapped in quotes, so it reads apart from a real outline at a
/// glance, `?` for a word nothing could type) and the strokes that type it, each on its own line
/// of the pair.
#[must_use]
pub fn display(segments: &[Option<Segment>]) -> (String, String) {
    let mut words_out = Vec::with_capacity(segments.len());
    let mut strokes_out = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment {
            Some(segment) if segment.fingerspelled => {
                words_out.push(format!("\"{}\"", segment.text));
                strokes_out.push(render_strokes(&segment.strokes));
            }
            Some(segment) => {
                words_out.push(segment.text.clone());
                strokes_out.push(render_strokes(&segment.strokes));
            }
            None => {
                words_out.push("?".to_string());
                strokes_out.push("?".to_string());
            }
        }
    }
    (words_out.join(" "), strokes_out.join(" / "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn translator_and_dict() -> (Translator, Dictionary) {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{
                "KAT": "cat",
                "TPH": "{&n}",
                "SR*": "{&v}",
                "SKW*": "{&i}",
                "PH*": "{&m}",
                "AEU": "I",
                "APL": "am"
            }"#,
        )
        .unwrap();
        (Translator::new(dict.clone()), dict)
    }

    #[test]
    fn an_all_caps_stroke_classifies_as_a_stroke() {
        assert_eq!(
            classify("KAT/-D"),
            Query::Stroke(vec![parse_stroke("KAT").unwrap(), parse_stroke("-D").unwrap()])
        );
    }

    #[test]
    fn lowercase_text_classifies_as_text_even_if_it_would_parse() {
        assert_eq!(classify("kat"), Query::Text("kat".to_string()));
        assert_eq!(classify("a whole phrase"), Query::Text("a whole phrase".to_string()));
    }

    #[test]
    fn a_dictionary_word_is_a_real_segment() {
        let (base, dict) = translator_and_dict();
        let segments = reverse_phrase(&base, &dict, "cat");
        assert_eq!(
            segments,
            [Some(Segment {
                text: "cat".to_string(),
                strokes: vec![parse_stroke("KAT").unwrap()],
                fingerspelled: false,
            })]
        );
        let (words, strokes) = display(&segments);
        assert_eq!(words, "cat");
        assert_eq!(strokes, "KAT");
    }

    #[test]
    fn a_word_with_no_entry_is_fingerspelled_and_marked() {
        let (base, dict) = translator_and_dict();
        let segments = reverse_phrase(&base, &dict, "nvim");
        let (words, _) = display(&segments);
        assert_eq!(words, "\"nvim\"");
        assert!(segments[0].as_ref().unwrap().fingerspelled);
    }

    #[test]
    fn a_phrase_mixes_real_words_and_fingerspelling() {
        let (base, dict) = translator_and_dict();
        let segments = reverse_phrase(&base, &dict, "I am nvim");
        let (words, _) = display(&segments);
        assert_eq!(words, "I am \"nvim\"");
    }

    #[test]
    fn a_word_with_no_stroke_at_all_is_none() {
        let (base, dict) = translator_and_dict();
        let segments = reverse_phrase(&base, &dict, "cat zzz9");
        assert!(segments[0].is_some());
        assert!(segments[1].is_none());
        let (words, strokes) = display(&segments);
        assert_eq!(words, "cat ?");
        assert_eq!(strokes, "KAT / ?");
    }
}
