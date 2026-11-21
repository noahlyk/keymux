//! One steno layer's runtime state: which physical keys are stroke keys, the
//! chord in progress, and the translator that turns strokes into text.

use super::chord::ChordState;
use super::dict::Dictionary;
use super::journal::{MachineId, SharedHistory};
use super::layout::{preset, Layout};
use super::live;
use super::lookup::{Lookup, LookupKey, LookupSearch};
use super::overlay::OverlayHub;
use super::plugin::Chain;
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

/// How strokes are repeated and sent early. See `StenoConfig`.
#[derive(Debug, Clone, Copy)]
struct Timing {
    double_tap_window: Duration,
    repeat_delay: Duration,
    repeat_rate: Duration,
    repeat: bool,
    first_up: bool,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            double_tap_window: Duration::from_millis(300),
            repeat_delay: Duration::from_millis(300),
            repeat_rate: Duration::from_millis(40),
            repeat: true,
            first_up: true,
        }
    }
}

/// How often [`StenoEngine::tick`] looks for a plugin's late reading while one is due.
const LATE_POLL: Duration = Duration::from_millis(4);

/// A stroke that is being sent again and again because its keys are held after a double tap.
#[derive(Debug, Clone, Copy)]
struct Repeating {
    stroke: u32,
    next: Instant,
}

#[derive(Debug)]
pub struct StenoEngine {
    keys: HashMap<KeyCode, u32>,
    /// Stroke keys physically down, so a release can tell which of their bits are still pressed
    held_keys: HashSet<KeyCode>,
    chord: ChordState,
    timing: Timing,
    /// The last stroke sent and when, to tell a double tap
    last: Option<(u32, Instant)>,
    /// The stroke held down is the last one again, pressed within the double tap window: the
    /// stroke and when it went down. It repeats once held for the repeat delay.
    arming: Option<(u32, Instant)>,
    repeating: Option<Repeating>,
    translator: Translator,
    /// The dictionary, first loaded and then again whenever its files change. `None` when
    /// there is nothing to follow.
    updates: Option<Receiver<Arc<Chain>>>,
    /// No dictionary has arrived yet, so strokes wait in `backlog`
    loading: bool,
    /// The dictionary that arrived is too small to be a real one. Worked out once when it
    /// arrives, since the key path asks on every press.
    too_small: bool,
    /// Strokes finished before the dictionaries arrived. Translated once they do.
    backlog: Vec<u32>,
    /// Where each stroke is recorded. `None` when the home directory is unknown.
    tape: Option<tape::Tape>,
    pub disabled_in_game_mode: bool,
    /// Where strokes are published for the on-screen overlay
    hub: &'static OverlayHub,
    /// The lookup that is open right now. While it is, keys are QWERTY input to it.
    lookup: Option<Lookup>,
    /// Finds the open lookup's rows off this thread; tests, which look at the result at once,
    /// have none and find them here
    lookup_search: Option<LookupSearch>,
    /// The layout stroke notation is read and written in, extra keys included, for the lookup
    layout: Layout,
    /// `{:outline:next}` is armed: the next stroke types its own notation (see
    /// [`super::outline_control`])
    outline_armed: bool,
    /// A stroke wrote `{plover:toggle}` or `{plover:suspend}`: the layer should be left
    leave_requested: bool,
    fresh_on_enter: bool,
    label: String,
    /// Where the dictionaries come from, to load them again after an unload
    source: Option<Source>,
    /// The dictionaries are unloaded (`{:engine:unload}`): the next stroke loads them
    unloaded: bool,
    /// Shared by every layer that caches in the same folder: one layer unloading unloads all
    scope: live::Scope,
    /// The scope's unload count as this layer last saw it
    epoch: u64,
}

/// What a layer's dictionaries are loaded from.
#[derive(Debug, Clone)]
struct Source {
    roots: Vec<PathBuf>,
    cache_dir: PathBuf,
}

/// Which set of dictionaries a layer stands for: the places they are loaded from, in order.
fn machine_id(roots: &[PathBuf]) -> MachineId {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    roots.hash(&mut hasher);
    hasher.finish()
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
        history: &SharedHistory,
        label: &str,
    ) -> Result<Self> {
        let layout =
            Layout::plover_with_joins(&config.extra_keys, &config.join_keys).map_err(|errors| anyhow!(errors.join("; ")))?;
        let preset = preset(&config.layout).map_err(|e| anyhow!(e))?;
        let overrides: HashMap<String, KeyAction> = config.layout_overrides.clone();
        let keys = layout
            .build_key_map(&preset, &overrides)
            .map_err(|errors| anyhow!(errors.join("; ")))?;

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
        let machine = machine_id(&roots);
        let cache_dir = super::setup::cache_dir(config_dir);
        let scope = live::scope(&cache_dir);
        // After an unload, a config reload that rebuilds the layers leaves the dictionaries
        // unloaded: they load when a stroke needs them
        let unloaded = scope.is_unloaded();
        let source = Source {
            roots,
            cache_dir,
        };
        let updates = (!unloaded).then(|| {
            live::subscribe(
                source.roots.clone(),
                source.cache_dir.clone(),
                layout.clone(),
            )
        });

        // Every layer and keyboard writes to the one history; this layer's dictionaries are its machine
        let mut translator =
            Translator::with_history(Arc::new(Chain::default()), Arc::clone(history), machine);
        translator.set_options(super::formatter::Options {
            spaces_after: config.spaces_after,
            start_attached: config.start_attached,
            start_capitalized: config.start_capitalized,
            ..super::formatter::Options::default()
        });
        translator.set_stroke_classes(!config.disable_transparent);
        translator.set_undo_stops_at_barrier(config.undo_stops_at_barrier);
        translator.set_plugin_deadline(Duration::from_millis(u64::from(config.plugin_deadline_ms)));
        Ok(Self {
            keys,
            held_keys: HashSet::new(),
            chord: ChordState::new(),
            timing: Timing {
                double_tap_window: Duration::from_millis(u64::from(config.double_tap_window_ms)),
                repeat_delay: Duration::from_millis(u64::from(config.repeat_delay_ms)),
                repeat_rate: Duration::from_millis(u64::from(config.repeat_rate_ms.max(1))),
                repeat: !config.disable_repeat,
                first_up: !config.disable_first_up,
            },
            last: None,
            arming: None,
            repeating: None,
            translator,
            updates,
            loading: !unloaded,
            too_small: false,
            backlog: Vec::new(),
            tape: home
                .map(|home| tape::Tape::with_layout(tape::default_path(home), layout.clone())),
            disabled_in_game_mode,
            hub: OverlayHub::global(),
            lookup: None,
            lookup_search: (!cfg!(test)).then(|| LookupSearch::new(OverlayHub::global())),
            layout,
            outline_armed: false,
            leave_requested: false,
            fresh_on_enter: config.fresh_on_enter,
            label: label.to_string(),
            epoch: scope.epoch(),
            scope,
            unloaded,
            source: Some(source),
        })
    }

    /// The layer was entered. By default nothing changes: the history carries on. With
    /// `fresh_on_enter` the first word starts a new sentence, as it used to.
    pub fn enter(&mut self) {
        // A lookup left open before leaving the layer isn't carried over
        self.lookup = None;
        self.outline_armed = false;
        if self.fresh_on_enter {
            self.start_fresh();
        }
    }

    /// This layer's dictionary once it has loaded, for suggestions on another thread.
    #[must_use]
    pub fn dictionary_handle(&self) -> Option<Arc<Dictionary>> {
        self.translator.dictionary_handle()
    }

    /// The name of the layer, for the tape.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether a live lookup is open. While it is, keys go through the layers as usual and
    /// what comes out types into it (see [`Self::lookup_input`]); nothing reaches the application.
    #[must_use]
    pub const fn lookup_active(&self) -> bool {
        self.lookup.is_some()
    }

    /// A key came out of the layers during an open lookup. It types into the query, edits it,
    /// or closes the lookup, and updates the overlay.
    pub fn lookup_input(&mut self, key: LookupKey) {
        let Some(lookup) = self.lookup.as_mut() else {
            return;
        };
        if lookup.apply(key) {
            self.publish_lookup();
        } else {
            self.close_lookup();
        }
    }

    /// Close the lookup, if one is open.
    pub fn close_lookup(&mut self) {
        if self.lookup.take().is_some() {
            if let Some(search) = &self.lookup_search {
                search.cancel();
            }
            self.hub.set_lookup(None);
        }
    }

    fn open_lookup(&mut self) {
        self.lookup = Some(Lookup::new(self.layout.clone()));
        // The stroke that opened it is done: nothing is lit while you type a word
        self.hub.set_keys(0, 0);
        self.publish_lookup();
    }

    /// Show what the lookup has found so far on the overlay.
    fn publish_lookup(&self) {
        let Some(lookup) = &self.lookup else {
            return;
        };
        match &self.lookup_search {
            Some(search) => search.search(lookup, self.translator.fresh()),
            None => self
                .hub
                .set_lookup(Some(lookup.view(&self.translator.fresh()))),
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
    pub const fn needs_setup(&self) -> bool {
        !self.loading && self.too_small
    }

    /// Translate with `dict` from here on.
    fn use_dictionary(&mut self, dict: Arc<Chain>) {
        self.translator.set_shared(dict);
        self.too_small = !self
            .translator
            .has_entries(super::setup::MIN_REAL_DICTIONARY);
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
    }

    /// A stroke key went down.
    pub fn press(&mut self, key: KeyCode, now: Instant) {
        if let Some(&bit) = self.keys.get(&key) {
            // Loading starts with the first key down, so it overlaps the time the chord takes
            self.wake();
            self.held_keys.insert(key);
            let before = self.chord.held();
            self.chord.press(bit);
            if self.chord.held() != before {
                // Another key ends a repeat. It starts a new one only if this is the last
                // stroke again, pressed soon after it.
                self.repeating = None;
                self.arming = self.double_tap(now);
            }
            self.publish_keys();
        }
    }

    /// Show the stroke in progress on the overlay.
    fn publish_keys(&self) {
        self.hub.set_keys(self.chord.pending(), self.chord.held());
    }

    /// The stroke held down, if it is the last stroke again and pressed within the window.
    fn double_tap(&self, now: Instant) -> Option<(u32, Instant)> {
        let (stroke, at) = self.last?;
        (self.timing.repeat
            && self.chord.is_fresh()
            && self.chord.pending() == stroke
            && now.saturating_duration_since(at) <= self.timing.double_tap_window)
            .then_some((stroke, now))
    }

    /// A stroke key came up. Returns output if this completed a stroke.
    pub fn release(&mut self, key: KeyCode) -> Option<StenoOutput> {
        self.release_at(key, Instant::now())
    }

    /// [`Self::release`], at a given moment.
    pub fn release_at(&mut self, key: KeyCode, now: Instant) -> Option<StenoOutput> {
        let bit = *self.keys.get(&key)?;
        self.sync_unload();
        self.held_keys.remove(&key);
        // A combined key (an override binding one physical key to several stroke keys at
        // once) shares bits with the keys still down, so keep those
        let still_held: u32 = self
            .held_keys
            .iter()
            .filter_map(|other| self.keys.get(other))
            .fold(0, |bits, &other| bits | other);
        // Letting go of a key ends a repeat in progress and cancels one about to start
        self.arming = None;
        self.repeating = None;
        let released = bit & !still_held;
        let finished = self
            .chord
            .release(released)
            .or_else(|| self.sent_early(released));
        let Some(stroke) = finished else {
            self.publish_keys();
            return None;
        };
        self.last = Some((stroke, now));
        self.on_stroke(stroke, false)
    }

    /// A key came up with others still down. A stroke whose dictionary entry carries
    /// `{:firstup}` is sent now (with `{:firstup:except=KEYS}`, only when a key outside KEYS came
    /// up: an exempt key coming up first sends nothing and the chord keeps waiting), or one a plugin's `send_early` asks for (a theory's sticky key,
    /// say), and the keys still down wait for another key to go with them.
    fn sent_early(&mut self, released: u32) -> Option<u32> {
        let (held, stroke) = (self.chord.held(), self.chord.pending());
        (!self.loading
            && released != 0
            && held != 0
            && self.chord.is_fresh()
            && ((self.timing.first_up
                && self
                    .translator
                    .first_up_exempt(stroke)
                    .is_some_and(|exempt| released & !exempt != 0))
                || self.translator.sends_early(held, stroke)))
        .then(|| self.chord.send_early())
        .flatten()
    }

    /// When [`Self::tick`] next has something to do without a key event: a held double tap to
    /// start repeating, the next repeat to send, or a late plugin reading to look for.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        // Armed the same way `repeat_tick` fires, so a deadline is never one it won't act on
        let repeat_starts = self
            .arming
            .filter(|&(stroke, _)| self.chord.is_fresh() && self.chord.pending() == stroke)
            .map(|(_, since)| since + self.timing.repeat_delay);
        let next_repeat = self.repeating.map(|repeating| repeating.next);
        // A plugin's reading came too late for the last stroke: look for it again shortly
        let late = self
            .translator
            .has_late()
            .then(|| Instant::now() + LATE_POLL);
        [repeat_starts, next_repeat, late]
            .into_iter()
            .flatten()
            .min()
    }

    /// Pick up finished dictionary loads and plugin readings that came too late, and send the
    /// next repeat of a held stroke. Returns output if strokes queued during loading were just
    /// translated, if a late reading replaced what its stroke typed, or if a repeat is due.
    /// A held stroke is never sent by the clock: only its keys coming up (or first-up) sends it.
    pub fn tick(&mut self, now: Instant) -> Option<StenoOutput> {
        self.sync_unload();
        if let Some(output) = self.poll_dictionary() {
            return Some(output);
        }
        if let Some(output) = self.settle_late() {
            return Some(output);
        }
        self.repeat_tick(now)
    }

    /// Start a repeat once a double tap has been held long enough, and send the next repeat
    /// when it is due.
    fn repeat_tick(&mut self, now: Instant) -> Option<StenoOutput> {
        if let Some((stroke, since)) = self.arming {
            let held_long = now.saturating_duration_since(since) >= self.timing.repeat_delay;
            if held_long && self.chord.is_fresh() && self.chord.pending() == stroke {
                self.arming = None;
                // A command (`{plover:...}`) is sent once, on release, as usual
                if self.translator.is_command(stroke) {
                    return None;
                }
                // Sent now, so letting go of the keys must not send it again
                self.chord.mark_sent();
                self.repeating = Some(Repeating {
                    stroke,
                    next: now + self.timing.repeat_rate,
                });
                self.last = Some((stroke, now));
                return self.on_stroke(stroke, true);
            }
        }
        let repeating = self.repeating.as_mut()?;
        if now < repeating.next {
            return None;
        }
        repeating.next = now + self.timing.repeat_rate;
        let stroke = repeating.stroke;
        self.last = Some((stroke, now));
        self.on_stroke(stroke, true)
    }

    /// The last stroke was typed without a plugin's reading that came too late: once it is in,
    /// put the stroke right with it (see [`Translator::settle_late`]). On the tape as a `late`
    /// line: what the replacement typed, and the plugin's reading.
    fn settle_late(&mut self) -> Option<StenoOutput> {
        if !self.translator.late_ready() {
            return None;
        }
        let output = self.translator.settle_late()?;
        let read = self.translator.plugin_read();
        let unmatched = self.translator.was_unmatched();
        let unmarked = tape::describe(Some(&output), unmatched);
        let description = tape::mark_plugin(
            &unmarked,
            read.as_ref()
                .map(|(name, text)| (name.as_str(), text.as_str())),
        );
        if let Some(tape) = &mut self.tape {
            tape.note("late", &description);
        }
        // On the overlay too, under the line it greys, so what is in full colour is what is on
        // screen
        self.hub.push_late(unmarked, unmatched);
        Some(output)
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
                newest = Some(Arc::new(Chain::default()));
            }
        } else {
            self.updates = Some(updates);
        }
        self.use_dictionary(newest?);
        // A dictionary that replaces a working one leaves nothing waiting
        if !std::mem::replace(&mut self.loading, false) {
            return None;
        }
        let mut merged = Merged::default();
        let mut queue = std::mem::take(&mut self.backlog).into_iter();
        while let Some(stroke) = queue.next() {
            if let Some(output) = self.translate(stroke, false) {
                merged.push(output);
            }
            // A stroke in the queue unloaded the dictionaries: the strokes after it wait, in
            // order, for them to load again
            if self.unloaded {
                self.backlog = queue.collect();
                if !self.backlog.is_empty() {
                    self.begin_load();
                }
                break;
            }
        }
        merged.finish()
    }

    /// Something needs the dictionaries: if they are unloaded, start loading them. Strokes wait
    /// in the backlog (in order, none dropped) until they arrive.
    fn wake(&mut self) {
        self.sync_unload();
        if self.unloaded {
            self.begin_load();
        }
    }

    /// Subscribe to the dictionaries again, on the loader thread: this thread never waits.
    fn begin_load(&mut self) {
        let Some(source) = &self.source else {
            return;
        };
        self.updates = Some(live::subscribe(
            source.roots.clone(),
            source.cache_dir.clone(),
            self.layout.clone(),
        ));
        self.loading = true;
        self.unloaded = false;
        self.epoch = self.scope.epoch();
        tracing::debug!("steno dictionaries loading again for layer {}", self.label);
    }

    /// Another layer unloaded the dictionaries they share: drop this layer's copy too.
    fn sync_unload(&mut self) {
        let epoch = self.scope.epoch();
        if epoch == self.epoch {
            return;
        }
        self.epoch = epoch;
        if self.source.is_some() && !self.unloaded {
            self.release_dictionary();
            // Strokes were waiting for a load that was cut off: load again for them
            if !self.backlog.is_empty() {
                self.begin_load();
            }
        }
    }

    /// Drop this layer's dictionaries, chain, plugin instances and lookup search, and give the
    /// memory back. What was typed stays in the history. See [`super::engine_control`].
    fn release_dictionary(&mut self) {
        // An open lookup is closed: its search holds a copy of the dictionary, and is cancelled
        self.close_lookup();
        self.translator.set_shared(Arc::new(Chain::default()));
        self.updates = None;
        self.loading = false;
        self.too_small = false;
        self.unloaded = true;
    }

    /// `{:engine:unload}`. A second one while unloaded does nothing.
    fn unload(&mut self) {
        let Some(source) = &self.source else {
            tracing::info!("steno engine has no files to load again, so it is not unloaded");
            return;
        };
        if self.unloaded {
            return;
        }
        let dir = source.cache_dir.clone();
        self.release_dictionary();
        // The loader thread drops what it holds and trims the heap; this thread goes on
        let _ = live::unload(&dir);
        self.epoch = self.scope.epoch();
    }

    /// A stroke is done. `repeated` is a repeat of the stroke before it, from holding the keys.
    fn on_stroke(&mut self, stroke: u32, repeated: bool) -> Option<StenoOutput> {
        self.wake();
        if self.loading {
            self.backlog.push(stroke);
            self.publish_keys();
            return None;
        }
        self.translate(stroke, repeated)
    }

    /// Translate a stroke. A repeat counts on the stroke's tape line instead of adding lines.
    fn translate(&mut self, stroke: u32, repeated: bool) -> Option<StenoOutput> {
        // Armed by `{:outline:next}`: the stroke types its notation instead of translating, unless
        // it is the arming stroke itself, which goes on to disarm
        let output = if self.outline_armed && !self.translator.arms_outline(stroke) {
            self.outline_armed = false;
            let notation = self.layout.render_strokes(&[stroke]);
            self.translator.type_text(&notation)
        } else {
            self.translator.stroke(stroke)
        };
        // A transparent stroke is on the tape (dimmed, marked `~` in the file) but not part of
        // the text: its line says what it ran, so describe it before the commands are consumed
        let transparent = self.translator.was_transparent();
        let ran = transparent.then(|| tape::describe(output.as_ref(), false));
        // A stroke that only runs an engine command (suspend, ...) typed nothing, so it has no
        // place on the tape: the tape shows what was written, and a line for it would only be
        // one more thing to undo past. Plover's undo already skips such a stroke.
        // The overlay controls are transparent strokes too (they leave the history and the join
        // alone) but, like other command strokes, they stay off the tape.
        let command_only =
            (!transparent && is_command_only(output.as_ref())) || is_control_only(output.as_ref());
        let output = self.run_commands(output);
        if command_only {
            tracing::debug!("steno command stroke {stroke:#x} kept off the tape");
            self.hub.clear_keys();
            return output;
        }
        if let Some(description) = ran {
            if let Some(tape) = &mut self.tape {
                tape.stroke(stroke, &format!("~ {description}"));
            }
            self.hub.push_transparent(stroke, description);
            return output;
        }
        let unmatched = self.translator.was_unmatched();
        let description = tape::describe(output.as_ref(), unmatched);
        if let Some(tape) = &mut self.tape {
            // The tape file says which plugin read the word, and as what; the overlay only flags that one did
            let read = self.translator.plugin_read();
            let marked = tape::mark_plugin(
                &description,
                read.as_ref()
                    .map(|(name, text)| (name.as_str(), text.as_str())),
            );
            if repeated {
                tape.repeat(stroke, &marked);
            } else {
                tape.stroke(stroke, &marked);
            }
        }
        // Same description as the tape file, so the overlay and `keymux steno tape` agree
        let read = self.translator.plugin_read().is_some();
        if repeated {
            self.hub.push_repeat_of(stroke, description, unmatched, read);
        } else if read {
            self.hub.push_read_stroke(stroke, description, unmatched);
        } else {
            self.hub.push_stroke(stroke, description, unmatched);
        }
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
                OutputOp::Command(command) => {
                    let outline = command
                        .strip_prefix(super::outline_control::COMMAND)
                        .and_then(|args| args.strip_prefix(':'));
                    match outline {
                        Some(args) => {
                            let typed = self.run_outline_control(args);
                            rest.extend(typed.map(StenoOutput::into_ops).unwrap_or_default());
                        }
                        None => self.run_command(&command),
                    }
                }
                other => rest.push(other),
            }
        }
        StenoOutput::from_ops(rest)
    }

    /// `{:outline:last}` and `{:outline:next}` (see [`super::outline_control`]), as the engine
    /// command `outline:last` or `outline:next`: `next` arms or disarms the mode, `last` returns
    /// what typing the last outline made.
    fn run_outline_control(&mut self, args: &str) -> Option<StenoOutput> {
        use super::outline_control::{verb_of, Verb};
        match verb_of(args) {
            Some(Verb::Next) => self.outline_armed = !self.outline_armed,
            Some(Verb::Last) => {
                let strokes = self.translator.last_outline();
                if !strokes.is_empty() {
                    let notation = self.layout.render_strokes(&strokes);
                    return self.translator.type_text(&notation);
                }
            }
            None => warn!("unknown outline control `{args}`"),
        }
        None
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
            super::engine_control::COMMAND => {
                if super::engine_control::is_unload(args.split_once(':').map_or("", |(_, p)| p)) {
                    self.unload();
                }
            }
            super::overlay_control::COMMAND => {
                let (name, param) = args.split_once(':').unwrap_or((args, ""));
                self.run_overlay_control(name, param);
            }
            "add_translation" | "suggestions" | "configure" | "focus" | "quit" => {
                tracing::info!(
                    "steno command `{name}` has no counterpart in keymux, so it does nothing"
                );
            }
            other => warn!("unknown steno command `{other}`"),
        }
    }

    /// `{:overlay:hide}`, `{:tape:clear}`, `{:lookup:toggle}`, ...: see [`super::overlay_control`].
    /// Separate from [`Self::run_command`] so it merges apart from Plover's own commands.
    fn run_overlay_control(&mut self, name: &str, param: &str) {
        use super::overlay_control::{parse, Control, Verb};
        match parse(name, param) {
            Some(Control::View(command)) => self.hub.control(command),
            Some(Control::ClearTape) => self.hub.clear_tape(),
            Some(Control::Lookup(verb)) => match (verb, self.lookup.is_some()) {
                (Verb::Show | Verb::Toggle, false) => self.open_lookup(),
                (Verb::Hide | Verb::Toggle, true) => self.close_lookup(),
                _ => {}
            },
            None => warn!("unknown overlay control `{name}:{param}`"),
        }
    }

    /// `{plover:set_config:'start_attached':True,...}`: the options of a session that keymux has.
    fn set_config(&mut self, args: &str) {
        let mut options = self.translator.options();
        for (key, value) in config_pairs(args) {
            let truth = value.eq_ignore_ascii_case("true");
            match key.as_str() {
                "start_attached" => options.start_attached = truth,
                "start_capitalized" => options.start_capitalized = truth,
                "space_placement" => {
                    options.spaces_after = value.eq_ignore_ascii_case("After Output");
                }
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
        self.wake();
        if self.loading {
            if let Some(Ok(dict)) = self.updates.as_ref().map(Receiver::recv) {
                self.use_dictionary(dict);
            }
            self.loading = false;
        }
    }
}

/// Whether an output is nothing but engine commands: no text, no keys, no backspaces.
fn is_command_only(output: Option<&StenoOutput>) -> bool {
    match output {
        Some(StenoOutput::Ops(ops)) => {
            !ops.is_empty() && ops.iter().all(|op| matches!(op, OutputOp::Command(_)))
        }
        _ => false,
    }
}

/// Whether an output is nothing but overlay controls (`{:overlay:toggle}`, `{:tape:clear}`, ...).
fn is_control_only(output: Option<&StenoOutput>) -> bool {
    let prefix = format!("{}:", super::overlay_control::COMMAND);
    let engine_prefix = format!("{}:", super::engine_control::COMMAND);
    match output {
        Some(StenoOutput::Ops(ops)) => {
            !ops.is_empty()
                && ops.iter().all(|op| {
                    matches!(op, OutputOp::Command(c)
                        if c.starts_with(&prefix) || c.starts_with(&engine_prefix))
                })
        }
        _ => false,
    }
}

#[cfg(test)]
impl StenoEngine {
    /// An engine over `dict`, keys from the qwerty preset with `overrides`, writing no tape:
    /// for a test to drive key by key.
    pub(crate) fn for_tests(
        dict: Dictionary,
        overrides: &HashMap<String, crate::config::KeyAction>,
    ) -> Self {
        use super::layout::build_key_map;
        let too_small = !dict.has_at_least(super::setup::MIN_REAL_DICTIONARY);
        Self {
            keys: build_key_map(&preset("qwerty").unwrap(), overrides).unwrap(),
            held_keys: HashSet::new(),
            chord: ChordState::new(),
            timing: Timing::default(),
            last: None,
            arming: None,
            repeating: None,
            translator: Translator::new(dict),
            updates: None,
            loading: false,
            too_small,
            backlog: Vec::new(),
            tape: None,
            disabled_in_game_mode: true,
            hub: OverlayHub::global(),
            lookup: None,
            lookup_search: None,
            layout: Layout::plover(),
            outline_armed: false,
            leave_requested: false,
            fresh_on_enter: false,
            label: String::new(),
            source: None,
            unloaded: false,
            scope: live::Scope::default(),
            epoch: 0,
        }
    }

    /// The stroke bits each physical key presses.
    pub(crate) fn key_map(&self) -> &HashMap<KeyCode, u32> {
        &self.keys
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
        dict.merge_json(json, &Layout::plover()).unwrap();
        StenoEngine::for_tests(dict, overrides)
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
    fn a_chord_held_for_hours_is_sent_only_when_a_key_comes_up() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let now = Instant::now();
        let keys = [1 << 2, 1 << 7, 1 << 18].map(|bit| key_for(&engine, bit));
        for key in keys {
            engine.press(key, now);
        }
        // Hours later, still held: no tick sends it, and nothing asks to be woken for it
        let later = now + Duration::from_hours(6);
        let ticked = engine.tick(later);
        assert_eq!(ticked, None, "a held stroke was sent by the clock");
        assert_eq!(engine.next_deadline(), None, "nothing is due while keys are held");
        assert_eq!(engine.release_at(keys[1], later), None);
        assert_eq!(engine.release_at(keys[0], later), None);
        // It still sends normally when the last key comes up
        assert_eq!(
            engine.release_at(keys[2], later),
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
        dict.merge_json(r#"{"KAT": "cat"}"#, &Layout::plover())
            .unwrap();
        sender.send(Arc::new(Chain::from(dict))).unwrap();
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
            extra_keys: Vec::new(),
            join_keys: Vec::new(),
            layout_overrides: HashMap::new(),
            dictionaries: Vec::new(),
            plugin_deadline_ms: 30,
            translation_timeout_ms: None,
            spaces_after: false,
            start_attached: true,
            start_capitalized: false,
            double_tap_window_ms: 300,
            repeat_delay_ms: 300,
            repeat_rate_ms: 40,
            disable_repeat: false,
            disable_first_up: false,
            disable_transparent: false,
            undo_stops_at_barrier: true,
            fresh_on_enter: false,
            disable_typing_history: false,
            typing_layout: "us".to_string(),
        };
        let history =
            crate::steno::journal::new_shared(crate::steno::formatter::Formatter::default());
        let err = StenoEngine::from_config(&config, true, Path::new("."), None, &history, "test")
            .unwrap_err();
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
        merged.push(StenoOutput::Retype {
            backspaces: 3,
            text: "perfect".into(),
        });
        merged.push(StenoOutput::Retype {
            backspaces: 2,
            text: String::new(),
        });
        assert_eq!(merged.finish(), Some(StenoOutput::Type("perfe".into())));

        let mut merged = Merged::default();
        merged.push(StenoOutput::Type("ab".into()));
        merged.push(StenoOutput::Retype {
            backspaces: 5,
            text: "x".into(),
        });
        // Three of the five were already on screen
        assert_eq!(
            merged.finish(),
            Some(StenoOutput::Retype {
                backspaces: 3,
                text: "x".into()
            })
        );
    }

    #[test]
    fn stroke_names_render_for_the_tape() {
        use crate::steno::layout::render_stroke;
        assert_eq!(render_stroke(1 << 2 | 1 << 7 | 1 << 18), "KAT");
    }

    fn kat_keys(engine: &StenoEngine) -> [KeyCode; 3] {
        [1 << 2, 1 << 7, 1 << 18].map(|bit| key_for(engine, bit))
    }

    fn typed(output: Option<StenoOutput>) -> String {
        match output {
            Some(StenoOutput::Type(text)) => text,
            other => panic!("expected typed text, got {other:?}"),
        }
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
    fn a_marked_stroke_is_sent_when_its_first_key_comes_up_and_rearms_on_the_next_key() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat{:firstup}"}"#);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert!(typed(engine.release(t)).contains("cat"));
        // The keys still down alone send nothing
        assert_eq!(engine.release(a), None);
        // Another key with the one still held is a stroke of its own
        engine.press(a, now);
        engine.press(t, now);
        assert!(typed(engine.release(t)).contains("cat"));
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(k), None);
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
    fn tapping_one_key_again_while_the_rest_are_held_repeats_the_stroke() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat{:firstup}"}"#);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert!(typed(engine.release(t)).contains("cat"));
        for _ in 0..3 {
            engine.press(t, now);
            assert!(typed(engine.release(t)).contains("cat"));
        }
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(k), None);
    }

    const EXCEPT_DICT: &str = r#"{"KAT": "cat{:firstup:except=-T}"}"#;

    #[test]
    fn lifting_an_exempt_key_first_sends_nothing_and_the_first_other_key_up_sends_the_whole_stroke() {
        let mut engine = engine_with_dict(EXCEPT_DICT);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert_eq!(engine.release(t), None);
        // Pressing the exempt key again changes nothing: it is still part of the stroke
        engine.press(t, now);
        assert_eq!(engine.release(t), None);
        assert!(typed(engine.release(a)).contains("cat"));
        // Held keys now wait as with plain `{:firstup}`
        assert_eq!(engine.release(k), None);
    }

    #[test]
    fn an_exception_list_sends_on_the_first_other_key_while_the_exempt_key_is_still_down() {
        let mut engine = engine_with_dict(EXCEPT_DICT);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert!(typed(engine.release(k)).contains("cat"));
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(t), None);
    }

    #[test]
    fn a_chord_of_only_exempt_keys_is_sent_on_the_last_key_up() {
        let mut engine = engine_with_dict(r#"{"T": "tee{:firstup:except=T}"}"#);
        let t = key_for(&engine, 1 << crate::steno::layout::slot_by_name("T-").unwrap());
        engine.press(t, Instant::now());
        assert!(typed(engine.release(t)).contains("tee"));
    }

    #[test]
    fn an_exception_list_is_ignored_when_first_up_is_disabled() {
        let mut engine = engine_with_dict(EXCEPT_DICT);
        engine.timing.first_up = false;
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert_eq!(engine.release(k), None);
        assert_eq!(engine.release(a), None);
        assert!(typed(engine.release(t)).contains("cat"));
    }

    #[test]
    fn an_unmarked_stroke_still_waits_for_its_last_key() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert_eq!(engine.release(t), None);
        assert_eq!(engine.release(a), None);
        assert!(typed(engine.release(k)).contains("cat"));
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
    fn the_marker_can_be_turned_off() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat{:firstup}"}"#);
        engine.timing.first_up = false;
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        assert_eq!(engine.release(t), None);
        assert_eq!(engine.release(a), None);
        assert!(typed(engine.release(k)).contains("cat"));
    }

    fn tap(
        engine: &mut StenoEngine,
        keys: [KeyCode; 3],
        down: Instant,
        up: Instant,
    ) -> Option<StenoOutput> {
        for key in keys {
            engine.press(key, down);
        }
        let mut out = None;
        for key in keys {
            out = engine.release_at(key, up).or(out);
        }
        out
    }

    #[test]
    fn a_late_plugin_reading_lands_on_a_tick_and_goes_on_the_tape() {
        let (go, gate) = std::sync::mpsc::channel::<()>();
        // A dictionary of one entry: with none at all, no stroke is looked up anywhere
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"TKOG": "dog"}"#, &Layout::plover())
            .unwrap();
        let chain = Chain::test_with_reader(
            dict,
            Layout::plover(),
            Box::new(
                move |outline: &str, _: &str, _: Option<&crate::steno::strokes::Window>| {
                    let _ = gate.recv();
                    Some(outline.to_lowercase())
                },
            ),
        );
        let mut engine = engine_with_dict("{}");
        engine.translator = Translator::with_shared(
            Arc::new(chain),
            super::super::formatter::Options {
                start_attached: true,
                ..super::super::formatter::Options::default()
            },
        );
        engine
            .translator
            .set_plugin_deadline(Duration::from_millis(5));
        let dir = std::env::temp_dir().join(format!("keymux-engine-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tape.txt");
        engine.tape = Some(tape::Tape::with_layout(path.clone(), Layout::plover()));
        let hub: &'static OverlayHub = Box::leak(Box::default());
        engine.set_hub(hub);
        let keys = kat_keys(&engine);
        let t0 = Instant::now();
        let raw = typed(tap(&mut engine, keys, t0, t0 + Duration::from_millis(30)));
        // As its keys, the way any stroke that matched nothing is typed
        assert_eq!(raw, "KA-T", "typed at once, without the reading");
        let due = engine.next_deadline().expect("the engine looks again soon");
        assert!(due <= Instant::now() + LATE_POLL);
        assert_eq!(engine.tick(Instant::now()), None, "not in yet");
        go.send(()).unwrap();
        let started = Instant::now();
        let replaced = loop {
            if let Some(output) = engine.tick(Instant::now()) {
                break output;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "never landed");
            std::thread::yield_now();
        };
        assert_eq!(engine.translator.text(), "kat");
        assert!(
            matches!(replaced, StenoOutput::Retype { backspaces: 4, ref text } if text == "kat"),
            "{replaced:?}"
        );
        assert_eq!(engine.next_deadline(), None, "nothing more to look for");
        engine.tape.as_ref().unwrap().flush();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        // A miss says what it typed after its `?`
        assert!(
            lines[0].contains("KAT") && lines[0].ends_with("? \"KA-T\""),
            "{text}"
        );
        assert!(
            lines[1].contains("late") && lines[1].ends_with("plugin:reader \"kat\""),
            "{text}"
        );
        // On the overlay, the late reading is a line of its own, and what the stroke typed
        // without it ("KA-T") is grey: taken back
        let tape = hub.snapshot().tape;
        assert_eq!(tape.len(), 2, "{tape:?}");
        assert!(tape[0].unmatched);
        assert_eq!(tape[0].gone, [[0, 4]]);
        assert_eq!((tape[1].stroke, tape[1].text.as_str()), (0, "-4 \"kat\""));
        assert!(tape[1].gone.is_empty(), "expected none");
        assert!(!tape[0].read && tape[1].read, "only the plugin's line is a reading: {tape:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stroke_a_plugin_read_is_flagged_on_the_overlay_tape_and_a_dictionary_entry_is_not() {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"TKOG": "dog"}"#, &Layout::plover()).unwrap();
        let chain = Chain::test_with_reader(
            dict,
            Layout::plover(),
            Box::new(|outline: &str, _: &str, _: Option<&crate::steno::strokes::Window>| Some(outline.to_lowercase())),
        );
        let mut engine = engine_with_dict("{}");
        engine.translator = Translator::with_shared(Arc::new(chain), super::super::formatter::Options::default());
        let hub: &'static OverlayHub = Box::leak(Box::default());
        engine.set_hub(hub);
        let t0 = Instant::now();
        let keys = kat_keys(&engine);
        let _ = tap(&mut engine, keys, t0, t0 + Duration::from_millis(30));
        let tape = hub.snapshot().tape;
        assert_eq!(tape.len(), 1, "{tape:?}");
        assert!(tape[0].read && tape[0].output().contains("kat"), "{tape:?}");
    }

    #[test]
    fn tap_then_press_and_hold_repeats_until_released() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        assert!(typed(tap(&mut engine, keys, t0, t0 + ms(50))).contains("cat"));
        // Pressed again soon after, and held
        let second = t0 + ms(150);
        for key in keys {
            engine.press(key, second);
        }
        assert_eq!(engine.tick(second + ms(200)), None);
        assert!(typed(engine.tick(second + ms(310))).contains("cat"));
        assert_eq!(engine.tick(second + ms(330)), None);
        assert!(typed(engine.tick(second + ms(360))).contains("cat"));
        // No time limit: still going a minute later
        assert!(typed(engine.tick(second + ms(60_000))).contains("cat"));
        // Letting go sends nothing more, and ends it
        for key in keys {
            assert_eq!(engine.release_at(key, second + ms(60_010)), None);
        }
        assert_eq!(engine.tick(second + ms(60_100)), None);
    }

    #[test]
    fn a_held_repeat_is_one_tape_line_with_a_count() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let hub: &'static OverlayHub = Box::leak(Box::default());
        engine.hub = hub;
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        let second = t0 + ms(150);
        for key in keys {
            engine.press(key, second);
        }
        // The first repeat once held long enough, then three more
        for at in [310, 360, 400, 440] {
            assert!(typed(engine.tick(second + ms(at))).contains("cat"));
        }
        // One line for the tap and all its repeats, which counts them
        let tape = hub.snapshot().tape;
        assert_eq!(tape.len(), 1, "{tape:?}");
        assert_eq!(tape[0].repeats, 4);
        // Letting go ends it: tapping the stroke again is a line of its own
        for key in keys {
            engine.release_at(key, second + ms(450));
        }
        tap(&mut engine, keys, second + ms(1000), second + ms(1050));
        assert_eq!(hub.snapshot().tape.len(), 2);
    }

    #[test]
    fn a_second_press_after_the_window_is_not_a_repeat() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        let late = t0 + ms(500);
        for key in keys {
            engine.press(key, late);
        }
        assert_eq!(engine.tick(late + ms(900)), None);
    }

    #[test]
    fn a_second_press_of_another_stroke_is_not_a_repeat() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat", "KA": "ka"}"#);
        let [k, a, t] = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, [k, a, t], t0, t0 + ms(50));
        let second = t0 + ms(150);
        engine.press(k, second);
        engine.press(a, second);
        assert_eq!(engine.tick(second + ms(800)), None);
    }

    #[test]
    fn repeating_can_be_turned_off() {
        let mut engine = engine_with_dict(r#"{"KAT": "cat"}"#);
        engine.timing.repeat = false;
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        for key in keys {
            engine.press(key, t0 + ms(150));
        }
        assert_eq!(engine.tick(t0 + ms(900)), None);
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a command, not a format
    fn a_command_stroke_never_repeats() {
        let mut engine = engine_with_dict(r#"{"KAT": "{:lookup}"}"#);
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        for key in keys {
            engine.press(key, t0 + ms(150));
        }
        assert_eq!(engine.tick(t0 + ms(600)), None);
        assert!(engine.repeating.is_none() && engine.arming.is_none());
        assert_eq!(engine.tick(t0 + ms(900)), None);
    }

    #[test]
    fn only_a_stroke_that_is_nothing_but_commands_stays_off_the_tape() {
        let command = || OutputOp::Command("LOOKUP".to_owned());
        assert!(is_command_only(Some(&StenoOutput::Ops(vec![command()]))));
        assert!(is_command_only(Some(&StenoOutput::Ops(vec![
            command(),
            command()
        ]))));
        let with_text = StenoOutput::Ops(vec![command(), OutputOp::Text("a".to_owned())]);
        assert!(!is_command_only(Some(&with_text)));
        let with_backspace = StenoOutput::Ops(vec![OutputOp::Backspaces(1), command()]);
        assert!(!is_command_only(Some(&with_backspace)));
        assert!(!is_command_only(Some(&StenoOutput::Type("a".to_owned()))));
        assert!(!is_command_only(None));
    }

    fn engine_with_hub(json: &str) -> (StenoEngine, &'static OverlayHub) {
        let mut engine = engine_with_dict(json);
        let hub: &'static OverlayHub = Box::leak(Box::default());
        engine.hub = hub;
        (engine, hub)
    }

    fn stroke_of(text: &str) -> u32 {
        Layout::plover().parse_stroke(text).unwrap()
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a command, not a format
    fn the_lookup_stroke_is_a_transparent_line_on_the_tape_and_opens_the_lookup() {
        let (mut engine, hub) = engine_with_hub(r#"{"PHRUP": "{:lookup}", "KAT": "cat"}"#);
        engine.translate(stroke_of("KAT"), false);
        engine.translate(stroke_of("PHRUP"), false);
        assert!(engine.lookup_active());
        let tape = hub.snapshot().tape;
        assert_eq!(tape.len(), 2);
        assert!(!tape[0].transparent);
        assert!(tape[1].transparent);
        assert!(tape[1].text.contains("lookup"), "{:?}", tape[1].text);
        // Nothing was added to the text, and the next word carries on from "cat"
        engine.close_lookup();
        engine.translate(stroke_of("KAT"), false);
        assert_eq!(engine.translator.text(), "cat cat");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a command, not a format
    fn with_the_classes_off_the_lookup_stroke_is_off_the_tape_as_before() {
        let (mut engine, hub) = engine_with_hub(r#"{"PHRUP": "{:lookup}"}"#);
        engine.translator.set_stroke_classes(false);
        engine.translate(stroke_of("PHRUP"), false);
        assert!(engine.lookup_active());
        assert!(hub.snapshot().tape.is_empty());
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // markers, not formats
    fn a_transparent_stroke_sent_on_its_first_key_up_leaves_the_text_and_spacing_alone() {
        let (mut engine, hub) = engine_with_hub(
            r#"{"TKOG": "dog", "KAT": "{:transparent}{:firstup}{plover:focus}"}"#,
        );
        engine.translate(stroke_of("TKOG"), false);
        let [k, a, t] = kat_keys(&engine);
        let now = Instant::now();
        for key in [k, a, t] {
            engine.press(key, now);
        }
        // Sent on the first key up, with the others still held; a transparent line, no output
        assert_eq!(engine.release(t), None);
        assert_eq!(hub.snapshot().tape.len(), 2);
        assert!(hub.snapshot().tape[1].transparent);
        // Tapping a held key again is another transparent stroke
        engine.press(t, now);
        assert_eq!(engine.release(t), None);
        assert_eq!(hub.snapshot().tape.len(), 3);
        assert_eq!(engine.release(a), None);
        assert_eq!(engine.release(k), None);
        engine.translate(stroke_of("TKOG"), false);
        assert_eq!(engine.translator.text(), "dog dog");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
    fn a_transparent_stroke_never_repeats() {
        let mut engine = engine_with_dict(r#"{"KAT": "{:transparent}"}"#);
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        for key in keys {
            engine.press(key, t0 + ms(150));
        }
        assert_eq!(engine.tick(t0 + ms(600)), None);
        assert!(engine.repeating.is_none() && engine.arming.is_none());
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a command, not a format
    fn keys_typed_into_an_open_lookup_never_reach_the_translator_or_tape() {
        let (mut engine, hub) = engine_with_hub(r#"{"PHRUP": "{:lookup}", "KAT": "cat"}"#);
        engine.translate(stroke_of("PHRUP"), false);
        for c in "kat".chars() {
            engine.lookup_input(LookupKey::Char(c));
        }
        assert_eq!(engine.translator.text(), "");
        assert_eq!(hub.snapshot().tape.len(), 1, "only the lookup stroke itself");
    }

    /// Strike the stroke written `text`, all its keys down and up together.
    fn strike(engine: &mut StenoEngine, text: &str) -> Option<StenoOutput> {
        let bits = Layout::plover().parse_stroke(text).unwrap();
        let now = Instant::now();
        let keys: Vec<KeyCode> = (0..32)
            .filter(|bit| bits & (1 << bit) != 0)
            .map(|bit| key_for(engine, 1 << bit))
            .collect();
        for key in &keys {
            engine.press(*key, now);
        }
        let mut out = None;
        for key in &keys {
            out = engine.release_at(*key, now).or(out);
        }
        out
    }

    const CONTROL_DICT: &str = r#"{"KAT": "{:overlay:toggle}", "TKOG": "cat", "-G": "{:tape:clear}",
        "PWAT": "{:tape:sideways}", "-T": "{:lookup:open}", "-D": "{:lookup:toggle}"}"#;

    #[test]
    fn overlay_control_entries_type_nothing_and_publish_view_commands() {
        use super::super::overlay_control::{Part, Verb, ViewCommand};
        let (mut engine, hub) = engine_with_hub(CONTROL_DICT);
        assert_eq!(strike(&mut engine, "KAT"), None, "nothing is typed");
        let state = hub.snapshot();
        assert_eq!(state.tape.len(), 0, "a control stroke stays off the tape");
        assert_eq!(
            state.controls.back().map(|entry| entry.1),
            Some(ViewCommand {
                part: Part::Overlay,
                verb: Verb::Toggle
            })
        );
        assert_eq!(state.control_seq, 1);
        strike(&mut engine, "KAT");
        assert_eq!(hub.snapshot().control_seq, 2, "striking it again runs it again");
    }

    #[test]
    fn a_control_stroke_never_repeats_when_held() {
        let (mut engine, hub) = engine_with_hub(CONTROL_DICT);
        let keys = kat_keys(&engine);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        tap(&mut engine, keys, t0, t0 + ms(50));
        for key in keys {
            engine.press(key, t0 + ms(150));
        }
        assert_eq!(engine.tick(t0 + ms(600)), None);
        assert!(engine.repeating.is_none() && engine.arming.is_none());
        assert_eq!(hub.snapshot().control_seq, 1);
    }

    #[test]
    fn clearing_the_tape_empties_the_overlays_tape_and_is_not_a_view_command() {
        let (mut engine, hub) = engine_with_hub(CONTROL_DICT);
        strike(&mut engine, "TKOG");
        assert_eq!(hub.snapshot().tape.len(), 1);
        assert_eq!(strike(&mut engine, "-G"), None);
        let state = hub.snapshot();
        assert!(state.tape.is_empty());
        assert_eq!(state.control_seq, 0);
    }

    #[test]
    fn the_lookup_control_opens_closes_and_toggles_it() {
        let (mut engine, _hub) = engine_with_hub(CONTROL_DICT);
        assert_eq!(strike(&mut engine, "-T"), None);
        assert!(engine.lookup_active());
        // Opening it again leaves it open
        engine.run_overlay_control("lookup", "open");
        assert!(engine.lookup_active());
        engine.run_overlay_control("lookup", "toggle");
        assert!(!engine.lookup_active());
        engine.run_overlay_control("lookup", "toggle");
        assert!(engine.lookup_active());
        engine.run_overlay_control("lookup", "close");
        engine.run_overlay_control("lookup", "close");
        assert!(!engine.lookup_active());
    }

    const OUTLINE_DICT: &str = r#"{"KAT": "cat", "TKOG": "dog", "KAT/TKOG": "catdog", "-L": "{:outline:last}",
        "-B": "{:outline:next}", "-T": "{:overlay:toggle}", "SKWRO*EUBGS": "joy", "*": "=undo"}"#;

    #[test]
    fn outline_last_types_the_outline_of_the_last_translation() {
        let (mut engine, hub) = engine_with_hub(OUTLINE_DICT);
        strike(&mut engine, "SKWRO*EUBGS");
        strike(&mut engine, "-L");
        assert_eq!(engine.translator.text(), "joy SKWRO*EUBGS");
        // The typed notation is never the last outline, and the control stroke is transparent
        strike(&mut engine, "-L");
        assert_eq!(engine.translator.text(), "joy SKWRO*EUBGS SKWRO*EUBGS");
        assert_eq!(hub.snapshot().tape.len(), 3);
    }

    #[test]
    fn outline_last_of_a_multi_stroke_entry_is_every_stroke() {
        let (mut engine, _hub) = engine_with_hub(OUTLINE_DICT);
        strike(&mut engine, "KAT");
        strike(&mut engine, "TKOG");
        assert_eq!(engine.translator.text(), "catdog");
        strike(&mut engine, "-L");
        assert_eq!(engine.translator.text(), "catdog KAT/TKOG");
    }

    #[test]
    fn outline_last_with_nothing_typed_types_nothing_and_undo_takes_it_back() {
        let (mut engine, _hub) = engine_with_hub(OUTLINE_DICT);
        assert_eq!(strike(&mut engine, "-L"), None);
        strike(&mut engine, "KAT");
        strike(&mut engine, "-L");
        assert_eq!(engine.translator.text(), "cat KAT");
        strike(&mut engine, "*");
        assert_eq!(engine.translator.text(), "cat");
        strike(&mut engine, "*");
        assert_eq!(engine.translator.text(), "");
    }

    #[test]
    fn outline_next_types_the_next_strokes_notation_once() {
        let (mut engine, _hub) = engine_with_hub(OUTLINE_DICT);
        strike(&mut engine, "TKOG");
        assert_eq!(strike(&mut engine, "-B"), None);
        strike(&mut engine, "KAT");
        assert_eq!(engine.translator.text(), "dog KAT");
        strike(&mut engine, "KAT");
        assert_eq!(engine.translator.text(), "dog KAT cat");
    }

    #[test]
    fn outline_next_pressed_again_disarms_it() {
        let (mut engine, _hub) = engine_with_hub(OUTLINE_DICT);
        strike(&mut engine, "-B");
        strike(&mut engine, "-B");
        strike(&mut engine, "KAT");
        assert_eq!(engine.translator.text(), "cat");
    }

    #[test]
    fn outline_next_does_not_run_commands_or_undo() {
        let (mut engine, hub) = engine_with_hub(OUTLINE_DICT);
        strike(&mut engine, "KAT");
        strike(&mut engine, "-B");
        strike(&mut engine, "-T");
        assert_eq!(hub.snapshot().control_seq, 0, "the overlay control did not run");
        assert_eq!(engine.translator.text(), "cat -T");
        strike(&mut engine, "-B");
        strike(&mut engine, "*");
        assert_eq!(engine.translator.text(), "cat -T *");
        // The typed notation is undone like a word
        strike(&mut engine, "*");
        assert_eq!(engine.translator.text(), "cat -T");
    }

    #[test]
    fn a_bad_control_does_nothing() {
        let (mut engine, hub) = engine_with_hub(CONTROL_DICT);
        assert_eq!(strike(&mut engine, "PWAT"), None);
        engine.run_overlay_control("tape", "sideways");
        engine.run_overlay_control("overlay", "");
        assert_eq!(hub.snapshot().control_seq, 0);
    }
}

#[cfg(test)]
#[path = "engine_unload_tests.rs"]
mod unload_tests;

#[cfg(test)]
#[path = "send_early_tests.rs"]
mod send_early_tests;
