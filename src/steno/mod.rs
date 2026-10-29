//! Stenography support for steno layers.
//!
//! A layer with `kind: Steno(...)` captures its stroke keys, groups them into
//! chords, and types the dictionary text for each completed stroke. Plover's dictionaries
//! provide the translations, and a dictionary written for Plover is meant to work here as it
//! does there: the translation language, the translator, the orthography and the dictionary
//! formats are ports of Plover's own, checked against it (see `tools/plover-golden`).

pub mod action;
pub mod chord;
pub mod commands;
pub mod dict;
pub mod engine;
pub mod formatter;
pub mod journal;
pub mod keycombo;
pub mod layout;
pub mod live;
pub mod lookup;
pub mod meta;
pub mod numbers;
pub mod orthography;
pub mod overlay;
pub mod plugin;
pub mod reverse;
pub mod rtf;
pub mod setup;
pub mod shortest;
pub mod source;
pub mod tape;
pub mod translate;
pub mod typing;

#[cfg(test)]
mod lapwing_tests;
#[cfg(test)]
mod plover_tests;

pub use engine::StenoEngine;
