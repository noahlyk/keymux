//! A mod: one key always continues the outline `{:outline:next}` or `{:outline:explain}` is
//! showing, so a word built with it is named whole even where the engine's own grouping would
//! start a new translation with that stroke.
//!
//! keymux has no notion of a key that continues an outline. This plugin gives it one through the
//! optional `outline_continues` export (see `src/steno/plugin.rs`), and has no `alloc` or
//! `fallback`: it never reads a word, and it changes nothing about how strokes translate. It is
//! asked only while an outline is being followed. Drop it in the steno folder like a dictionary
//! file. Which key continues is the stroke bit below: bit 23 is a layer's first `extra_keys`
//! entry (a theory's join key `+`, say).

/// The continuing key's stroke bit.
const JOIN: i64 = 1 << 23;

/// Asked for each stroke struck while an outline is followed. A stroke with the key down is part
/// of that outline.
#[no_mangle]
pub extern "C" fn outline_continues(stroke: i64) -> i32 {
    i32::from(stroke & JOIN != 0)
}
