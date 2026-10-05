//! Stenography support for steno layers.
//!
//! A layer with `kind: Steno(...)` captures its stroke keys, groups them into
//! chords, and types the dictionary text for each completed stroke. Plover's JSON
//! dictionaries provide the translations.

pub mod chord;
pub mod commands;
pub mod dict;
pub mod engine;
pub mod format;
pub mod layout;
pub mod numbers;
pub mod orthography;
pub mod setup;
pub mod tape;
pub mod translate;

pub use engine::StenoEngine;
