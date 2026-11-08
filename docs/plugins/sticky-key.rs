//! A mod: one key made sticky. Held down, it stays in every stroke while the other keys come and
//! go, each stroke sent as its other keys come up; letting go of it alone sends nothing.
//!
//! keymux has no notion of a sticky key. This plugin gives it one through the optional
//! `send_early` export (see `src/steno/plugin.rs`), and has no `alloc` or `fallback`: it never
//! reads a word, it only changes when a stroke is sent. Drop it in the steno folder like a
//! dictionary file. Which key is sticky is the stroke bit below: bit 23 is a layer's first
//! `extra_keys` entry (a theory's join key `+`, say, with `join_keys: ['+']`), so a theory that
//! joins strokes with `+` can hold `+` across stroke after stroke instead of striking it in each.

/// The sticky key's stroke bit.
const STICKY: i64 = 1 << 23;

/// Asked each time a stroke key comes up while others are still down. Send the stroke now when
/// the sticky key is the only one left down and the stroke has more than it.
#[no_mangle]
pub extern "C" fn send_early(held: i64, stroke: i64) -> i32 {
    i32::from(held == STICKY && stroke & !STICKY != 0)
}
