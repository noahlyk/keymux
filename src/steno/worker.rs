//! A plugin's word readings, made on a thread of their own so a slow one never holds up typing.
//!
//! The thread owns the plugin's reading instance (its wasmtime `Store`) and answers one outline
//! at a time, in the order asked. Whoever asks waits only until a deadline: an answer in time is
//! used exactly as if the call had been made in place; one that is late is kept here, for the
//! translator to pick up once it lands (see [`super::translate::Translator::settle_late`]).
//! Asking for a newer stroke drops whatever earlier strokes asked for that has not started, so
//! the thread is always working toward the newest run, never a backlog of runs nobody is
//! waiting on any more. A reading that has started cannot be stopped; it finishes and is kept.
//!
//! A preview (the overlay showing what a held chord would type, see [`super::preview`]) asks too,
//! but only ever as a guest: its question waits in a slot of its own that the thread takes only
//! when no stroke is waiting, a newer preview question replaces an older one there, and its answer
//! is looked at, never taken, so the stroke that follows finds it ready and does not read it again.
//!
//! The thread makes its reader first (see [`Worker::spawn_with`]): a plugin's instance, and
//! whatever the plugin builds up front in its `init`. A question asked meanwhile waits in the
//! queue like any other, so a stroke asked about then is late, never held up.

use super::strokes::Window;
use crate::lock;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Readings kept that nobody has picked up yet. Past this many, the oldest go.
const KEPT: usize = 64;

/// One plugin's reading of an outline (the first argument), given the text typed before it (the
/// second; empty for a plugin that does not ask for it) and the strokes before it (the third;
/// none for a plugin that does not ask for them): `None` for no reading.
pub type Read = Box<dyn FnMut(&str, &str, Option<&Window>) -> Option<String> + Send>;

/// What makes a [`Read`], run once on the worker's own thread before it reads anything.
pub type Setup = Box<dyn FnOnce() -> Read + Send>;

/// One question: an outline, the text before it, and the strokes before it. Two questions are
/// the same if all three are.
type Question = (String, String, Option<Window>);

/// What asking for a reading came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    /// The reading, `None` if the plugin has none, in time
    Answered(Option<String>),
    /// Not by the deadline: the thread is still on it, or on something before it
    Late,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Question>,
    /// The newest preview's question, read only when nothing is waiting
    preview: Option<Question>,
    busy: Option<Question>,
    /// Finished readings not yet picked up, oldest first
    answers: VecDeque<(Question, Option<String>)>,
    /// The reader is made (see [`Worker::spawn_with`])
    ready: bool,
    closed: bool,
}

fn is(question: &Question, outline: &str, before: &str, window: Option<&Window>) -> bool {
    question.0 == outline
        && question.1 == before
        && question.2.as_ref().map(Window::seen) == window.map(Window::seen)
}

impl Queue {
    /// The reading of `outline` after `before` if one is in, taken: `Some(None)` is a reading
    /// of nothing.
    #[allow(clippy::option_option)] // read as nothing, and not read yet, are different things
    fn take(
        &mut self,
        outline: &str,
        before: &str,
        window: Option<&Window>,
    ) -> Option<Option<String>> {
        let at = self
            .answers
            .iter()
            .position(|(q, _)| is(q, outline, before, window))?;
        self.answers.remove(at).map(|(_, answer)| answer)
    }

    /// [`Self::take`], leaving the reading in for whoever asks next.
    #[allow(clippy::option_option)]
    fn peek(&self, outline: &str, before: &str, window: Option<&Window>) -> Option<Option<String>> {
        self.answers
            .iter()
            .rev()
            .find(|(q, _)| is(q, outline, before, window))
            .map(|(_, answer)| answer.clone())
    }

    fn is_asked(&self, outline: &str, before: &str, window: Option<&Window>) -> bool {
        self.busy
            .as_ref()
            .is_some_and(|q| is(q, outline, before, window))
            || self.waiting.iter().any(|q| is(q, outline, before, window))
            || self
                .preview
                .as_ref()
                .is_some_and(|q| is(q, outline, before, window))
    }
}

type Shared = Arc<(Mutex<Queue>, Condvar)>;

/// The thread reading for one plugin. Dropping it lets the thread go once its current reading
/// is done.
pub struct Worker {
    shared: Shared,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl Worker {
    /// Start a thread that reads with `read`. `None` if no thread could be started.
    #[cfg(test)]
    pub fn spawn(name: &str, read: Read) -> Option<Self> {
        Self::spawn_with(name, Box::new(move || read))
    }

    /// Start a thread that first makes its reader with `setup`, then reads with it. Questions
    /// asked before the reader is made wait for it like any other. `None` if no thread could
    /// be started.
    pub fn spawn_with(name: &str, setup: Setup) -> Option<Self> {
        let shared: Shared = Arc::default();
        let theirs = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name(format!("steno-plugin-{name}"))
            .spawn(move || {
                let (queue, rung) = &*theirs;
                let mut read = setup();
                lock(queue).ready = true;
                rung.notify_all();
                loop {
                    let question = {
                        let mut queue = lock(queue);
                        loop {
                            if queue.closed {
                                return;
                            }
                            if let Some(question) =
                                queue.waiting.pop_front().or_else(|| queue.preview.take())
                            {
                                queue.busy = Some(question.clone());
                                break question;
                            }
                            queue = rung.wait(queue).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let answer = read(&question.0, &question.1, question.2.as_ref());
                    let mut queue = lock(queue);
                    queue.busy = None;
                    queue.answers.push_back((question, answer));
                    while queue.answers.len() > KEPT {
                        queue.answers.pop_front();
                    }
                    drop(queue);
                    rung.notify_all();
                }
            });
        match spawned {
            Ok(_) => Some(Self { shared }),
            Err(e) => {
                tracing::warn!("plugin {name}: no thread to read on: {e}");
                None
            }
        }
    }

    /// Wait until the reader is made (see [`Self::spawn_with`]), no later than `until`.
    /// Whether it is.
    pub fn wait_ready(&self, until: Instant) -> bool {
        let (queue, rung) = &*self.shared;
        let mut queue = lock(queue);
        while !queue.ready {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = rung
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(queue);
        true
    }

    /// Ask for a reading of `outline`, typed after `before`, waiting no later than `until`
    /// (`None`: as long as it takes). `newest` says this is the first ask of a newer stroke:
    /// what older ones asked for that has not started is dropped first.
    pub fn ask(
        &self,
        outline: &str,
        before: &str,
        window: Option<&Window>,
        until: Option<Instant>,
        newest: bool,
    ) -> Asked {
        let (queue, rung) = &*self.shared;
        let mut queue = lock(queue);
        if let Some(answer) = queue.take(outline, before, window) {
            return Asked::Answered(answer);
        }
        if newest {
            queue.waiting.clear();
        }
        // A preview asked it already: it is a stroke's question now, read in the stroke's turn
        if queue
            .preview
            .as_ref()
            .is_some_and(|q| is(q, outline, before, window))
            || newest
        {
            queue.preview = None;
        }
        if !queue.is_asked(outline, before, window) {
            queue
                .waiting
                .push_back((outline.to_owned(), before.to_owned(), window.cloned()));
            rung.notify_all();
        }
        loop {
            if let Some(answer) = queue.take(outline, before, window) {
                return Asked::Answered(answer);
            }
            queue = match until {
                None => rung.wait(queue).unwrap_or_else(|e| e.into_inner()),
                Some(until) => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Asked::Late;
                    }
                    rung.wait_timeout(queue, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    /// [`Self::ask`] for a preview: the question waits only for a thread with nothing else to
    /// do, in place of any older preview's, and a reading is looked at and left in, for the
    /// stroke that may follow to take.
    pub fn ask_preview(
        &self,
        outline: &str,
        before: &str,
        window: Option<&Window>,
        until: Option<Instant>,
    ) -> Asked {
        let (queue, rung) = &*self.shared;
        let mut queue = lock(queue);
        if let Some(answer) = queue.peek(outline, before, window) {
            return Asked::Answered(answer);
        }
        if !queue.is_asked(outline, before, window) {
            queue.preview = Some((outline.to_owned(), before.to_owned(), window.cloned()));
            rung.notify_all();
        }
        loop {
            if let Some(answer) = queue.peek(outline, before, window) {
                return Asked::Answered(answer);
            }
            let left = until.map_or(std::time::Duration::MAX, |until| {
                until.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                return Asked::Late;
            }
            queue = rung
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// [`Self::ready`] for a preview: the reading is left in.
    #[allow(clippy::option_option)]
    pub fn peek(
        &self,
        outline: &str,
        before: &str,
        window: Option<&Window>,
    ) -> Option<Option<String>> {
        lock(&self.shared.0).peek(outline, before, window)
    }

    /// A reading already made of `outline` after `before`, if there is one, without asking or
    /// waiting.
    #[allow(clippy::option_option)] // read as nothing, and not read yet, are different things
    pub fn ready(
        &self,
        outline: &str,
        before: &str,
        window: Option<&Window>,
    ) -> Option<Option<String>> {
        lock(&self.shared.0).take(outline, before, window)
    }

    /// Whether asking for `outline` after `before` now would be answered at once: its reading
    /// is in, or it was never asked for (or was dropped for a newer stroke's) and nothing is
    /// being read.
    pub fn settled(&self, outline: &str, before: &str, window: Option<&Window>) -> bool {
        let queue = lock(&self.shared.0);
        queue
            .answers
            .iter()
            .any(|(q, _)| is(q, outline, before, window))
            || (!queue.is_asked(outline, before, window) && queue.busy.is_none())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let (queue, rung) = &*self.shared;
        lock(queue).closed = true;
        rung.notify_all();
    }
}

/// How long a stroke may wait on plugins, and what came of asking them, for one stroke.
#[derive(Debug, Default)]
pub struct Budget {
    until: std::cell::Cell<Option<Instant>>,
    asked: std::cell::Cell<bool>,
    /// The outline a plugin was late with, and the text before it, and so every plugin asked
    /// after it this stroke was not waited on
    late: std::cell::RefCell<Option<(Vec<u32>, String, u64)>>,
    /// Every reading a plugin gave this stroke: outline, plugin, reading
    read: std::cell::RefCell<Vec<(Vec<u32>, String, String)>>,
    /// The strokes are a preview's (see [`Worker::ask_preview`]): never [`Self::start`]ed away
    preview: std::cell::Cell<bool>,
}

impl Budget {
    /// A fresh budget: plugins may be waited on until `until`.
    pub fn start(&self, until: Instant) {
        self.until.set(Some(until));
        self.asked.set(false);
        self.late.borrow_mut().take();
        self.read.borrow_mut().clear();
    }

    /// Ask plugins as a preview from now on (see [`Worker::ask_preview`]).
    pub fn set_preview(&self) {
        self.preview.set(true);
    }

    pub const fn is_preview(&self) -> bool {
        self.preview.get()
    }

    pub const fn until(&self) -> Option<Instant> {
        self.until.get()
    }

    /// Whether this is the first plugin ask of the stroke, and it is one no longer.
    pub const fn first_ask(&self) -> bool {
        !self.asked.replace(true)
    }

    pub fn is_late(&self) -> bool {
        self.late.borrow().is_some()
    }

    /// `base` is the absolute index of the first stroke of `strokes` in the stroke log
    pub fn mark_late(&self, strokes: &[u32], before: &str, base: u64) {
        self.late
            .borrow_mut()
            .get_or_insert_with(|| (strokes.to_vec(), before.to_owned(), base));
    }

    pub fn take_late(&self) -> Option<(Vec<u32>, String, u64)> {
        self.late.borrow_mut().take()
    }

    pub fn note_read(&self, strokes: &[u32], plugin: &str, text: &str) {
        self.read
            .borrow_mut()
            .push((strokes.to_vec(), plugin.to_owned(), text.to_owned()));
    }

    /// The plugin that read `strokes` as exactly `text` this stroke, if one did.
    pub fn reader_of(&self, strokes: &[u32], text: &str) -> Option<String> {
        self.read
            .borrow()
            .iter()
            .find(|(s, _, t)| s == strokes && t == text)
            .map(|(_, plugin, _)| plugin.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A reader that answers each outline doubled, but only once the test lets it go on.
    fn gated() -> (Worker, mpsc::Sender<()>) {
        let (go, gate) = mpsc::channel::<()>();
        let worker = Worker::spawn(
            "gated",
            Box::new(move |outline: &str, _: &str, _: Option<&Window>| {
                let _ = gate.recv();
                Some(outline.repeat(2))
            }),
        )
        .unwrap();
        (worker, go)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(20)
    }

    #[test]
    fn an_answer_in_time_is_the_answer() {
        let worker = Worker::spawn(
            "quick",
            Box::new(|o: &str, _: &str, _: Option<&Window>| Some(o.to_lowercase())),
        )
        .unwrap();
        assert_eq!(
            worker.ask("KAT", "", None, None, true),
            Asked::Answered(Some("kat".into()))
        );
        assert_eq!(
            worker.ask(
                "TOP",
                "",
                None,
                Some(Instant::now() + Duration::from_secs(5)),
                true
            ),
            Asked::Answered(Some("top".into()))
        );
    }

    #[test]
    fn a_late_answer_is_kept_until_picked_up_once() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("KAT", "", None, Some(soon()), true), Asked::Late);
        assert!(!worker.settled("KAT", "", None));
        go.send(()).unwrap();
        while !worker.settled("KAT", "", None) {
            std::thread::yield_now();
        }
        assert_eq!(worker.ready("KAT", "", None), Some(Some("KATKAT".into())));
        assert_eq!(worker.ready("KAT", "", None), None, "picked up once");
    }

    #[test]
    fn asking_again_while_it_is_read_does_not_read_it_twice() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("KAT", "", None, Some(soon()), true), Asked::Late);
        assert_eq!(
            worker.ask("KAT", "", None, Some(soon()), false),
            Asked::Late
        );
        go.send(()).unwrap();
        assert_eq!(
            worker.ask("KAT", "", None, None, false),
            Asked::Answered(Some("KATKAT".into()))
        );
        drop(go);
        // Nothing else was queued: the thread is idle, not reading KAT a second time
        while !worker.settled("TOP", "", None) {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_newer_stroke_drops_what_older_ones_asked_that_has_not_started() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("A", "", None, Some(soon()), true), Asked::Late);
        assert_eq!(
            worker.ask("A/+B", "", None, Some(soon()), true),
            Asked::Late
        );
        assert_eq!(
            worker.ask("A/+B/+C", "", None, Some(soon()), true),
            Asked::Late
        );
        // A had started and finishes; A/+B was dropped before it started; A/+B/+C is next
        go.send(()).unwrap();
        go.send(()).unwrap();
        assert_eq!(
            worker.ask("A/+B/+C", "", None, None, false),
            Asked::Answered(Some("A/+B/+CA/+B/+C".into()))
        );
        assert_eq!(worker.ready("A", "", None), Some(Some("AA".into())));
        assert_eq!(worker.ready("A/+B", "", None), None, "never read");
        assert!(
            worker.settled("A/+B", "", None),
            "and nothing is coming for it"
        );
    }

    #[test]
    fn a_preview_waits_behind_strokes_and_leaves_its_reading_for_the_stroke() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("X", "", None, Some(soon()), true), Asked::Late);
        // The thread is on X: a preview's question waits in its slot, a newer one replacing it
        assert_eq!(
            worker.ask_preview("OLD", "", None, Some(soon())),
            Asked::Late
        );
        assert_eq!(worker.ask_preview("P", "", None, Some(soon())), Asked::Late);
        // A stroke's question goes ahead of the preview's
        assert_eq!(worker.ask("Y", "", None, Some(soon()), false), Asked::Late);
        go.send(()).unwrap(); // X
        go.send(()).unwrap(); // Y, before P
        assert_eq!(
            worker.ask("Y", "", None, None, false),
            Asked::Answered(Some("YY".into()))
        );
        go.send(()).unwrap(); // P
        assert_eq!(
            worker.ask_preview("P", "", None, None),
            Asked::Answered(Some("PP".into()))
        );
        assert_eq!(
            worker.peek("P", "", None),
            Some(Some("PP".into())),
            "left in"
        );
        assert_eq!(
            worker.ready("P", "", None),
            Some(Some("PP".into())),
            "for the stroke"
        );
        assert_eq!(worker.ready("OLD", "", None), None, "replaced, never read");
    }

    #[test]
    fn the_text_before_is_part_of_the_question() {
        let worker = Worker::spawn(
            "context",
            Box::new(|o: &str, before: &str, _: Option<&Window>| Some(format!("{before}|{o}"))),
        )
        .unwrap();
        assert_eq!(
            worker.ask("KAT", "the", None, None, true),
            Asked::Answered(Some("the|KAT".into()))
        );
        assert_eq!(
            worker.ask("KAT", "a", None, None, true),
            Asked::Answered(Some("a|KAT".into())),
            "the same outline after other text is another question"
        );
    }

    #[test]
    fn the_reader_is_made_on_the_thread_first_and_questions_wait_for_it() {
        let (go, gate) = mpsc::channel::<()>();
        let (made_on, thread) = mpsc::channel::<std::thread::ThreadId>();
        let worker = Worker::spawn_with(
            "slow-setup",
            Box::new(move || {
                let _ = made_on.send(std::thread::current().id());
                let _ = gate.recv();
                Box::new(|o: &str, _: &str, _: Option<&Window>| Some(o.to_lowercase())) as Read
            }),
        )
        .unwrap();
        let setup_thread = thread.recv().unwrap();
        assert_ne!(
            setup_thread,
            std::thread::current().id(),
            "made on its own thread"
        );
        assert!(!worker.wait_ready(soon()), "not made yet");
        // A question asked while it is made is late, never held up, and not yet settled
        assert_eq!(worker.ask("KAT", "", None, Some(soon()), true), Asked::Late);
        assert!(!worker.settled("KAT", "", None));
        go.send(()).unwrap();
        assert!(worker.wait_ready(Instant::now() + Duration::from_secs(10)));
        // And it is read once the reader is made
        assert_eq!(
            worker.ask("KAT", "", None, None, false),
            Asked::Answered(Some("kat".into()))
        );
    }

    #[test]
    fn the_budget_remembers_the_first_late_outline_and_who_read_what() {
        let budget = Budget::default();
        budget.start(Instant::now());
        assert!(budget.first_ask());
        assert!(!budget.first_ask());
        budget.mark_late(&[1, 2], "the", 7);
        budget.mark_late(&[2], "", 9);
        budget.note_read(&[3], "p", "cat");
        assert_eq!(budget.reader_of(&[3], "cat"), Some("p".into()));
        assert_eq!(budget.reader_of(&[3], "dog"), None);
        assert_eq!(budget.take_late(), Some((vec![1, 2], "the".into(), 7)));
        budget.start(Instant::now());
        assert!(budget.first_ask() && !budget.is_late());
        assert_eq!(budget.reader_of(&[3], "cat"), None);
    }
}
