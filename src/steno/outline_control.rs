//! `{:outline:last}`, `{:outline:next}` and `{:outline:explain}`: type a stroke's own steno
//! notation as text, to paste into notes, and what it looks up to.
//!
//! `{:outline:last}` types the outline of the last translation: every stroke of it, in the
//! layout's notation, joined with `/` (`SKWRO*EUBGS`, `KAT/-S`). That is the translation the
//! text on screen comes from: the last one in the history that strokes made, so an undo moves it
//! back and these commands themselves never count. With nothing to name it types nothing.
//!
//! `{:outline:next}` arms a mode that follows the next translation: the strokes after it
//! translate as ever, but silently (nothing they would type, run or press happens), only to see
//! how the engine groups them, and the notation of that translation is typed instead. A stroke
//! that grows it (the next stroke of a multi-stroke entry, `KPA/TKAOEU`, or a stroke a plugin
//! joins onto the word before, `TPHAT/+STKPWAEUGS`) types the whole outline again in place of the
//! old one, the engine's usual taking back and typing over. The first stroke that starts a
//! translation of its own ends the mode and is translated as usual. A mod can make some strokes
//! always continue the outline (`outline_continues`, see [`super::plugin`]). A first stroke that
//! makes no translation of its own (a transparent one such as the arming stroke itself, or a macro
//! such as undo) types its own notation once, without running, and the mode clears: striking the
//! arming stroke twice types its own notation. To cancel, strike anything and undo what it typed.
//!
//! `{:outline:explain}` does the same, typing the outline and the raw text of the entry it looks
//! up to, unexecuted: `+*-G -> {^ing}`, `KPA/TKAOEU -> today`. An entry a plugin read is shown as
//! its plain text, a stroke a plugin did not answer in time as `(plugin late)`, and a stroke with
//! nothing at all as `(none)`. Where a mod continued the outline past what the engine grouped, the
//! entries of its translations are shown in order, separated by spaces. A first stroke with no
//! translation of its own shows that one stroke on its own, looked up once the way a stroke is.
//! Asking a plugin runs nothing: its reading is text the engine is given back.
//!
//! The typed text is one word to undo, whatever it replaced: `*` takes the whole explanation
//! back. A `{:surround:next:...}` armed before the command wraps it whole, and keeps wrapping it
//! each time it is typed again. Following costs nothing while nothing is armed.
//!
//! All three are transparent strokes like the overlay controls: they stay out of the history and
//! keep the spacing and any pending join around them. The notation they type is a translation of
//! its own with no strokes: it is spaced and capitalised like a word, typed literally (braces in
//! an entry are text, not metas), can be undone like one, and never counts as the last outline.
//! Like the other controls they never repeat when held.

use super::action::Action;
use super::formatter::Context;

/// The engine command this meta makes: `outline_control:outline:last`, `...:next` or
/// `...:explain`.
pub const COMMAND: &str = "outline_control";

/// The meta's name.
pub const NAME: &str = "outline";

/// What the meta is asked to type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// The outline of the last translation
    Last,
    /// The notation of the next stroke
    Next,
    /// The notation of the next stroke and the entry it looks up to
    Explain,
}

impl Verb {
    /// The verb as the meta spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::Next => "next",
            Self::Explain => "explain",
        }
    }
}

/// What a translation looks up to, for `{:outline:explain}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// An entry in the dictionaries (or a suffix key folded off one), or a plugin's reading, as
    /// raw text
    Found(String),
    /// A plugin was asked and did not answer in time
    Late,
    /// Nothing reads the stroke
    None,
}

/// The text `{:outline:explain}` types for an outline: `NOTATION -> ENTRY`.
///
/// `notation` is the outline written out and `entries` what its translations look up to,
/// separated by spaces, with braces escaped so an entry is typed as it is written and never runs.
#[must_use]
pub fn explanation(notation: &str, entries: &[Entry]) -> String {
    let shown: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            Entry::Found(text) => escape(text),
            Entry::Late => "(plugin late)".to_string(),
            Entry::None => "(none)".to_string(),
        })
        .collect();
    format!("{} -> {}", escape(notation), shown.join(" "))
}

/// Where `{:outline:next}` and `{:outline:explain}` stand.
#[derive(Debug, Default)]
pub enum Mode {
    /// Nothing armed: strokes translate as ever
    #[default]
    Off,
    /// Armed: the next stroke starts the outline to follow
    Armed(Verb),
    /// Following an outline: a stroke that grows it types it again
    Following(Box<Following>),
}

/// An outline being followed.
#[derive(Debug)]
pub struct Following {
    pub verb: Verb,
    /// A copy of the translator the strokes are struck on, to see how they group
    pub shadow: super::translate::Translator,
    /// The text typed for the outline so far
    pub typed: String,
}

/// `text` with every brace escaped, so the formatter types it as text (see
/// [`super::formatter::unescape_atom`]).
fn escape(text: &str) -> String {
    text.replace('{', "\\{").replace('}', "\\}")
}

/// Whether an entry's text carries `{:outline:`, which like the other controls must never repeat
/// when the stroke is held.
#[must_use]
pub fn is_in(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{:outline:")
}

/// The verb `param` names, or `None` for anything else.
#[must_use]
pub fn parse(param: &str) -> Option<Verb> {
    match param.trim().to_ascii_lowercase().as_str() {
        "last" => Some(Verb::Last),
        "next" => Some(Verb::Next),
        "explain" => Some(Verb::Explain),
        _ => None,
    }
}

/// The verb of an engine command `outline_control:outline:last`, as `Engine::run_command` sees
/// it after the first colon is split off (`outline:last`).
#[must_use]
pub fn verb_of(args: &str) -> Option<Verb> {
    parse(args.split_once(':').map_or("", |(_, verb)| verb))
}

/// The action for `{:outline:param}`: no text and the engine command to run, or nothing at all
/// for a verb that isn't `last`, `next` or `explain`.
#[must_use]
pub fn meta_action(ctx: &Context, param: &str) -> Action {
    let Some(verb) = parse(param) else {
        return ctx.new_action();
    };
    let mut action = ctx.copy_last_action();
    action.command = Some(format!("{COMMAND}:{NAME}:{}", verb.as_str()));
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_parse() {
        assert_eq!(parse("last"), Some(Verb::Last));
        assert_eq!(parse(" NEXT "), Some(Verb::Next));
        assert_eq!(parse("explain"), Some(Verb::Explain));
        assert_eq!(parse("sideways"), None);
        assert_eq!(parse(""), None);
        assert_eq!(verb_of("outline:next"), Some(Verb::Next));
        assert_eq!(verb_of("outline:other"), None);
    }

    #[test]
    fn entries_are_recognised() {
        assert!(is_in("{:outline:last}") && is_in("{:Outline:next}"));
        assert!(is_in("{:outline:explain}"));
        assert!(!is_in("{:tape:toggle}"));
    }

    #[test]
    fn an_explanation_escapes_the_entry() {
        let entry = Entry::Found("{^ing}".to_string());
        assert_eq!(explanation("+*-G", &[entry]), "+*-G -> \\{^ing\\}");
        let read = Entry::Found("word".to_string());
        assert_eq!(explanation("STKPW", &[read]), "STKPW -> word");
        assert_eq!(explanation("-Z", &[Entry::None]), "-Z -> (none)");
        assert_eq!(explanation("-Z", &[Entry::Late]), "-Z -> (plugin late)");
        let two = [Entry::Found("national".into()), Entry::None];
        assert_eq!(
            explanation("TPHAT/+-Z", &two),
            "TPHAT/+-Z -> national (none)"
        );
    }
}
