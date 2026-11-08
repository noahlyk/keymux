//! `{:engine:unload}`: free the memory the steno dictionaries hold.
//!
//! The dictionaries, their lookup indexes, the plugin instances and every cache are dropped and
//! the memory is handed back to the system. There is no command to load them again: the next
//! stroke of any kind loads them first (see `StenoEngine`), and is then translated exactly as if
//! nothing had happened. The translation history stays, so undo and joins carry on across it.
//! To pick up an edited dictionary, unload and then stroke anything.
//!
//! Like the overlay controls it is a transparent stroke that types nothing and stays off the
//! tape: the meta makes an engine command (`engine_control:engine:unload`) and no text.

use super::action::Action;
use super::formatter::Context;

/// The engine command this meta makes.
pub const COMMAND: &str = "engine_control";

/// The meta's name.
pub const NAME: &str = "engine";

/// Whether an entry's text carries `{:engine:`, which like the engine commands must never
/// repeat when the stroke is held.
#[must_use]
pub fn is_in(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{:engine:")
}

/// Whether `param` is the one verb this meta has.
#[must_use]
pub fn is_unload(param: &str) -> bool {
    param.trim().eq_ignore_ascii_case("unload")
}

/// The action for `{:engine:param}`: no text and the engine command to run, or nothing at all
/// for a verb that isn't `unload`.
#[must_use]
pub fn meta_action(ctx: &Context, param: &str) -> Action {
    if !is_unload(param) {
        return ctx.new_action();
    }
    let mut action = ctx.copy_last_action();
    action.command = Some(format!("{COMMAND}:{NAME}:unload"));
    action
}
