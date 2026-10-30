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

/// Dictionary entries closest to `strokes`, nearest first - "did you mean" outlines for a raw
/// stroke a key or two off from something real.
///
/// So Engine Resolver mode can show more than the one exact (and maybe empty) answer. Only
/// entries with the same number of strokes are compared, key by key, by how many keys
/// differ in total (`a ^ b` leaves only the bits that differ, and `count_ones` counts them -
/// cheap enough to run on every one of a dictionary's entries, every keystroke, with room to
/// spare); `strokes` itself is left out, since that's already shown as the exact answer. Keeps
/// only the closest `limit`, without ever fully sorting a large dictionary: the rest are
/// discarded by a single partition before the final sort.
#[must_use]
pub fn nearby_strokes(dict: &Dictionary, strokes: &[u32], limit: usize) -> Vec<Vec<u32>> {
    if limit == 0 || strokes.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<(u32, &Vec<u32>)> = dict
        .iter()
        .filter(|(outline, _)| outline.len() == strokes.len() && outline.as_slice() != strokes)
        .map(|(outline, _)| {
            let distance = outline
                .iter()
                .zip(strokes)
                .map(|(&a, &b)| (a ^ b).count_ones())
                .sum();
            (distance, outline)
        })
        .collect();
    if limit < candidates.len() {
        candidates.select_nth_unstable_by_key(limit - 1, |&(distance, _)| distance);
        candidates.truncate(limit);
    }
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    candidates.into_iter().map(|(_, outline)| outline.clone()).collect()
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

    #[test]
    fn nearby_strokes_ranks_by_how_many_keys_differ_nearest_first() {
        let mut dict = Dictionary::default();
        // KAT = K,A,T. Built so each entry's distance from KAT is obvious: KA is 1 key off, TKAT
        // is 1 key off, GAT is 1 key off (2 differing bits, since G and K share no keys, but
        // everything else matches - still the single "column" that differs), KPAT is 2 off.
        dict.merge_json(
            r#"{
                "KAT": "cat",
                "KA": "ca",
                "TKAT": "dat",
                "GAT": "gat",
                "KPAT": "kpat",
                "-T": "the"
            }"#,
        )
        .unwrap();
        let kat = parse_stroke("KAT").unwrap();
        let nearby = nearby_strokes(&dict, &[kat], 10);
        // "-T" has a different stroke length in nobody's eyes here - it's 1 stroke, same as the
        // rest, so it's a candidate too, just a farther one. The query itself is never included.
        assert!(!nearby.contains(&vec![kat]), "{nearby:?}");
        assert_eq!(nearby.len(), 4, "{nearby:?}");
        // Nearest (fewest differing keys) sort first
        let distances: Vec<u32> = nearby
            .iter()
            .map(|outline| (outline[0] ^ kat).count_ones())
            .collect();
        assert!(
            distances.windows(2).all(|w| w[0] <= w[1]),
            "not sorted nearest-first: {distances:?}"
        );
    }

    #[test]
    fn nearby_strokes_only_compares_outlines_of_the_same_length() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KAT/-D": "catted"}"#)
            .unwrap();
        let kat = parse_stroke("KAT").unwrap();
        // "KAT/-D" is two strokes; a one-stroke query has nothing of the same shape to compare
        // it against here, so the search turns up nothing rather than a meaningless answer.
        assert_eq!(nearby_strokes(&dict, &[kat], 10), Vec::<Vec<u32>>::new());
    }

    #[test]
    fn nearby_strokes_respects_its_limit() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "KA": "ca", "TKAT": "dat", "GAT": "gat"}"#)
            .unwrap();
        let kat = parse_stroke("KAT").unwrap();
        assert_eq!(nearby_strokes(&dict, &[kat], 2).len(), 2);
        assert_eq!(nearby_strokes(&dict, &[kat], 0).len(), 0);
    }

    #[test]
    fn nearby_strokes_stays_fast_over_a_large_dictionary() {
        // Every keystroke of a raw-steno query re-runs this over the whole dictionary (see its
        // own doc comment), and a real one can run to six figures of entries - this is the
        // "indetectable" requirement as an actual regression guard, not just an assumption.
        // Generous even for a slow debug build on a loaded CI box; a real release build of the
        // daemon measured under 5ms for the same size.
        use std::fmt::Write as _;
        let mut json = String::from("{");
        let mut state: u32 = 0x9E37_79B9;
        let mut seen = std::collections::HashSet::new();
        let mut i = 0u32;
        while seen.len() < 150_000 {
            state = state.wrapping_mul(2_654_435_761).wrapping_add(i);
            i += 1;
            let bits = state & 0x007F_FFFF; // 23 real stroke bits
            if bits == 0 || !seen.insert(bits) {
                continue;
            }
            if seen.len() > 1 {
                json.push(',');
            }
            let outline = crate::steno::layout::render_strokes(&[bits]);
            write!(json, "{outline:?}:\"w{i}\"").unwrap();
        }
        json.push('}');
        let mut dict = Dictionary::default();
        dict.merge_json(&json).unwrap();

        let query = [0x0041_2345u32 & 0x007F_FFFF];
        let start = std::time::Instant::now();
        let found = nearby_strokes(&dict, &query, 11);
        let elapsed = start.elapsed();
        assert_eq!(found.len(), 11);
        assert!(
            elapsed.as_millis() < 200,
            "nearby_strokes over {} entries took {elapsed:?}, expected well under 200ms",
            dict.len()
        );
    }
}
