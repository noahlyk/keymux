//! What the focused text box says is around the cursor.
//!
//! The translator keeps its own record of the text it typed, and backspaces by that record. It
//! goes wrong the moment the text changes behind its back: a click moves the cursor, the
//! application autocorrects, a key is dropped. Some applications will say what is really
//! there, over the Wayland input-method protocol (see `keymux textbox`). This module holds what
//! they report, and decides whether the record still agrees.
//!
//! Applications that don't report anything leave the record as the only source, exactly as
//! before. Nothing here is ever required.
//!
//! Only the last few characters before the cursor are kept, in memory, and never logged.

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How many characters before the cursor are kept. More than a stroke ever retypes.
pub const MAX_BEFORE: usize = 64;

/// How long a report must have gone without a newer one before it is believed.
///
/// One edit makes the application report several times (each backspace, the text), and the
/// first of them describes a half-finished edit.
pub const SETTLE: Duration = Duration::from_millis(80);

/// What one text box reports about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBoxState {
    /// A text box that reports its text has the focus. False for everything else (a terminal,
    /// a password field), where the translator's own record is all there is.
    pub active: bool,
    /// Up to [`MAX_BEFORE`] characters immediately before the cursor
    pub before: String,
    /// Some text is selected, so the next thing typed replaces it
    pub selection: bool,
}

impl TextBoxState {
    /// The state for an application's `surrounding_text`: its text and the byte positions of
    /// the cursor and of the other end of the selection.
    #[must_use]
    pub fn from_surrounding(text: &str, cursor: u32, anchor: u32) -> Self {
        let mut end = (cursor as usize).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let before = &text[..end];
        let skip = before.chars().count().saturating_sub(MAX_BEFORE);
        Self {
            active: true,
            before: before.chars().skip(skip).collect(),
            selection: cursor != anchor,
        }
    }
}

/// One message from `keymux textbox` to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBoxReport {
    /// Counts the text boxes the user has moved between. A new number is a different box.
    pub focus: u64,
    pub state: TextBoxState,
}

/// Builds [`TextBoxReport`]s from an input method's events. They arrive in pieces and take
/// effect together at `done`.
#[derive(Debug, Default)]
pub struct ReportBuilder {
    focus: u64,
    active: bool,
    sensitive: bool,
    surrounding: Option<(String, u32, u32)>,
}

impl ReportBuilder {
    /// A text box asked for the input method. Whatever it said before is no longer true.
    pub fn activate(&mut self) {
        self.focus += 1;
        self.active = true;
        self.sensitive = false;
        self.surrounding = None;
    }

    /// The text box lost the focus, or turned the input method off.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.surrounding = None;
    }

    pub fn surrounding_text(&mut self, text: String, cursor: u32, anchor: u32) {
        self.surrounding = Some((text, cursor, anchor));
    }

    /// `sensitive` is a password, a PIN, or anything the application marked as sensitive. Its
    /// text is not read at all.
    pub const fn content_type(&mut self, sensitive: bool) {
        self.sensitive = sensitive;
    }

    /// The events so far take effect.
    #[must_use]
    pub fn done(&self) -> TextBoxReport {
        let state = match &self.surrounding {
            Some((text, cursor, anchor)) if self.active && !self.sensitive => {
                TextBoxState::from_surrounding(text, *cursor, *anchor)
            }
            _ => TextBoxState::default(),
        };
        TextBoxReport {
            focus: self.focus,
            state,
        }
    }
}

#[derive(Debug)]
struct Current {
    report: TextBoxReport,
    /// Which report this is, counting every one the hub has received
    serial: u64,
    at: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    current: Option<Current>,
    serial: u64,
}

/// The latest report from the session. Shared by the connection that receives it and the
/// engines that read it.
#[derive(Debug, Default)]
pub struct TextBoxHub {
    inner: Mutex<Inner>,
}

/// A report and when it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBoxView {
    pub focus: u64,
    pub state: TextBoxState,
    pub serial: u64,
    /// How long ago it arrived
    pub age: Duration,
}

impl TextBoxHub {
    /// The daemon's hub.
    pub fn global() -> &'static Self {
        static HUB: OnceLock<TextBoxHub> = OnceLock::new();
        HUB.get_or_init(Self::default)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A report arrived.
    pub fn report(&self, report: TextBoxReport) {
        self.report_at(report, Instant::now());
    }

    fn report_at(&self, report: TextBoxReport, at: Instant) {
        let mut inner = self.lock();
        inner.serial += 1;
        let serial = inner.serial;
        inner.current = Some(Current { report, serial, at });
    }

    /// Whoever was reporting has gone. Nothing is known about any text box until they return.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.serial += 1;
        inner.current = None;
    }

    /// How many reports have arrived. See [`TextBoxSync::note_edit`].
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.lock().serial
    }

    /// The latest report, or `None` when nobody is reporting.
    #[must_use]
    pub fn view(&self) -> Option<TextBoxView> {
        self.lock().current.as_ref().map(|current| TextBoxView {
            focus: current.report.focus,
            state: current.report.state.clone(),
            serial: current.serial,
            age: current.at.elapsed(),
        })
    }
}

/// What the text box says about the translator's record of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing reliable to compare with: nobody reports, this box doesn't, or it hasn't caught
    /// up with the last edit. The record stays as it is.
    Unknown,
    /// The text before the cursor is what the record says
    InSync,
    /// It is not. This is the text really before the cursor.
    Resync(String),
}

/// One engine's view of the hub: which box it has already looked at, and when it last typed.
#[derive(Debug)]
pub struct TextBoxSync {
    hub: &'static TextBoxHub,
    settle: Duration,
    /// The text box the record was last checked against
    seen_focus: Option<u64>,
    /// The hub's serial when the engine last typed. A report numbered above it was made after.
    edit_serial: u64,
}

impl TextBoxSync {
    #[must_use]
    pub const fn new(hub: &'static TextBoxHub) -> Self {
        Self {
            hub,
            settle: SETTLE,
            seen_focus: None,
            edit_serial: 0,
        }
    }

    /// Believe reports at once. Tests use this so they don't sleep.
    #[cfg(test)]
    pub const fn without_settling(mut self) -> Self {
        self.settle = Duration::ZERO;
        self
    }

    /// The engine just typed. Reports from before this describe the text before the edit.
    pub fn note_edit(&mut self) {
        self.edit_serial = self.hub.serial();
    }

    /// Compare `remembered`, the text the translator believes it has typed, with the text box.
    pub fn check(&mut self, remembered: &str) -> Verdict {
        let Some(view) = self.hub.view() else {
            return Verdict::Unknown;
        };
        if !view.state.active {
            // Whatever box comes next is a different one, even if it reports the same number
            self.seen_focus = None;
            return Verdict::Unknown;
        }
        // The box has not said anything since the last edit, or is still saying it
        if view.serial <= self.edit_serial || view.age < self.settle {
            return Verdict::Unknown;
        }
        let new_box = self.seen_focus != Some(view.focus);
        self.seen_focus = Some(view.focus);
        // A selection is replaced by what is typed next, so the old record doesn't carry over
        if new_box || view.state.selection || !agrees(remembered, &view.state.before) {
            Verdict::Resync(view.state.before)
        } else {
            Verdict::InSync
        }
    }
}

/// Whether the end of `remembered` is the end of `before`, as far as both reach back. The text
/// box only reports so many characters, and the record may be longer or shorter than that.
fn agrees(remembered: &str, before: &str) -> bool {
    let shared = remembered
        .chars()
        .count()
        .min(before.chars().count());
    if shared == 0 {
        // One of them has nothing. They agree only if both do.
        return remembered.is_empty() == before.is_empty();
    }
    remembered.chars().rev().take(shared).eq(before.chars().rev().take(shared))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> &'static TextBoxHub {
        Box::leak(Box::default())
    }

    fn report(focus: u64, before: &str) -> TextBoxReport {
        TextBoxReport {
            focus,
            state: TextBoxState {
                active: true,
                before: before.to_string(),
                selection: false,
            },
        }
    }

    #[test]
    fn the_text_before_the_cursor_is_cut_at_the_cursor() {
        let state = TextBoxState::from_surrounding("hello world", 5, 5);
        assert_eq!(state.before, "hello");
        assert!(state.active);
        assert!(!state.selection);
    }

    #[test]
    fn a_cursor_inside_a_character_is_moved_back_to_its_start() {
        // "é" is two bytes; 2 is the middle of it
        let state = TextBoxState::from_surrounding("aéb", 2, 2);
        assert_eq!(state.before, "a");
    }

    #[test]
    fn a_cursor_past_the_end_is_the_end() {
        let state = TextBoxState::from_surrounding("hi", 99, 99);
        assert_eq!(state.before, "hi");
    }

    #[test]
    fn a_selection_is_when_the_cursor_and_the_anchor_differ() {
        assert!(TextBoxState::from_surrounding("hello", 5, 1).selection);
    }

    #[test]
    fn only_the_last_characters_before_the_cursor_are_kept() {
        let text = "x".repeat(MAX_BEFORE + 30);
        let state = TextBoxState::from_surrounding(&text, text.len() as u32, text.len() as u32);
        assert_eq!(state.before.chars().count(), MAX_BEFORE);
    }

    #[test]
    fn a_text_box_is_reported_once_it_has_said_where_the_cursor_is() {
        let mut builder = ReportBuilder::default();
        builder.activate();
        // Active, but it hasn't said anything about its text
        assert!(!builder.done().state.active);
        builder.surrounding_text("hello world".to_string(), 5, 5);
        let report = builder.done();
        assert_eq!((report.focus, report.state.active), (1, true));
        assert_eq!(report.state.before, "hello");
    }

    #[test]
    fn each_activation_is_a_new_text_box() {
        let mut builder = ReportBuilder::default();
        builder.activate();
        builder.surrounding_text("a".to_string(), 1, 1);
        let first = builder.done();
        builder.activate();
        builder.surrounding_text("a".to_string(), 1, 1);
        assert_eq!(builder.done().focus, first.focus + 1);
    }

    #[test]
    fn a_new_activation_forgets_the_old_text() {
        let mut builder = ReportBuilder::default();
        builder.activate();
        builder.surrounding_text("secret".to_string(), 6, 6);
        builder.activate();
        // The new box has said nothing yet
        assert_eq!(builder.done().state, TextBoxState::default());
    }

    #[test]
    fn leaving_the_text_box_reports_nothing_about_it() {
        let mut builder = ReportBuilder::default();
        builder.activate();
        builder.surrounding_text("hello".to_string(), 5, 5);
        builder.deactivate();
        assert_eq!(builder.done().state, TextBoxState::default());
    }

    #[test]
    fn a_sensitive_field_is_never_read() {
        let mut builder = ReportBuilder::default();
        builder.activate();
        builder.content_type(true);
        builder.surrounding_text("hunter2".to_string(), 7, 7);
        assert_eq!(builder.done().state, TextBoxState::default());
    }

    #[test]
    fn it_knows_nothing_until_a_text_box_reports() {
        let mut sync = TextBoxSync::new(hub()).without_settling();
        assert_eq!(sync.check("abc"), Verdict::Unknown);
    }

    #[test]
    fn a_text_box_that_does_not_report_leaves_the_record_alone() {
        let hub = hub();
        hub.report(TextBoxReport {
            focus: 1,
            state: TextBoxState::default(),
        });
        let mut sync = TextBoxSync::new(hub).without_settling();
        assert_eq!(sync.check("abc"), Verdict::Unknown);
    }

    #[test]
    fn the_first_look_at_a_box_takes_its_text() {
        let hub = hub();
        hub.report(report(1, "Hello"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        assert_eq!(sync.check(""), Verdict::Resync("Hello".to_string()));
        // The same box, now in step
        assert_eq!(sync.check("Hello"), Verdict::InSync);
    }

    #[test]
    fn a_record_that_matches_is_in_sync() {
        let hub = hub();
        hub.report(report(1, "so the cat"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        assert_eq!(sync.check("so the cat"), Verdict::InSync);
        // The record may reach back further than the box reports
        assert_eq!(sync.check("and so the cat"), Verdict::InSync);
    }

    #[test]
    fn a_record_that_differs_is_replaced_by_the_text() {
        let hub = hub();
        hub.report(report(1, "so the cat"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        assert_eq!(
            sync.check("so the dog"),
            Verdict::Resync("so the cat".to_string())
        );
    }

    #[test]
    fn a_box_that_was_emptied_is_not_the_record() {
        let hub = hub();
        hub.report(report(1, "word"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("word");
        hub.report(report(1, ""));
        assert_eq!(sync.check("word"), Verdict::Resync(String::new()));
    }

    #[test]
    fn a_new_text_box_is_taken_as_it_is_even_if_the_text_matches() {
        let hub = hub();
        hub.report(report(1, "ab"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        hub.report(report(2, "ab"));
        // Same text, but a different box, so any history of the old one is let go
        assert_eq!(sync.check("ab"), Verdict::Resync("ab".to_string()));
    }

    #[test]
    fn a_selection_is_never_in_sync() {
        let hub = hub();
        hub.report(TextBoxReport {
            focus: 1,
            state: TextBoxState {
                active: true,
                before: "ab".to_string(),
                selection: true,
            },
        });
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        assert_eq!(sync.check("ab"), Verdict::Resync("ab".to_string()));
    }

    #[test]
    fn a_report_from_before_the_last_edit_is_not_believed() {
        let hub = hub();
        hub.report(report(1, "ab"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        // The engine types; the box has not said anything since
        sync.note_edit();
        assert_eq!(sync.check("abc"), Verdict::Unknown);
        // Now it has
        hub.report(report(1, "abc"));
        assert_eq!(sync.check("abc"), Verdict::InSync);
    }

    #[test]
    fn a_report_is_believed_only_once_it_has_settled() {
        let hub = hub();
        hub.report(report(1, "ab"));
        // The default settling time, and a report that has only just arrived
        let mut sync = TextBoxSync::new(hub);
        assert_eq!(sync.check("zz"), Verdict::Unknown);
        // A report old enough to have settled
        hub.report_at(report(1, "ab"), Instant::now() - SETTLE * 2);
        assert_eq!(sync.check("zz"), Verdict::Resync("ab".to_string()));
    }

    #[test]
    fn losing_the_reporter_means_knowing_nothing_again() {
        let hub = hub();
        hub.report(report(1, "ab"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        hub.clear();
        assert_eq!(sync.check("xyz"), Verdict::Unknown);
    }

    #[test]
    fn leaving_for_a_box_that_does_not_report_and_coming_back_is_a_new_box() {
        let hub = hub();
        hub.report(report(1, "ab"));
        let mut sync = TextBoxSync::new(hub).without_settling();
        sync.check("");
        hub.report(TextBoxReport {
            focus: 2,
            state: TextBoxState::default(),
        });
        assert_eq!(sync.check("ab"), Verdict::Unknown);
        hub.report(report(1, "ab"));
        assert_eq!(sync.check("ab"), Verdict::Resync("ab".to_string()));
    }
}
