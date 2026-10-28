//! One steno layer's runtime state: which physical keys are stroke keys, the
//! chord in progress, and the translator that turns strokes into text.

use super::chord::ChordState;
use super::dict::Dictionary;
use super::layout::{build_key_map, parse_strokes_trimmed, preset};
use super::lookup::{classify, Lookup};
use super::overlay::OverlayHub;
use super::live;
use super::setup::default_dictionaries;
use super::tape;
use super::translate::{OutputOp, StenoOutput, Translator};
use crate::config::{KeyAction, StenoConfig};
use crate::keycode::KeyCode;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Debug)]
pub struct StenoEngine {
    keys: HashMap<KeyCode, u32>,
    /// Stroke keys physically down, so a release can tell which of their bits are still pressed
    held_keys: HashSet<KeyCode>,
    chord: ChordState,
    translator: Translator,
    /// The dictionary, first loaded and then again whenever its files change. `None` when
    /// there is nothing to follow.
    updates: Option<Receiver<Arc<Dictionary>>>,
    /// No dictionary has arrived yet, so strokes wait in `backlog`
    loading: bool,
    /// Strokes finished before the dictionaries arrived. Translated once they do.
    backlog: Vec<u32>,
    /// Where each stroke is recorded. `None` when the home directory is unknown.
    tape: Option<PathBuf>,
    pub disabled_in_game_mode: bool,
    /// Where strokes are published for the on-screen overlay
    hub: &'static OverlayHub,
    /// The strokes that open a live lookup, in order: one stroke, or several (`HRAOUP/AER`).
    /// Empty when no lookup is configured.
    lookup_trigger: Vec<u32>,
    /// The leading strokes of the lookup trigger written so far, held back unspoken: they might
    /// be the start of the trigger, so they are neither typed nor on the tape until the next
    /// stroke shows whether they are. Always shorter than `lookup_trigger`.
    trigger_held: Vec<u32>,
    /// The lookup that is open right now. While it is, keys are QWERTY input to it.
    lookup: Option<Lookup>,
    /// A stroke wrote `{plover:toggle}` or `{plover:suspend}`: the layer should be left
    leave_requested: bool,
}

impl StenoEngine {
    /// Build an engine from a layer's steno config.
    ///
    /// Dictionaries load on a background thread, so the daemon doesn't wait on
    /// a large file. Until they arrive, strokes wait in the chord. After that they follow their
    /// files: when one changes, the new dictionary is swapped in between two strokes.
    ///
    /// `home` is the session user's home directory, used to expand `~/` in
    /// dictionary paths and to place the tape. Relative dictionary paths are
    /// resolved against `config_dir`.
    pub fn from_config(
        config: &StenoConfig,
        disabled_in_game_mode: bool,
        config_dir: &Path,
        home: Option<&Path>,
    ) -> Result<Self> {
        let preset = preset(&config.layout).map_err(|e| anyhow!(e))?;
        let overrides: HashMap<String, KeyAction> = config.layout_overrides.clone();
        let keys =
            build_key_map(&preset, &overrides).map_err(|errors| anyhow!(errors.join("; ")))?;

        // Without an explicit list, read the whole steno folder. Each entry is a file or a folder.
        let roots: Vec<PathBuf> = if config.dictionaries.is_empty() {
            default_dictionaries(config_dir)
        } else {
            config
                .dictionaries
                .iter()
                .map(|raw| resolve_dictionary_path(raw, config_dir, home))
                .collect()
        };
        let updates = live::subscribe(roots, super::setup::cache_dir(config_dir));

        let mut translator = Translator::new(Dictionary::default());
        translator.set_options(super::formatter::Options {
            spaces_after: config.spaces_after,
            start_attached: config.start_attached,
            start_capitalized: config.start_capitalized,
            ..super::formatter::Options::default()
        });
        Ok(Self {
            keys,
            held_keys: HashSet::new(),
            chord: ChordState::new(Duration::from_millis(u64::from(config.stroke_timeout_ms))),
            translator,
            updates: Some(updates),
            loading: true,
            backlog: Vec::new(),
            tape: home.map(tape::default_path),
            disabled_in_game_mode,
            hub: OverlayHub::global(),
            lookup_trigger: config
                .lookup_stroke
                .as_deref()
                .map(|text| {
                    // The empty stroke of a word boundary entry is no stroke you can press
                    parse_strokes_trimmed(text).filter(|strokes| !strokes.contains(&0)).unwrap_or_else(|| {
                        warn!(
                            "steno lookup_stroke {text:?} is not a stroke, or strokes separated \
                             by `/`, in Plover notation, so lookup is off"
                        );
                        Vec::new()
                    })
                })
                .unwrap_or_default(),
            trigger_held: Vec::new(),
            lookup: None,
            leave_requested: false,
        })
    }

    /// Whether a live lookup is open. While it is, the layer's keys are plain QWERTY input to
    /// it (see [`Self::lookup_key`]) and nothing reaches the application.
    #[must_use]
    pub const fn lookup_active(&self) -> bool {
        self.lookup.is_some()
    }

    /// A key went down during an open lookup. `shift` is whether Shift is held. It types into
    /// the query, edits it, or closes the lookup, and updates the overlay.
    pub fn lookup_key(&mut self, key: KeyCode, shift: bool) {
        let Some(lookup) = self.lookup.as_mut() else {
            return;
        };
        if lookup.apply(classify(key, shift)) {
            self.publish_lookup();
        } else {
            self.lookup = None;
            self.hub.set_lookup(None);
        }
    }

    fn open_lookup(&mut self) {
        self.lookup = Some(Lookup::default());
        // The stroke that opened it is done: nothing is lit while you type a word
        self.hub.set_keys(0, 0);
        self.publish_lookup();
    }

    /// Show what the lookup has found so far on the overlay.
    fn publish_lookup(&self) {
        if let Some(lookup) = &self.lookup {
            self.hub
                .set_lookup(Some(lookup.view(&self.translator.fresh())));
        }
    }

    /// Publish to `hub` instead of the daemon's. Tests use this so they don't share state.
    #[cfg(test)]
    pub const fn set_hub(&mut self, hub: &'static OverlayHub) {
        self.hub = hub;
    }

    /// Whether there is no real dictionary: no theory is installed, and what is in the steno
    /// folder is only the starter entries or a few of your own. Still loading counts as set
    /// up, so the hint doesn't fire during startup.
    #[must_use]
    pub fn needs_setup(&self) -> bool {
        !self.loading && self.translator.entry_count() < super::setup::MIN_REAL_DICTIONARY
    }

    /// Whether this physical key is a stroke key on this layer.
    #[must_use]
    pub fn owns(&self, key: KeyCode) -> bool {
        self.keys.contains_key(&key)
    }

    /// Start a fresh sentence, as when the layer is entered. See [`Translator::start_fresh`].
    pub fn start_fresh(&mut self) {
        self.translator.start_fresh();
        // A lookup left open when the layer was left must not still be eating keys
        self.lookup = None;
        // Half a lookup trigger written before leaving the layer is not carried over
        self.trigger_held.clear();
    }

    /// A stroke key went down.
    pub fn press(&mut self, key: KeyCode, now: Instant) {
        if let Some(&bit) = self.keys.get(&key) {
            self.held_keys.insert(key);
            self.chord.press(bit, now);
            self.publish_keys();
        }
    }

    /// Show the stroke in progress on the overlay.
    fn publish_keys(&self) {
        self.hub.set_keys(self.chord.pending(), self.chord.held());
    }

    /// A stroke key came up. Returns output if this completed a stroke.
    pub fn release(&mut self, key: KeyCode) -> Option<StenoOutput> {
        let bit = *self.keys.get(&key)?;
        self.held_keys.remove(&key);
        // A combined key (an override binding one physical key to several stroke keys at
        // once) shares bits with the keys still down, so keep those
        let still_held: u32 = self
            .held_keys
            .iter()
            .filter_map(|other| self.keys.get(other))
            .fold(0, |bits, &other| bits | other);
        let Some(stroke) = self.chord.release(bit & !still_held) else {
            self.publish_keys();
            return None;
        };
        self.on_stroke(stroke)
    }

    /// Check the stuck-key timeout and pick up finished dictionary loads.
    /// Returns output if a stuck stroke was flushed, or if strokes queued during
    /// loading were just translated.
    pub fn tick(&mut self, now: Instant) -> Option<StenoOutput> {
        if let Some(output) = self.poll_dictionary() {
            return Some(output);
        }
        let stroke = self.chord.tick(now)?;
        self.on_stroke(stroke)
    }

    /// Swap in a dictionary when one arrives: the first, after which the strokes queued while
    /// waiting for it are translated, or a newer one after its files changed.
    fn poll_dictionary(&mut self) -> Option<StenoOutput> {
        let updates = self.updates.take()?;
        // Only the newest matters if several came in
        let mut newest = None;
        let mut closed = false;
        loop {
            match updates.try_recv() {
                Ok(dict) => newest = Some(dict),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            // Nothing will arrive any more. Steno stays without a dictionary instead of
            // waiting for one forever.
            if self.loading && newest.is_none() {
                newest = Some(Arc::new(Dictionary::default()));
            }
        } else {
            self.updates = Some(updates);
        }
        self.translator.set_shared(newest?);
        // A dictionary that replaces a working one leaves nothing waiting
        if !std::mem::replace(&mut self.loading, false) {
            return None;
        }
        let mut merged = Merged::default();
        for stroke in std::mem::take(&mut self.backlog) {
            if let Some(output) = self.translate(stroke) {
                merged.push(output);
            }
        }
        merged.finish()
    }

    fn on_stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        if self.lookup_trigger.is_empty() {
            return self.type_stroke(stroke);
        }
        // With a lookup configured the stroke may be (the start of) its trigger. Strokes that
        // turn out not to be are typed, possibly several at once, so they combine into one edit.
        let mut merged = Merged::default();
        self.feed(stroke, &mut merged);
        merged.finish()
    }

    /// Match `stroke` against the lookup trigger. The trigger's strokes open a lookup and are
    /// not typed. A stroke that continues a partial match is held back. Anything else is an
    /// ordinary stroke: first any held-back strokes (they weren't the trigger after all), then
    /// this one, each checked again, since the trigger might start part-way through them.
    fn feed(&mut self, stroke: u32, merged: &mut Merged) {
        let matched = self.trigger_held.len();
        if self.lookup_trigger.get(matched) == Some(&stroke) {
            self.trigger_held.push(stroke);
            if self.trigger_held.len() == self.lookup_trigger.len() {
                self.trigger_held.clear();
                self.open_lookup();
            }
            return;
        }
        if self.trigger_held.is_empty() {
            if let Some(output) = self.type_stroke(stroke) {
                merged.push(output);
            }
            return;
        }
        // The held strokes weren't the trigger. The oldest is an ordinary stroke; the rest, and
        // this one, are looked at again from scratch.
        let mut again = std::mem::take(&mut self.trigger_held);
        again.push(stroke);
        let first = again.remove(0);
        if let Some(output) = self.type_stroke(first) {
            merged.push(output);
        }
        for stroke in again {
            self.feed(stroke, merged);
        }
    }

    /// A stroke that is not part of a lookup trigger: translate it, or queue it until the
    /// dictionaries have loaded.
    fn type_stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        if self.loading {
            self.backlog.push(stroke);
            self.publish_keys();
            return None;
        }
        self.translate(stroke)
    }

    fn translate(&mut self, stroke: u32) -> Option<StenoOutput> {
        let output = self.translator.stroke(stroke);
        let output = self.run_commands(output);
        let unmatched = self.translator.was_unmatched();
        let description = tape::describe(output.as_ref(), unmatched);
        if let Some(path) = &self.tape {
            tape::append(path, stroke, &description);
        }
        // Same description as the tape file, so the overlay and `keymux steno tape` agree
        self.hub.push_stroke(stroke, description, unmatched);
        output
    }

    /// Run the engine commands (`{plover:...}`) in an output, and return the rest of it.
    fn run_commands(&mut self, output: Option<StenoOutput>) -> Option<StenoOutput> {
        let Some(StenoOutput::Ops(ops)) = output else {
            return output;
        };
        let mut rest = Vec::new();
        for op in ops {
            match op {
                OutputOp::Command(command) => self.run_command(&command),
                other => rest.push(other),
            }
        }
        StenoOutput::from_ops(rest)
    }

    /// Run one of Plover's engine commands, as it is written after `{plover:`. What has a
    /// counterpart in keymux is done: `suspend` and `toggle` leave the steno layer (see
    /// [`Self::take_leave_request`]), `lookup` opens the live lookup, `set_config` sets how a
    /// session starts. The rest belong to Plover's windows and do nothing here.
    fn run_command(&mut self, command: &str) {
        let (name, args) = command.split_once(':').unwrap_or((command, ""));
        match name.trim().to_lowercase().as_str() {
            "suspend" | "toggle" => self.leave_requested = true,
            // The layer's own keys turn steno on again, so there is nothing to resume
            "resume" => {}
            "lookup" if self.lookup.is_none() => self.open_lookup(),
            "lookup" => {}
            "set_config" => self.set_config(args),
            "add_translation" | "suggestions" | "configure" | "focus" | "quit" => {
                tracing::info!("steno command `{name}` has no counterpart in keymux, so it does nothing");
            }
            other => warn!("unknown steno command `{other}`"),
        }
    }

    /// `{plover:set_config:'start_attached':True,...}`: the options of a session that keymux has.
    fn set_config(&mut self, args: &str) {
        let mut options = self.translator.options().clone();
        for (key, value) in config_pairs(args) {
            let truth = value.eq_ignore_ascii_case("true");
            match key.as_str() {
                "start_attached" => options.start_attached = truth,
                "start_capitalized" => options.start_capitalized = truth,
                "space_placement" => options.spaces_after = value.eq_ignore_ascii_case("After Output"),
                other => tracing::info!("steno option `{other}` is not one keymux has"),
            }
        }
        self.translator.set_options(options);
    }

    /// Whether a stroke asked to leave the steno layer. Asking again is up to the next stroke.
    pub fn take_leave_request(&mut self) -> bool {
        std::mem::take(&mut self.leave_requested)
    }

    /// Block until the dictionaries are loaded. Tests use this so they don't race
    /// the loader thread.
    #[cfg(test)]
    pub fn wait_until_loaded(&mut self) {
        if self.loading {
            if let Some(Ok(dict)) = self.updates.as_ref().map(Receiver::recv) {
                self.translator.set_shared(dict);
            }
            self.loading = false;
        }
    }
}

/// Several outputs combined into one, so a backlog replays as a single edit. Typing that a later
/// output deletes again is never sent.
#[derive(Default)]
struct Merged {
    ops: Vec<OutputOp>,
}

impl Merged {
    fn push(&mut self, output: StenoOutput) {
        for op in output.into_ops() {
            let OutputOp::Backspaces(count) = op else {
                self.ops.push(op);
                continue;
            };
            let mut remaining = count;
            // Take back what this backlog typed first; the rest was already on screen
            while remaining > 0 {
                let Some(OutputOp::Text(text)) = self.ops.last_mut() else {
                    break;
                };
                let len = text.chars().count();
                if len > remaining {
                    *text = text.chars().take(len - remaining).collect();
                    remaining = 0;
                } else {
                    remaining -= len;
                    self.ops.pop();
                }
            }
            if remaining > 0 {
                self.ops.push(OutputOp::Backspaces(remaining));
            }
        }
    }

    fn finish(self) -> Option<StenoOutput> {
        StenoOutput::from_ops(self.ops)
    }
}

/// The `'key':value` pairs of a Python dictionary body, with quotes taken off. Commas inside
/// quotes don't separate pairs.
fn config_pairs(args: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut push = |part: &mut String| {
        if let Some((key, value)) = part.split_once(':') {
            let clean = |text: &str| text.trim().trim_matches(['\'', '"']).to_string();
            pairs.push((clean(key), clean(value)));
        }
        part.clear();
    };
    for c in args.chars() {
        match (c, quote) {
            ('\'' | '"', None) => {
                quote = Some(c);
                part.push(c);
            }
            (c2, Some(q)) if c2 == q => {
                quote = None;
                part.push(c2);
            }
            (',', None) => push(&mut part),
            _ => part.push(c),
        }
    }
    push(&mut part);
    pairs
}

/// Resolve a dictionary path from config: `~/` goes to `home`, relative paths to `config_dir`.
#[must_use]
pub fn resolve_dictionary_path(raw: &str, config_dir: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (raw.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_dict(json: &str) -> StenoEngine {
        engine_with_dict_and_overrides(json, &HashMap::new())
    }

    fn engine_with_dict_and_overrides(
        json: &str,
        overrides: &HashMap<String, crate::config::KeyAction>,
    ) -> StenoEngine {
        let mut dict = Dictionary::default();
        dict.merge_json(json).unwrap();
        StenoEngine {
            keys: build_key_map(&preset("qwerty").unwrap(), overrides).unwrap(),
            held_keys: HashSet::new(),
            chord: ChordState::new(Duration::from_secs(1)),
            translator: Translator::new(dict),
            updates: None,
            loading: false,
            backlog: Vec::new(),
            tape: None,
            disabled_in_game_mode: true,
            hub: OverlayHub::global(),
            lookup_trigger: Vec::new(),
            trigger_held: Vec::new(),
            lookup: None,
            leave_requested: false,
        }
    }

    fn key_for(engine: &StenoEngine, bit: u32) -> KeyCode {
        *engine.keys.iter().find(|(_, b)| **b == bit).unwrap().0
    }

    #[test]
    fn combined_key_keeps_bits_that_another_held_key_still_presses() {
        // The default preset has no combined keys any more, but an override can still add
        // one, and the chord logic around it is unchanged.
        let mut overrides = HashMap::new();
        overrides.insert(
            "A-+O-".to_string(),
            crate::config::KeyAction::Key(KeyCode::KC_X),
        );
        let mut engine = engine_with_dict_and_overrides(r#"{"AO": "ao"}"#, &overrides);
        let now = Instant::now();
        let a_bit = 1 << crate::steno::layout::slot_by_name("A-").unwrap();
        let o_bit = 1 << crate::steno::layout::slot_by_name("O-").unwrap();
        let a_key = key_for(&engine, a_bit);
        let combined_key = key_for(&engine, a_bit | o_bit);

        // Letting go of the lone A- key must not lift A-, since the combined key still presses it
        engine.press(a_key, now);
        engine.press(combined_key, now);
        assert!(engine.release(a_key).is_none());
        // The combined key was the last one held, so the stroke A- O- completes
        let out = engine.release(combined_key);
        assert!(matches!(out, Some(StenoOutput::Type(ref text)) if text.trim() == "ao"));
    }

    #[test]
    fn chord_types_dictionary_text() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let k = key_for(&engine, 1 << 2);
        let a = key_for(&engine, 1 << 7);
        let t = key_for(&engine, 1 << 18);
        engine.press(k, now);
        engine.press(a, now);
        engine.press(t, now);
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(k), None);
        assert_eq!(
            engine.release(t),
            Some(StenoOutput::Type("cat".to_string()))
        );
    }

    #[test]
    fn unknown_stroke_types_its_keys() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let k = key_for(&engine, 1 << 2);
        engine.press(k, now);
        assert_eq!(engine.release(k), Some(StenoOutput::Type("K-".into())));
    }

    #[test]
    fn strokes_made_before_the_dictionary_arrives_type_once_it_does() {
        let mut engine = engine_with_dict("{}");
        let (sender, updates) = std::sync::mpsc::channel();
        engine.updates = Some(updates);
        engine.loading = true;
        engine.translator = Translator::new(Dictionary::default());

        let now = Instant::now();
        let keys = [1 << 2, 1 << 7, 1 << 18].map(|bit| key_for(&engine, bit));
        for key in keys {
            engine.press(key, now);
        }
        for key in keys {
            // Still loading, so the stroke waits in the backlog
            assert_eq!(engine.release(key), None);
        }

        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat"}"#).unwrap();
        sender.send(Arc::new(dict)).unwrap();
        assert_eq!(engine.tick(now), Some(StenoOutput::Type("cat".to_string())));
    }

    #[test]
    fn owns_only_stroke_keys() {
        let engine = engine_with_dict("{}");
        let k = key_for(&engine, 1 << 2);
        assert!(engine.owns(k));
        assert!(!engine.owns(KeyCode::KC_ESC));
    }

    #[test]
    fn dictionary_paths_resolve() {
        let config_dir = Path::new("/etc/keymux");
        let home = Path::new("/home/alice");
        assert_eq!(
            resolve_dictionary_path("~/steno/main.json", config_dir, Some(home)),
            PathBuf::from("/home/alice/steno/main.json")
        );
        assert_eq!(
            resolve_dictionary_path("main.json", config_dir, Some(home)),
            PathBuf::from("/etc/keymux/main.json")
        );
        assert_eq!(
            resolve_dictionary_path("/usr/share/x.json", config_dir, Some(home)),
            PathBuf::from("/usr/share/x.json")
        );
    }

    #[test]
    fn from_config_rejects_unknown_preset() {
        let config = StenoConfig {
            layout: "dvorak".to_string(),
            layout_overrides: HashMap::new(),
            dictionaries: Vec::new(),
            stroke_timeout_ms: 5000,
            translation_timeout_ms: None,
            lookup_stroke: None,
            spaces_after: false,
            start_attached: true,
            start_capitalized: false,
        };
        let err = StenoEngine::from_config(&config, true, Path::new("."), None).unwrap_err();
        assert!(err.to_string().contains("unknown steno layout preset"));
    }

    #[test]
    fn backlogged_key_presses_and_text_all_come_out_in_order() {
        use crate::keycode::KeyCode;
        let press = |key| OutputOp::Keys(vec![(key, true), (key, false)]);
        let mut merged = Merged::default();
        merged.push(StenoOutput::Ops(vec![press(KeyCode::KC_LEFT)]));
        merged.push(StenoOutput::Type("cat".into()));
        merged.push(StenoOutput::Ops(vec![press(KeyCode::KC_RGHT)]));
        assert_eq!(
            merged.finish(),
            Some(StenoOutput::Ops(vec![
                press(KeyCode::KC_LEFT),
                OutputOp::Text("cat".into()),
                press(KeyCode::KC_RGHT),
            ]))
        );
    }

    #[test]
    fn backlogged_typing_that_a_later_output_deletes_is_never_sent() {
        let mut merged = Merged::default();
        merged.push(StenoOutput::Type("per".into()));
        merged.push(StenoOutput::Retype { backspaces: 3, text: "perfect".into() });
        merged.push(StenoOutput::Retype { backspaces: 2, text: String::new() });
        assert_eq!(merged.finish(), Some(StenoOutput::Type("perfe".into())));

        let mut merged = Merged::default();
        merged.push(StenoOutput::Type("ab".into()));
        merged.push(StenoOutput::Retype { backspaces: 5, text: "x".into() });
        // Three of the five were already on screen
        assert_eq!(
            merged.finish(),
            Some(StenoOutput::Retype { backspaces: 3, text: "x".into() })
        );
    }

    #[test]
    fn stroke_names_render_for_the_tape() {
        use crate::steno::layout::render_stroke;
        assert_eq!(render_stroke(1 << 2 | 1 << 7 | 1 << 18), "KAT");
    }
}
