//! English spelling rules for attaching a suffix to a word: `make` + `ing` is `making`, and
//! `carry` + `s` is `carries`.
//!
//! This is Plover's English Stenotype orthography: the same ordered rules, tried the same way
//! against its word list. A rule's result counts as a word if it is in the list, and the most
//! common one wins; failing that, the first rule that matches decides, and failing that the two
//! are simply joined. Without a word list only the last two steps happen. `keymux steno theory`
//! downloads Plover's list (`american_english_words.txt`) into the steno folder.

use fancy_regex::{Captures, Regex};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Plover's `ORTHOGRAPHY_RULES`, in order: a pattern over `word ^ suffix` and what it becomes.
/// All are matched ignoring case.
const RULES: &[(&str, &str)] = &[
    // == +ly ==
    // artistic + ly = artistically
    (r"^(.*[aeiou]c) \^ ly$", r"\1ally"),
    // humble + ly = humbly (*humblely)
    // questionable +ly = questionably
    // triple +ly = triply
    (r"^(.+[aeioubmnp])le \^ ly$", r"\1ly"),
    // == +ry ==
    // statute + ry = statutory
    (r"^(.*t)e \^ (ry|ary)$", r"\1ory"),
    // confirm +tory = confirmatory (*confirmtory)
    (r"^(.+)m \^ tor(y|ily)$", r"\1mator\2"),
    // supervise +ary = supervisory (*supervisary)
    (r"^(.+)se \^ ar(y|ies)$", r"\1sor\2"),
    // == t +cy ==
    // frequent + cy = frequency (tcy/tecy removal)
    (r"^(.*[naeiou])te? \^ cy$", r"\1cy"),
    // == +s ==
    // establish + s = establishes (sibilant pluralization)
    (r"^(.*(?:s|sh|x|z|zh)) \^ s$", r"\1es"),
    // speech + s = speeches (soft ch pluralization)
    (r"^(.*(?:oa|ea|i|ee|oo|au|ou|l|n|(?<![gin]a)r|t)ch) \^ s$", r"\1es"),
    // cherry + s = cherries (consonant + y pluralization)
    (r"^(.+[bcdfghjklmnpqrstvwxz])y \^ s$", r"\1ies"),
    // == y ==
    // die+ing = dying
    (r"^(.+)ie \^ ing$", r"\1ying"),
    // metallurgy + ist = metallurgist
    (r"^(.+[cdfghlmnpr])y \^ ist$", r"\1ist"),
    // beauty + ful = beautiful (y -> i)
    (r"^(.+[bcdfghjklmnpqrstvwxz])y \^ ([a-hj-xz].*)$", r"\1i\2"),
    // == +en ==
    // write + en = written
    (r"^(.+)te \^ en$", r"\1tten"),
    // Minessota +en = Minessotan (*Minessotaen)
    (r"^(.+[ae]) \^ e(n|ns)$", r"\1\2"),
    // == +ial ==
    // ceremony +ial = ceremonial (*ceremonyial)
    (r"^(.+)y \^ (ial|ially)$", r"\1\2"),
    // == +if ==
    // spaghetti +ification = spaghettification (*spaghettiification)
    // pretty +ify = prettify (*prettyify)
    (r"^(.+i) \^ if(y|ying|ied|ies|ication|ications)$", r"\1f\2"),
    (r"^(.*[^aeiou])y \^ if(y|ying|ied|ies|ication|ications)$", r"\1if\2"),
    // == +ical ==
    // fantastic +ical = fantastical (*fantasticcal)
    (r"^(.+)ic \^ (ical|ically)$", r"\1\2"),
    // epistomology +ical = epistomological
    (r"^(.+)ology \^ ic(al|ally)$", r"\1ologic\2"),
    // oratory +ical = oratorical (*oratoryical)
    (r"^(.*)ry \^ ica(l|lly|lity)$", r"\1rica\2"),
    // == +ist ==
    // radical +ist = radicalist (*radicallist)
    (r"^(.*[l]) \^ is(t|ts)$", r"\1is\2"),
    // == +ity ==
    // complementary +ity = complementarity (*complementaryity)
    (r"^(.*)ry \^ ity$", r"\1rity"),
    // disproportional +ity = disproportionality (*disproportionallity)
    (r"^(.*)l \^ ity$", r"\1lity"),
    // == +ive, +tive ==
    // perform +tive = performative (*performtive)
    (r"^(.+)rm \^ tiv(e|ity|ities)$", r"\1rmativ\2"),
    // restore +tive = restorative
    (r"^(.+)e \^ tiv(e|ity|ities)$", r"\1ativ\2"),
    // == +ize ==
    // token +ize = tokenize (*tokennize)
    // token +ise = tokenise (*tokennise)
    (r"^(.+)y \^ iz(e|es|ing|ed|er|ers|ation|ations|able|ability)$", r"\1iz\2"),
    (r"^(.+)y \^ is(e|es|ing|ed|er|ers|ation|ations|able|ability)$", r"\1is\2"),
    // conditional +ize = conditionalize (*conditionallize)
    (
        r"^(.+)al \^ iz(e|ed|es|ing|er|ers|ation|ations|m|ms|able|ability|abilities)$",
        r"\1aliz\2",
    ),
    (
        r"^(.+)al \^ is(e|ed|es|ing|er|ers|ation|ations|m|ms|able|ability|abilities)$",
        r"\1alis\2",
    ),
    // spectacular +ization = spectacularization (*spectacularrization)
    (r"^(.+)ar \^ iz(e|ed|es|ing|er|ers|ation|ations|m|ms)$", r"\1ariz\2"),
    (r"^(.+)ar \^ is(e|ed|es|ing|er|ers|ation|ations|m|ms)$", r"\1aris\2"),
    // category +ize/+ise = categorize/categorise (*categoryize/*categoryise)
    // custom +izable/+isable = customizable/customisable (*custommizable/*custommisable)
    // fantasy +ize = fantasize (*fantasyize)
    (
        r"^(.*[lmnty]) \^ iz(e|es|ing|ed|er|ers|ation|ations|m|ms|able|ability|abilities)$",
        r"\1iz\2",
    ),
    (
        r"^(.*[lmnty]) \^ is(e|es|ing|ed|er|ers|ation|ations|m|ms|able|ability|abilities)$",
        r"\1is\2",
    ),
    // == +olog ==
    // criminal + ology = criminology
    // criminal + ologist = criminalogist (*criminallologist)
    (r"^(.+)al \^ olog(y|ist|ists|ical|ically)$", r"\1olog\2"),
    // == +ish ==
    // similar +ish = similarish (*similarrish)
    (r"^(.+)(ar|er|or) \^ ish$", r"\1\2ish"),
    // free + ed = freed
    (r"^(.+e)e \^ (e.+)$", r"\1\2"),
    // narrate + ing = narrating (silent e)
    (r"^(.+[bcdfghjklmnpqrstuvwxz])e \^ ([aeiouy].*)$", r"\1\2"),
    // == misc ==
    // defer + ed = deferred (consonant doubling)   XXX monitor(stress not on last syllable)
    (
        r"^(.*(?:[bcdfghjklmnprstvwxyz]|qu)[aeiou])([bcdfgklmnprtvz]) \^ ([aeiouy].*)$",
        r"\1\2\2\3",
    ),
];

/// Suffixes that stand for another before the rules are tried: `able` is also tried as `ible`.
const ALIASES: &[(&str, &str)] = &[("able", "ible"), ("ability", "ibility")];

/// The file Plover keeps its word list in.
pub const WORDLIST_FILE: &str = "american_english_words.txt";

fn rules() -> &'static [(Regex, &'static str)] {
    static COMPILED: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        RULES
            .iter()
            .map(|(pattern, replacement)| {
                let regex = Regex::new(&format!("(?i){pattern}"))
                    .unwrap_or_else(|e| unreachable!("orthography rule {pattern}: {e}"));
                (regex, *replacement)
            })
            .collect()
    })
}

/// What `\1` and `\2` in a rule's replacement stand for.
fn expand(template: &str, captures: &Captures) -> String {
    let mut out = String::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(digit) = chars.clone().next().and_then(|d| d.to_digit(10)) {
                chars.next();
                out.push_str(captures.get(digit as usize).map_or("", |m| m.as_str()));
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Plover's English orthography: its rules, and the words that tell good spellings from bad.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Orthography {
    /// Each word and how common it is: the lower the number, the commoner
    words: FxHashMap<String, u32>,
}

impl Orthography {
    /// Read a word list, as Plover does: whitespace-separated words, each followed by its rank.
    #[must_use]
    pub fn from_wordlist(text: &str) -> Self {
        let mut fields = text.split_whitespace();
        let mut words = FxHashMap::default();
        while let (Some(word), Some(rank)) = (fields.next(), fields.next()) {
            if let Ok(rank) = rank.parse() {
                words.insert(word.to_string(), rank);
            }
        }
        Self { words }
    }

    /// Whether a word list is loaded
    #[must_use]
    pub fn has_words(&self) -> bool {
        !self.words.is_empty()
    }

    /// The results of the rules that match `word ^ suffix`, and that pass `check`, in rule order.
    fn candidates(&self, word: &str, suffix: &str, check: &dyn Fn(&str) -> bool) -> Vec<String> {
        let text = format!("{word} ^ {suffix}");
        rules()
            .iter()
            .filter_map(|(regex, replacement)| {
                let captures = regex.captures(&text).ok().flatten()?;
                let expanded = expand(replacement, &captures);
                check(&expanded).then_some(expanded)
            })
            .collect()
    }

    fn add_suffix_to_word(&self, word: &str, suffix: &str) -> String {
        let in_list = |candidate: &str| self.words.contains_key(candidate);
        let mut candidates = Vec::new();
        if let Some((_, alias)) = ALIASES.iter().find(|(from, _)| *from == suffix) {
            candidates.extend(self.candidates(word, alias, &in_list));
        }
        // A plain join, if that is a word
        let simple = format!("{word}{suffix}");
        if in_list(&simple) {
            candidates.push(simple.clone());
        }
        candidates.extend(self.candidates(word, suffix, &in_list));
        // The commonest word wins, and of equally common ones the earlier
        if !candidates.is_empty() {
            candidates.sort_by_key(|candidate| self.words[candidate.as_str()]);
            return candidates.swap_remove(0);
        }
        // No word to go by: the first rule that matches
        let by_rule = self.candidates(word, suffix, &|_| true);
        by_rule.into_iter().next().unwrap_or(simple)
    }

    /// `suffix` attached to `word` the way English spells it. Anything after a space in `suffix`
    /// is kept as it is.
    #[must_use]
    pub fn add_suffix(&self, word: &str, suffix: &str) -> String {
        let (head, rest) = suffix.split_once(' ').map_or((suffix, None), |(h, r)| (h, Some(r)));
        let mut out = self.add_suffix_to_word(word, head);
        if let Some(rest) = rest {
            out.push(' ');
            out.push_str(rest);
        }
        out
    }
}

/// The word `stem` with `suffix` attached, spelled the way English would.
#[must_use]
pub fn attach(stem: &str, suffix: &str) -> String {
    if stem.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return stem.to_string();
    }
    let lower = stem.to_ascii_lowercase();
    let suf = suffix.to_ascii_lowercase();
    let starts_vowel = suf.starts_with(['a', 'e', 'i', 'o', 'u']);

    // -s / -es
    if suf == "s" {
        if let Some(stripped) = lower.strip_suffix('y').filter(|_| ends_consonant_y(&lower)) {
            return format!("{}{}", &stem[..stripped.len()], "ies");
        }
        if ends_with_any(&lower, &["s", "x", "z", "ch", "sh"]) {
            return format!("{stem}es");
        }
        return format!("{stem}s");
    }

    // ie + i-suffix: lie + ing = lying
    if lower.ends_with("ie") && suf.starts_with('i') {
        return format!("{}y{suffix}", &stem[..stem.len() - 2]);
    }

    // consonant + y + e-suffix: carry + ed = carried
    if ends_consonant_y(&lower) && suf.starts_with('e') {
        return format!("{}i{suffix}", &stem[..stem.len() - 1]);
    }

    // silent e before a vowel suffix: make + ing = making, make + ed = made
    if lower.ends_with('e') && !ends_with_any(&lower, &["ee", "ye", "oe"]) {
        if suf.starts_with('e') {
            return format!("{stem}{}", &suffix[1..]);
        }
        if starts_vowel {
            return format!("{}{suffix}", &stem[..stem.len() - 1]);
        }
    }

    // ee + e-suffix: agree + ed = agreed
    if lower.ends_with("ee") && suf.starts_with('e') {
        return format!("{stem}{}", &suffix[1..]);
    }

    // Double the final consonant of a one-syllable word: stop + ing = stopping
    if starts_vowel && is_doubling_stem(&lower) {
        let last = stem.chars().last().unwrap_or_default();
        return format!("{stem}{last}{suffix}");
    }

    format!("{stem}{suffix}")
}

fn ends_with_any(word: &str, endings: &[&str]) -> bool {
    endings.iter().any(|ending| word.ends_with(ending))
}

const fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// A `y` that follows a consonant. `day` ends in a vowel + y and keeps the y.
fn ends_consonant_y(word: &str) -> bool {
    let mut chars = word.chars().rev();
    chars.next() == Some('y') && chars.next().is_some_and(|c| !is_vowel(c))
}

/// A one-syllable word ending consonant-vowel-consonant, where the last
/// consonant is not w, x, or y. Those double before a vowel suffix.
fn is_doubling_stem(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n < 3 || !chars.iter().all(char::is_ascii_alphabetic) {
        return false;
    }
    let last = chars[n - 1];
    let before_last = chars[n - 2];
    let consonant_end =
        !is_vowel(last) && !matches!(last, 'w' | 'x' | 'y') && is_vowel(before_last);
    consonant_end && vowel_groups(&chars) == 1
}

fn vowel_groups(chars: &[char]) -> usize {
    let mut groups = 0;
    let mut in_group = false;
    for &c in chars {
        if is_vowel(c) {
            if !in_group {
                groups += 1;
            }
            in_group = true;
        } else {
            in_group = false;
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_silent_e_before_vowel_suffix() {
        assert_eq!(attach("make", "ing"), "making");
        assert_eq!(attach("use", "able"), "usable");
    }

    #[test]
    fn e_suffix_after_silent_e_drops_its_e() {
        assert_eq!(attach("hope", "ed"), "hoped");
        assert_eq!(attach("hope", "er"), "hoper");
    }

    #[test]
    fn ee_keeps_its_e() {
        assert_eq!(attach("agree", "ed"), "agreed");
        assert_eq!(attach("agree", "ing"), "agreeing");
    }

    #[test]
    fn consonant_y_becomes_ies_and_ied() {
        assert_eq!(attach("carry", "s"), "carries");
        assert_eq!(attach("carry", "ed"), "carried");
        assert_eq!(attach("play", "s"), "plays");
        assert_eq!(attach("play", "ed"), "played");
    }

    #[test]
    fn sibilants_take_es() {
        assert_eq!(attach("box", "s"), "boxes");
        assert_eq!(attach("wish", "s"), "wishes");
        assert_eq!(attach("cat", "s"), "cats");
    }

    #[test]
    fn doubles_one_syllable_consonant_ending() {
        assert_eq!(attach("stop", "ing"), "stopping");
        assert_eq!(attach("run", "ing"), "running");
        assert_eq!(attach("fix", "ing"), "fixing");
        assert_eq!(attach("play", "ing"), "playing");
    }

    #[test]
    fn does_not_double_longer_words() {
        assert_eq!(attach("visit", "ing"), "visiting");
        assert_eq!(attach("open", "ed"), "opened");
    }

    #[test]
    fn ie_becomes_y_before_i() {
        assert_eq!(attach("lie", "ing"), "lying");
    }

    #[test]
    fn plain_attach_when_no_rule_applies() {
        assert_eq!(attach("cat", "s"), "cats");
        assert_eq!(attach("jump", "ed"), "jumped");
        assert_eq!(attach("", "ing"), "ing");
    }
}

#[cfg(test)]
mod plover_tests {
    use super::*;

    /// Plover's own answers for 7,800 words and suffixes, and the part of its word list those
    /// looked at. See `tools/plover-golden`.
    #[test]
    fn suffixes_attach_exactly_as_plover_does() {
        let orthography = Orthography::from_wordlist(include_str!("testdata/orthography_words.txt"));
        assert!(orthography.has_words());
        let cases: Vec<(String, String, String)> =
            serde_json::from_str(include_str!("testdata/orthography_golden.json")).unwrap();
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(word, suffix, expected)| {
                let got = orthography.add_suffix(word, suffix);
                (&got != expected).then(|| format!("{word} + {suffix}: Plover {expected:?}, keymux {got:?}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "{} of {} differ from Plover, such as:\n{}",
            wrong.len(),
            cases.len(),
            wrong.iter().take(25).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn without_a_word_list_the_first_matching_rule_decides() {
        let orthography = Orthography::default();
        assert!(!orthography.has_words());
        assert_eq!(orthography.add_suffix("carry", "s"), "carries");
        assert_eq!(orthography.add_suffix("die", "ing"), "dying");
        assert_eq!(orthography.add_suffix("cat", "s"), "cats");
        assert_eq!(orthography.add_suffix("make", "ing the"), "making the");
    }
}
