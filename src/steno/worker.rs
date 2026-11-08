//! A plugin's word readings, made on a thread of their own so a slow one never holds up typing.
//!
//! The thread owns the plugin's reading instance (its wasmtime `Store`) and answers one outline
//! at a time, in the order asked. Whoever asks waits only until a deadline: an answer in time is
//! used exactly as if the call had been made in place; one that is late is kept here, for the
//! translator to pick up once it lands (see [`super::translate::Translator::settle_late`]).
//! Asking for a newer stroke drops whatever earlier strokes asked for that has not started, so
//! the thread is always working toward the newest run, never a backlog of runs nobody is
//! waiting on any more. A reading that has started cannot be stopped; it finishes and is kept.

use crate::lock;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Readings kept that nobody has picked up yet. Past this many, the oldest go.
const KEPT: usize = 64;

/// One plugin's reading of an outline: `None` for no reading.
pub type Read = Box<dyn FnMut(&str) -> Option<String> + Send>;

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
    waiting: VecDeque<String>,
    busy: Option<String>,
    /// Finished readings not yet picked up, oldest first
    answers: VecDeque<(String, Option<String>)>,
    closed: bool,
}

impl Queue {
    /// The reading of `outline` if one is in, taken: `Some(None)` is a reading of nothing.
    #[allow(clippy::option_option)] // read as nothing, and not read yet, are different things
    fn take(&mut self, outline: &str) -> Option<Option<String>> {
        let at = self.answers.iter().position(|(o, _)| o == outline)?;
        self.answers.remove(at).map(|(_, answer)| answer)
    }

    fn is_asked(&self, outline: &str) -> bool {
        self.busy.as_deref() == Some(outline) || self.waiting.iter().any(|o| o == outline)
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
    pub fn spawn(name: &str, mut read: Read) -> Option<Self> {
        let shared: Shared = Arc::default();
        let theirs = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name(format!("steno-plugin-{name}"))
            .spawn(move || {
                let (queue, rung) = &*theirs;
                loop {
                    let outline = {
                        let mut queue = lock(queue);
                        loop {
                            if queue.closed {
                                return;
                            }
                            if let Some(outline) = queue.waiting.pop_front() {
                                queue.busy = Some(outline.clone());
                                break outline;
                            }
                            queue = rung.wait(queue).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let answer = read(&outline);
                    let mut queue = lock(queue);
                    queue.busy = None;
                    queue.answers.push_back((outline, answer));
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

    /// Ask for a reading of `outline`, waiting no later than `until` (`None`: as long as it
    /// takes). `newest` says this is the first ask of a newer stroke: what older ones asked for
    /// that has not started is dropped first.
    pub fn ask(&self, outline: &str, until: Option<Instant>, newest: bool) -> Asked {
        let (queue, rung) = &*self.shared;
        let mut queue = lock(queue);
        if let Some(answer) = queue.take(outline) {
            return Asked::Answered(answer);
        }
        if newest {
            queue.waiting.clear();
        }
        if !queue.is_asked(outline) {
            queue.waiting.push_back(outline.to_owned());
            rung.notify_all();
        }
        loop {
            if let Some(answer) = queue.take(outline) {
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

    /// A reading already made of `outline`, if there is one, without asking or waiting.
    #[allow(clippy::option_option)] // read as nothing, and not read yet, are different things
    pub fn ready(&self, outline: &str) -> Option<Option<String>> {
        lock(&self.shared.0).take(outline)
    }

    /// Whether asking for `outline` now would be answered at once: its reading is in, or it was
    /// never asked for (or was dropped for a newer stroke's) and nothing is being read.
    pub fn settled(&self, outline: &str) -> bool {
        let queue = lock(&self.shared.0);
        queue.answers.iter().any(|(o, _)| o == outline)
            || (!queue.is_asked(outline) && queue.busy.is_none())
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
    /// The outline a plugin was late with, and so every plugin asked after it this stroke was
    /// not waited on
    late: std::cell::RefCell<Option<Vec<u32>>>,
    /// Every reading a plugin gave this stroke: outline, plugin, reading
    read: std::cell::RefCell<Vec<(Vec<u32>, String, String)>>,
}

impl Budget {
    /// A fresh budget: plugins may be waited on until `until`.
    pub fn start(&self, until: Instant) {
        self.until.set(Some(until));
        self.asked.set(false);
        self.late.borrow_mut().take();
        self.read.borrow_mut().clear();
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

    pub fn mark_late(&self, strokes: &[u32]) {
        self.late
            .borrow_mut()
            .get_or_insert_with(|| strokes.to_vec());
    }

    pub fn take_late(&self) -> Option<Vec<u32>> {
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
            Box::new(move |outline: &str| {
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
        let worker = Worker::spawn("quick", Box::new(|o: &str| Some(o.to_lowercase()))).unwrap();
        assert_eq!(
            worker.ask("KAT", None, true),
            Asked::Answered(Some("kat".into()))
        );
        assert_eq!(
            worker.ask("TOP", Some(Instant::now() + Duration::from_secs(5)), true),
            Asked::Answered(Some("top".into()))
        );
    }

    #[test]
    fn a_late_answer_is_kept_until_picked_up_once() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("KAT", Some(soon()), true), Asked::Late);
        assert!(!worker.settled("KAT"));
        go.send(()).unwrap();
        while !worker.settled("KAT") {
            std::thread::yield_now();
        }
        assert_eq!(worker.ready("KAT"), Some(Some("KATKAT".into())));
        assert_eq!(worker.ready("KAT"), None, "picked up once");
    }

    #[test]
    fn asking_again_while_it_is_read_does_not_read_it_twice() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("KAT", Some(soon()), true), Asked::Late);
        assert_eq!(worker.ask("KAT", Some(soon()), false), Asked::Late);
        go.send(()).unwrap();
        assert_eq!(
            worker.ask("KAT", None, false),
            Asked::Answered(Some("KATKAT".into()))
        );
        drop(go);
        // Nothing else was queued: the thread is idle, not reading KAT a second time
        while !worker.settled("TOP") {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_newer_stroke_drops_what_older_ones_asked_that_has_not_started() {
        let (worker, go) = gated();
        assert_eq!(worker.ask("A", Some(soon()), true), Asked::Late);
        assert_eq!(worker.ask("A/+B", Some(soon()), true), Asked::Late);
        assert_eq!(worker.ask("A/+B/+C", Some(soon()), true), Asked::Late);
        // A had started and finishes; A/+B was dropped before it started; A/+B/+C is next
        go.send(()).unwrap();
        go.send(()).unwrap();
        assert_eq!(
            worker.ask("A/+B/+C", None, false),
            Asked::Answered(Some("A/+B/+CA/+B/+C".into()))
        );
        assert_eq!(worker.ready("A"), Some(Some("AA".into())));
        assert_eq!(worker.ready("A/+B"), None, "never read");
        assert!(worker.settled("A/+B"), "and nothing is coming for it");
    }

    #[test]
    fn the_budget_remembers_the_first_late_outline_and_who_read_what() {
        let budget = Budget::default();
        budget.start(Instant::now());
        assert!(budget.first_ask());
        assert!(!budget.first_ask());
        budget.mark_late(&[1, 2]);
        budget.mark_late(&[2]);
        budget.note_read(&[3], "p", "cat");
        assert_eq!(budget.reader_of(&[3], "cat"), Some("p".into()));
        assert_eq!(budget.reader_of(&[3], "dog"), None);
        assert_eq!(budget.take_late(), Some(vec![1, 2]));
        budget.start(Instant::now());
        assert!(budget.first_ask() && !budget.is_late());
        assert_eq!(budget.reader_of(&[3], "cat"), None);
    }
}
