//! `{:outline:last}`, `{:outline:next}` and `{:outline:explain}`: type a stroke's own steno
//! notation as text, to paste into notes, and what it looks up to.
//!
//! `{:outline:last}` types the outline of the last translation: every stroke of it, in the
//! layout's notation, joined with `/` (`SKWRO*EUBGS`, `KAT/-S`). That is the translation the
//! text on screen comes from: the last one in the history that strokes made, so an undo moves it
//! back and these commands themselves never count. With nothing to name it types nothing.
//!
//! `{:outline:next}` arms a one-shot mode: the next stroke does not translate or act (a command,
//! a key combination or an undo in it does not run) and types its own notation instead, then the
//! mode clears. That next stroke can be anything, the arming stroke included: striking it twice
//! types its own notation. To cancel, strike anything and undo what it typed.
//!
//! `{:outline:explain}` arms the same one-shot mode, but the next stroke types its notation and
//! the raw text it looks up to, unexecuted: `+*-G -> {^ing}`. What it shows is that one stroke
//! on its own, looked up once the way a stroke is (the dictionaries, then a plugin's reading,
//! then a suffix key folded off), never a longer multi-stroke entry it may start. A plugin's
//! reading is marked with the plugin's name (`STKPWHR -> word (plugin NAME)`), a plugin that
//! did not answer in time with `(plugin late)`, and a stroke with nothing at all with `(none)`.
//! Asking a plugin runs nothing: its reading is text the engine is given back.
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

/// What one stroke on its own looks up to, for `{:outline:explain}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// An entry in the dictionaries (or a suffix key folded off one), as raw text
    Dictionary(String),
    /// A plugin's reading, and the plugin's name
    Plugin { text: String, plugin: String },
    /// A plugin was asked and did not answer in time
    Late,
    /// Nothing reads the stroke
    None,
}

/// The text `{:outline:explain}` types for a stroke written `notation` that looks up to
/// `entry`: `NOTATION -> ENTRY`, with braces escaped so the entry is typed as it is written and
/// never runs.
#[must_use]
pub fn explanation(notation: &str, entry: &Entry) -> String {
    let shown = match entry {
        Entry::Dictionary(text) => escape(text),
        Entry::Plugin { text, plugin } => format!("{} (plugin {})", escape(text), escape(plugin)),
        Entry::Late => "(plugin late)".to_string(),
        Entry::None => "(none)".to_string(),
    };
    format!("{} -> {shown}", escape(notation))
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
        let entry = Entry::Dictionary("{^ing}".to_string());
        assert_eq!(explanation("+*-G", &entry), "+*-G -> \\{^ing\\}");
        let read = Entry::Plugin {
            text: "word".to_string(),
            plugin: "solver".to_string(),
        };
        assert_eq!(explanation("STKPW", &read), "STKPW -> word (plugin solver)");
        assert_eq!(explanation("-Z", &Entry::None), "-Z -> (none)");
        assert_eq!(explanation("-Z", &Entry::Late), "-Z -> (plugin late)");
    }
}
