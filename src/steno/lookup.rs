//! Live lookup: "I'm mid-sentence and forgot the stroke for a word."
//!
//! A dictionary entry for `{:lookup}` opens a lookup (`{PLOVER:lookup}` still does, a legacy alias). The steno layer stays on, but its
//! keys go back to what the layers below it make of them: every key goes through the usual
//! remaps, mod-taps and layers, and what comes out types into a query that is shown on the
//! overlay instead of reaching the application. The overlay lists the outlines that type the
//! word, best first, and lights the first stroke of the best one on its keyboard. Esc or Enter
//! closes it.

use super::commands::proper_outlines_in;
use super::layout::Layout;
use super::notation::Notation;
use super::overlay::{LookupMode, LookupRow, LookupView, OverlayHub, TAPE_CAPACITY};
use super::reverse::{self, Query};
use super::translate::Translator;
use super::typing::TypingLayout;
use crate::keycode::KeyCode;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Most rows a lookup collects: as many as the tape area could ever hold lines (the tape is
/// never longer than [`TAPE_CAPACITY`]), best first, so the overlay can fill all of it and no row
/// that could be shown is dropped here. It was 12, with 4 outlines for the exact word.
const MAX_ROWS: usize = TAPE_CAPACITY;
/// Words that merely start with the query are only suggested from this length on: a scan
/// of the whole dictionary per key press isn't worth it for a single letter.
const MIN_PREFIX: usize = 2;

/// What a key press means inside a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKey {
    Char(char),
    Backspace,
    /// Esc or Enter: close the lookup
    Close,
    /// Any other key does nothing, and is still swallowed
    Ignore,
}

/// Turns the keys that come out of the layers into what a lookup does with them.
///
/// These are the keys the application would have got (after remaps, mod-taps and layers), so a
/// symbol layer or a Shift on a thumb key work as they do everywhere else. Shift capitalizes:
/// lookups are case-sensitive.
#[derive(Debug, Clone, Default)]
pub struct LookupInput {
    layout: TypingLayout,
    shift: HashSet<KeyCode>,
    /// Ctrl, Alt or Super held: the next key is a shortcut, not text
    shortcut: HashSet<KeyCode>,
    caps: bool,
}

impl LookupInput {
    #[must_use]
    pub fn new(layout: TypingLayout) -> Self {
        Self {
            layout,
            ..Self::default()
        }
    }

    /// A key came out of the layers (`pressed` is false for its release). Returns what the
    /// lookup should do about it, if anything.
    pub fn key(&mut self, key: KeyCode, pressed: bool) -> Option<LookupKey> {
        if key.is_modifier() {
            let held = if matches!(key, KeyCode::KC_LSFT | KeyCode::KC_RSFT) {
                &mut self.shift
            } else {
                &mut self.shortcut
            };
            if pressed {
                held.insert(key);
            } else {
                held.remove(&key);
            }
            return None;
        }
        if !pressed {
            return None;
        }
        match key {
            KeyCode::KC_ESC | KeyCode::KC_ENT => Some(LookupKey::Close),
            KeyCode::KC_BSPC => Some(LookupKey::Backspace),
            KeyCode::KC_CAPS => {
                self.caps = !self.caps;
                None
            }
            _ if !self.shortcut.is_empty() => Some(LookupKey::Ignore),
            _ => Some(
                match self
                    .layout
                    .character(key, !self.shift.is_empty(), self.caps)
                {
                    Some('\t' | '\n') | None => LookupKey::Ignore,
                    Some(c) => LookupKey::Char(c),
                },
            ),
        }
    }

    /// Forget which modifiers are held, as a lookup opens.
    pub fn reset(&mut self) {
        self.shift.clear();
        self.shortcut.clear();
        self.caps = false;
    }
}

/// An open lookup: the text typed so far, and the layout its stroke notation is read and written
/// in (so an extra key such as the `+` join key, in `ED/+-D`, parses, lights and prints).
///
/// A query is read in the layer's notation too (`NAEM` as well as `TPHAEPL`, see
/// [`super::notation`]). The rows' outlines stay in Plover's notation: the overlay draws them
/// key by key, and it reads them back with nothing but the layout.
#[derive(Debug)]
pub struct Lookup {
    query: String,
    notation: Notation,
}

impl Default for Lookup {
    fn default() -> Self {
        Self::new(Layout::plover())
    }
}

impl Lookup {
    /// A lookup with nothing typed yet, in `layout`'s stroke notation.
    #[must_use]
    pub const fn new(layout: Layout) -> Self {
        Self::with_notation(Notation::plain(layout))
    }

    /// A lookup with nothing typed yet, reading queries in `notation` as well as Plover's.
    #[must_use]
    pub const fn with_notation(notation: Notation) -> Self {
        Self {
            query: String::new(),
            notation,
        }
    }

    const fn layout(&self) -> &Layout {
        self.notation.layout()
    }

    /// Apply a key. Returns `false` when the lookup should close.
    pub fn apply(&mut self, key: LookupKey) -> bool {
        match key {
            LookupKey::Char(c) => self.query.push(c),
            LookupKey::Backspace => {
                self.query.pop();
            }
            LookupKey::Close => return false,
            LookupKey::Ignore => {}
        }
        true
    }

    /// What the overlay shows for the query.
    ///
    /// All-caps text with no lowercase letter that parses as steno notation (`KAT/-D`) resolves
    /// forward: one row, what it types. Anything else is a reverse lookup: a query with a space
    /// in it is a whole phrase, shown as one row, word by word, falling back to fingerspelling
    /// (marked in quotes) for a word with no entry — a single word keeps the fuller view this
    /// always had, its own outlines first then words that start with it, but now also falls back
    /// to fingerspelling when the dictionary has nothing at all, instead of showing nothing.
    ///
    /// `base` is a translator with nothing typed ([`Translator::fresh`]), over the dictionary
    /// the engine is using. See [`super::reverse`] for the engine this and the CLI's `keymux
    /// steno lookup` share.
    #[must_use]
    pub fn view(&self, base: &Translator) -> LookupView {
        self.view_unless(base, &|| false)
            .unwrap_or_else(|| self.blank())
    }

    /// What the overlay shows before the search has found anything: the query, no rows.
    fn blank(&self) -> LookupView {
        LookupView {
            query: self.query.clone(),
            rows: Vec::new(),
            hint: 0,
            strokes: Vec::new(),
            mode: LookupMode::Word,
        }
    }

    /// Like [`Self::view`], giving up (`None`) as soon as `cancelled` says so: it is asked
    /// between the steps of the search, so a search for text that has since changed stops early.
    fn view_unless(&self, base: &Translator, cancelled: &dyn Fn() -> bool) -> Option<LookupView> {
        let query = self.query.trim();
        if query.is_empty() {
            return Some(self.blank());
        }
        let (rows, mode) = match reverse::classify_in(&self.notation, query) {
            Query::Stroke(strokes) => (
                stroke_rows(self.layout(), base, &strokes),
                LookupMode::Stroke,
            ),
            Query::Text(text) if text.contains(char::is_whitespace) => (
                vec![phrase_row(self.layout(), base, &text)],
                LookupMode::Phrase,
            ),
            Query::Text(text) => (
                word_rows(self.layout(), base, &text, cancelled)?,
                LookupMode::Word,
            ),
        };
        if cancelled() {
            return None;
        }
        let hint = rows.first().map_or(0, |row: &LookupRow| {
            row.outline
                .split('/')
                .next()
                .and_then(|stroke| self.layout().parse_stroke(stroke))
                .unwrap_or(0)
        });
        // A phrase's outline is one group per word, not one stroke sequence to play through
        let strokes = match (mode, rows.first()) {
            (LookupMode::Phrase, _) | (_, None) => Vec::new(),
            (_, Some(row)) => self
                .layout()
                .parse_strokes_trimmed(&row.outline)
                .unwrap_or_default(),
        };
        Some(LookupView {
            query: self.query.clone(),
            rows,
            hint,
            strokes,
            mode,
        })
    }
}

/// One search to run: what was typed, over the dictionary as it was.
struct Job {
    generation: u64,
    lookup: Lookup,
    base: Translator,
}

/// Runs lookups on a thread of their own, so typing into one never waits for a search.
///
/// Every key press supersedes the search before it: [`Self::search`] bumps the generation, which
/// the search running now sees at its next step and gives up; the query shows at once with no
/// rows, and the new search starts as soon as the worker is free, skipping any it was behind on.
/// A result is published only if it is still the newest, checked under the same lock as the
/// blank view, so an old answer never lands over a newer query.
#[derive(Debug)]
pub struct LookupSearch {
    jobs: mpsc::Sender<Job>,
    newest: Arc<AtomicU64>,
    /// Held to publish: the generation is checked and the view shown without another key press
    /// slipping between
    publishing: Arc<Mutex<()>>,
    hub: &'static OverlayHub,
}

impl LookupSearch {
    #[must_use]
    pub fn new(hub: &'static OverlayHub) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let newest = Arc::new(AtomicU64::new(0));
        let publishing = Arc::new(Mutex::new(()));
        let (seen, gate) = (Arc::clone(&newest), Arc::clone(&publishing));
        std::thread::Builder::new()
            .name("steno-lookup".into())
            .spawn(move || {
                while let Ok(mut job) = queue.recv() {
                    // Only the newest is worth running
                    while let Ok(newer) = queue.try_recv() {
                        job = newer;
                    }
                    let cancelled = || seen.load(Ordering::Acquire) != job.generation;
                    let Some(view) = job.lookup.view_unless(&job.base, &cancelled) else {
                        continue;
                    };
                    let _gate = gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !cancelled() {
                        hub.set_lookup(Some(view));
                    }
                }
            })
            .expect("a thread for lookups");
        Self {
            jobs,
            newest,
            publishing,
            hub,
        }
    }

    /// Show `lookup`'s query now and find its rows without holding anything up.
    pub fn search(&self, lookup: &Lookup, base: Translator) {
        let gate = self
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = self.newest.fetch_add(1, Ordering::AcqRel) + 1;
        self.hub.set_lookup(Some(lookup.blank()));
        drop(gate);
        let query = Lookup {
            query: lookup.query.clone(),
            notation: lookup.notation.clone(),
        };
        let _ = self.jobs.send(Job {
            generation,
            lookup: query,
            base,
        });
    }

    /// The lookup closed: whatever is being found is no longer wanted.
    pub fn cancel(&self) {
        let _gate = self
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.newest.fetch_add(1, Ordering::AcqRel);
    }
}

/// What a stroke sequence types, as a single row: outline is the notation itself, text is the
/// result in quotes (or a note that nothing matched).
fn stroke_row(layout: &Layout, base: &Translator, strokes: &[u32]) -> LookupRow {
    let mut probe = base.clone();
    for &bits in strokes {
        probe.stroke(bits);
    }
    let text = probe.text();
    LookupRow {
        text: if text.is_empty() {
            "(no translation)".to_string()
        } else {
            format!("\u{2192} {text:?}")
        },
        outline: layout.render_strokes(strokes),
    }
}

/// Every row the raw-steno view shows: what the typed chord actually resolves to, first, then
/// the dictionary's other entries nearest to it - a chord a key or two off from a real entry
/// still shows what it was probably reaching for, instead of leaving it at "(no translation)".
fn stroke_rows(layout: &Layout, base: &Translator, strokes: &[u32]) -> Vec<LookupRow> {
    let mut rows = vec![stroke_row(layout, base, strokes)];
    for outline in reverse::nearby_strokes(base.dictionary(), strokes, MAX_ROWS - 1) {
        rows.push(stroke_row(layout, base, &outline));
    }
    rows
}

/// A whole phrase as a single row: the words (a fingerspelled one in quotes), and every stroke
/// that types them in order.
fn phrase_row(layout: &Layout, base: &Translator, phrase: &str) -> LookupRow {
    let segments = reverse::reverse_phrase(base, base.dictionary(), phrase);
    let (words, strokes) = reverse::display_with(layout, &segments);
    LookupRow {
        text: words,
        outline: strokes,
    }
}

/// A command as the lookup shows it: the entry's text without the markers that only say how it
/// repeats or joins (`{:firstup}`, `{:firstup:except=KEYS}`, `{:transparent}`), which are not what it does.
// The markers are dictionary metas, not format strings.
#[allow(clippy::literal_string_with_formatting_args)]
fn command_text(text: &str) -> String {
    let shown = super::meta::strip_first_up(text).replace("{:transparent}", "");
    if shown.is_empty() {
        text.to_string()
    } else {
        shown
    }
}

/// The existing single-word view: the word's own outlines first, then words that start with it,
/// each with its best outline — falling back to fingerspelling, marked in quotes, when the exact
/// query has no dictionary entry at all.
fn word_rows(
    layout: &Layout,
    base: &Translator,
    query: &str,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<LookupRow>> {
    let mut rows = Vec::new();
    let exact = proper_outlines_in(base, query);
    let plain = query
        .chars()
        .all(|c| c.is_alphanumeric() || c == '\'' || c == '-');
    // Entries that are not words (`{^ing}`, `{.}`, `{:tape:toggle}`) are found by what they are
    // written as, with or without their braces and markers: `^ing`, `{^ing}` and `ing` all find
    // the suffix. They type nothing as words, and "what was that stroke" is asked of them just
    // as often. A short plain query is not worth the scan.
    let entries: Vec<LookupRow> = if query.chars().count() >= MIN_PREFIX || !plain {
        if cancelled() {
            return None;
        }
        base.dictionary()
            .entries_matching(query, MAX_ROWS)
            .into_iter()
            .map(|(strokes, text)| LookupRow {
                text: command_text(&text),
                outline: layout.render_strokes(&strokes),
            })
            .collect()
    } else {
        Vec::new()
    };
    if exact.is_empty() {
        // Written like an entry that was found, it is not a word to fingerspell
        let missing = if plain || entries.is_empty() {
            reverse::missing_word(base, base.dictionary(), query, cancelled)
        } else {
            None
        };
        if cancelled() {
            return None;
        }
        if let Some(found) = &missing {
            rows.push(LookupRow {
                text: match (&found.composed_of, found.fingerspelled) {
                    (Some(pieces), _) => {
                        format!("{query} {}", reverse::composed_note(pieces))
                    }
                    (None, true) => format!("\"{query}\""),
                    (None, false) => query.to_string(),
                },
                outline: layout.render_strokes(&found.strokes),
            });
        }
    } else {
        for outline in exact.iter().take(MAX_ROWS) {
            rows.push(LookupRow {
                text: query.to_string(),
                outline: layout.render_strokes(outline),
            });
        }
    }
    // Right after the word's best outline: a word can have dozens of others, and an entry
    // listed behind them would be off the screen
    let at = rows.len().min(1);
    rows.splice(at..at, entries);
    if query.chars().count() >= MIN_PREFIX {
        // Ask for extra: a word with no outline that types it properly is skipped
        if cancelled() {
            return None;
        }
        for word in base.dictionary().completions(query, MAX_ROWS * 2) {
            if rows.len() >= MAX_ROWS {
                break;
            }
            // Cancelled between words, so a key press never waits on the rest of the list
            if cancelled() {
                return None;
            }
            if let Some(outline) = proper_outlines_in(base, word).first() {
                rows.push(LookupRow {
                    text: word.to_string(),
                    outline: layout.render_strokes(outline),
                });
            }
        }
    }
    rows.truncate(MAX_ROWS);
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::dict::Dictionary;
    use crate::steno::layout::Layout;

    fn translator() -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{
                "-T": "the",
                "KAPB": "can",
                "KAPBD": "candy",
                "KAPBL": "candle",
                "KAPBT": "cant",
                "KAPB/TKEU": "candy",
                "KA": "ca",
                "SPH-PL": "{^ing}"
            }"#,
            &Layout::plover(),
        )
        .unwrap();
        Translator::new(dict)
    }

    fn typed(query: &str) -> Lookup {
        let mut lookup = Lookup::default();
        for c in query.chars() {
            assert!(lookup.apply(LookupKey::Char(c)));
        }
        lookup
    }

    /// The lookup shown on a hub, once it holds rows or `wait` is up.
    fn shown(hub: &OverlayHub, wait: std::time::Duration) -> Option<LookupView> {
        let until = std::time::Instant::now() + wait;
        loop {
            if let Some(view) = hub.snapshot().lookup.filter(|view| !view.rows.is_empty()) {
                return Some(view);
            }
            if std::time::Instant::now() > until {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn typing_on_shows_the_query_at_once_and_only_the_newest_answer_lands() {
        let hub: &'static OverlayHub = Box::leak(Box::default());
        let search = LookupSearch::new(hub);
        // A burst of keys: each supersedes the one before
        let mut lookup = Lookup::default();
        for c in "candy".chars() {
            lookup.apply(LookupKey::Char(c));
            search.search(&lookup, translator());
            // The query is on show before any search has had to finish
            assert_eq!(hub.snapshot().lookup.unwrap().query, lookup.query);
        }
        let view = shown(hub, std::time::Duration::from_secs(5)).expect("an answer");
        assert_eq!(view.query, "candy");
        assert_eq!(view.rows[0].outline, "KAPBD");
        // Nothing older arrives afterwards
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hub.snapshot().lookup.unwrap().query, "candy");
    }

    #[test]
    fn a_search_cancelled_by_closing_never_shows() {
        let hub: &'static OverlayHub = Box::leak(Box::default());
        let search = LookupSearch::new(hub);
        search.search(&typed("candy"), translator());
        search.cancel();
        hub.set_lookup(None);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(hub.snapshot().lookup.is_none());
    }
    #[test]
    fn a_lookup_keeps_every_result_up_to_what_the_tape_area_can_hold() {
        use std::fmt::Write as _;
        // 150 words starting with "ca", and 20 outlines for one word: both past the old caps
        // (12 rows, 4 outlines)
        let layout = Layout::plover();
        let mut json = String::from("{");
        for i in 0..150u32 {
            let outline = crate::steno::layout::render_strokes(&[1 + i]);
            write!(json, "{outline:?}:\"ca{i:03}\",").unwrap();
        }
        for i in 0..20u32 {
            let outline = crate::steno::layout::render_strokes(&[0x1000 + i]);
            write!(json, "{outline:?}:\"many\",").unwrap();
        }
        json.push_str("\"-T\":\"the\"}");
        let mut dict = Dictionary::default();
        dict.merge_json(&json, &layout).unwrap();
        let base = Translator::new(dict);
        let rows = typed("ca").view(&base).rows;
        assert_eq!(rows.len(), 150, "every completion is a row");
        let rows = typed("many").view(&base).rows;
        assert_eq!(rows.len(), 20, "every outline of the word is a row");
    }

    // The entries are dictionary metas, not format strings.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[test]
    fn a_command_is_found_by_the_name_in_it() {
        let layout = Layout::plover();
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{
                "TAEP": "tape",
                "T*PL": "{:firstup}{:transparent}{:tape:toggle}",
                "T*PLZ": "{:transparent}{:tape:clear}",
                "KHR*F": "{#Control_L(v)}",
                "-T": "the",
                "SPH-PL": "{^tape}"
            }"#,
            &layout,
        )
        .unwrap();
        let base = Translator::new(dict);
        let rows = typed("tape").view(&base).rows;
        let shown: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.text.as_str(), r.outline.as_str()))
            .collect();
        assert_eq!(
            shown[0],
            ("tape", "TAEP"),
            "the word's best outline comes first"
        );
        assert!(shown.contains(&("{:tape:toggle}", "T*PL")), "{shown:?}");
        assert!(shown.contains(&("{:tape:clear}", "T*PLZ")), "{shown:?}");
        assert!(shown.contains(&("{^tape}", "SPH-PL")), "{shown:?}");
        assert_eq!(shown.len(), 4, "{shown:?}");
        assert_eq!(
            shown[1].1, "SPH-PL",
            "entries follow the best outline at once, the nearest first: {shown:?}"
        );
        assert_eq!(shown[2].1, "T*PL", "{shown:?}");
        let rows = typed("CONTROL").view(&base).rows;
        assert_eq!(rows[0].text, "{#Control_L(v)}");
        // A word with no command in it adds none
        assert_eq!(typed("the").view(&base).rows.len(), 1);
    }

    // The entries are dictionary metas, not format strings.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[test]
    fn any_entry_is_found_by_its_text_with_or_without_the_markers() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{
                "-G": "{^ing}",
                "SPEUPBG": "sing",
                "KPHAOEUPB": "{mine^}",
                "TP-PL": "{.}",
                "STKPW": "{:transparent}{PLOVER:SUSPEND}",
                "KPWH": "hello world",
                "-T": "the"
            }"#,
            &Layout::plover(),
        )
        .unwrap();
        let base = Translator::new(dict);
        let rows_of = |query: &str| -> Vec<(String, String)> {
            typed(query)
                .view(&base)
                .rows
                .into_iter()
                .map(|r| (r.text, r.outline))
                .collect()
        };
        let has = |rows: &[(String, String)], text: &str, outline: &str| {
            rows.iter().any(|(t, o)| t == text && o == outline)
        };
        for query in ["^ing", "{^ing}", "{^ING}", "ing", "{^in"] {
            let rows = rows_of(query);
            assert!(has(&rows, "{^ing}", "-G"), "{query}: {rows:?}");
            assert!(
                !rows.iter().any(|(t, _)| t.starts_with('"')),
                "{query}: {rows:?}"
            );
        }
        assert_eq!(rows_of("{^ing}")[0].1, "-G", "exact text first");
        assert_eq!(rows_of("^ing")[0].1, "-G");
        assert_eq!(rows_of("{mine^}")[0].1, "KPHAOEUPB");
        assert_eq!(rows_of("mine^")[0].0, "{mine^}");
        assert_eq!(rows_of("{.}")[0].1, "TP-PL");
        assert_eq!(rows_of(".")[0].1, "TP-PL");
        assert_eq!(rows_of("suspend")[0].0, "{PLOVER:SUSPEND}");
        assert_eq!(rows_of("{PLOVER:SUSPEND}")[0].1, "STKPW");
        // Under 3 characters a query only finds the entries it begins, not ones it is inside
        assert!(!has(&rows_of("ng"), "{^ing}", "-G"), "{:?}", rows_of("ng"));
        assert!(has(&rows_of("^in"), "{^ing}", "-G"));
        assert!(has(&rows_of("ing"), "{^ing}", "-G"));
        assert_eq!(
            rows_of("the").len(),
            1,
            "a word with no entry in it adds none"
        );
    }

    /// A dictionary and lookup in a layout with the `+` join key, as the lyk theory has it.
    fn joined() -> (Layout, Translator) {
        let layout = Layout::plover_with(&['+']).unwrap();
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"ED": "end", "+-D": "{^ed}", "ED/+-D": "ended", "KAT": "cat"}"#,
            &layout,
        )
        .unwrap();
        (layout, Translator::new(dict))
    }

    fn typed_in(layout: &Layout, query: &str) -> Lookup {
        let mut lookup = Lookup::new(layout.clone());
        for c in query.chars() {
            assert!(lookup.apply(LookupKey::Char(c)));
        }
        lookup
    }

    #[test]
    fn a_word_whose_outline_has_the_join_key_prints_and_plays_it() {
        let (layout, base) = joined();
        let join = 1u32 << layout.extra_start();
        let view = typed_in(&layout, "ended").view(&base);
        assert_eq!(view.rows[0].outline, "ED/+-D");
        assert_eq!(view.strokes.len(), 2, "both strokes are played");
        assert_eq!(view.strokes[0] & join, 0, "ED has no join key");
        assert_ne!(view.strokes[1] & join, 0, "+-D lights the join key");
        assert_eq!(view.strokes[1], layout.parse_stroke("+-D").unwrap());
        assert_eq!(view.hint, view.strokes[0]);
    }

    #[test]
    fn a_query_in_the_theorys_notation_resolves_and_rows_stay_plover_for_the_overlay() {
        let (layout, base) = joined();
        let notation = Notation::from_json(layout.clone(), r#"{"right": {"PB": "N"}}"#).unwrap();
        let mut lookup = Lookup::with_notation(notation);
        for c in "EN".chars() {
            assert!(lookup.apply(LookupKey::Char(c)));
        }
        let view = lookup.view(&base);
        assert_eq!(view.mode, LookupMode::Stroke);
        assert_eq!(view.rows[0].outline, "EPB");
        assert_eq!(view.strokes, [layout.parse_stroke("EPB").unwrap()]);
    }

    #[test]
    fn a_suffix_that_is_one_join_stroke_lights_the_join_key() {
        let (layout, base) = joined();
        let join = 1u32 << layout.extra_start();
        let view = typed_in(&layout, "+-D").view(&base);
        assert_eq!(view.mode, LookupMode::Stroke);
        assert_eq!(view.rows[0].outline, "+-D");
        assert_eq!(view.strokes.len(), 1);
        assert_ne!(view.hint & join, 0);
    }

    #[test]
    fn join_key_notation_typed_into_the_lookup_resolves_forward() {
        let (layout, base) = joined();
        let view = typed_in(&layout, "ED/+-D").view(&base);
        assert_eq!(view.mode, LookupMode::Stroke);
        assert_eq!(view.rows[0].outline, "ED/+-D");
        assert_eq!(view.rows[0].text, "\u{2192} \"ended\"");
        assert_eq!(view.strokes.len(), 2);
        // Without the key in the layout it is not notation at all: text, not a stroke
        assert_eq!(typed("ED/+-D").view(&translator()).mode, LookupMode::Word);
    }

    #[test]
    fn a_plain_outline_is_unchanged_by_a_layout_with_the_join_key() {
        let (layout, base) = joined();
        let view = typed_in(&layout, "cat").view(&base);
        assert_eq!(view.rows[0].outline, "KAT");
        assert_eq!(view.strokes, [layout.parse_stroke("KAT").unwrap()]);
    }

    #[test]
    fn an_exact_word_lists_its_outlines_fewest_strokes_first_and_lights_the_best() {
        let view = typed("candy").view(&translator());
        assert_eq!(view.query, "candy");
        let outlines: Vec<_> = view.rows.iter().map(|r| r.outline.as_str()).collect();
        assert_eq!(outlines, ["KAPBD", "KAPB/TKEU"]);
        assert!(view.rows.iter().all(|row| row.text == "candy"));
        // The keyboard lights the first stroke of the best outline
        assert_eq!(view.hint, Layout::plover().parse_stroke("KAPBD").unwrap());
    }

    #[test]
    fn the_view_carries_every_stroke_of_the_best_outline_for_the_overlay_to_play() {
        let layout = Layout::plover();
        // One stroke: just that one
        let one = typed("candy").view(&translator());
        assert_eq!(one.strokes, [layout.parse_stroke("KAPBD").unwrap()]);
        // Several: all of them in order, the first being the hint
        let several = typed("KAPB/TKEU").view(&translator());
        assert_eq!(several.rows[0].outline, "KAPB/TKEU");
        assert_eq!(
            several.strokes,
            [
                layout.parse_stroke("KAPB").unwrap(),
                layout.parse_stroke("TKEU").unwrap()
            ]
        );
        assert_eq!(several.hint, several.strokes[0]);
        // Nothing found, nothing to play
        assert!(
            typed("zzzz").view(&translator()).strokes.is_empty(),
            "expected none"
        );
        // A phrase's outline is a group per word, not a sequence to step through
        assert!(
            typed("candy candy").view(&translator()).strokes.is_empty(),
            "expected none"
        );
    }

    #[test]
    fn words_that_start_with_the_query_follow_the_exact_match_shortest_first() {
        let view = typed("can").view(&translator());
        let rows: Vec<_> = view
            .rows
            .iter()
            .map(|r| (r.text.as_str(), r.outline.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("can", "KAPB"),
                ("cant", "KAPBT"),
                ("candy", "KAPBD"),
                ("candle", "KAPBL"),
            ]
        );
    }

    #[test]
    fn an_unknown_or_empty_query_has_no_rows_and_no_hint() {
        for query in ["", "qzx", "   "] {
            let view = typed(query).view(&translator());
            assert!(view.rows.is_empty(), "{query:?}");
            assert_eq!(view.hint, 0);
        }
    }

    #[test]
    fn one_letter_gives_no_suggestions_but_an_exact_word_still_does() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"-T": "the", "A": "a", "AB": "about"}"#,
            &Layout::plover(),
        )
        .unwrap();
        let base = Translator::new(dict);
        let rows = typed("a").view(&base).rows;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outline, "A");
    }

    fn press(input: &mut LookupInput, key: KeyCode) -> Option<LookupKey> {
        let result = input.key(key, true);
        input.key(key, false);
        result
    }

    #[test]
    fn lookups_are_case_sensitive_and_shift_capitalizes() {
        let mut input = LookupInput::default();
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('c')));
        input.key(KeyCode::KC_LSFT, true);
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('C')));
        input.key(KeyCode::KC_LSFT, false);
        assert_eq!(press(&mut input, KeyCode::KC_C), Some(LookupKey::Char('c')));
        let view = typed("Can").view(&translator());
        assert!(view.rows.is_empty(), "the dictionary has `can`, not `Can`");
    }

    #[test]
    fn keys_mean_what_the_layers_made_of_them() {
        let mut input = LookupInput::default();
        assert_eq!(
            press(&mut input, KeyCode::KC_SPC),
            Some(LookupKey::Char(' '))
        );
        assert_eq!(press(&mut input, KeyCode::KC_5), Some(LookupKey::Char('5')));
        assert_eq!(
            press(&mut input, KeyCode::KC_MINS),
            Some(LookupKey::Char('-'))
        );
        // Symbols come with Shift, as from a symbol layer, and so does Caps Lock for letters
        input.key(KeyCode::KC_RSFT, true);
        assert_eq!(press(&mut input, KeyCode::KC_5), Some(LookupKey::Char('%')));
        assert_eq!(
            press(&mut input, KeyCode::KC_QUOT),
            Some(LookupKey::Char('"'))
        );
        input.key(KeyCode::KC_RSFT, false);
        input.key(KeyCode::KC_CAPS, true);
        assert_eq!(press(&mut input, KeyCode::KC_A), Some(LookupKey::Char('A')));
        // F-keys, arrows and Tab do nothing; a shortcut isn't text; Esc and Enter close
        assert_eq!(press(&mut input, KeyCode::KC_F1), Some(LookupKey::Ignore));
        assert_eq!(press(&mut input, KeyCode::KC_TAB), Some(LookupKey::Ignore));
        input.key(KeyCode::KC_LCTL, true);
        assert_eq!(press(&mut input, KeyCode::KC_A), Some(LookupKey::Ignore));
        input.key(KeyCode::KC_LCTL, false);
        assert_eq!(press(&mut input, KeyCode::KC_ESC), Some(LookupKey::Close));
        assert_eq!(press(&mut input, KeyCode::KC_ENT), Some(LookupKey::Close));
        assert_eq!(
            press(&mut input, KeyCode::KC_BSPC),
            Some(LookupKey::Backspace)
        );
    }

    #[test]
    fn the_typing_layout_decides_the_letters() {
        let mut input = LookupInput::new(TypingLayout::Dvorak);
        assert_eq!(press(&mut input, KeyCode::KC_S), Some(LookupKey::Char('o')));
    }

    #[test]
    fn backspace_edits_the_query_and_close_ends_it() {
        let mut lookup = typed("cat");
        assert!(lookup.apply(LookupKey::Backspace));
        assert_eq!(lookup.view(&translator()).query, "ca");
        // Backspacing past the start is harmless
        for _ in 0..5 {
            assert!(lookup.apply(LookupKey::Backspace));
        }
        assert_eq!(lookup.view(&translator()).query, "");
        assert!(!lookup.apply(LookupKey::Close));
    }

    #[test]
    fn an_all_caps_query_resolves_the_stroke_forward() {
        let view = typed("KAPB").view(&translator());
        // The exact resolution always leads
        assert_eq!(view.rows[0].text, "\u{2192} \"can\"");
        assert_eq!(view.rows[0].outline, "KAPB");
        assert_eq!(view.hint, Layout::plover().parse_stroke("KAPB").unwrap());
        // Raw steno notation: the overlay labels this "Engine Resolver", not "Lookup"
        assert_eq!(view.mode, LookupMode::Stroke);
        // Every single-stroke entry in `translator()`'s dictionary besides "KAPB" itself and the
        // two-stroke "KAPB/TKEU" is close enough (they're all a handful of keys apart) to show
        // up as a "did you mean" row after it.
        let outlines: Vec<&str> = view.rows.iter().map(|r| r.outline.as_str()).collect();
        assert_eq!(outlines.len(), 7, "{outlines:?}");
        for expected in ["-T", "KAPBD", "KAPBL", "KAPBT", "KA", "SPH-PL"] {
            assert!(
                outlines.contains(&expected),
                "expected {expected} in {outlines:?}"
            );
        }
    }

    #[test]
    fn a_query_with_a_space_looks_up_the_whole_phrase_as_one_row() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"KAPB": "can", "-T": "the", "KAPBD": "candy"}"#,
            &Layout::plover(),
        )
        .unwrap();
        let view = typed("can the candy").view(&Translator::new(dict));
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].text, "can the candy");
        assert_eq!(view.rows[0].outline, "KAPB / -T / KAPBD");
        // A whole phrase: the overlay labels this "Translator", not "Lookup"
        assert_eq!(view.mode, LookupMode::Phrase);
    }

    #[test]
    fn a_single_word_query_and_an_empty_one_are_the_plain_lookup_mode() {
        assert_eq!(typed("candy").view(&translator()).mode, LookupMode::Word);
        assert_eq!(typed("").view(&translator()).mode, LookupMode::Word);
    }

    #[test]
    fn a_word_with_no_dictionary_entry_falls_back_to_fingerspelling_in_quotes() {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"TPH": "{&n}", "SR*": "{&v}", "SKW*": "{&i}", "PH*": "{&m}"}"#,
            &Layout::plover(),
        )
        .unwrap();
        let view = typed("nvim").view(&Translator::new(dict));
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].text, "\"nvim\"");
        assert_eq!(view.rows[0].outline, "TPH/SR*/SKW*/PH*");
    }
}
