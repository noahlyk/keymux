//! Stenography support for steno layers.
//!
//! A layer with `kind: Steno(...)` captures its stroke keys, groups them into
//! chords, and types the dictionary text for each completed stroke. Plover's JSON
//! dictionaries provide the translations.

pub mod chord;
pub mod dict;
pub mod engine;
pub mod layout;

pub use engine::StenoEngine;
