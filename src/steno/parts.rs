//! Dictionary files kept one by one, so a change to one file costs one file.
//!
//! A steno folder is many files (`01-commands.json`, `02-words.json`, ...), and an edit
//! usually touches one of them. Each file is parsed on its own into a [`Dictionary`] and kept as
//! a [`Part`]; the dictionary a layer uses is the parts layered in priority order
//! ([`Dictionary::layered`]). Checking whether anything changed is then a `stat` per file, and
//! reloading after a change parses only the files that changed:
//!
//! 1. **Same size and modification time** as when the part was made: nothing is read at all.
//!    (Unless the file was modified within [`RACY`] of being read: a write in the same clock tick
//!    could leave size and time as they were, so such a file is hashed again next time.)
//! 2. **Different, but same contents** (touched, saved again unchanged): the file is read and
//!    hashed, the part stays, and nothing is rebuilt.
//! 3. **New contents:** the file's own binary copy in the cache folder is restored if one was
//!    made from exactly these contents (another keymux, an earlier run); otherwise the file is
//!    parsed and that copy renewed.
//!
//! The cache folder holds one `.bin` per dictionary file, named for its path, so there is no
//! whole-set copy to throw away and rewrite when a single file changes.

use super::dict::{cache_name, prune_cache, read_cache, write_cache, Dictionary, Source};
use super::layout::Layout;
use super::source::is_plugin_file;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, info};

/// The folder inside the cache folder that holds the per-file copies
const PARTS_DIR: &str = "parts";

/// A file modified this recently when it was read can't be told apart from a later write by size
/// and time alone, so its contents are compared again next time (as git does for its index).
const RACY: Duration = Duration::from_secs(2);

/// A file as `stat` shows it: enough to say "it is the same file" without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// One file as it was when read.
struct Part {
    stamp: Stamp,
    /// sha256 of the contents
    digest: String,
    /// Too recently modified for the stamp to prove the contents are unchanged: see [`RACY`]
    racy: bool,
    /// The file parsed on its own. `None` for a plugin, which is not a dictionary: it is tracked
    /// so a change to it is noticed, and compiled elsewhere.
    dict: Option<Arc<Dictionary>>,
}

/// How a [`Parts::refresh`] went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    /// Whether anything a dictionary is built from is different from before: a file's contents,
    /// or which files there are, or their order.
    pub changed: bool,
    /// What went wrong with each file that couldn't be read or parsed
    pub problems: Vec<String>,
    pub parsed: usize,
    pub restored: usize,
    pub unchanged: usize,
}

/// The parsed state of an ordered list of files.
#[derive(Default)]
pub struct Parts {
    by_path: HashMap<PathBuf, Part>,
    /// The files, in order, the last time they were refreshed
    order: Vec<PathBuf>,
    /// Counts the refreshes that changed something, so a holder of a dictionary built earlier can
    /// tell whether it is out of date even if the refresh that made it stale reported a problem
    generation: u64,
}

impl Parts {
    /// Which state the parts are in: different after every refresh that changed anything.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether `files` are exactly the files, in the order, and with the size and modification
    /// time they had at the last [`Self::refresh`]. Only `stat`s; reads nothing.
    #[must_use]
    pub fn is_current(&self, files: &[PathBuf]) -> bool {
        self.order == files
            && files.iter().all(|path| {
                self.by_path.get(path).is_some_and(|part| {
                    !part.racy && Stamp::of(path).is_ok_and(|now| now == part.stamp)
                })
            })
    }

    /// Bring every part up to date with `files` (highest priority first), reading only what
    /// changed, and forget parts for files that are gone. A file that can't be read or parsed is
    /// reported and keeps its last good part, if it had one, so the caller can decide to keep
    /// what it has; it is tried again on the next refresh.
    pub fn refresh(&mut self, files: &[PathBuf], cache_dir: &Path, layout: &Layout) -> Refresh {
        let started = Instant::now();
        let parts_dir = cache_dir.join(PARTS_DIR);
        let mut report = Refresh {
            changed: self.order != files,
            ..Refresh::default()
        };
        for path in files {
            match self.update(path, &parts_dir, layout) {
                Ok(Update::Same) => report.unchanged += 1,
                Ok(Update::Touched) => report.unchanged += 1,
                Ok(Update::Restored) => {
                    report.restored += 1;
                    report.changed = true;
                }
                Ok(Update::Parsed) => {
                    report.parsed += 1;
                    report.changed = true;
                }
                Err(e) => report.problems.push(format!("{e:#}")),
            }
        }
        if self.by_path.len() > files.len() || self.order != files {
            self.by_path.retain(|path, _| files.contains(path));
        }
        if report.changed {
            let keep: Vec<PathBuf> = files
                .iter()
                .map(|path| parts_dir.join(format!("{}.bin", cache_name(path))))
                .collect();
            prune_cache(&parts_dir, &keep, cache_dir);
        }
        self.order = files.to_vec();
        self.generation += u64::from(report.changed);
        info!(
            "steno dictionary files: {} parsed, {} restored from cache, {} unchanged, {} problems, {:?}",
            report.parsed,
            report.restored,
            report.unchanged,
            report.problems.len(),
            started.elapsed()
        );
        report
    }

    fn update(&mut self, path: &Path, parts_dir: &Path, layout: &Layout) -> anyhow::Result<Update> {
        use anyhow::Context;
        // The stamp is taken before the contents are read: a write in between leaves the stamp
        // older than the contents, so the next check sees a difference and reads again.
        let stamp = Stamp::of(path).with_context(|| format!("reading {}", path.display()))?;
        if self.by_path.get(path).is_some_and(|part| !part.racy && part.stamp == stamp) {
            return Ok(Update::Same);
        }
        // A time in the future (a clock set back) can't be trusted either
        let racy = stamp
            .modified
            .is_none_or(|time| time.elapsed().map_or(true, |age| age < RACY));
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if let Some(part) = self.by_path.get_mut(path) {
            if part.digest == digest {
                part.stamp = stamp;
                part.racy = racy;
                return Ok(Update::Touched);
            }
        }
        let (dict, restored) = if is_plugin_file(path) {
            (None, false)
        } else {
            let source: Vec<Source> = vec![(path.display().to_string(), digest.clone())];
            let copy = parts_dir.join(format!("{}.bin", cache_name(path)));
            if let Some(dict) = read_cache(&copy, &source, layout) {
                debug!("steno dictionary {} restored from {}", path.display(), copy.display());
                (Some(Arc::new(dict)), true)
            } else {
                let mut dict = Dictionary::default();
                let stats = dict.merge_bytes(path, &bytes, layout)?;
                info!(
                    "steno dictionary {}: {} entries ({} invalid skipped)",
                    path.display(),
                    stats.loaded,
                    stats.invalid
                );
                write_cache(&copy, &source, &dict, layout);
                (Some(Arc::new(dict)), false)
            }
        };
        self.by_path.insert(path.to_path_buf(), Part { stamp, digest, racy, dict });
        Ok(if restored { Update::Restored } else { Update::Parsed })
    }

    /// The dictionary files among `files`, layered in the order given. A file with no part (one
    /// that failed to load) is left out, as a skipped file always was.
    #[must_use]
    pub fn layered(&self, files: &[PathBuf]) -> Dictionary {
        Dictionary::layered(
            files
                .iter()
                .filter_map(|path| self.by_path.get(path)?.dict.as_deref()),
        )
    }
}

enum Update {
    /// Same size and time as before: not read
    Same,
    /// A new time, same contents: read, nothing changed
    Touched,
    /// New contents, found in the file's cache copy
    Restored,
    /// New contents, parsed
    Parsed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-parts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `json` to `path` with a modification time `hours` hours ago, so it is not racy.
    fn put(path: &Path, json: &str, hours: u64) {
        fs::write(path, json).unwrap();
        let then = SystemTime::now() - Duration::from_secs(hours * 3600);
        fs::File::options().write(true).open(path).unwrap().set_modified(then).unwrap();
    }

    fn files(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|name| dir.join(name)).collect()
    }

    #[test]
    fn only_the_file_that_changed_is_parsed_again() {
        let dir = folder("one-changes");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        put(&dir.join("b.json"), r#"{"TKOG": "dog"}"#, 5);
        let files = files(&dir, &["a.json", "b.json"]);
        let cache = dir.join("cache");
        let mut parts = Parts::default();

        let first = parts.refresh(&files, &cache, &Layout::plover());
        assert_eq!((first.parsed, first.unchanged, first.changed), (2, 0, true));
        assert!(parts.is_current(&files));

        put(&dir.join("b.json"), r#"{"TKOG": "hound", "KAT": "kitten"}"#, 4);
        assert!(!parts.is_current(&files));
        let second = parts.refresh(&files, &cache, &Layout::plover());
        assert_eq!((second.parsed, second.unchanged, second.changed), (1, 1, true));
        let dict = parts.layered(&files);
        assert_eq!(dict.strokes_for("hound").len(), 1);
        assert_eq!(
            dict.strokes_for("cat").len(),
            1,
            "a.json is earlier, so it keeps KAT"
        );
        assert!(dict.strokes_for("kitten").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_saved_again_unchanged_rebuilds_nothing() {
        let dir = folder("touched");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        let files = files(&dir, &["a.json"]);
        let cache = dir.join("cache");
        let mut parts = Parts::default();
        parts.refresh(&files, &cache, &Layout::plover());

        // Same bytes, new modification time (a different length would show up as a change)
        std::thread::sleep(std::time::Duration::from_millis(20));
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        let again = parts.refresh(&files, &cache, &Layout::plover());
        assert!(!again.changed, "{again:?}");
        assert_eq!((again.parsed, again.restored, again.unchanged), (0, 0, 1));
        assert!(parts.is_current(&files), "the new time is remembered");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_run_restores_each_file_from_its_own_copy() {
        let dir = folder("restore");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        put(&dir.join("b.json"), r#"{"TKOG": "dog"}"#, 5);
        let files = files(&dir, &["a.json", "b.json"]);
        let cache = dir.join("cache");
        Parts::default().refresh(&files, &cache, &Layout::plover());

        // A fresh process: nothing in memory, only the copies on disk
        let mut parts = Parts::default();
        let report = parts.refresh(&files, &cache, &Layout::plover());
        assert_eq!((report.parsed, report.restored), (0, 2));
        assert_eq!(parts.layered(&files).strokes_for("dog").len(), 1);

        // One file changes: the other is still restored, only the changed one is parsed
        put(&dir.join("b.json"), r#"{"TKOG": "hound"}"#, 4);
        let mut parts = Parts::default();
        let report = parts.refresh(&files, &cache, &Layout::plover());
        assert_eq!((report.parsed, report.restored), (1, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_keeps_its_last_good_part_and_is_reported() {
        let dir = folder("broken");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        let files = files(&dir, &["a.json"]);
        let cache = dir.join("cache");
        let mut parts = Parts::default();
        parts.refresh(&files, &cache, &Layout::plover());

        put(&dir.join("a.json"), r#"{"KAT": "ca"#, 4);
        let report = parts.refresh(&files, &cache, &Layout::plover());
        assert_eq!(report.problems.len(), 1);
        assert!(!report.changed);
        assert!(!parts.is_current(&files), "it is tried again next time");
        assert_eq!(parts.layered(&files).strokes_for("cat").len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_removing_and_reordering_files_is_a_change() {
        let dir = folder("order");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        put(&dir.join("b.json"), r#"{"KAT": "kitten"}"#, 5);
        let cache = dir.join("cache");
        let mut parts = Parts::default();
        let ab = files(&dir, &["a.json", "b.json"]);
        let ba = files(&dir, &["b.json", "a.json"]);
        parts.refresh(&ab, &cache, &Layout::plover());
        assert_eq!(parts.layered(&ab).lookup(&[KAT]), Some("cat"));

        let reordered = parts.refresh(&ba, &cache, &Layout::plover());
        assert!(reordered.changed);
        assert_eq!((reordered.parsed, reordered.unchanged), (0, 2));
        assert_eq!(parts.layered(&ba).lookup(&[KAT]), Some("kitten"));

        let removed = parts.refresh(&ba[..1], &cache, &Layout::plover());
        assert!(removed.changed);
        assert_eq!(parts.layered(&ba[..1]).lookup(&[KAT]), Some("kitten"));
        assert!(parts.by_path.len() == 1);
        let _ = fs::remove_dir_all(&dir);
    }

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);

    #[test]
    fn a_deletion_in_an_earlier_file_hides_the_entry_in_a_later_one() {
        let dir = folder("deleted");
        put(&dir.join("a.json"), &format!(r#"{{"KAT": "{}"}}"#, super::super::dict::DELETED), 5);
        put(&dir.join("b.json"), r#"{"KAT": "cat", "TKOG": "dog"}"#, 5);
        let files = files(&dir, &["a.json", "b.json"]);
        let mut parts = Parts::default();
        parts.refresh(&files, &dir.join("cache"), &Layout::plover());
        let dict = parts.layered(&files);
        assert_eq!(dict.lookup(&[KAT]), None);
        assert!(dict.strokes_for("cat").is_empty());
        assert_eq!(dict.strokes_for("dog").len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_copy_from_another_version_is_not_trusted() {
        let dir = folder("stale");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        let files = files(&dir, &["a.json"]);
        let cache = dir.join("cache");
        Parts::default().refresh(&files, &cache, &Layout::plover());
        for entry in fs::read_dir(cache.join(PARTS_DIR)).unwrap().flatten() {
            fs::write(entry.path(), b"not a cache").unwrap();
        }
        let report = Parts::default().refresh(&files, &cache, &Layout::plover());
        assert_eq!((report.parsed, report.restored), (1, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plugin_is_tracked_but_never_parsed() {
        let dir = folder("plugin");
        put(&dir.join("a.json"), r#"{"KAT": "cat"}"#, 5);
        put(&dir.join("z.rs"), "// not parsed here", 5);
        let files = files(&dir, &["a.json", "z.rs"]);
        let mut parts = Parts::default();
        let report = parts.refresh(&files, &dir.join("cache"), &Layout::plover());
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(parts.layered(&files).len(), 1);
        put(&dir.join("z.rs"), "// changed", 4);
        assert!(!parts.is_current(&files));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_just_written_is_compared_again_in_case_a_same_sized_write_follows() {
        let dir = folder("racy");
        let a = dir.join("a.json");
        fs::write(&a, r#"{"KAT": "cat"}"#).unwrap();
        let files = vec![a.clone()];
        let mut parts = Parts::default();
        parts.refresh(&files, &dir.join("cache"), &Layout::plover());
        assert!(!parts.is_current(&files), "modified a moment ago, so not provably unchanged");
        // The same size, written straight away: the contents are what is compared
        fs::write(&a, r#"{"KAT": "dog"}"#).unwrap();
        let report = parts.refresh(&files, &dir.join("cache"), &Layout::plover());
        assert!(report.changed);
        assert_eq!(parts.layered(&files).strokes_for("dog").len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_later_file_never_shows_through_an_earlier_one_in_any_view() {
        let dir = folder("views");
        // a.json says KAT is "cat" and deletes TKOG; b.json would give KAT "kitten", TKOG "dog"
        // and has KWRAT "catalog" besides
        put(&dir.join("a.json"), &format!(r#"{{"KAT": "cat", "TKOG": "{}"}}"#, super::super::dict::DELETED), 5);
        put(&dir.join("b.json"), r#"{"KAT": "kitten", "TKOG": "dog", "KWRAT": "catalog"}"#, 5);
        let files = files(&dir, &["a.json", "b.json"]);
        let mut parts = Parts::default();
        parts.refresh(&files, &dir.join("cache"), &Layout::plover());
        let dict = parts.layered(&files);

        assert!(dict.strokes_for("kitten").is_empty(), "replaced outline");
        assert!(dict.strokes_for("dog").is_empty(), "deleted outline");
        assert_eq!(dict.strokes_for("cat").len(), 1);
        assert_eq!(dict.completions("ca", 10), ["cat", "catalog"]);
        assert!(dict.completions("ki", 10).is_empty());
        assert_eq!(dict.len(), 3, "KAT, TKOG (a deletion counts) and KWRAT");
        let mut listed: Vec<&str> = dict.iter().map(|(_, text)| text.as_str()).collect();
        listed.sort_unstable();
        assert_eq!(listed, ["cat", "catalog"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Timing on a real theory folder: `KEYMUX_BENCH_DIR=/path cargo test --release bench_ -- --ignored --nocapture`
    #[test]
    #[ignore = "timing only; needs KEYMUX_BENCH_DIR"]
    fn bench_reload_after_one_file_changes() {
        let Some(dir) = std::env::var_os("KEYMUX_BENCH_DIR") else { return };
        let copy = folder("bench");
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let to = copy.join(entry.file_name());
                fs::copy(entry.path(), &to).unwrap();
                files.push(to);
            }
        }
        files.sort();
        let cache = copy.join("cache");
        let layout = Layout::plover();
        let time = |what: &str, f: &mut dyn FnMut()| {
            let t = Instant::now();
            f();
            println!("{what}: {:?}", t.elapsed());
        };
        let mut parts = Parts::default();
        time("cold (parse everything)", &mut || {
            parts.refresh(&files, &cache, &layout);
        });
        time("merge all files", &mut || {
            let _ = parts.layered(&files);
        });
        time("nothing changed (is_current)", &mut || {
            assert!(parts.is_current(&files) || true);
        });
        let mut fresh = Parts::default();
        time("restart (restore every file from cache)", &mut || {
            fresh.refresh(&files, &cache, &layout);
        });
        let one = files.iter().find(|f| f.ends_with("02-lyk-extras.json")).unwrap().clone();
        let text = fs::read_to_string(&one).unwrap().replacen("{", "{\"STKPWHRAOEUFRPBLGTSDZ\": \"zzbench\",", 1);
        put(&one, &text, 3);
        time("one small file changed (refresh)", &mut || {
            fresh.refresh(&files, &cache, &layout);
        });
        time("... and merge", &mut || {
            let _ = fresh.layered(&files);
        });
        let _ = fs::remove_dir_all(&copy);
    }
}
