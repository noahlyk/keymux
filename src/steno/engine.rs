//! One steno layer's runtime state: which physical keys are stroke keys, the
//! chord in progress, and the translator that turns strokes into text.

use super::chord::ChordState;
use super::dict::Dictionary;
use super::format::KeyPress;
use super::layout::{build_key_map, parse_strokes, preset};
use super::lookup::{classify, Lookup};
use super::overlay::OverlayHub;
use super::setup::default_dictionaries;
use super::tape;
use super::textbox::{TextBoxHub, TextBoxSync, Verdict};
use super::translate::{StenoOutput, Translator};
use crate::config::{KeyAction, StenoConfig};
use crate::keycode::KeyCode;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Debug)]
pub struct StenoEngine {
    keys: HashMap<KeyCode, u32>,
    /// Stroke keys physically down, so a release can tell which of their bits are still pressed
    held_keys: HashSet<KeyCode>,
    chord: ChordState,
    translator: Translator,
    /// Dictionaries still loading on a background thread
    loading: Option<Receiver<Dictionary>>,
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
    /// What the focused text box says is there, when it says anything
    text_box: TextBoxSync,
}

impl StenoEngine {
    /// Build an engine from a layer's steno config.
    ///
    /// Dictionaries load on a background thread, so the daemon doesn't wait on
    /// a large file. Until they arrive, strokes wait in the chord.
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

        // Without an explicit list, use what `keymux steno setup` installed
        let paths: Vec<PathBuf> = if config.dictionaries.is_empty() {
            default_dictionaries(config_dir)
        } else {
            config
                .dictionaries
                .iter()
                .map(|raw| resolve_dictionary_path(raw, config_dir, home))
                .collect()
        };
        let (sender, loading) = mpsc::channel();
        let cache = super::setup::cache_dir(config_dir);
        std::thread::spawn(move || {
            // The engine may be dropped before this finishes; nothing to do then
            let _ = sender.send(Dictionary::load_cached(&paths, &cache));
        });

        Ok(Self {
            keys,
            held_keys: HashSet::new(),
            chord: ChordState::new(Duration::from_millis(u64::from(config.stroke_timeout_ms))),
            translator: Translator::new(Dictionary::default()),
            loading: Some(loading),
            backlog: Vec::new(),
            tape: home.map(tape::default_path),
            disabled_in_game_mode,
            hub: OverlayHub::global(),
            lookup_trigger: config
                .lookup_stroke
                .as_deref()
                .map(|text| {
                    parse_strokes(text).unwrap_or_else(|| {
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
            text_box: TextBoxSync::new(TextBoxHub::global()),
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

    /// Read the text box from `hub` instead of the daemon's, and believe it at once. Tests use
    /// this so they don't share state or wait.
    #[cfg(test)]
    pub const fn set_text_box_hub(&mut self, hub: &'static TextBoxHub) {
        self.text_box = TextBoxSync::new(hub).without_settling();
    }

    /// Whether dictionaries are loaded. False means `keymux steno setup` hasn't run.
    /// Still loading counts as set up, so the hint doesn't fire during startup.
    #[must_use]
    pub fn needs_setup(&self) -> bool {
        self.loading.is_none() && !self.translator.has_dictionary()
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

    /// Backspace was pressed on this layer while the tape is empty, so it reaches the screen,
    /// and the translator drops the character it removed.
    pub fn backspace(&mut self) {
        self.translator.backspace();
    }

    /// Whether there is nothing left to undo. Space and Backspace act as normal keys then. A
    /// held-back lookup stroke counts as something: Backspace must not delete text that comes
    /// before it, and the stroke is not on screen to take back.
    #[must_use]
    pub fn tape_is_empty(&self) -> bool {
        self.trigger_held.is_empty() && self.translator.tape_is_empty()
    }

    /// A space was typed on the steno layer. Returns what to type.
    pub fn space(&mut self) -> StenoOutput {
        self.sync_with_text_box();
        self.text_box.note_edit();
        self.translator.space()
    }

    /// Check what the translator remembers of the text against the text box itself, when the
    /// application reports it. If they differ the text box is right: it knows about the click,
    /// the autocorrect or the dropped key that the translator didn't.
    fn sync_with_text_box(&mut self) {
        if let Verdict::Resync { before, truncated } = self.text_box.check(self.translator.text())
        {
            // The words already there become steps `*` can undo, and the tape shows the strokes
            // that would have written them
            let rows = self.translator.adopt(&before, truncated);
            self.hub.replace_tape(&rows);
        }
    }

    /// Look at the text box while nothing is being typed, so the tape follows the text as soon as
    /// the cursor lands in it, not at the next stroke. Cheap when the text box has said nothing
    /// new, so it can run on every pass of the idle loop.
    pub fn sync_idle(&mut self) {
        let writing = self.chord.pending() != 0 || self.lookup.is_some();
        // Strokes waiting for the dictionary would be undone by words it doesn't know yet
        if writing || self.loading.is_some() || !self.trigger_held.is_empty() {
            return;
        }
        if self.text_box.has_news() {
            self.sync_with_text_box();
        }
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

    /// Install the dictionaries once they arrive, and translate what was queued.
    fn poll_dictionary(&mut self) -> Option<StenoOutput> {
        let loading = self.loading.as_ref()?;
        let dict = match loading.try_recv() {
            Ok(dict) => dict,
            Err(TryRecvError::Empty) => return None,
            // The loader thread died. Steno stays unset up instead of stuck loading.
            Err(TryRecvError::Disconnected) => Dictionary::default(),
        };
        self.loading = None;
        self.translator.set_dictionary(dict);
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
        if self.loading.is_some() {
            self.backlog.push(stroke);
            self.publish_keys();
            return None;
        }
        self.translate(stroke)
    }

    fn translate(&mut self, stroke: u32) -> Option<StenoOutput> {
        self.sync_with_text_box();
        let output = self.translator.stroke(stroke);
        if output.is_some() {
            self.text_box.note_edit();
        }
        let unmatched = self.translator.was_unmatched();
        let description = tape::describe(output.as_ref(), unmatched);
        if let Some(path) = &self.tape {
            tape::append(path, stroke, &description);
        }
        // Same description as the tape file, so the overlay and `keymux steno tape` agree
        self.hub.push_stroke(stroke, description, unmatched);
        output
    }

    /// Block until the dictionaries are loaded. Tests use this so they don't race
    /// the loader thread.
    #[cfg(test)]
    pub fn wait_until_loaded(&mut self) {
        if let Some(loading) = self.loading.take() {
            if let Ok(dict) = loading.recv() {
                self.translator.set_dictionary(dict);
            }
        }
    }
}

/// Several outputs combined into one, so a backlog replays as a single edit.
#[derive(Default)]
struct Merged {
    backspaces: usize,
    text: String,
    keys: Vec<KeyPress>,
}

impl Merged {
    fn push(&mut self, output: StenoOutput) {
        match output {
            StenoOutput::Type(text) => self.text.push_str(&text),
            StenoOutput::Retype { backspaces, text } => {
                let mut remaining = backspaces;
                while remaining > 0 && self.text.pop().is_some() {
                    remaining -= 1;
                }
                // Whatever is left was already on screen
                self.backspaces += remaining;
                self.text.push_str(&text);
            }
            StenoOutput::Keys(presses) => self.keys.extend(presses),
        }
    }

    fn finish(self) -> Option<StenoOutput> {
        match (self.backspaces, self.text.is_empty()) {
            (0, true) if self.keys.is_empty() => None,
            (0, true) => Some(StenoOutput::Keys(self.keys)),
            // Text and key presses backlogged together can't share one output. The text
            // wins, as in the translator, so the key presses are dropped.
            (0, false) => Some(StenoOutput::Type(self.text)),
            (backspaces, _) => Some(StenoOutput::Retype {
                backspaces,
                text: self.text,
            }),
        }
    }
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
            loading: None,
            backlog: Vec::new(),
            tape: None,
            disabled_in_game_mode: true,
            hub: OverlayHub::global(),
            lookup_trigger: Vec::new(),
            trigger_held: Vec::new(),
            lookup: None,
            text_box: TextBoxSync::new(TextBoxHub::global()),
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
    fn unknown_stroke_types_nothing() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let k = key_for(&engine, 1 << 2);
        engine.press(k, now);
        assert_eq!(engine.release(k), None);
    }

    #[test]
    fn strokes_made_before_the_dictionary_arrives_type_once_it_does() {
        let mut engine = engine_with_dict("{}");
        let (sender, loading) = mpsc::channel();
        engine.loading = Some(loading);
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
        sender.send(dict).unwrap();
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
        };
        let err = StenoEngine::from_config(&config, true, Path::new("."), None).unwrap_err();
        assert!(err.to_string().contains("unknown steno layout preset"));
    }

    #[test]
    fn backlogged_key_presses_come_out_as_keys() {
        use crate::keycode::KeyCode;
        use crate::steno::format::KeyPress;
        let press = |key| KeyPress {
            modifiers: vec![],
            key,
        };
        let mut merged = Merged::default();
        merged.push(StenoOutput::Keys(vec![press(KeyCode::KC_LEFT)]));
        merged.push(StenoOutput::Keys(vec![press(KeyCode::KC_RGHT)]));
        assert_eq!(
            merged.finish(),
            Some(StenoOutput::Keys(vec![
                press(KeyCode::KC_LEFT),
                press(KeyCode::KC_RGHT),
            ]))
        );
    }

    #[test]
    fn backlogged_text_wins_over_key_presses_in_one_batch() {
        use crate::keycode::KeyCode;
        use crate::steno::format::KeyPress;
        let mut merged = Merged::default();
        merged.push(StenoOutput::Keys(vec![KeyPress {
            modifiers: vec![],
            key: KeyCode::KC_LEFT,
        }]));
        merged.push(StenoOutput::Type("cat".into()));
        assert_eq!(merged.finish(), Some(StenoOutput::Type("cat".into())));
    }

    #[test]
    fn stroke_names_render_for_the_tape() {
        use crate::steno::layout::render_stroke;
        assert_eq!(render_stroke(1 << 2 | 1 << 7 | 1 << 18), "KAT");
    }

    mod text_box {
        use super::*;
        use crate::steno::layout::parse_stroke;
        use crate::steno::textbox::{TextBoxReport, TextBoxState};
        use crate::steno::translate::UNDO_STROKE;

        fn hub() -> &'static TextBoxHub {
            Box::leak(Box::default())
        }

        /// What the focused text box reports: box number `focus`, with `before` before the cursor.
        fn says(hub: &TextBoxHub, focus: u64, before: &str) {
            hub.report(TextBoxReport {
                focus,
                state: TextBoxState {
                    active: true,
                    before: before.to_string(),
                    selection: false,
                    ..TextBoxState::default()
                },
            });
        }

        fn engine(hub: &'static TextBoxHub) -> StenoEngine {
            let mut engine = engine_with_dict(r#"{"KAT": "cat", "-PB": "and"}"#);
            engine.set_text_box_hub(hub);
            engine
        }

        fn stroke(text: &str) -> u32 {
            parse_stroke(text).unwrap()
        }

        #[test]
        fn a_word_typed_into_a_box_with_text_follows_that_text() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "the");
            // Without the box, a fresh layer would type "cat" with no space before it
            assert_eq!(
                engine.translate(stroke("KAT")),
                Some(StenoOutput::Type(" cat".into()))
            );
        }

        #[test]
        fn a_word_typed_at_the_start_of_a_line_has_no_space_before_it() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "one two");
            engine.translate(stroke("KAT"));
            // The cursor was clicked to the start of an empty line
            says(hub, 1, "");
            assert_eq!(
                engine.translate(stroke("-PB")),
                Some(StenoOutput::Type("and".into()))
            );
        }

        #[test]
        fn undo_takes_back_the_last_word_of_text_the_translator_never_typed() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "");
            engine.translate(stroke("KAT"));
            // The user clicked into the middle of other text. Undoing "cat" now would delete
            // three characters of that text; the last word of it is what `*` takes back.
            says(hub, 1, "some other text");
            assert_eq!(
                engine.translate(UNDO_STROKE),
                Some(StenoOutput::Retype {
                    backspaces: 5,
                    text: String::new()
                })
            );
        }

        #[test]
        fn undo_still_works_when_the_box_agrees() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "");
            engine.translate(stroke("KAT"));
            says(hub, 1, "cat");
            assert_eq!(
                engine.translate(UNDO_STROKE),
                Some(StenoOutput::Retype {
                    backspaces: 3,
                    text: String::new()
                })
            );
        }

        #[test]
        fn a_box_that_has_not_caught_up_with_the_last_edit_is_not_believed() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "");
            engine.translate(stroke("KAT"));
            // The box still shows the text from before "cat" went in
            assert_eq!(
                engine.translate(stroke("-PB")),
                Some(StenoOutput::Type(" and".into()))
            );
        }

        #[test]
        fn without_any_report_the_translator_goes_by_what_it_typed() {
            let mut engine = engine(hub());
            engine.translate(stroke("KAT"));
            assert_eq!(
                engine.translate(stroke("-PB")),
                Some(StenoOutput::Type(" and".into()))
            );
        }

        fn engine_with_overlay(
            text_box: &'static TextBoxHub,
            overlay: &'static OverlayHub,
        ) -> StenoEngine {
            let mut engine = engine(text_box);
            engine.set_hub(overlay);
            engine
        }

        /// The words on the tape, newest last.
        fn tape(overlay: &OverlayHub) -> Vec<String> {
            overlay
                .snapshot()
                .tape
                .iter()
                .map(|entry| entry.text.clone())
                .collect()
        }

        #[test]
        fn the_tape_shows_the_strokes_for_the_text_the_cursor_landed_in() {
            let text_box = hub();
            let overlay: &'static OverlayHub = Box::leak(Box::default());
            let mut engine = engine_with_overlay(text_box, overlay);
            says(text_box, 1, "cat and");
            // No stroke has been written: the cursor landing in the text is enough
            engine.sync_idle();
            assert_eq!(tape(overlay), ["\"cat\"", "\"and\""]);
            assert!(overlay.snapshot().tape.iter().all(|entry| entry.adopted));
            // A stroke written next goes after them
            engine.translate(stroke("KAT"));
            assert_eq!(tape(overlay).len(), 3);
            assert!(!overlay.snapshot().tape.back().unwrap().adopted);
        }

        #[test]
        fn the_tape_is_replaced_when_the_cursor_moves_to_other_text() {
            let text_box = hub();
            let overlay: &'static OverlayHub = Box::leak(Box::default());
            let mut engine = engine_with_overlay(text_box, overlay);
            says(text_box, 1, "cat and");
            engine.sync_idle();
            says(text_box, 1, "and");
            engine.sync_idle();
            assert_eq!(tape(overlay), ["\"and\""]);
        }

        #[test]
        fn nothing_changes_while_the_text_box_has_nothing_new_to_say() {
            let text_box = hub();
            let overlay: &'static OverlayHub = Box::leak(Box::default());
            let mut engine = engine_with_overlay(text_box, overlay);
            says(text_box, 1, "cat");
            engine.sync_idle();
            let generation = overlay.snapshot().generation;
            engine.sync_idle();
            engine.sync_idle();
            assert_eq!(overlay.snapshot().generation, generation);
        }

        #[test]
        fn the_tape_is_left_alone_in_the_middle_of_a_stroke() {
            let text_box = hub();
            let overlay: &'static OverlayHub = Box::leak(Box::default());
            let mut engine = engine_with_overlay(text_box, overlay);
            engine.press(key_for(&engine, 1 << 2), Instant::now());
            says(text_box, 1, "cat and");
            engine.sync_idle();
            assert!(tape(overlay).is_empty());
        }

        #[test]
        fn a_space_goes_in_after_the_box_is_checked() {
            let hub = hub();
            let mut engine = engine(hub);
            says(hub, 1, "the");
            // The space is typed, and the word after it needs none of its own
            assert_eq!(engine.space(), StenoOutput::Type(" ".into()));
            says(hub, 1, "the ");
            assert_eq!(
                engine.translate(stroke("KAT")),
                Some(StenoOutput::Type("cat".into()))
            );
        }
    }
}
