//! Runtime overlay controls a dictionary entry can carry: `{:overlay:hide}`, `{:tape:toggle}`,
//! `{:keyboard:show}`, `{:tape:clear}`, `{:lookup:toggle}`.
//!
//! The bare `{:lookup}` is not one of these: it makes the engine command `lookup`
//! ([`super::meta`]), the same as the legacy alias `{plover:lookup}`.
//!
//! They change what the overlay shows right now and nothing else: not the `overlay:` config,
//! not what it shows when it starts, not any file. The state lives in the running overlay
//! process ([`ViewState`]), so a restart of it goes back to the configured start, and a config
//! reload leaves it alone. With no overlay running they do nothing. They type nothing: the
//! meta makes an engine command (see `Engine::run_command`) and no text.

use super::action::Action;
use super::formatter::Context;
use serde::{Deserialize, Serialize};

/// A piece of the overlay that can be shown or hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Part {
    /// The whole panel
    Overlay,
    /// The tape only (an open lookup still shows: it is a thing you asked for)
    Tape,
    /// The keyboard diagram only
    Keyboard,
}

/// What to do to a [`Part`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verb {
    Show,
    Hide,
    Toggle,
}

/// A view command as it travels from the daemon to the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewCommand {
    pub part: Part,
    pub verb: Verb,
}

/// A control as a dictionary writes it, once parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Show, hide or toggle a part of the overlay (runs in the overlay process)
    View(ViewCommand),
    /// Empty the overlay's tape (the tape file keeps its lines)
    ClearTape,
    /// Open, close or toggle the live lookup
    Lookup(Verb),
}

/// The engine command these metas make, `overlay_control:tape:clear`. It has a name of its own
/// because `lookup` is also the engine command `{:lookup}` opens (and its legacy alias `{PLOVER:LOOKUP}`).
pub const COMMAND: &str = "overlay_control";

/// The names the metas go by, which `Translator::is_command` also uses to tell them from text.
pub const NAMES: [&str; 4] = ["overlay", "tape", "keyboard", "lookup"];

/// Whether an entry's text carries one of these metas (`{:overlay:`, ...) or the bare `{:lookup}`,
/// which like the engine commands must never repeat when the stroke is held.
#[must_use]
#[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
pub fn is_in(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("{:lookup}")
        || NAMES
            .iter()
            .any(|name| lower.contains(&format!("{{:{name}:")))
}

/// The control `name` and `param` mean (`tape`, `clear`), or `None` for anything else.
#[must_use]
pub fn parse(name: &str, param: &str) -> Option<Control> {
    let param = param.trim().to_ascii_lowercase();
    let verb = match param.as_str() {
        "show" => Some(Verb::Show),
        "hide" => Some(Verb::Hide),
        "toggle" => Some(Verb::Toggle),
        _ => None,
    };
    let view = |part| verb.map(|verb| Control::View(ViewCommand { part, verb }));
    match name.trim().to_ascii_lowercase().as_str() {
        "overlay" => view(Part::Overlay),
        "keyboard" => view(Part::Keyboard),
        "tape" if param == "clear" => Some(Control::ClearTape),
        "tape" => view(Part::Tape),
        "lookup" => match param.as_str() {
            "open" => Some(Control::Lookup(Verb::Show)),
            "close" => Some(Control::Lookup(Verb::Hide)),
            "toggle" => Some(Control::Lookup(Verb::Toggle)),
            _ => None,
        },
        _ => None,
    }
}

/// The action for `{:name:param}` of these controls: no text, and the engine command
/// `name:param` to run. A control that isn't one (`{:tape:sideways}`) does nothing at all.
#[must_use]
pub fn meta_action(ctx: &Context, name: &str, param: &str) -> Action {
    if parse(name, param).is_none() {
        return ctx.new_action();
    }
    let mut action = ctx.copy_last_action();
    action.command = Some(format!("{COMMAND}:{}:{}", name.trim(), param.trim()));
    action
}

/// What the overlay shows now, apart from what its config and the steno layer decide. Starts
/// with everything shown; only controls change it, and it is never saved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewState {
    pub hidden: bool,
    pub tape_hidden: bool,
    pub keyboard_hidden: bool,
}

impl ViewState {
    /// Apply one command. Returns whether anything changed, so repeating one is free.
    pub const fn apply(&mut self, command: ViewCommand) -> bool {
        let flag = match command.part {
            Part::Overlay => &mut self.hidden,
            Part::Tape => &mut self.tape_hidden,
            Part::Keyboard => &mut self.keyboard_hidden,
        };
        let hide = match command.verb {
            Verb::Show => false,
            Verb::Hide => true,
            Verb::Toggle => !*flag,
        };
        let changed = *flag != hide;
        *flag = hide;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(part: Part, verb: Verb) -> ViewCommand {
        ViewCommand { part, verb }
    }

    #[test]
    fn every_control_parses() {
        let view = |part, verb| Some(Control::View(cmd(part, verb)));
        assert_eq!(parse("overlay", "show"), view(Part::Overlay, Verb::Show));
        assert_eq!(parse("overlay", "hide"), view(Part::Overlay, Verb::Hide));
        assert_eq!(parse("overlay", "toggle"), view(Part::Overlay, Verb::Toggle));
        assert_eq!(parse("tape", "show"), view(Part::Tape, Verb::Show));
        assert_eq!(parse("tape", "hide"), view(Part::Tape, Verb::Hide));
        assert_eq!(parse("tape", "toggle"), view(Part::Tape, Verb::Toggle));
        assert_eq!(parse("tape", "clear"), Some(Control::ClearTape));
        assert_eq!(parse("keyboard", "show"), view(Part::Keyboard, Verb::Show));
        assert_eq!(parse("keyboard", "hide"), view(Part::Keyboard, Verb::Hide));
        assert_eq!(parse("keyboard", "toggle"), view(Part::Keyboard, Verb::Toggle));
        assert_eq!(parse("lookup", "open"), Some(Control::Lookup(Verb::Show)));
        assert_eq!(parse("lookup", "close"), Some(Control::Lookup(Verb::Hide)));
        assert_eq!(parse("lookup", "toggle"), Some(Control::Lookup(Verb::Toggle)));
        // Case does not matter, as with every other meta
        assert_eq!(parse("Overlay", "HIDE"), view(Part::Overlay, Verb::Hide));
    }

    #[test]
    fn what_is_not_a_control_does_not_parse() {
        for (name, param) in [
            ("overlay", ""),
            ("overlay", "clear"),
            ("overlay", "open"),
            ("keyboard", "clear"),
            ("tape", "open"),
            ("lookup", "show"),
            ("lookup", "clear"),
            ("window", "hide"),
        ] {
            assert_eq!(parse(name, param), None, "{name}:{param}");
        }
    }

    #[test]
    fn show_hide_and_toggle_are_idempotent_where_they_should_be() {
        let mut view = ViewState::default();
        assert!(!view.apply(cmd(Part::Overlay, Verb::Show)), "already shown");
        assert!(view.apply(cmd(Part::Overlay, Verb::Hide)));
        assert!(!view.apply(cmd(Part::Overlay, Verb::Hide)), "already hidden");
        assert!(view.hidden);
        assert!(view.apply(cmd(Part::Overlay, Verb::Toggle)));
        assert!(!view.hidden);
        assert!(view.apply(cmd(Part::Overlay, Verb::Toggle)));
        assert!(view.hidden);
        assert!(view.apply(cmd(Part::Overlay, Verb::Show)));
        assert!(!view.hidden);
    }

    #[test]
    fn the_tape_and_the_keyboard_are_hidden_apart_from_the_whole_overlay() {
        let mut view = ViewState::default();
        view.apply(cmd(Part::Tape, Verb::Hide));
        assert_eq!(
            view,
            ViewState {
                tape_hidden: true,
                ..ViewState::default()
            }
        );
        view.apply(cmd(Part::Keyboard, Verb::Toggle));
        assert!(view.keyboard_hidden && view.tape_hidden && !view.hidden);
        // Hiding and showing the whole overlay remembers which parts were hidden
        view.apply(cmd(Part::Overlay, Verb::Hide));
        view.apply(cmd(Part::Overlay, Verb::Show));
        assert!(view.keyboard_hidden && view.tape_hidden && !view.hidden);
    }
}
