//! `{:engine:unload}`: free the memory the steno dictionaries hold.
//!
//! The dictionaries, their lookup indexes, the plugin instances and every cache are dropped and
//! the memory is handed back to the system. There is no command to load them again: the next
//! stroke of any kind loads them first (see `StenoEngine`), and is then translated exactly as if
//! nothing had happened. The translation history stays, so undo and joins carry on across it.
//! To pick up an edited dictionary, unload and then stroke anything.
//!
//! `{:engine:suspend}` and `{:engine:toggle}` leave the steno layer (the legacy `{PLOVER:SUSPEND}`
//! and `{PLOVER:TOGGLE}` do the same); the layer's own keys turn steno on again.
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

/// Whether `param` is the `unload` verb.
#[must_use]
pub fn is_unload(param: &str) -> bool {
    param.trim().eq_ignore_ascii_case("unload")
}

/// Whether an entry's text unloads the dictionaries, which makes its stroke transparent. The
/// leave-the-layer verbs are plain commands, as their legacy spellings are.
#[must_use]
pub fn is_transparent(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{:engine:unload")
}

/// The action for `{:engine:param}`: no text and the engine command to run, or nothing at all
/// for a verb that isn't `unload`, `suspend` or `toggle`.
#[must_use]
pub fn meta_action(ctx: &Context, param: &str) -> Action {
    let verb = param.trim().to_ascii_lowercase();
    let command = match verb.as_str() {
        "unload" => format!("{COMMAND}:{NAME}:unload"),
        "suspend" | "toggle" => verb,
        _ => return ctx.new_action(),
    };
    let mut action = ctx.copy_last_action();
    action.command = Some(command);
    action
}
