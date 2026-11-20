//! `{:outline:last}` and `{:outline:next}`: type a stroke's own steno notation as text, to paste
//! into notes.
//!
//! `{:outline:last}` types the outline of the last translation: every stroke of it, in the
//! layout's notation, joined with `/` (`SKWRO*EUBGS`, `KAT/-S`). That is the translation the
//! text on screen comes from: the last one in the history that strokes made, so an undo moves it
//! back and these commands themselves never count. With nothing to name it types nothing.
//!
//! `{:outline:next}` arms a one-shot mode: the next stroke does not translate or act (a command,
//! a key combination or an undo in it does not run) and types its own notation instead, then the
//! mode clears. Striking the arming stroke again disarms it.
//!
//! Both are transparent strokes like the overlay controls: they stay out of the history and keep
//! the spacing and any pending join around them. The notation they type is a translation of its
//! own with no strokes: it is spaced and capitalised like a word, can be undone like one, and
//! never counts as the last outline. Like the other controls they never repeat when held.

use super::action::Action;
use super::formatter::Context;

/// The engine command this meta makes: `outline_control:outline:last` or `...:next`.
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
}

/// Whether an entry's text carries `{:outline:`, which like the other controls must never repeat
/// when the stroke is held.
#[must_use]
pub fn is_in(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{:outline:")
}

/// Whether an entry's text arms the next-stroke mode.
#[must_use]
pub fn is_next(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{:outline:next}")
}

/// The verb `param` names, or `None` for anything else.
#[must_use]
pub fn parse(param: &str) -> Option<Verb> {
    match param.trim().to_ascii_lowercase().as_str() {
        "last" => Some(Verb::Last),
        "next" => Some(Verb::Next),
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
/// for a verb that isn't `last` or `next`.
#[must_use]
pub fn meta_action(ctx: &Context, param: &str) -> Action {
    let Some(verb) = parse(param) else {
        return ctx.new_action();
    };
    let verb = match verb {
        Verb::Last => "last",
        Verb::Next => "next",
    };
    let mut action = ctx.copy_last_action();
    action.command = Some(format!("{COMMAND}:{NAME}:{verb}"));
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_parse() {
        assert_eq!(parse("last"), Some(Verb::Last));
        assert_eq!(parse(" NEXT "), Some(Verb::Next));
        assert_eq!(parse("sideways"), None);
        assert_eq!(parse(""), None);
        assert_eq!(verb_of("outline:next"), Some(Verb::Next));
        assert_eq!(verb_of("outline:other"), None);
    }

    #[test]
    fn entries_are_recognised() {
        assert!(is_in("{:outline:last}") && is_in("{:Outline:next}"));
        assert!(is_next("{:outline:NEXT}") && !is_next("{:outline:last}"));
        assert!(!is_in("{:tape:toggle}"));
    }
}
