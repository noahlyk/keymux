//! Plugins: ordinary `.rs` files that extend the engine beyond a dictionary.
//!
//! A plugin lives in the steno folder exactly like a dictionary file, any `.rs` found the same
//! way a `.json` or `.rtf` is (see [`super::source`]), and keeps its place in that same
//! priority order: a [`Chain`] built from the folder tries each in turn, dictionary files
//! merged together where they are consecutive, a plugin at its own position otherwise, each
//! giving way to whatever is next if it has no answer. A plugin numbered above a dictionary can
//! override it outright; one numbered below, or with no number, is a fallback for whatever
//! nothing earlier in the folder answers.
//!
//! A plugin is real Rust, compiled with the exact `rustc` this build of keymux runs on, to a
//! sandboxed WebAssembly module via [`wasmtime`], the moment it is first used and again
//! whenever the file changes, cached by content hash exactly the way a dictionary is parsed
//! once and kept as a binary copy (see [`super::dict::load_cached`]). Running it this way,
//! rather than as a native library, means a bug in a plugin cannot corrupt keymux's memory or
//! bring the daemon down with it: a crash, a bad read or an infinite loop becomes an ordinary
//! `Err` from the call into it, nothing more, and the module stays loadable by any compliant
//! runtime indefinitely, with no ABI to keep matching a particular `rustc`.
//!
//! # The fallback interface
//!
//! The one thing a plugin can be asked right now: a reading of a stroke outline that missed
//! everything ahead of it, such as an orthographic guess from a theory's own phonetic rules.
//! A plugin exports two functions:
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn alloc(len: i32) -> i32;
//!
//! #[no_mangle]
//! pub extern "C" fn fallback(ptr: i32, len: i32) -> i64;
//! ```
//!
//! `alloc` is asked for `len` bytes of the plugin's own memory and returns where they start, or
//! `0` for none to give. `fallback` is given the stroke outline exactly as Plover notation
//! would render it (`KAT`, `KOPB/^TKEUGS`), written at the address `alloc` returned, and answers
//! with its own reading of it, written into memory it also allocated, as a packed pointer and
//! length (`ptr << 32 | len`, as an `i64`), or a negative number for no reading at all.
//!
//! See `docs/plugins/orthographic-fallback.rs` for a complete, working example.

use super::dict::Dictionary;
use super::layout::render_strokes;
use super::source::is_plugin_file;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Bumped whenever the interface above changes, so a plugin compiled against an older one is
/// recompiled rather than trusted.
const ABI_VERSION: u32 = 1;

/// Where compiled plugins are cached, under the same cache root a dictionary's own binary copy
/// is kept (see [`super::setup::cache_dir`]): `<cache>/steno/plugins`.
#[must_use]
pub fn cache_dir(config_dir: &Path) -> PathBuf {
    super::setup::cache_dir(config_dir).join("plugins")
}

/// Compile `source` to a WebAssembly module with this build's own `rustc`, caching the result
/// under `cache_dir` by its content hash: unchanged, it is never recompiled. The `wasm32-unknown-
/// unknown` target must be installed (`rustup target add wasm32-unknown-unknown`).
fn compiled(source: &Path, cache_dir: &Path) -> Result<PathBuf> {
    let bytes = fs::read(source).with_context(|| source.display().to_string())?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let out = cache_dir.join(format!("{hash}-{ABI_VERSION}.wasm"));
    if out.is_file() {
        return Ok(out);
    }
    fs::create_dir_all(cache_dir).with_context(|| cache_dir.display().to_string())?;
    // Unique per attempt: two threads (or two keymux processes) building the same plugin at
    // once must not write the same temporary file.
    let part = cache_dir.join(format!(
        "{hash}-{ABI_VERSION}-{:?}-{:?}.wasm.part",
        std::process::id(),
        std::thread::current().id()
    ));
    let output = Command::new("rustc")
        .arg("--edition")
        .arg("2021")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--crate-type")
        .arg("cdylib")
        .arg("-C")
        .arg("opt-level=2")
        .arg("-o")
        .arg(&part)
        .arg(source)
        .output()
        .context("running rustc to build a plugin (is wasm32-unknown-unknown installed?)")?;
    if !output.status.success() {
        bail!(
            "{} did not compile to wasm32-unknown-unknown:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    match fs::rename(&part, &out) {
        Ok(()) => {}
        // Another thread or process finished building the exact same plugin first; its
        // output is just as good, since the name is the content's own hash.
        Err(_) if out.is_file() => {
            let _ = fs::remove_file(&part);
        }
        Err(e) => return Err(e).context("moving a compiled plugin into place"),
    }
    Ok(out)
}

/// One loaded plugin: a WebAssembly module, instantiated so its exports can be called.
pub struct Plugin {
    name: String,
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    fallback: TypedFunc<(i32, i32), i64>,
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin").field("name", &self.name).finish()
    }
}

impl Plugin {
    fn load(name: String, wasm: &Path) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, wasm)
            .map_err(|e| anyhow::anyhow!("{}: {e}", wasm.display()))?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|e| anyhow::anyhow!("instantiating plugin {name:?}: {e}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .with_context(|| format!("plugin {name:?} has no memory"))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| anyhow::anyhow!("plugin {name:?} has no alloc(len) -> ptr: {e}"))?;
        let fallback = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "fallback")
            .map_err(|e| {
                anyhow::anyhow!("plugin {name:?} has no fallback(ptr, len) -> packed: {e}")
            })?;
        Ok(Self {
            name,
            store,
            memory,
            alloc,
            fallback,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ask this plugin for a reading of a stroke outline that missed every dictionary, in
    /// Plover notation (`KAT`, `KOPB/^TKEUGS`). `None` for no reading, if the call itself
    /// failed (a bad plugin answers nothing, it cannot bring the daemon down with it), or if
    /// the reading is empty: an empty string is never a useful translation, and treating it as
    /// one would mean this is "mapped" as far as the engine can tell, silently defeating
    /// anything that only acts when a stroke maps to nothing at all — Plover's own rule for the
    /// bare `*` undo stroke among them.
    pub fn fallback(&mut self, outline: &str) -> Option<String> {
        let bytes = outline.as_bytes();
        let len = i32::try_from(bytes.len()).ok()?;
        let ptr = self.alloc.call(&mut self.store, len).ok()?;
        if ptr == 0 {
            return None;
        }
        self.memory
            .write(&mut self.store, usize::try_from(ptr).ok()?, bytes)
            .ok()?;
        let packed = self.fallback.call(&mut self.store, (ptr, len)).ok()?;
        if packed < 0 {
            return None;
        }
        #[allow(clippy::cast_sign_loss)]
        let out_ptr = usize::try_from((packed >> 32) & 0xFFFF_FFFF).ok()?;
        #[allow(clippy::cast_sign_loss)]
        let out_len = usize::try_from(packed & 0xFFFF_FFFF).ok()?;
        let mut buf = vec![0u8; out_len];
        self.memory.read(&self.store, out_ptr, &mut buf).ok()?;
        String::from_utf8(buf).ok().filter(|s| !s.is_empty())
    }
}

/// Compile and load the plugin at `source`, by its own file stem. A plugin that fails to
/// compile or load is `Err`, logged by the caller and left out rather than stopping the daemon.
fn load_one(source: &Path, cache_dir: &Path) -> Result<Plugin> {
    let name = source.file_stem().map_or_else(
        || "plugin".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let wasm = compiled(source, cache_dir)?;
    Plugin::load(name, &wasm)
}

/// One step of a lookup: a run of dictionary files merged together, or one plugin.
enum Step {
    Dict(Dictionary),
    Plugin(Mutex<Plugin>),
}

/// A dictionary lookup pipeline built from one priority-ordered file list.
///
/// It is the same list [`super::source::expand`] gives a plain dictionary, except that a `.rs`
/// file in it is a plugin: asked at its own place in the order, giving way to whatever is next
/// if it has nothing, the same as a dictionary file would.
pub struct Chain {
    /// Every dictionary file in the list merged into one, ignoring where any plugins fell: for
    /// everything a plain [`Dictionary`] already does that a plugin cannot meaningfully join in
    /// on anyway (reverse lookup, completions, listing).
    dict: Dictionary,
    steps: Vec<Step>,
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("dict", &self.dict)
            .field("steps", &self.steps.len())
            .finish()
    }
}

impl Default for Chain {
    fn default() -> Self {
        Self::from(Dictionary::default())
    }
}

/// A chain of exactly the one dictionary, no plugins: for a `Dictionary` already in memory
/// (built from a literal `.json` string in a test, say) rather than files on disk.
impl From<Dictionary> for Chain {
    fn from(dict: Dictionary) -> Self {
        Self {
            dict: dict.clone(),
            steps: vec![Step::Dict(dict)],
        }
    }
}

impl Chain {
    /// Load `files` (already expanded, highest priority first) into a chain, caching both the
    /// merged dictionary and each compiled plugin under `cache_dir`, with no error even if
    /// every file fails: a working but empty chain, exactly as an all-missing dictionary is.
    #[must_use]
    pub fn load_cached(files: &[PathBuf], cache_dir: &Path) -> Self {
        Self::load_cached_reporting(files, cache_dir).0
    }

    /// Like [`Self::load_cached`], and also returns what went wrong with any dictionary file
    /// that was skipped (a plugin that fails is only ever logged, never reported: unlike a
    /// malformed dictionary, there is no older, still-good copy of it to protect).
    #[must_use]
    pub fn load_cached_reporting(files: &[PathBuf], cache_dir: &Path) -> (Self, Vec<String>) {
        let dict_only: Vec<PathBuf> = files
            .iter()
            .filter(|p| !is_plugin_file(p))
            .cloned()
            .collect();
        let names: Vec<String> = dict_only.iter().map(|p| p.display().to_string()).collect();
        let (dict, problems) = Dictionary::load_cached_reporting(
            &dict_only,
            &super::dict::fingerprint(&dict_only),
            cache_dir,
            &super::dict::cache_name(&names),
        );
        if !problems.is_empty() {
            return (
                Self {
                    dict,
                    steps: Vec::new(),
                },
                problems,
            );
        }
        // No plugin at all: the common case, and the one every keymux without one already
        // has, needs nothing further, the merged dictionary standing as the only step.
        if dict_only.len() == files.len() {
            return (
                Self {
                    dict: dict.clone(),
                    steps: vec![Step::Dict(dict)],
                },
                Vec::new(),
            );
        }
        let plugin_cache = cache_dir.join("plugins");
        let mut steps = Vec::new();
        let mut pending: Vec<PathBuf> = Vec::new();
        for path in files {
            if is_plugin_file(path) {
                if !pending.is_empty() {
                    steps.push(Step::Dict(Dictionary::load_cached(&pending, cache_dir)));
                    pending.clear();
                }
                match load_one(path, &plugin_cache) {
                    Ok(plugin) => steps.push(Step::Plugin(Mutex::new(plugin))),
                    Err(e) => tracing::warn!("plugin {} not loaded: {e:#}", path.display()),
                }
            } else {
                pending.push(path.clone());
            }
        }
        if !pending.is_empty() {
            steps.push(Step::Dict(Dictionary::load_cached(&pending, cache_dir)));
        }
        (Self { dict, steps }, Vec::new())
    }

    /// The translation of an exact outline: the first step with an answer, trying each in the
    /// priority order the files were given in.
    #[must_use]
    pub fn lookup(&self, strokes: &[u32]) -> Option<String> {
        for step in &self.steps {
            let found = match step {
                Step::Dict(dict) => dict.lookup(strokes).map(str::to_owned),
                Step::Plugin(plugin) => {
                    let outline = render_strokes(strokes);
                    lock(plugin).fallback(&outline)
                }
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// The merged view of every dictionary file in the chain, ignoring where any plugin fell:
    /// for a caller that wants a plain `Dictionary` specifically (reverse lookup, completions),
    /// since there is nothing a plugin could meaningfully add to either.
    #[must_use]
    pub const fn as_dictionary(&self) -> &Dictionary {
        &self.dict
    }

    #[must_use]
    pub const fn orthography(&self) -> &super::orthography::Orthography {
        self.dict.orthography()
    }

    #[must_use]
    pub const fn longest_key(&self) -> usize {
        self.dict.longest_key()
    }

    #[must_use]
    pub fn strokes_for(&self, text: &str) -> Vec<Vec<u32>> {
        self.dict.strokes_for(text)
    }

    #[must_use]
    pub fn completions(&self, prefix: &str, limit: usize) -> Vec<&str> {
        self.dict.completions(prefix, limit)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u32>, &String)> {
        self.dict.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.dict.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }
}

fn lock(mutex: &Mutex<Plugin>) -> std::sync::MutexGuard<'_, Plugin> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::super::layout::parse_stroke;
    use super::*;
    use std::sync::OnceLock;

    /// A real, minimal plugin, in ordinary `std` Rust (heap allocation on `wasm32-unknown-
    /// unknown` just works): doubles every letter it is given, so the test proves the whole
    /// pipeline (rustc to wasm32, caching, wasmtime, the alloc/fallback ABI) actually runs, not
    /// just that the host-side plumbing compiles.
    const DOUBLER: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut out: Vec<u8> = Vec::with_capacity(input.len() * 2);
    for &b in input {
        out.push(b);
        out.push(b);
    }
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#;

    fn tmp_dir() -> &'static std::path::Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("keymux-plugin-test-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            dir
        })
    }

    #[test]
    fn a_real_plugin_compiles_runs_and_is_cached() {
        let dir = tmp_dir().join("a_real_plugin_compiles_runs_and_is_cached");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("doubler.rs");
        fs::write(&source, DOUBLER).unwrap();
        let cache = dir.join("cache");

        let wasm = compiled(&source, &cache).expect("rustc should build this plugin");
        assert!(wasm.is_file());
        let mut plugin = Plugin::load("doubler".to_string(), &wasm).unwrap();
        assert_eq!(plugin.fallback("KAT"), Some("KKAATT".to_string()));

        // Recompiling the same source reuses the cached module, not a fresh build
        let modified = fs::metadata(&wasm).unwrap().modified().unwrap();
        let again = compiled(&source, &cache).unwrap();
        assert_eq!(again, wasm);
        assert_eq!(fs::metadata(&wasm).unwrap().modified().unwrap(), modified);
    }

    /// Always answers with a reading, but an empty one for an outline with no letters in it —
    /// exactly what a phonetic-spelling plugin does for a bare `*`, which has no onset, coda or
    /// real vowel to read.
    const BLANK_FOR_SYMBOLS: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut out: Vec<u8> = input.iter().copied().filter(u8::is_ascii_alphabetic).collect();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#;

    #[test]
    fn an_empty_reading_is_no_reading_at_all() {
        let dir = tmp_dir().join("an_empty_reading_is_no_reading_at_all");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("blank_for_symbols.rs");
        fs::write(&source, BLANK_FOR_SYMBOLS).unwrap();
        let wasm = compiled(&source, &dir.join("cache")).unwrap();
        let mut plugin = Plugin::load("blank_for_symbols".to_string(), &wasm).unwrap();
        assert_eq!(
            plugin.fallback("*"),
            None,
            "an empty answer must not be mistaken for a found translation"
        );
        assert_eq!(plugin.fallback("KAT"), Some("KAT".to_string()));
    }

    #[test]
    fn the_documented_example_plugin_compiles_and_gives_a_plausible_reading() {
        // The example a real plugin author would copy and adapt: keep it loadable, and keep
        // giving a reading once it sits in a chain the way it would in a real steno folder.
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("plugins")
            .join("orthographic-fallback.rs");
        let cache =
            tmp_dir().join("the_documented_example_plugin_compiles_and_gives_a_plausible_reading");
        let chain = Chain::load_cached(&[source], &cache);
        assert_eq!(
            chain.lookup(&[parse_stroke("KAT").unwrap()]),
            Some("kat".to_string())
        );
        assert_eq!(
            chain.lookup(&[parse_stroke("STOP").unwrap()]),
            Some("stop".to_string())
        );
        assert_eq!(
            chain.lookup(&[parse_stroke("TKPWEUPB").unwrap()]),
            Some("gin".to_string())
        );
    }

    #[test]
    fn a_chain_skips_a_plugin_that_does_not_compile_and_still_uses_the_one_after_it() {
        let dir = tmp_dir()
            .join("a_chain_skips_a_plugin_that_does_not_compile_and_still_uses_the_one_after_it");
        fs::create_dir_all(&dir).unwrap();
        let broken = dir.join("01-broken.rs");
        let good = dir.join("02-good.rs");
        fs::write(&broken, "this is not rust").unwrap();
        fs::write(&good, DOUBLER).unwrap();
        let chain = Chain::load_cached(&[broken, good], &dir.join("cache"));
        assert_eq!(
            chain.lookup(&[parse_stroke("KAT").unwrap()]),
            Some("KKAATT".to_string()),
            "the broken plugin is skipped, the good one after it still answers"
        );
    }

    #[test]
    fn cache_dir_matches_the_steno_folder_convention() {
        assert_eq!(
            cache_dir(Path::new("/home/a/.config/keymux")),
            PathBuf::from("/home/a/.cache/keymux/steno/plugins")
        );
        assert_eq!(
            cache_dir(Path::new("/tmp/elsewhere")),
            PathBuf::from("/tmp/elsewhere/steno/.cache/plugins")
        );
    }
}
