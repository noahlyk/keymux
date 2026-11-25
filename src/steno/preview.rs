//! The overlay's preview: while stroke keys are held, the live line of the tape shows, dimmed,
//! what the chord would type if it were let go now.
//!
//! The chord is struck on a copy of the translator ([`Translator::preview`]), so the preview is
//! exactly what the release would type: a dictionary entry, a plugin's reading (waited on no
//! longer than a stroke waits, `…` when it is late), a `+` continuation or the next stroke of a
//! multi-stroke entry as the whole word the engine would retranslate it to, spaced and
//! capitalised after what is on screen (shown without its spaces: `Night` after a full stop). A
//! stroke whose entry runs something (a command, a macro such as undo, key presses, a meta that
//! types nothing yet) shows that entry as written, `{:tape:clear}`, and never runs it. A stroke
//! with no translation shows `?`, as the tape does.
//!
//! Work happens on a thread of its own, never on the key path: a key change only bumps a counter
//! and sends the chord; the thread takes the newest chord, copies the translator's history (under
//! its lock, which a stroke takes too), strikes it and publishes to the hub, which drops a preview
//! that is no longer for the keys held. Nothing is computed unless an overlay is subscribed and
//! showing the tape (no lookup open), and the config's `overlay: (preview: true)` is on.

use super::notation::{Notation, Split};
use super::overlay::OverlayHub;
use super::translate::Translator;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// What a held chord would do, as the overlay draws it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// It types `text`
    #[default]
    Text,
    /// It runs or presses something, or types nothing yet: `text` is its entry as written
    Entry,
    /// A plugin is still reading it
    Late,
    /// Nothing translates it
    Unmatched,
}

/// The preview of the chord held, published on the hub (see [`OverlayHub::set_preview`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    /// The stroke bits it is for: the overlay draws it only while exactly these are lit
    pub chord: u32,
    pub kind: Kind,
    pub text: String,
    /// A plugin read it, not a dictionary entry
    pub read: bool,
    /// How the chord's keys split, as the outline it would end is written (see
    /// [`super::overlay::TapeEntry::split`]); `None`: the notation's table
    pub split: Option<Split>,
}

impl Preview {
    /// The text the live line shows after the stroke.
    #[must_use]
    pub fn shown(&self) -> &str {
        match self.kind {
            Kind::Text | Kind::Entry => &self.text,
            Kind::Late => "…",
            Kind::Unmatched => "?",
        }
    }
}

/// What striking a chord on a copy of the translator came to (see [`Translator::preview`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Struck {
    pub kind: Kind,
    pub text: String,
    pub read: bool,
    /// The strokes of the translation it ends
    pub outline: Vec<u32>,
    /// What the plugin that read it reported of their sound groups
    pub groups: Option<(Vec<u32>, Vec<Vec<u32>>)>,
}

/// The word a stroke changed, as it reads on screen afterwards.
///
/// From the start of the word where `before` and `after` first differ (or from the space a new
/// word starts with) to the end, without the spaces around it. Empty when the stroke only took
/// text away.
#[must_use]
pub fn changed_word(before: &str, after: &str) -> String {
    let mut at = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !after.is_char_boundary(at) {
        at -= 1;
    }
    let rest = &after[at..];
    if rest.trim().is_empty() {
        return String::new();
    }
    let start = if rest.starts_with(char::is_whitespace) {
        at
    } else {
        after[..at]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8())
    };
    after[start..].trim().to_string()
}

/// How each stroke of `strokes` splits into key groups: the one rule behind the preview, the
/// tape (file and overlay) and the text the outline commands type (see the README's theory
/// notation, **Precedence**).
///
/// By precedence: the notation file's own `outlines` entry for that outline, then what the
/// plugin that read it reported (`groups`), then the outline's known split (`agree`), then none.
#[must_use]
pub fn splits_of(
    notation: &Notation,
    strokes: &[u32],
    groups: Option<&(Vec<u32>, Vec<Vec<u32>>)>,
) -> Vec<Option<Split>> {
    let known = || notation.outline_split(strokes);
    let own = notation.outline_override(strokes).and_then(|_| known());
    let reported = || {
        groups
            .filter(|(read, groups)| read == strokes && groups.len() == strokes.len())
            .map(|(_, groups)| groups.clone())
    };
    own.or_else(reported).or_else(known).map_or_else(
        || vec![None; strokes.len()],
        |split| split.into_iter().map(Some).collect(),
    )
}

/// The preview of `chord` struck on `base`, waiting on a plugin as long as a stroke would.
#[must_use]
pub fn compute(base: &Translator, notation: &Notation, chord: u32) -> Preview {
    let struck = base.preview(chord);
    made(notation, chord, struck)
}

/// [`compute`], waiting on a plugin's reading no longer than `wait` (see
/// [`Translator::preview_within`]).
#[must_use]
pub fn compute_within(
    base: &Translator,
    notation: &Notation,
    chord: u32,
    wait: Duration,
) -> Preview {
    let struck = base.preview_within(chord, wait);
    made(notation, chord, struck)
}

fn made(notation: &Notation, chord: u32, struck: Struck) -> Preview {
    let split = (!notation.is_plain()
        && struck.outline.last() == Some(&chord)
        && matches!(struck.kind, Kind::Text))
    .then(|| splits_of(notation, &struck.outline, struck.groups.as_ref()))
    .and_then(|mut splits| splits.pop().flatten());
    Preview {
        chord,
        kind: struck.kind,
        text: struck.text,
        read: struck.read,
        split,
    }
}

/// How long a late preview keeps looking for the plugin's reading before it gives up.
const LATE_PATIENCE: Duration = Duration::from_secs(2);

/// How long the first look at a chord waits on a plugin: next to nothing, so a word only a
/// plugin reads shows `…` at once instead of after the stroke's whole deadline, while a reading
/// already in (or one that takes a millisecond) shows straight away. A dictionary word never
/// waits on a plugin at all.
const FIRST_WAIT: Duration = Duration::from_millis(2);

/// How long each later look waits on the plugin's reading: it returns the moment the reading
/// is in, and a newer chord is seen between looks, so this bounds how late a new key's preview
/// can be while a plugin reads.
const LATE_POLL: Duration = Duration::from_millis(10);

enum Job {
    /// What to strike chords on from now on: a [`Translator::preview_handle`] and the notation
    Base(Box<(Translator, Notation)>),
    /// The chord held now, and its number
    Chord(u64, u32),
}

/// Makes previews on a thread of its own (see the module doc).
#[derive(Debug)]
pub struct Previewer {
    jobs: mpsc::Sender<Job>,
    newest: Arc<AtomicU64>,
    /// Held to publish: the number is checked and the preview shown with no key change between
    publishing: Arc<Mutex<()>>,
    /// The base sent is the translator's as it is now
    based: bool,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Base(_) => f.write_str("Base"),
            Self::Chord(seq, chord) => write!(f, "Chord({seq}, {chord:#x})"),
        }
    }
}

impl Previewer {
    /// Start the thread, publishing to `hub`. `None` if no thread could be started.
    #[must_use]
    pub fn spawn(hub: &'static OverlayHub) -> Option<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let newest = Arc::new(AtomicU64::new(0));
        let publishing = Arc::new(Mutex::new(()));
        let (seen, gate) = (Arc::clone(&newest), Arc::clone(&publishing));
        let spawned = std::thread::Builder::new()
            .name("steno-preview".into())
            .spawn(move || run(&queue, hub, &seen, &gate));
        match spawned {
            Ok(_) => Some(Self {
                jobs,
                newest,
                publishing,
                based: false,
            }),
            Err(e) => {
                tracing::warn!("no thread for the overlay's preview: {e}");
                None
            }
        }
    }

    /// The translator's dictionaries or settings changed: the next chord sends a new base.
    pub const fn rebase(&mut self) {
        self.based = false;
    }

    /// Preview `chord` (struck on `translator`, written in `notation`). Costs the key path a
    /// counter and a channel send, and a base (a few reference counts) when it changed.
    pub fn request(&mut self, translator: &Translator, notation: &Notation, chord: u32) {
        if !self.based {
            let base = Box::new((translator.preview_handle(), notation.clone()));
            if self.jobs.send(Job::Base(base)).is_err() {
                return;
            }
            self.based = true;
        }
        let seq = self.bump();
        let _ = self.jobs.send(Job::Chord(seq, chord));
    }

    /// No preview is wanted any more (the stroke went, or nothing is held): one being made is
    /// not shown.
    pub fn cancel(&self) {
        self.bump();
    }

    fn bump(&self) -> u64 {
        let _gate = self
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.newest.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// The preview thread: the newest chord, struck on the newest base.
fn run(queue: &mpsc::Receiver<Job>, hub: &OverlayHub, seen: &AtomicU64, gate: &Mutex<()>) {
    let mut base: Option<Box<(Translator, Notation)>> = None;
    let mut next: Option<Job> = None;
    loop {
        let job = match next.take() {
            Some(job) => job,
            None => match queue.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        let mut chord = None;
        for job in std::iter::once(job).chain(std::iter::from_fn(|| queue.try_recv().ok())) {
            match job {
                Job::Base(new) => base = Some(new),
                Job::Chord(seq, bits) => chord = Some((seq, bits)),
            }
        }
        let (Some((seq, bits)), Some(made)) = (chord, base.as_deref()) else {
            continue;
        };
        let (translator, notation) = made;
        let current = || seen.load(Ordering::Acquire) == seq;
        if !current() {
            continue;
        }
        let started = Instant::now();
        let mut preview = compute_within(translator, notation, bits, FIRST_WAIT);
        tracing::trace!(
            "preview of {bits:#x}: {:?} in {:?}",
            preview.kind,
            started.elapsed()
        );
        publish(hub, gate, &current, &preview);
        // A plugin still reading (`…` shows meanwhile): look again until its reading is in,
        // unless a newer chord comes. Each look returns as soon as the reading lands.
        let late_since = Instant::now();
        let mut quick = false;
        while preview.kind == Kind::Late && late_since.elapsed() < LATE_PATIENCE {
            // A look that came back at once did not wait: wait here instead, never spin
            let newer = if quick {
                queue.recv_timeout(LATE_POLL)
            } else {
                queue.try_recv().map_err(|e| match e {
                    mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
                })
            };
            match newer {
                Ok(job) => {
                    next = Some(job);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if !current() {
                break;
            }
            let looked = Instant::now();
            preview = compute_within(translator, notation, bits, LATE_POLL);
            quick = looked.elapsed() < LATE_POLL / 2;
            if preview.kind != Kind::Late {
                publish(hub, gate, &current, &preview);
            }
        }
    }
}

fn publish(hub: &OverlayHub, gate: &Mutex<()>, current: &dyn Fn() -> bool, preview: &Preview) {
    let _gate = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current() {
        hub.set_preview(preview.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changed_word_is_the_whole_word_without_its_spaces() {
        assert_eq!(changed_word("the", "the night"), "night");
        assert_eq!(changed_word(" make", " making"), "making");
        assert_eq!(
            changed_word("go nat", "go internationalization"),
            "internationalization"
        );
        assert_eq!(changed_word("end.", "end. Night"), "Night");
        assert_eq!(changed_word("", "Today"), "Today");
        assert_eq!(changed_word("a cat", "a"), "", "only taken away");
        assert_eq!(changed_word("a cat", "a cat "), "", "only a space");
        assert_eq!(
            changed_word("é", "éa"),
            "éa",
            "a character boundary is kept"
        );
        assert_eq!(changed_word("x", "x in the"), "in the");
    }

    #[test]
    fn a_split_is_the_outlines_entry_then_a_plugins_report_then_the_known_split() {
        let layout = super::super::layout::Layout::plover();
        let notation = Notation::from_json(
            layout.clone(),
            r#"{"left": {"TPH": "N", "TP": "F"}, "right": {"GS": "ŠN"},
                "outlines": {"TPAGS": "FAŠN"}, "agree": ["TPH-GS"]}"#,
        )
        .unwrap();
        let s = |text: &str| layout.parse_stroke(text).unwrap();
        let (nation, fashion) = (s("TPH-GS"), s("TPAGS"));
        // A report that groups `T P` (F) where the table groups `T P H` (N)
        let report = |stroke: u32| (vec![stroke], vec![vec![s("TP"), s("-GS")]]);
        let known = splits_of(&notation, &[nation], None);
        assert_eq!(known, [Some(vec![s("TPH"), s("-GS")])]);
        assert_eq!(
            splits_of(&notation, &[nation], Some(&report(nation))),
            [Some(vec![s("TP"), s("-GS")])],
            "a report beats the known split"
        );
        let own = splits_of(&notation, &[fashion], None);
        assert_eq!(
            splits_of(&notation, &[fashion], Some(&report(fashion))),
            own
        );
        assert_eq!(
            splits_of(&notation, &[s("KAT")], None),
            [None],
            "nothing known: no split"
        );
    }

    fn translator(json: &str) -> Translator {
        let mut dict = super::super::dict::Dictionary::default();
        dict.merge_json(json, &super::super::layout::Layout::plover())
            .unwrap();
        Translator::new(dict)
    }

    fn wait_for(hub: &OverlayHub, chord: u32) -> Preview {
        let started = Instant::now();
        loop {
            if let Some(preview) = hub.snapshot().preview.filter(|p| p.chord == chord) {
                return preview;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no preview");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn the_thread_publishes_the_newest_chord_struck_on_what_is_typed_then() {
        let layout = super::super::layout::Layout::plover();
        let notation = Notation::plain(layout.clone());
        let kat = layout.parse_stroke("KAT").unwrap();
        let hub: &'static OverlayHub = Box::leak(Box::default());
        let mut t = translator(r#"{"KAT": "cat", "TP-PL": "{.}"}"#);
        let mut previewer = Previewer::spawn(hub).unwrap();
        hub.set_keys(kat, kat);
        previewer.request(&t, &notation, kat);
        assert_eq!(wait_for(hub, kat).text, "cat");
        // Typed since: the same base, sharing the history, sees it
        previewer.cancel();
        hub.set_keys(0, 0);
        assert_eq!(hub.snapshot().preview, None, "the keys went");
        t.stroke(layout.parse_stroke("TP-PL").unwrap());
        hub.set_keys(kat, kat);
        previewer.request(&t, &notation, kat);
        assert_eq!(wait_for(hub, kat).text, "Cat");
        // A preview for keys no longer the chord is never shown
        hub.set_keys(1, 1);
        hub.set_preview(Preview {
            chord: kat,
            ..Preview::default()
        });
        assert_eq!(hub.snapshot().preview, None);
    }

    /// Times previews on a real theory's folder (its dictionaries and plugins, the solver
    /// included): a dictionary word, a `+` join, and words only a plugin reads, cold (asked
    /// for the first time) and warm (its reading already in), after a few hundred words typed.
    /// `KEYMUX_PREVIEW_STENO=<folder> cargo test --release --lib time_previews -- --ignored
    /// --nocapture`; its `+` is an extra key.
    #[test]
    #[ignore = "times previews on a real theory folder: KEYMUX_PREVIEW_STENO"]
    fn time_previews_on_a_real_theory() {
        use super::super::formatter::Options;
        use super::super::layout::Layout;
        use super::super::plugin::Chain;
        let Some(dir) = std::env::var_os("KEYMUX_PREVIEW_STENO") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "rs")))
            .collect();
        files.sort();
        let layout = Layout::plover_with_joins(&['+'], &[]).unwrap();
        let started = Instant::now();
        let chain = Chain::load_cached(&files, &dir.join("cache"), &layout);
        eprintln!("loaded {} files in {:?}", files.len(), started.elapsed());
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        t.set_plugin_deadline(Duration::from_millis(30));
        let s = |text: &str| layout.parse_stroke(text).unwrap();
        // The plugins are built and ready: a word only the solver reads is read in time
        let started = Instant::now();
        while t.preview(s("SKWRAOEFRP")).kind == Kind::Late {
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "plugin never ready"
            );
        }
        eprintln!("plugins ready after {:?}", started.elapsed());
        for word in ["TPHAOEUT", "TPHAT", "SKP", "TKOG", "KAT"]
            .iter()
            .cycle()
            .take(400)
        {
            t.stroke(s(word));
        }
        let time = |t: &Translator, label: &str, stroke: u32, runs: usize| {
            let mut took: Vec<Duration> = (0..runs)
                .map(|_| {
                    let at = Instant::now();
                    let preview = t.preview(stroke);
                    let took = at.elapsed();
                    assert!(!preview.text.is_empty() || preview.kind != Kind::Text);
                    took
                })
                .collect();
            took.sort();
            let shown = t.preview(stroke);
            eprintln!(
                "{label:<28} {:?} {:<24} median {:>9.1?} p99 {:>9.1?} max {:>9.1?}",
                shown.kind,
                shown.text,
                took[runs / 2],
                took[runs * 99 / 100],
                took[runs - 1]
            );
        };
        time(&t, "dictionary word TPHAOEUT", s("TPHAOEUT"), 500);
        // For scale: the same strokes struck for real (on a copy, so the history stays put)
        for (label, word) in [
            ("real stroke TPHAOEUT", "TPHAOEUT"),
            ("real stroke TKPWRAOUFL", "TKPWRAOUFL"),
        ] {
            let mut took: Vec<Duration> = (0..200)
                .map(|_| {
                    let mut copy = t.clone();
                    let at = Instant::now();
                    copy.stroke(s(word));
                    at.elapsed()
                })
                .collect();
            took.sort();
            eprintln!(
                "{label:<28} median {:>9.1?} p99 {:>9.1?}",
                took[100], took[198]
            );
        }
        t.stroke(s("TPHAT"));
        time(&t, "join TPHAT/+STKPWAEUGS", s("+STKPWAEUGS"), 500);
        t.stroke(s("TKOG"));
        for word in ["SKWRAOEFRP", "TKPWRAOUFL", "PWRAOEUFT", "STKPWHRAOEUFRPBLG"] {
            let at = Instant::now();
            let cold = t.preview(s(word));
            eprintln!(
                "solver {word:<20} cold {:?} {:?} in {:?}",
                cold.kind,
                cold.text,
                at.elapsed()
            );
            time(&t, &format!("solver {word} warm"), s(word), 200);
        }
        // The key path's share: a counter and a channel send per key change
        let hub: &'static OverlayHub = Box::leak(Box::default());
        let mut previewer = Previewer::spawn(hub).unwrap();
        let notation = Notation::plain(layout.clone());
        let at = Instant::now();
        for _ in 0..10_000 {
            previewer.request(&t, &notation, s("TPHAOEUT"));
        }
        eprintln!("request on the key path: {:?} each", at.elapsed() / 10_000);
    }
}
