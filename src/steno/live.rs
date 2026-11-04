//! Dictionaries that follow their files. Edit, add, move or remove a dictionary file and the
//! running steno layers pick it up, with nothing to restart and no reload command.
//!
//! One copy of each set of dictionaries is shared by every keyboard that uses it. A layer
//! subscribes to its set and is sent the dictionary: first when it is loaded, then again each
//! time the files change. Typing is never held up: loading happens on a worker thread running
//! at low priority, and the layer swaps the new dictionary in between two strokes.
//!
//! Waiting costs nothing. The worker sleeps on a channel until the watcher has something for
//! it, and the watcher is the kernel's inotify, which wakes only when a watched file changes.
//! An edit is acted on after things have been quiet for [`QUIET`], so an editor saving in
//! several steps, or a download writing a big file, is one reload.
//!
//! A reload that finds a file it can't read, such as one saved half-finished, keeps the
//! dictionary already in use and tries again on the next change.

use super::layout::Layout;
use super::parts::Parts;
use super::plugin::Chain;
use super::source::expand;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use crate::lock;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long the files must stay unchanged before they are read again
const QUIET: Duration = Duration::from_millis(300);

/// Niceness of the worker thread: loading a big dictionary mustn't compete with typing
const WORKER_NICE: libc::c_int = 10;

/// What a layer's dictionaries are made of: files and folders in priority order, the folder
/// holding cached copies of the parsed result, and the steno [`Layout`] to parse them against -
/// two layers with the same files but different theories (different `extra_keys`) must never
/// share a parsed copy, since the same text can parse to different outlines under each.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    roots: Vec<PathBuf>,
    cache_dir: PathBuf,
    layout: Layout,
}

#[derive(Default)]
struct Entry {
    /// The dictionary built from `parts` as they were at generation `built_from`
    latest: Option<Arc<Chain>>,
    built_from: u64,
    /// Every file parsed on its own, kept between reloads: only a changed file is read again
    parts: Parts,
    subscribers: Vec<Sender<Arc<Chain>>>,
}

/// What the watcher needs to judge an event, shared with its callback.
#[derive(Default)]
struct Interest {
    roots: Vec<PathBuf>,
    cache_dirs: Vec<PathBuf>,
}

impl Interest {
    /// Whether a change at `path` could change a dictionary: it is in or is one of the roots,
    /// and is not a cached copy being written.
    fn matches(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
            && !self.cache_dirs.iter().any(|dir| path.starts_with(dir))
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    watcher: Option<RecommendedWatcher>,
    /// Folders the watcher already has, and whether it takes their sub-folders too
    watched: HashSet<(PathBuf, bool)>,
}

enum Job {
    /// Bring this set up to date now: a layer has just subscribed to it. Boxed: a `Key` now
    /// carries a whole steno [`Layout`], making it much bigger than `Changed`'s no data at all.
    Check(Box<Key>),
    /// Something under a watched path changed: wait for quiet, then check every set
    Changed,
}

struct Hub {
    state: Mutex<State>,
    interest: Arc<Mutex<Interest>>,
    jobs: Sender<Job>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| {
        let (jobs, queue) = mpsc::channel();
        std::thread::Builder::new()
            .name("steno-dictionaries".into())
            .spawn(move || worker(&queue))
            .expect("starting the steno dictionary thread");
        Hub {
            state: Mutex::new(State::default()),
            interest: Arc::new(Mutex::new(Interest::default())),
            jobs,
        }
    })
}

/// Receive the dictionary made of `roots`, and every later version of it.
///
/// `roots` are files and folders, highest priority first (see [`expand`]). The dictionary comes
/// now if it is loaded, otherwise as soon as it is, and again whenever any of its files change.
/// Layers with the same `roots` share one copy.
#[must_use]
pub fn subscribe(roots: Vec<PathBuf>, cache_dir: PathBuf, layout: Layout) -> Receiver<Arc<Chain>> {
    let hub = hub();
    let key = Key { roots, cache_dir, layout };
    let (sender, receiver) = mpsc::channel();
    {
        let mut interest = lock(&hub.interest);
        for root in &key.roots {
            if !interest.roots.contains(root) {
                interest.roots.push(root.clone());
            }
        }
        if !interest.cache_dirs.contains(&key.cache_dir) {
            interest.cache_dirs.push(key.cache_dir.clone());
        }
    }
    {
        let mut state = lock(&hub.state);
        let entry = state.entries.entry(key.clone()).or_default();
        if let Some(latest) = &entry.latest {
            let _ = sender.send(Arc::clone(latest));
        }
        entry.subscribers.push(sender);
        drop(state);
    }
    // Even a set that is loaded is checked, so a layer starting up never has stale entries
    let _ = hub.jobs.send(Job::Check(Box::new(key)));
    receiver
}

fn worker(queue: &Receiver<Job>) {
    lower_priority();
    while let Ok(job) = queue.recv() {
        match job {
            Job::Check(key) => refresh(&key),
            Job::Changed => {
                // Wait until the files have been quiet, still serving new subscribers
                loop {
                    match queue.recv_timeout(QUIET) {
                        Ok(Job::Check(key)) => refresh(&key),
                        Ok(Job::Changed) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                let keys: Vec<Key> = lock(&hub().state).entries.keys().cloned().collect();
                for key in keys {
                    refresh(&key);
                }
            }
        }
    }
}

fn lower_priority() {
    // SAFETY: plain system call. On Linux, `who` 0 with PRIO_PROCESS means the calling
    // thread, so only this thread is slowed down.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, WORKER_NICE);
    }
}

/// Read `key`'s files again if they changed, and send the result to everyone subscribed.
///
/// Only the files that changed are read and parsed: a `stat` per file says which (see
/// [`Parts`]), the others keep the parse they already have, and the new dictionary is those
/// parts layered again.
fn refresh(key: &Key) {
    let started = Instant::now();
    let files = expand(&key.roots, &key.cache_dir);
    let hub = hub();
    let (mut parts, built_from, first_time) = {
        let mut state = lock(&hub.state);
        if !state.entries.contains_key(key) {
            return;
        }
        watch_roots(&mut state, key, &hub.jobs, &hub.interest);
        let Some(entry) = state.entries.get_mut(key) else {
            return;
        };
        let first_time = entry.latest.is_none();
        if !first_time && entry.parts.is_current(&files) && entry.built_from == entry.parts.generation()
        {
            return;
        }
        let taken = (std::mem::take(&mut entry.parts), entry.built_from, first_time);
        drop(state);
        taken
    };

    // Parsing a changed dictionary file takes a moment, so it happens with nothing locked
    let report = parts.refresh(&files, &key.cache_dir, &key.layout);
    let up_to_date = !first_time && built_from == parts.generation();
    let keep_old = !first_time && !report.problems.is_empty();
    if keep_old {
        warn!(
            "steno dictionaries not reloaded, keeping the ones in use: {}",
            report.problems.join("; ")
        );
    }
    let dict = (!up_to_date && !keep_old)
        .then(|| Arc::new(Chain::from_parts(&files, &parts, &key.cache_dir, &key.layout)));
    if let Some(dict) = &dict {
        info!(
            "steno dictionaries {}: {} entries from {} files in {:?}",
            if first_time { "loaded" } else { "reloaded" },
            dict.len(),
            files.len(),
            started.elapsed()
        );
    }
    let generation = parts.generation();
    let mut state = lock(&hub.state);
    let Some(entry) = state.entries.get_mut(key) else {
        return;
    };
    entry.parts = parts;
    let Some(dict) = dict else {
        return;
    };
    entry.latest = Some(Arc::clone(&dict));
    entry.built_from = generation;
    entry
        .subscribers
        .retain(|subscriber| subscriber.send(Arc::clone(&dict)).is_ok());
    drop(state);
}

/// Make sure the watcher covers every folder `key`'s files can appear in.
fn watch_roots(state: &mut State, key: &Key, jobs: &Sender<Job>, interest: &Arc<Mutex<Interest>>) {
    if state.watcher.is_none() {
        let jobs = jobs.clone();
        let interest = Arc::clone(interest);
        let made = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let interest = lock(&interest);
            if event.paths.iter().any(|path| interest.matches(path)) {
                let _ = jobs.send(Job::Changed);
            }
        });
        match made {
            Ok(watcher) => state.watcher = Some(watcher),
            Err(e) => {
                warn!("steno dictionaries won't follow file changes: {e}");
                return;
            }
        }
    }
    for root in &key.roots {
        // A folder is watched with everything under it. A file is watched through the folder it
        // is in, since editors save by replacing the file, which a watch on the file would lose.
        let (path, recursive) = if root.is_dir() {
            (root.clone(), true)
        } else if let Some(parent) = root.parent().filter(|parent| parent.is_dir()) {
            (parent.to_path_buf(), false)
        } else {
            continue;
        };
        if state.watched.contains(&(path.clone(), recursive)) {
            continue;
        }
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let Some(watcher) = state.watcher.as_mut() else {
            return;
        };
        match watcher.watch(&path, mode) {
            Ok(()) => {
                state.watched.insert((path, recursive));
            }
            Err(e) => warn!("steno dictionaries: can't watch {}: {e}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-live-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn next(updates: &Receiver<Arc<Chain>>) -> Arc<Chain> {
        updates
            .recv_timeout(Duration::from_secs(10))
            .expect("a dictionary within 10 seconds")
    }

    fn says(dict: &Chain, text: &str) -> bool {
        !dict.as_dictionary().strokes_for(text).is_empty()
    }

    /// Keep receiving until a dictionary that satisfies `done` arrives.
    fn until(updates: &Receiver<Arc<Chain>>, done: impl Fn(&Chain) -> bool) -> Arc<Chain> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let dict = next(updates);
            if done(&dict) {
                return dict;
            }
            assert!(
                Instant::now() < end,
                "never got the dictionary we waited for"
            );
        }
    }

    #[test]
    fn a_layer_gets_its_dictionary_and_then_every_change_to_the_files() {
        let dir = folder("changes");
        fs::write(dir.join("a.json"), r#"{"KAT": "cat"}"#).unwrap();
        let updates = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        assert!(says(&until(&updates, |d| says(d, "cat")), "cat"));

        // Edited in place
        fs::write(dir.join("a.json"), r#"{"KAT": "dog"}"#).unwrap();
        let dict = until(&updates, |d| says(d, "dog"));
        assert!(!says(&dict, "cat"));

        // A new file, in a folder that didn't exist, is picked up too
        fs::create_dir_all(dir.join("more/deeper")).unwrap();
        fs::write(dir.join("more/deeper/b.json"), r#"{"TKOG": "hound"}"#).unwrap();
        until(&updates, |d| says(d, "hound"));

        // A file switched off by renaming it goes away, and comes back when renamed back
        fs::rename(dir.join("more"), dir.join("disabled_more")).unwrap();
        until(&updates, |d| !says(d, "hound"));
        fs::rename(dir.join("disabled_more"), dir.join("more")).unwrap();
        until(&updates, |d| says(d, "hound"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn layers_with_the_same_files_share_one_dictionary() {
        let dir = folder("shared");
        fs::write(dir.join("a.json"), r#"{"KAT": "cat"}"#).unwrap();
        let first = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        let second = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        let one = until(&first, |d| says(d, "cat"));
        let two = until(&second, |d| says(d, "cat"));
        assert!(Arc::ptr_eq(&one, &two), "one copy in memory, not one each");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_saved_half_finished_leaves_the_working_dictionary_in_place() {
        let dir = folder("broken");
        fs::write(dir.join("a.json"), r#"{"KAT": "cat"}"#).unwrap();
        let updates = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        until(&updates, |d| says(d, "cat"));

        fs::write(dir.join("a.json"), r#"{"KAT": "ca"#).unwrap();
        assert!(
            updates.recv_timeout(Duration::from_secs(2)).is_err(),
            "a broken file must not replace a good dictionary"
        );
        fs::write(dir.join("a.json"), r#"{"KAT": "kitten"}"#).unwrap();
        until(&updates, |d| says(d, "kitten"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_the_cache_does_not_trigger_a_reload() {
        let dir = folder("cache");
        fs::write(dir.join("a.json"), r#"{"KAT": "cat"}"#).unwrap();
        let updates = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        until(&updates, |d| says(d, "cat"));
        // The first load wrote a cache file inside the watched folder. Nothing more comes.
        assert!(updates.recv_timeout(Duration::from_secs(2)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// How many threads this module runs, and the CPU time in clock ticks they have used so far.
    fn our_threads() -> (usize, u64) {
        let (mut threads, mut ticks) = (0, 0);
        for task in fs::read_dir("/proc/self/task").unwrap().flatten() {
            let Ok(stat) = fs::read_to_string(task.path().join("stat")) else {
                continue;
            };
            // "pid (name) state ..." with the name possibly holding spaces and parentheses
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
                continue;
            };
            let name = &stat[open + 1..close];
            if name.starts_with("steno-dic") || name.starts_with("notify-rs") {
                threads += 1;
                let fields: Vec<&str> = stat[close + 2..].split_whitespace().collect();
                // utime and stime are the 14th and 15th fields, which start here at the 3rd
                ticks += fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
            }
        }
        (threads, ticks)
    }

    #[test]
    #[ignore = "counts CPU on threads other tests share; run alone: cargo test uses_no_cpu -- --ignored"]
    fn watching_an_idle_dictionary_folder_uses_no_cpu() {
        let dir = folder("idle");
        fs::write(dir.join("a.json"), r#"{"KAT": "cat"}"#).unwrap();
        let updates = subscribe(vec![dir.clone()], dir.join("cache"), Layout::plover());
        until(&updates, |d| says(d, "cat"));
        // Let the load and the cache write finish, then count what five idle seconds cost
        std::thread::sleep(Duration::from_secs(1));
        let (threads, before) = our_threads();
        assert_eq!(
            threads, 2,
            "the worker and the inotify thread are both running"
        );
        std::thread::sleep(Duration::from_secs(5));
        let used = our_threads().1 - before;
        assert_eq!(
            used, 0,
            "{used} ticks (1/100 s each) of CPU while nothing changed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_is_in_a_root_matters_and_the_cache_does_not() {
        let interest = Interest {
            roots: vec![PathBuf::from("/steno")],
            cache_dirs: vec![PathBuf::from("/steno/cache")],
        };
        assert!(interest.matches(Path::new("/steno/a.json")));
        assert!(interest.matches(Path::new("/steno/lapwing/b.json")));
        assert!(!interest.matches(Path::new("/steno/cache/x.bin")));
        assert!(!interest.matches(Path::new("/elsewhere/a.json")));
    }
}
