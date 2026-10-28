//! Formatting actions, a port of Plover's `plover/formatting.py`.
//!
//! An action is what one piece of a translation does to the text, and the state it leaves for
//! the next piece, so that a dictionary written for Plover types the same text here.
//!
//! A translation such as `{^ing}` or `re{^}` is cut into atoms (text, or one `{...}` meta), and
//! each becomes an [`Action`]. An action carries both the instructions to render it (the text,
//! what to delete first) and the state for whatever follows (attach flags, case, the current
//! word). The text helpers at the bottom define what counts as a word.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// The character put between words
pub const SPACE: &str = " ";

/// How text is cased: for one word, or as a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Case {
    CapFirstWord,
    Lower,
    LowerFirstChar,
    Title,
    Upper,
    UpperFirstWord,
}

impl Case {
    /// The case named as Plover names them (`cap_first_word`), in any letter case.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "cap_first_word" => Some(Self::CapFirstWord),
            "lower" => Some(Self::Lower),
            "lower_first_char" => Some(Self::LowerFirstChar),
            "title" => Some(Self::Title),
            "upper" => Some(Self::Upper),
            "upper_first_word" => Some(Self::UpperFirstWord),
            _ => None,
        }
    }
}

/// Which of two actions applies, decided by the text that comes next (`{=regex/yes/no}`).
#[derive(Debug, Clone, PartialEq)]
pub struct LookAhead {
    pub pattern: String,
    pub action1: Action,
    pub action2: Action,
    /// Whether `action1` is the one in force: the next text matched the pattern
    pub first: bool,
}

impl LookAhead {
    fn matches(&self, text: &str) -> bool {
        // Python's `re.match`: a match has to start at the beginning
        compiled(&self.pattern)
            .and_then(|regex| regex.find(text).ok().flatten())
            .is_some_and(|found| found.start() == 0)
    }

    /// Choose by `text`, the start of what follows. Returns the action now in force.
    pub fn update(&mut self, text: &str) -> Action {
        self.first = self.matches(text);
        self.current().clone()
    }

    #[must_use]
    pub const fn current(&self) -> &Action {
        if self.first {
            &self.action1
        } else {
            &self.action2
        }
    }
}

/// A pattern compiled once and remembered, since dictionaries repeat the same few.
fn compiled(pattern: &str) -> Option<fancy_regex::Regex> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<HashMap<String, Option<fancy_regex::Regex>>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(pattern.to_string())
        .or_insert_with(|| fancy_regex::Regex::new(pattern).ok())
        .clone()
}

/// Instructions and resulting state for one atom of a translation.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    // Previous.
    /// No space between this and the previous action
    pub prev_attach: bool,
    /// Text that has to be deleted for this action
    pub prev_replace: String,
    // Current.
    /// No space between this and the next action if that one is glued too
    pub glue: bool,
    /// The current root word, without prefix and un-cased: context for suffixes
    pub word: Option<String>,
    /// Whether orthography rules apply when a suffix is added to this action
    pub orthography: bool,
    /// What replaces spaces after all other formatting
    pub space_char: String,
    /// Whether the current word is being upper-cased
    pub upper_carry: bool,
    /// The case mode in force
    pub case: Option<Case>,
    /// The text to render. `None` for actions that only change state or send a command.
    pub text: Option<String>,
    /// The space that comes after this action when spaces are placed after the output
    pub trailing_space: String,
    /// Whether the word is complete, so a following stroke may start a new one
    pub word_is_finished: bool,
    /// A key combination to send, in Plover's key combination language
    pub combo: Option<String>,
    /// A command for the steno engine
    pub command: Option<String>,
    // Next.
    /// No space between this and the next action
    pub next_attach: bool,
    /// The case to apply to the next action
    pub next_case: Option<Case>,
    /// Set for a look-ahead action: it stands for whichever of two actions the next text picks
    pub look_ahead: Option<Box<LookAhead>>,
}

impl Default for Action {
    fn default() -> Self {
        Self {
            prev_attach: false,
            prev_replace: String::new(),
            glue: false,
            word: None,
            orthography: true,
            space_char: SPACE.to_string(),
            upper_carry: false,
            case: None,
            text: None,
            trailing_space: String::new(),
            word_is_finished: true,
            combo: None,
            command: None,
            next_attach: false,
            next_case: None,
            look_ahead: None,
        }
    }
}

impl Action {
    /// The action in force: this one, or for a look-ahead the alternative it has picked.
    #[must_use]
    pub fn effective(&self) -> &Self {
        self.look_ahead.as_ref().map_or(self, |look| look.current())
    }

    /// A copy of the state that carries on to the next action, with nothing to render.
    #[must_use]
    pub fn copy_state(&self) -> Self {
        let this = self.effective();
        Self {
            prev_attach: this.next_attach,
            case: this.case,
            glue: this.glue,
            orthography: this.orthography,
            space_char: this.space_char.clone(),
            upper_carry: this.upper_carry,
            word: this.word.clone(),
            trailing_space: this.trailing_space.clone(),
            word_is_finished: this.word_is_finished,
            next_attach: this.next_attach,
            next_case: this.next_case,
            ..Self::default()
        }
    }

    /// A new action that only keeps the global state: space character, case mode, trailing space.
    #[must_use]
    pub fn new_state(&self) -> Self {
        let this = self.effective();
        Self {
            prev_attach: this.next_attach,
            space_char: this.space_char.clone(),
            case: this.case,
            trailing_space: this.trailing_space.clone(),
            ..Self::default()
        }
    }

    /// A look-ahead between two actions, undecided until the next text is known.
    #[must_use]
    pub fn look_ahead(pattern: String, action1: Self, action2: Self) -> Self {
        let mut look = LookAhead {
            pattern,
            action1,
            action2,
            first: false,
        };
        look.update("");
        Self {
            look_ahead: Some(Box::new(look)),
            ..Self::default()
        }
    }

    /// The text of the action in force, if it has any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.effective().text.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------
// Words and text, as Plover's formatter sees them
// ---------------------------------------------------------------------------------------------

fn word_regex() -> &'static Regex {
    static WORD: OnceLock<Regex> = OnceLock::new();
    WORD.get_or_init(|| {
        Regex::new(r"(?:\d+(?:[.,]\d+)+|['\w]+[-\w']*|[^\w\s]+)\s*").expect("word pattern")
    })
}

fn word_start_regex() -> &'static Regex {
    static WORD: OnceLock<Regex> = OnceLock::new();
    WORD.get_or_init(|| {
        Regex::new(r"^(?:\d+(?:[.,]\d+)+|['\w]+[-\w']*|[^\w\s]+)\s*").expect("word pattern")
    })
}

fn fragment_regex() -> &'static Regex {
    static FRAGMENT: OnceLock<Regex> = OnceLock::new();
    FRAGMENT.get_or_init(|| Regex::new(r"\s*[^\s]+\s*|^\s*$").expect("fragment pattern"))
}

/// Every word in `s`, each with the whitespace after it.
#[must_use]
pub fn words(s: &str) -> Vec<&str> {
    word_regex().find_iter(s).map(|m| m.as_str()).collect()
}

/// The last word of `s`, or "" if `s` ends in whitespace or has no words.
#[must_use]
pub fn rightmost_word(s: &str) -> String {
    match words(s).last() {
        Some(last) if !last.chars().last().is_some_and(char::is_whitespace) => (*last).to_string(),
        _ => String::new(),
    }
}

/// Whether `s` has more than one word, or whitespace at either end.
#[must_use]
pub fn has_word_boundary(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().is_some_and(char::is_whitespace);
    let last = s.chars().last().is_some_and(char::is_whitespace);
    first || last || words(s).len() > 1
}

/// Python's `str.isspace`: not empty, and nothing but whitespace.
#[must_use]
pub fn is_space(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_whitespace)
}

/// Python's `str.isdigit` for the digits a dictionary has.
#[must_use]
pub fn is_digits(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || (c.is_numeric() && !c.is_alphabetic()))
}

/// Capitalize the first letter of `s`: `foo bar` is `Foo bar` and `STUFF` stays as it is.
#[must_use]
pub fn capitalize_first_word(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut out: String = first.to_uppercase().collect();
        out.push_str(chars.as_str());
        out
    })
}

/// Lowercase the first letter of `s`.
#[must_use]
pub fn lower_first_character(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut out: String = first.to_lowercase().collect();
        out.push_str(chars.as_str());
        out
    })
}

/// Uppercase the first word of `s`: `cat dog` is `CAT dog`.
#[must_use]
pub fn upper_first_word(s: &str) -> String {
    word_start_regex().find(s).map_or_else(
        || s.to_string(),
        |first| format!("{}{}", first.as_str().to_uppercase(), &s[first.end()..]),
    )
}

/// Capitalize each word of `s`, lowercasing the rest of it: `O'Something STUFF` is
/// `O'something Stuff`. Python's `string.capwords(s, " ")`.
#[must_use]
pub fn capitalize_all_words(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                let mut out: String = first.to_uppercase().collect();
                out.push_str(&chars.as_str().to_lowercase());
                out
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Apply a one-off case to `text`.
#[must_use]
pub fn apply_case(text: &str, case: Option<Case>) -> String {
    match case {
        None => text.to_string(),
        Some(Case::CapFirstWord) => capitalize_first_word(text),
        Some(Case::LowerFirstChar) => lower_first_character(text),
        Some(Case::UpperFirstWord) => upper_first_word(text),
        // Modes, not one-off cases: leave the text alone
        Some(Case::Lower | Case::Title | Case::Upper) => text.to_string(),
    }
}

fn apply_mode_case(text: &str, case: Option<Case>, appended: bool) -> String {
    match case {
        Some(Case::Lower) => text.to_lowercase(),
        Some(Case::Upper) => text.to_uppercase(),
        // Output that is joined onto a word is left as it is
        Some(Case::Title) if appended => text.to_string(),
        Some(Case::Title) => capitalize_all_words(text),
        _ => text.to_string(),
    }
}

/// Apply the case mode and the space character to `text`.
#[must_use]
pub fn apply_mode(
    text: &str,
    case: Option<Case>,
    space_char: &str,
    begin: bool,
    last_action: &Action,
) -> String {
    // Should title case be applied to the beginning of the next string?
    let lower_title_case = begin
        && !matches!(
            last_action.case,
            Some(Case::CapFirstWord | Case::UpperFirstWord)
        );
    let mut text = apply_mode_case(text, case, lower_title_case);
    if space_char != SPACE {
        text = text.replace(SPACE, space_char);
    }
    // Title case is sensitive to the lower flag
    if last_action.next_case == Some(Case::LowerFirstChar)
        && !text.is_empty()
        && case == Some(Case::Title)
    {
        text = lower_first_character(&text);
    }
    text
}

// ---------------------------------------------------------------------------------------------
// Looking back over what was typed
// ---------------------------------------------------------------------------------------------

/// The last text fragments, last first, of the text the actions (last first) produced. A
/// fragment is a run of non-whitespace followed by the whitespace after it.
#[must_use]
pub fn last_fragments(actions: &[&Action]) -> Vec<String> {
    let mut found = Vec::new();
    let mut replace = 0usize;
    let mut next: Option<&Action> = None;
    let mut current = String::new();
    for action in actions {
        let this = action.effective();
        let mut part = this.text.clone().unwrap_or_default();
        if let Some(next) = next.map(Action::effective) {
            if next.text.is_some() && !next.prev_attach {
                part.push_str(&next.space_char);
            }
        }
        if replace > 0 {
            // Ignore what a later action replaced
            let len = part.chars().count();
            if len > replace {
                part = part.chars().take(len - replace).collect();
                replace = 0;
            } else {
                replace -= len;
                part.clear();
            }
        }
        if !part.is_empty() {
            // Find the new complete fragments
            let combined = format!("{part}{current}");
            let fragments: Vec<&str> = fragment_regex()
                .find_iter(&combined)
                .map(|m| m.as_str())
                .collect();
            found.extend(fragments.iter().skip(1).rev().map(|f| (*f).to_string()));
            current = fragments
                .first()
                .map_or_else(String::new, |f| (*f).to_string());
        }
        replace += this.prev_replace.chars().count();
        next = Some(action);
    }
    // The first fragment, which nothing came before
    if !is_space(&current) {
        found.push(current.trim_start().to_string());
    }
    found
}

/// The last `count` words typed, in the order they were typed, each with the whitespace after it.
#[must_use]
pub fn last_words(actions: &[&Action], count: usize) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    'fragments: for fragment in last_fragments(actions) {
        for word in words(&fragment).into_iter().rev() {
            found.insert(0, word.to_string());
            if found.len() == count {
                break 'fragments;
            }
        }
    }
    found
}

/// The last word typed, with the whitespace after it.
#[must_use]
pub fn last_word(actions: &[&Action]) -> Option<String> {
    last_fragments(actions)
        .iter()
        .find_map(|fragment| words(fragment).last().map(|word| (*word).to_string()))
}

/// The last `count` fragments typed, in the order they were typed.
#[must_use]
pub fn last_fragments_in_order(actions: &[&Action], count: usize) -> Vec<String> {
    let mut found: Vec<String> = last_fragments(actions).into_iter().take(count).collect();
    found.reverse();
    found
}

/// The last `size` characters typed.
#[must_use]
pub fn last_text(actions: &[&Action], size: usize) -> String {
    if size == 0 {
        return String::new();
    }
    let mut text = String::new();
    for fragment in last_fragments(actions) {
        text = format!("{fragment}{text}");
        if text.chars().count() >= size {
            break;
        }
    }
    let len = text.chars().count();
    text.chars().skip(len.saturating_sub(size)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_include_trailing_space_and_split_numbers_and_punctuation() {
        assert_eq!(
            words("Something something, blah! Blah: 45.8... (blah: foo42)   "),
            [
                "Something ",
                "something",
                ", ",
                "blah",
                "! ",
                "Blah",
                ": ",
                "45.8",
                "... ",
                "(",
                "blah",
                ": ",
                "foo42",
                ")   "
            ]
        );
    }

    #[test]
    fn the_rightmost_word_is_empty_after_whitespace() {
        assert_eq!(rightmost_word("good morning"), "morning");
        assert_eq!(rightmost_word("good morning "), "");
        assert_eq!(rightmost_word(""), "");
    }

    #[test]
    fn word_boundaries() {
        assert!(!has_word_boundary(""));
        assert!(!has_word_boundary("cat"));
        // Punctuation is a word of its own in Plover's reckoning, so `-to-` is three
        assert!(has_word_boundary("-to-"));
        assert!(has_word_boundary(" x"));
        assert!(has_word_boundary("a b"));
    }

    #[test]
    fn case_helpers() {
        assert_eq!(capitalize_first_word("foo bar"), "Foo bar");
        assert_eq!(capitalize_first_word("STUFF"), "STUFF");
        assert_eq!(lower_first_character("Plover"), "plover");
        assert_eq!(upper_first_word("cat dog"), "CAT dog");
        assert_eq!(
            capitalize_all_words("O'Something STUFF"),
            "O'something Stuff"
        );
    }

    #[test]
    fn a_new_action_after_an_attaching_one_attaches() {
        let attaching = Action {
            next_attach: true,
            ..Action::default()
        };
        assert!(attaching.new_state().prev_attach);
        assert!(!Action::default().new_state().prev_attach);
    }
}
