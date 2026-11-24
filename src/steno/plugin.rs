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
//! sandboxed WebAssembly module via [`wasmtime`], in the background as the steno folder loads
//! and again whenever the file changes, cached by content hash exactly the way a dictionary is parsed
//! once and kept as a binary copy (see [`super::dict::load_cached`]). Running it this way,
//! rather than as a native library, means a bug in a plugin cannot corrupt keymux's memory or
//! bring the daemon down with it: a crash, a bad read or an infinite loop becomes an ordinary
//! `Err` from the call into it, nothing more, and the module stays loadable by any compliant
//! runtime indefinitely, with no ABI to keep matching a particular `rustc`.
//!
//! # The fallback interface
//!
//! The one thing every plugin answers: a reading of one word that missed everything ahead of
//! it, such as an orthographic guess from a theory's own phonetic rules — a single stroke, or
//! several where every one after the first continues the word before it on purpose (see
//! `joins` below; a word that needed more keys than one stroke has). It is never asked about a
//! run of otherwise-unrelated strokes — the engine's own search for the longest dictionary
//! match tries windows of many strokes together regardless, and a plugin that answered those
//! too would win every one of them, swallowing unrelated strokes into one guess instead of
//! leaving them as separate words. A plugin exports two required functions:
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
//! `0` for none to give. `fallback` is given that word's outline exactly as Plover notation
//! would render it (`KAT`, `KOPB`, `KAT/+-G`), written at the address `alloc` returned, and
//! answers with its own reading of it, written into memory it also allocated, as a packed
//! pointer and length
//! (`ptr << 32 | len`, as an `i64`), or a negative number for no reading at all.
//!
//! # The join interface (optional)
//!
//! keymux itself has no notion of any key meaning "continue the previous word" - every stroke
//! key, standard or a theory's own extra one, is just a bit to the engine. A theory that wants
//! one of its keys to behave that way exports a third function:
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn joins(bits: i64) -> i32;
//! ```
//!
//! Called with one stroke's bits (the same ones a dictionary key or the tape would carry),
//! answering non-zero if this stroke should glue onto whatever came before it rather than start
//! a word of its own. A plugin that has no use for this leaves it out entirely; a stroke is
//! then never treated as a continuation by anything in keymux. See
//! `docs/plugins/orthographic-fallback.rs` for a complete, working example (including `joins`).
//!
//! # The send-early interface (optional)
//!
//! A stroke is sent when its last key comes up. A theory that wants some strokes to go sooner,
//! with keys still down, exports:
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn send_early(held: i64, stroke: i64) -> i32;
//! ```
//!
//! Asked each time a stroke key comes up while others are still down, with the bits of the keys
//! still down (`held`) and of every key pressed for the stroke so far (`stroke`). Non-zero sends
//! the stroke now, exactly as a `{:firstup}` entry is: the keys still down stay in the chord and
//! go with the next key pressed, and letting go of them alone sends nothing. A key a theory makes
//! sticky this way (held across stroke after stroke, each stroke going as the other keys come
//! up) is its own business; keymux has no notion of any particular key behaving so. See
//! `docs/plugins/sticky-key.rs`.
//!
//! # The outline-continues interface (optional)
//!
//! `{:outline:next}` and `{:outline:explain}` follow the next translation as the engine groups
//! it (see [`super::outline_control`]). A theory that wants some strokes to always continue the
//! outline being followed, even where the engine would start a new translation with them,
//! exports:
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn outline_continues(stroke: i64) -> i32;
//! ```
//!
//! Asked with a stroke's bits only while one of those commands is following an outline, never
//! otherwise. Non-zero makes the stroke part of the outline shown. See
//! `docs/plugins/outline-join.rs`.
//!
//! # The init interface (optional)
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn init(); // or `-> i32`, ignored
//! ```
//!
//! Called once on the instance that reads words, on its own thread, right after it is made and
//! before any `context` or `fallback`: the place to build tables up front. The instance is made,
//! and `init` run, as the steno folder loads (or a plugin file changes), and the load waits for
//! it (up to `READY_WAIT`, 2 seconds) before the new chain is used, so no stroke pays for it; a stroke
//! asked about while it is still running is simply late. A mod's one instance is `init`ed as it
//! loads; the instance answering `joins` and `send_early` for a plugin that reads words is not.
//! An `init` that traps leaves the plugin reading nothing (or a mod left out).
//!
//! # The context interface (optional)
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn context(ptr: i32, len: i32);
//! ```
//!
//! Called on the reading instance immediately before every `fallback`, with the UTF-8 text typed
//! before the outline's first stroke, written at an address `alloc` returned: at most the last
//! [`CONTEXT_BYTES`] bytes (cut forward to a character boundary) of what keymux knows is on
//! screen, which starts over empty at a wall: anything that fences the history (a key that
//! moves the caret, a shortcut, a window change), or a barrier stroke (a key combination that
//! moves the cursor or deletes). Text typed by hand (Enter and Tab too) is known, so it is
//! included. It never includes what the outline's own strokes typed so far (a run's provisional
//! text). Empty when nothing is known. A question's text is part of it: the same outline after
//! other text is another question, read again. A plugin without `context` is asked
//! exactly as before, its questions carrying no text.
//!
//! # The history interface (optional)
//!
//! A plugin may import `history_len() -> i32` and `history_stroke(k: i32) -> i64` from the
//! module `keymux` to read back the strokes struck before the run it is asked about, newest
//! first (`k = 0` the stroke right before the run), `-1` past the oldest, a wall or outside a
//! `fallback`. The strokes are a [`super::strokes::StrokeLog`]; see `docs/plugins/README.md`.
//!
//! # The groups interface (optional)
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "C" fn groups(ptr: i32, len: i32) -> i64;
//! ```
//!
//! For a theory notation (see [`super::notation`]): how the plugin split the strokes of the
//! outline it just read into sound groups, so a word it read is written (`{:outline:...}`, the
//! tape) and drawn (the overlay) the way it read it. Called on the reading instance right after
//! a `fallback` that gave a reading, with the same outline, only when the plugin exports it.
//! Answers like `fallback` (packed pointer and length, negative for none) with little-endian
//! `u32` key sets in stroke bits, each a group of two keys or more of one hand, or of the vowel
//! keys, that sound as one, a `0` after each stroke's groups. A report that does not fit the
//! strokes (a group not all in its stroke, across two parts of the keyboard, overlapping
//! another) is not used. Precedence: the notation file's own entry for the outline, then this
//! report, then the outline's known split (`agree`), then none.
//!
//! # Mods
//!
//! `alloc` and `fallback` are required only of a plugin that reads words. One that only changes
//! how the engine behaves (`joins`, `send_early`, `outline_continues`) leaves them out: a mod,
//! never asked for a word.
//!
//! # A slow reading never holds up typing
//!
//! A plugin that reads words does it on a thread of its own (see [`super::worker`]), with an
//! instance of its own: `joins` and `send_early`, asked on every stroke and key, go to a second
//! instance of the same module on the engine's thread, so a long `fallback` never makes them
//! wait. The translator waits for a reading only until the stroke's deadline (`plugin_deadline_ms`
//! in the steno config, 30 by default). A reading in time is used as ever; one that is late is
//! the stroke typed as if the plugin had no reading, and once it lands, if the stroke is still
//! the last thing typed, the stroke is translated again with it, replacing what was typed. The
//! two instances share nothing, so a plugin keeps whatever state it needs for `fallback` (a cache
//! of the run read last, say) in the instance that reads, never relying on `joins` or
//! `send_early` to see it.

use super::dict::Dictionary;
use super::layout::Layout;
use super::parts::Parts;
use super::run_control::Trust;
use super::source::is_plugin_file;
use super::strokes::{StrokeLog, Window};
use super::worker::{Asked, Budget, Read, Worker};
use crate::lock;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

/// Bumped whenever the interface above changes, so a plugin compiled against an older one is
/// recompiled rather than trusted.
const ABI_VERSION: u32 = 2;

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

/// A word-reading plugin's `alloc(len) -> ptr` and `fallback(ptr, len) -> packed`.
type Reads = (TypedFunc<i32, i32>, TypedFunc<(i32, i32), i64>);

/// How long a folder's load waits for its plugins to be ready to read (see [`Chain::from_parts`])
/// before it is used anyway: a plugin still not ready reads late, never holding typing up.
const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Most of the text before an outline given to a plugin's `context`, in bytes.
pub const CONTEXT_BYTES: usize = 256;

/// What the host functions a plugin imports (see the module doc's "history interface") can see
/// of the instance they are called from.
#[derive(Default)]
struct Host {
    /// The strokes before the run the plugin is being asked about, while its `fallback` runs
    /// and at no other time
    asked: Option<Window>,
}

/// The module the host functions a plugin may import are in.
const HOST_MODULE: &str = "keymux";

/// Make the linker every plugin instance is made with: the host functions a plugin may import,
/// whether it does or not.
fn host_linker(engine: &Engine) -> wasmtime::Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        HOST_MODULE,
        "history_len",
        |caller: Caller<'_, Host>| -> i32 { caller.data().asked.as_ref().map_or(0, Window::len) },
    )?;
    linker.func_wrap(
        HOST_MODULE,
        "history_stroke",
        |caller: Caller<'_, Host>, k: i32| -> i64 {
            caller.data().asked.as_ref().map_or(-1, |w| w.stroke(k))
        },
    )?;
    Ok(linker)
}

/// A plugin's `init()`, with or without an `i32` it returns (and that is ignored).
#[derive(Clone)]
enum Init {
    Plain(TypedFunc<(), ()>),
    Number(TypedFunc<(), i32>),
}

/// One loaded plugin: a WebAssembly module, instantiated so its exports can be called.
pub(crate) struct Plugin {
    name: String,
    /// The compiled module, to make another instance of it from (see [`Self::into_step`])
    module: Module,
    store: Store<Host>,
    /// Whether the module imports the history functions
    history: bool,
    memory: Option<Memory>,
    /// `alloc` and `fallback`, for a plugin that reads words; a mod has neither
    reads: Option<Reads>,
    /// `joins(bits) -> bool`, if this plugin exports one - see the module doc's "join
    /// interface". Not every plugin cares, so this is the one export that's optional.
    joins: Option<TypedFunc<i64, i32>>,
    /// What `joins` answered for each stroke asked so far. A pure function of the stroke, and
    /// the longest-match search asks about the same few strokes many times per stroke typed.
    joins_known: rustc_hash::FxHashMap<u32, bool>,
    /// `groups(ptr, len) -> packed`, if this plugin exports one: how it split the strokes of the
    /// outline it last read into sound groups (see the module doc)
    groups: Option<TypedFunc<(i32, i32), i64>>,
    /// Where the reading instance keeps what `groups` answered, for the engine to find
    reported: Option<Reports>,
    /// `send_early(held, stroke) -> bool`, if this plugin exports one: see the module doc.
    send_early: Option<TypedFunc<(i64, i64), i32>>,
    /// `outline_continues(stroke) -> bool`, if this plugin exports one: see the module doc.
    outline_continues: Option<TypedFunc<i64, i32>>,
    /// `init()`, if this plugin exports one: see the module doc.
    init: Option<Init>,
    /// `context(ptr, len)`, if this plugin exports one: see the module doc.
    context: Option<TypedFunc<(i32, i32), ()>>,
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
        Self::instantiate(name, module)
    }

    /// A fresh instance of `module`, with its own memory and state.
    fn instantiate(name: String, module: Module) -> Result<Self> {
        let mut store = Store::new(module.engine(), Host::default());
        let history = module.imports().any(|import| {
            import.module() == HOST_MODULE
                && matches!(import.name(), "history_len" | "history_stroke")
        });
        let instance = host_linker(module.engine())
            .and_then(|linker| linker.instantiate(&mut store, &module))
            .map_err(|e| anyhow::anyhow!("instantiating plugin {name:?}: {e}"))?;
        let memory = instance.get_memory(&mut store, "memory");
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .ok();
        let fallback = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "fallback")
            .ok();
        let reads = match (alloc, fallback) {
            (Some(alloc), Some(fallback)) => {
                if memory.is_none() {
                    bail!("plugin {name:?} reads words but has no memory");
                }
                Some((alloc, fallback))
            }
            (None, None) => None,
            (None, Some(_)) => bail!("plugin {name:?} has fallback but no alloc(len) -> ptr"),
            (Some(_), None) => {
                bail!("plugin {name:?} has alloc but no fallback(ptr, len) -> packed")
            }
        };
        let joins = instance
            .get_typed_func::<i64, i32>(&mut store, "joins")
            .ok();
        let send_early = instance
            .get_typed_func::<(i64, i64), i32>(&mut store, "send_early")
            .ok();
        let outline_continues = instance
            .get_typed_func::<i64, i32>(&mut store, "outline_continues")
            .ok();
        if outline_continues.is_none()
            && instance.get_func(&mut store, "outline_continues").is_some()
        {
            bail!(
                "plugin {name:?} has outline_continues, but not outline_continues(stroke) -> i32"
            );
        }
        if reads.is_none() && joins.is_none() && send_early.is_none() && outline_continues.is_none()
        {
            bail!("plugin {name:?} exports none of fallback, joins, send_early, outline_continues");
        }
        let init = instance
            .get_typed_func::<(), ()>(&mut store, "init")
            .ok()
            .map(Init::Plain)
            .or_else(|| {
                instance
                    .get_typed_func::<(), i32>(&mut store, "init")
                    .ok()
                    .map(Init::Number)
            });
        if init.is_none() && instance.get_func(&mut store, "init").is_some() {
            bail!("plugin {name:?} has init, but not init() or init() -> i32");
        }
        let context = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "context")
            .ok();
        if context.is_none() && instance.get_func(&mut store, "context").is_some() {
            bail!("plugin {name:?} has context, but not context(ptr, len)");
        }
        let groups = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "groups")
            .ok();
        if groups.is_none() && instance.get_func(&mut store, "groups").is_some() {
            bail!("plugin {name:?} has groups, but not groups(ptr, len) -> packed");
        }
        Ok(Self {
            name,
            module,
            store,
            history,
            memory,
            reads,
            joins,
            joins_known: rustc_hash::FxHashMap::default(),
            send_early,
            outline_continues,
            init,
            context,
            groups,
            reported: None,
        })
    }

    /// Ready this plugin to sit in a chain: a plugin that reads words reads on a thread of its
    /// own, from an instance made (and `init`ed) there, and answers `joins` and `send_early`
    /// (if it has them) from this instance, so neither ever waits on a reading. A mod is
    /// `init`ed here. See the module doc.
    fn into_step(self) -> PluginStep {
        let name = self.name.clone();
        let mut plugin = self;
        if plugin.reads.is_none() {
            let mods = match plugin.init() {
                Ok(()) => Some(Mutex::new(plugin)),
                Err(e) => {
                    tracing::warn!("plugin {name:?} left out: {e:#}");
                    None
                }
            };
            return PluginStep {
                name,
                mods,
                reader: None,
                context: false,
                history: false,
                reported: None,
            };
        }
        let context = plugin.context.is_some();
        let history = plugin.history;
        let module = plugin.module.clone();
        let reported = plugin.groups.is_some().then(Reports::default);
        let mods = (plugin.joins.is_some()
            || plugin.send_early.is_some()
            || plugin.outline_continues.is_some())
        .then(|| {
            plugin.reads = None;
            Mutex::new(plugin)
        });
        let reader_name = name.clone();
        let theirs = reported.clone();
        let reader = Worker::spawn_with(
            &name,
            Box::new(move || Self::reader(reader_name, module, theirs)),
        );
        PluginStep {
            name,
            mods,
            reader,
            context,
            history,
            reported,
        }
    }

    /// What reads words on a plugin's own thread: an instance of `module` made there, `init`ed
    /// before it reads anything. One that cannot be made, or whose `init` fails, reads nothing.
    fn reader(name: String, module: Module, reported: Option<Reports>) -> Read {
        let made = Self::instantiate(name.clone(), module).and_then(|mut plugin| {
            plugin.init()?;
            plugin.reported = reported;
            Ok(plugin)
        });
        match made {
            Ok(mut plugin) => Box::new(
                move |outline: &str, before: &str, window: Option<&Window>| {
                    plugin.read(outline, before, window)
                },
            ),
            Err(e) => {
                tracing::warn!("plugin {name:?} reads nothing: {e:#}");
                Box::new(|_: &str, _: &str, _: Option<&Window>| None)
            }
        }
    }

    /// Call the plugin's `init`, if it has one. `Err` if it failed (trapped).
    fn init(&mut self) -> Result<()> {
        match self.init.clone() {
            None => Ok(()),
            Some(Init::Plain(init)) => init.call(&mut self.store, ()),
            Some(Init::Number(init)) => init.call(&mut self.store, ()).map(|_| ()),
        }
        .map_err(|e| anyhow::anyhow!("init failed: {e}"))
    }

    /// Write `bytes` into memory the plugin's `alloc` gives: where, and how long. `None` if it
    /// has no `alloc`, or declined.
    fn write(&mut self, bytes: &[u8]) -> Option<(i32, i32)> {
        let (alloc, _) = self.reads.clone()?;
        let memory = self.memory?;
        let len = i32::try_from(bytes.len()).ok()?;
        let ptr = alloc.call(&mut self.store, len).ok()?;
        if ptr == 0 && len > 0 {
            return None;
        }
        memory
            .write(&mut self.store, usize::try_from(ptr).ok()?, bytes)
            .ok()?;
        Some((ptr, len))
    }

    /// Read `outline`, typed after `before` and the strokes of `window`: the text is given to
    /// `context` first, if the plugin has one, then `fallback` reads, with the strokes
    /// readable through the history functions.
    fn read(&mut self, outline: &str, before: &str, window: Option<&Window>) -> Option<String> {
        if let Some(context) = self.context.clone() {
            if let Some(at) = self.write(before.as_bytes()) {
                let _ = context.call(&mut self.store, at);
            }
        }
        let text = self.fallback_after(outline, window)?;
        self.report_groups(outline, &text);
        Some(text)
    }

    /// After a reading, ask `groups` (if the plugin has it) how the outline's strokes split, and
    /// keep the answer for the engine (see [`Chain::plugin_groups`]). Only a plugin that exports
    /// it is asked: for any other this costs nothing.
    fn report_groups(&mut self, outline: &str, text: &str) {
        let (Some(groups), Some(reported), Some(memory)) =
            (self.groups.clone(), self.reported.clone(), self.memory)
        else {
            return;
        };
        let Some((ptr, len)) = self.write(outline.as_bytes()).filter(|(ptr, _)| *ptr != 0) else {
            return;
        };
        let answer = groups
            .call(&mut self.store, (ptr, len))
            .ok()
            .filter(|&packed| packed >= 0)
            .and_then(|packed| {
                let at = usize::try_from((packed >> 32) & 0xFFFF_FFFF).ok()?;
                let len = usize::try_from(packed & 0xFFFF_FFFF).ok()?;
                let mut buf = vec![0u8; len];
                memory.read(&self.store, at, &mut buf).ok()?;
                Some(split_groups(&buf))
            });
        let Some(answer) = answer else {
            return;
        };
        let mut kept = lock(&reported.0);
        if kept.len() == REPORTS_KEPT {
            kept.pop_front();
        }
        kept.push_back((outline.to_owned(), text.to_owned(), answer));
    }

    /// Ask this plugin for a reading of a stroke outline that missed every dictionary, in
    /// Plover notation (`KAT`, `KOPB`). `None` for no reading, if the call itself
    /// failed (a bad plugin answers nothing, it cannot bring the daemon down with it), or if
    /// the reading is empty: an empty string is never a useful translation, and treating it as
    /// one would mean this is "mapped" as far as the engine can tell, silently defeating
    /// anything that only acts when a stroke maps to nothing at all — Plover's own rule for the
    /// bare `*` undo stroke among them.
    #[cfg(test)]
    pub fn fallback(&mut self, outline: &str) -> Option<String> {
        self.fallback_after(outline, None)
    }

    /// [`Self::fallback`], with `window` readable by the plugin while it runs.
    fn fallback_after(&mut self, outline: &str, window: Option<&Window>) -> Option<String> {
        let (_, fallback) = self.reads.clone()?;
        let memory = self.memory?;
        let (ptr, len) = self.write(outline.as_bytes())?;
        if ptr == 0 {
            return None;
        }
        self.store.data_mut().asked = window.cloned();
        let packed = fallback.call(&mut self.store, (ptr, len));
        self.store.data_mut().asked = None;
        let packed = packed.ok()?;
        if packed < 0 {
            return None;
        }
        #[allow(clippy::cast_sign_loss)]
        let out_ptr = usize::try_from((packed >> 32) & 0xFFFF_FFFF).ok()?;
        #[allow(clippy::cast_sign_loss)]
        let out_len = usize::try_from(packed & 0xFFFF_FFFF).ok()?;
        let mut buf = vec![0u8; out_len];
        memory.read(&self.store, out_ptr, &mut buf).ok()?;
        String::from_utf8(buf).ok().filter(|s| !s.is_empty())
    }

    /// Ask this plugin whether `bits` (one stroke) continues the word before it, rather than
    /// starting a new one - a theory's own notion of a join key, if it gave one. `false` if
    /// this plugin doesn't export `joins` at all, or if the call itself failed.
    pub fn joins(&mut self, bits: u32) -> bool {
        let Some(joins) = &self.joins else {
            return false;
        };
        if let Some(&known) = self.joins_known.get(&bits) {
            return known;
        }
        let answer = joins.call(&mut self.store, i64::from(bits)).unwrap_or(0) != 0;
        self.joins_known.insert(bits, answer);
        answer
    }

    /// Ask this plugin whether the stroke in progress (`stroke`, every key pressed for it so
    /// far) should be sent now, with `held` still down - see the module doc's "send-early
    /// interface". `false` if it doesn't export `send_early`, or if the call failed.
    pub fn send_early(&mut self, held: u32, stroke: u32) -> bool {
        let Some(send_early) = &self.send_early else {
            return false;
        };
        send_early
            .call(&mut self.store, (i64::from(held), i64::from(stroke)))
            .unwrap_or(0)
            != 0
    }

    /// Ask this plugin whether `stroke` continues the outline `{:outline:next}` or
    /// `{:outline:explain}` follows - see the module doc's "outline-continues interface".
    /// `false` if it doesn't export `outline_continues`, or if the call failed.
    pub fn outline_continues(&mut self, stroke: u32) -> bool {
        let Some(continues) = &self.outline_continues else {
            return false;
        };
        continues
            .call(&mut self.store, i64::from(stroke))
            .unwrap_or(0)
            != 0
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

/// A plugin at its place in a chain.
struct PluginStep {
    name: String,
    /// The instance asked `joins` and `send_early`, if the plugin has either
    mods: Option<Mutex<Plugin>>,
    /// The thread reading words, for a plugin that reads them
    reader: Option<Worker>,
    /// Whether it is given the text before each outline (it exports `context`)
    context: bool,
    /// Whether it reads the strokes before each outline (it imports the history functions)
    history: bool,
    /// What `groups` answered for the readings made last, for a plugin that exports it
    reported: Option<Reports>,
}

/// Readings' sound groups kept for the engine to pick up.
const REPORTS_KEPT: usize = 16;

/// What a plugin's `groups` answered after its newest readings: the outline it read, the reading,
/// and each stroke's groups (in stroke bits). Shared by the reading thread and the chain.
#[derive(Clone, Default)]
struct Reports(Arc<Mutex<std::collections::VecDeque<Report>>>);

/// One reading's report: the outline read, the reading, and each stroke's groups.
type Report = (String, String, Vec<Vec<u32>>);

/// A `groups` answer, little-endian `u32` key sets, `0` between strokes, as one list of groups
/// per stroke (a trailing `0` ends the last stroke; it is not one more, empty).
fn split_groups(bytes: &[u8]) -> Vec<Vec<u32>> {
    let mut strokes = vec![Vec::new()];
    for word in bytes.as_chunks::<4>().0 {
        let keys = u32::from_le_bytes(*word);
        if keys == 0 {
            strokes.push(Vec::new());
        } else if let Some(last) = strokes.last_mut() {
            last.push(keys);
        }
    }
    if strokes.len() > 1 && strokes.last().is_some_and(Vec::is_empty) {
        strokes.pop();
    }
    strokes
}

impl PluginStep {
    /// The text before an outline as this plugin is asked it: none for one that has no use
    /// for it, so its questions are exactly what they always were.
    const fn before<'a>(&self, before: &'a str) -> &'a str {
        if self.context {
            before
        } else {
            ""
        }
    }

    /// The strokes before the run, as this plugin may read them: none for one that does not
    /// import the history functions, so its questions are exactly what they always were.
    fn window(&self, before: &Before<'_>) -> Option<Window> {
        if !self.history {
            return None;
        }
        before.log.map(|log| log.window(before.base))
    }
}

/// What is before a run of strokes that a plugin may be told: the text typed (see `context`),
/// and the strokes struck (see the history functions), as of the run's first stroke.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Before<'a> {
    /// The text typed before the run
    pub text: &'a str,
    /// The absolute index in `log` of the run's first stroke
    pub base: u64,
    /// The strokes struck, if known
    pub log: Option<&'a StrokeLog>,
}

/// One step of a lookup: a run of dictionary files merged together, or one plugin.
enum Step {
    Dict(Arc<Dictionary>),
    Plugin(Box<PluginStep>),
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
    dict: Arc<Dictionary>,
    steps: Vec<Step>,
    /// The layout strokes are rendered with before a plugin ever sees them: must be the same
    /// layout the keyboard itself parses strokes with, extra keys (a theory's own join key,
    /// say) included, or a plugin fallback call silently loses any bit the plain 23-key Plover
    /// layout has no name for - see [`Self::lookup`].
    layout: Layout,
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
/// (built from a literal `.json` string in a test, say) rather than files on disk. No plugin
/// step ever runs here, so which layout is recorded makes no difference.
impl From<Dictionary> for Chain {
    fn from(dict: Dictionary) -> Self {
        let dict = Arc::new(dict);
        Self {
            steps: vec![Step::Dict(Arc::clone(&dict))],
            dict,
            layout: Layout::plover(),
        }
    }
}

#[cfg(test)]
impl Chain {
    /// A chain over an already-built dictionary (see [`Dictionary::insert`], for an entry no
    /// dictionary *file* could express) with one plugin, loaded the normal way. `pub(crate)`
    /// since this exists for tests that need both at once, not as a general-purpose
    /// constructor - `load_cached` is that, for the files a real steno folder actually has.
    pub(crate) fn test_with_plugin(dict: Dictionary, plugin_file: &Path, cache_dir: &Path) -> Self {
        let plugin = load_one(plugin_file, cache_dir).expect("test plugin should load");
        let dict = Arc::new(dict);
        Self {
            steps: vec![
                Step::Dict(Arc::clone(&dict)),
                Step::Plugin(Box::new(plugin.into_step())),
            ],
            dict,
            layout: Layout::plover(),
        }
    }

    /// A chain over `dict` with a plugin that reads words with `read` (no wasm at all), in
    /// `layout`: for tests that need a plugin to be slow exactly when they say.
    pub(crate) fn test_with_reader(
        dict: Dictionary,
        layout: Layout,
        read: super::worker::Read,
    ) -> Self {
        Self::test_with_reader_of(dict, layout, read, false)
    }

    /// [`Self::test_with_reader`], `context` saying whether the reader is given the text
    /// before each outline, as a plugin that exports `context` is.
    pub(crate) fn test_with_reader_of(
        dict: Dictionary,
        layout: Layout,
        read: super::worker::Read,
        context: bool,
    ) -> Self {
        let dict = Arc::new(dict);
        Self {
            steps: vec![
                Step::Dict(Arc::clone(&dict)),
                Step::Plugin(Box::new(PluginStep {
                    name: "reader".into(),
                    mods: None,
                    reader: Worker::spawn("reader", read),
                    context,
                    history: false,
                    reported: None,
                })),
            ],
            dict,
            layout,
        }
    }

    /// Whether every plugin's reader is made: see [`Worker::wait_ready`].
    pub(crate) fn is_ready(&self) -> bool {
        self.steps.iter().all(|step| match step {
            Step::Plugin(plugin) => plugin
                .reader
                .as_ref()
                .is_none_or(|reader| reader.wait_ready(std::time::Instant::now())),
            Step::Dict(_) => true,
        })
    }
}

impl Chain {
    /// Load `files` (already expanded, highest priority first) into a chain, each dictionary
    /// file restored from its own cache copy or parsed (see [`Parts`]) and each plugin compiled
    /// once, with no error even if every file fails: a working but empty chain, exactly as an
    /// all-missing dictionary is.
    #[must_use]
    pub fn load_cached(files: &[PathBuf], cache_dir: &Path, layout: &Layout) -> Self {
        let mut parts = Parts::default();
        parts.refresh(files, cache_dir, layout);
        Self::from_parts(files, &parts, cache_dir, layout, &Trust::default())
    }

    /// The chain `files` make, out of `parts` already brought up to date with them: layered
    /// dictionaries in the order given, with a plugin at its own place among them. Nothing is
    /// read or parsed here, only merged, so it is what a reload costs when a file changed.
    #[must_use]
    pub fn from_parts(
        files: &[PathBuf],
        parts: &Parts,
        cache_dir: &Path,
        layout: &Layout,
        trust: &Trust,
    ) -> Self {
        let dict_only: Vec<PathBuf> = files
            .iter()
            .filter(|p| !is_plugin_file(p))
            .cloned()
            .collect();
        let dict = Arc::new(parts.layered_trusting(&dict_only, trust));
        // No plugin at all: the common case, and the one every keymux without one already
        // has, needs nothing further, the merged dictionary standing as the only step.
        if dict_only.len() == files.len() {
            return Self {
                steps: vec![Step::Dict(Arc::clone(&dict))],
                dict,
                layout: layout.clone(),
            };
        }
        let plugin_cache = cache_dir.join("plugins");
        let mut steps = Vec::new();
        let mut pending: Vec<PathBuf> = Vec::new();
        for path in files {
            if is_plugin_file(path) {
                if !pending.is_empty() {
                    steps.push(Step::Dict(Arc::new(
                        parts.layered_trusting(&pending, trust),
                    )));
                    pending.clear();
                }
                match load_one(path, &plugin_cache) {
                    Ok(plugin) => steps.push(Step::Plugin(Box::new(plugin.into_step()))),
                    Err(e) => tracing::warn!("plugin {} not loaded: {e:#}", path.display()),
                }
            } else {
                pending.push(path.clone());
            }
        }
        if !pending.is_empty() {
            steps.push(Step::Dict(Arc::new(
                parts.layered_trusting(&pending, trust),
            )));
        }
        // Each plugin makes its reader on its own thread, `init` and all, side by side with the
        // others. The chain is wanted ready to read, so the first stroke after a load or a
        // change does not pay for it; one that takes too long reads late instead.
        let until = std::time::Instant::now() + READY_WAIT;
        for step in &steps {
            if let Step::Plugin(plugin) = step {
                if let Some(reader) = &plugin.reader {
                    if !reader.wait_ready(until) {
                        tracing::warn!(
                            "plugin {} not ready yet; it reads late until it is",
                            plugin.name
                        );
                    }
                }
            }
        }
        Self {
            dict,
            steps,
            layout: layout.clone(),
        }
    }

    /// How a plugin split the strokes of an outline it read as `text` into sound groups, if it
    /// reported one (it exports `groups`; see the module doc): one [`Split`] per stroke, each
    /// group a set of two keys or more of one hand (or of the vowel keys) all in its stroke. `None` when no
    /// plugin reported, or the report does not fit the strokes (then it is not used at all).
    ///
    /// [`Split`]: super::notation::Split
    #[must_use]
    pub fn plugin_groups(&self, strokes: &[u32], text: &str) -> Option<Vec<Vec<u32>>> {
        let mut reports = self.steps.iter().filter_map(|step| match step {
            Step::Plugin(plugin) => plugin.reported.as_ref(),
            Step::Dict(_) => None,
        });
        let first = reports.next()?;
        let outline = self.layout.render_strokes(strokes);
        let found = std::iter::once(first).chain(reports).find_map(|reported| {
            lock(&reported.0)
                .iter()
                .rev()
                .find(|(asked, read, _)| *asked == outline && read == text)
                .map(|(_, _, groups)| groups.clone())
        })?;
        let layout = &self.layout;
        let hand = |range: std::ops::Range<usize>| range.fold(0u32, |m, slot| m | (1 << slot));
        let left = hand(0..layout.left_end());
        let right = hand(layout.right_start()..layout.number_slot());
        let middle = hand(layout.left_end()..layout.right_start());
        let fits = found.len() == strokes.len()
            && found.iter().zip(strokes).all(|(groups, &stroke)| {
                let mut taken = 0u32;
                groups.iter().all(|&group| {
                    let ok = group.count_ones() > 1
                        && group & !stroke == 0
                        && group & taken == 0
                        && (group & !left == 0 || group & !right == 0 || group & !middle == 0);
                    taken |= group;
                    ok
                })
            });
        if !fits {
            tracing::debug!("plugin groups for {outline} do not fit its strokes; not used");
        }
        fits.then_some(found)
    }

    /// How many plugins are running in this chain. Fewer than the plugin files there are means
    /// some did not compile or load, which the chain logs and carries on without.
    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::Plugin(_)))
            .count()
    }

    /// The translation of an exact outline: the first step with an answer, trying each in the
    /// priority order the files were given in. A plugin only ever answers for a single stroke,
    /// or a run of strokes that is really one word a theory's own join key stitched together on
    /// purpose (every stroke after the first answers `true` to [`Self::joins`]): the engine's
    /// own search for the longest dictionary match also tries windows of many unrelated strokes
    /// at once, and a plugin that answered those too would win every one of them (it never
    /// really has "no reading"), swallowing whole runs of unrelated strokes into one guess
    /// instead of leaving them as separate words. A real multi-stroke brief is the dictionary's
    /// job; the plugin is the last resort for what nothing else recognizes, one word at a time.
    ///
    /// A plugin is waited on for as long as its reading takes; the translator, which must not
    /// wait that long, asks through [`Self::lookup_within`].
    ///
    /// A plugin that exports `context` is given no text before the outline: nothing is known
    /// to precede it here. The translator, which knows, asks through `lookup_within`.
    #[must_use]
    pub fn lookup(&self, strokes: &[u32]) -> Option<String> {
        self.lookup_by(
            strokes,
            Before::default(),
            |plugin, reader, outline, before, window| match reader
                .ask(outline, before, window, None, false)
            {
                Asked::Answered(found) => found,
                Asked::Late => {
                    tracing::warn!("plugin {}: no reading of {outline}", plugin.name);
                    None
                }
            },
        )
    }

    /// [`Self::lookup`], waiting on a plugin no later than `budget` allows. A plugin late with
    /// its reading counts as having none, and `budget` keeps which outline it was late with;
    /// after that, no plugin is waited on again this stroke (a reading already in is still
    /// used). Every reading a plugin gave is noted in `budget` too, for the tape. `before` is
    /// the text typed before the outline's first stroke (at most [`CONTEXT_BYTES`]), given to
    /// a plugin that exports `context`.
    #[must_use]
    pub(crate) fn lookup_within(
        &self,
        strokes: &[u32],
        before: Before<'_>,
        budget: &Budget,
    ) -> Option<String> {
        self.lookup_by(
            strokes,
            before,
            |plugin, reader, outline, asked_before, window| {
                let found = if budget.is_late() {
                    reader.ready(outline, asked_before, window).flatten()
                } else {
                    match reader.ask(
                        outline,
                        asked_before,
                        window,
                        budget.until(),
                        budget.first_ask(),
                    ) {
                        Asked::Answered(found) => found,
                        Asked::Late => {
                            budget.mark_late(strokes, before.text, before.base);
                            None
                        }
                    }
                };
                if let Some(text) = &found {
                    budget.note_read(strokes, &plugin.name, text);
                }
                found
            },
        )
    }

    fn lookup_by(
        &self,
        strokes: &[u32],
        before: Before<'_>,
        mut ask: impl FnMut(&PluginStep, &Worker, &str, &str, Option<&Window>) -> Option<String>,
    ) -> Option<String> {
        for step in &self.steps {
            let found = match step {
                Step::Dict(dict) => dict.lookup_for_use(strokes),
                Step::Plugin(plugin) => match &plugin.reader {
                    Some(reader) if self.is_one_word(strokes) => ask(
                        plugin,
                        reader,
                        &self.layout.render_strokes(strokes),
                        plugin.before(before.text),
                        plugin.window(&before).as_ref(),
                    )
                    // A plugin's reading is a guess at a word: it never runs a command line
                    .map(|text| super::run_control::strip_shells(&text).unwrap_or(text)),
                    _ => None,
                },
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Whether every plugin would now answer `strokes` typed after `before` at once: its late
    /// reading is in, or nothing more is coming for it. See [`super::worker::Worker::settled`].
    #[must_use]
    pub(crate) fn settled(&self, strokes: &[u32], before: Before<'_>) -> bool {
        let outline = self.layout.render_strokes(strokes);
        self.steps.iter().all(|step| match step {
            Step::Plugin(plugin) => plugin.reader.as_ref().is_none_or(|reader| {
                reader.settled(
                    &outline,
                    plugin.before(before.text),
                    plugin.window(&before).as_ref(),
                )
            }),
            Step::Dict(_) => true,
        })
    }

    /// Whether `bits` (one stroke) continues the word before it: it has one of the layer's
    /// declared `join_keys` down, or any plugin says so through [`Plugin::joins`] (the two are
    /// unioned). `false` if neither does, including when there are no plugins and no
    /// `join_keys`: nothing here is a built-in notion of what any particular key means.
    #[must_use]
    pub fn joins(&self, bits: u32) -> bool {
        bits & self.layout.join_mask() != 0
            || self.steps.iter().any(|step| match step {
                Step::Plugin(plugin) => plugin.mods.as_ref().is_some_and(|m| lock(m).joins(bits)),
                Step::Dict(_) => false,
            })
    }

    /// The layout strokes are parsed with.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Whether the stroke in progress should be sent now, with `held` still down: any plugin
    /// says so through [`Plugin::send_early`]. `false` with no plugin that exports it.
    #[must_use]
    pub fn send_early(&self, held: u32, stroke: u32) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Plugin(plugin) => plugin
                .mods
                .as_ref()
                .is_some_and(|m| lock(m).send_early(held, stroke)),
            Step::Dict(_) => false,
        })
    }

    /// Whether `stroke` continues the outline `{:outline:next}` or `{:outline:explain}` is
    /// following: any plugin says so through [`Plugin::outline_continues`]. `false` with no
    /// plugin that exports it. Asked only while an outline is followed.
    #[must_use]
    pub fn outline_continues(&self, stroke: u32) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Plugin(plugin) => plugin
                .mods
                .as_ref()
                .is_some_and(|m| lock(m).outline_continues(stroke)),
            Step::Dict(_) => false,
        })
    }

    /// Whether `strokes` reads as one word to ask a plugin about: a single stroke, or several
    /// where every one after the first continues the word before it (see [`Self::joins`]),
    /// declaring itself a continuation on purpose. Anything else is a window the engine's own
    /// search built out of unrelated strokes to see if the dictionary recognizes them together,
    /// which a plugin must never be asked to settle. A leading `0` is never a real stroke (the
    /// engine's own "could a word start here" probe uses it as a placeholder; a real stroke
    /// always has a key down, see `ChordState::take`), so a window starting with one is never
    /// one word either, however the rest of it looks.
    fn is_one_word(&self, strokes: &[u32]) -> bool {
        match strokes {
            [] => false,
            [single] => *single != 0,
            [first, rest @ ..] => *first != 0 && rest.iter().all(|&s| self.joins(s)),
        }
    }

    /// The merged view of every dictionary file in the chain, ignoring where any plugin fell:
    /// for a caller that wants a plain `Dictionary` specifically (reverse lookup, completions),
    /// since there is nothing a plugin could meaningfully add to either.
    #[must_use]
    pub fn as_dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// The same merged view, to hand to another thread.
    #[must_use]
    pub fn dictionary(&self) -> Arc<Dictionary> {
        Arc::clone(&self.dict)
    }

    #[must_use]
    pub fn orthography(&self) -> &super::orthography::Orthography {
        self.dict.orthography()
    }

    #[must_use]
    pub fn longest_key(&self) -> usize {
        self.dict.longest_key()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.dict.len()
    }

    /// How many entries the files hold, an outline in two files counted twice: for a log line,
    /// since it does not read the entries, which [`Self::len`] does.
    #[must_use]
    pub fn stored(&self) -> usize {
        self.dict.stored()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }

    #[must_use]
    pub fn has_at_least(&self, count: usize) -> bool {
        self.dict.has_at_least(count)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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

    /// Like `DOUBLER`, with a `joins` export too: bit 23 (the first one beyond keymux's own
    /// 23 standard keys) continues the previous word - a made-up theory's own join key, for
    /// tests that need one without keymux having any built-in notion of what it is.
    const DOUBLER_WITH_JOIN: &str = r#"
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

#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
"#;

    /// A fresh folder for the test named `test`, removed when the test ends.
    fn tmp_dir(test: &str) -> crate::test_dir::TestDir {
        crate::test_dir::TestDir::new(&format!("keymux-plugin-test-{test}"))
    }

    #[test]
    fn a_real_plugin_compiles_runs_and_is_cached() {
        let dir = tmp_dir("a_real_plugin_compiles_runs_and_is_cached");
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
        let dir = tmp_dir("an_empty_reading_is_no_reading_at_all");
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
        let cache = tmp_dir("the_documented_example_plugin_compiles_and_gives_a_plausible_reading");
        let chain = Chain::load_cached(&[source], &cache, &Layout::plover());
        assert_eq!(
            chain.lookup(&[Layout::plover().parse_stroke("KAT").unwrap()]),
            Some("kat".to_string())
        );
        assert_eq!(
            chain.lookup(&[Layout::plover().parse_stroke("STOP").unwrap()]),
            Some("stop".to_string())
        );
        assert_eq!(
            chain.lookup(&[Layout::plover().parse_stroke("TKPWEUPB").unwrap()]),
            Some("gin".to_string())
        );
    }

    #[test]
    fn the_documented_mod_loads_without_fallback_and_answers_send_early() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("plugins")
            .join("sticky-key.rs");
        let cache = tmp_dir("the_documented_mod_loads_without_fallback_and_answers_send_early");
        let chain = Chain::load_cached(&[source], &cache, &Layout::plover());
        assert_eq!(
            chain.plugin_count(),
            1,
            "a mod with no fallback still loads"
        );
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let sticky = 1 << 23;
        assert!(
            chain.send_early(sticky, kat | sticky),
            "only the sticky key left down: send"
        );
        assert!(
            !chain.send_early(sticky | kat, kat | sticky),
            "other keys still down: wait"
        );
        assert!(
            !chain.send_early(sticky, sticky),
            "nothing but the sticky key: nothing to send"
        );
        assert_eq!(chain.lookup(&[kat]), None, "and it never reads a word");
    }

    #[test]
    fn the_documented_outline_mod_continues_a_followed_outline_with_its_key() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("plugins")
            .join("outline-join.rs");
        let cache = tmp_dir("the_documented_outline_mod_continues_a_followed_outline");
        let chain = Chain::load_cached(&[source], &cache, &Layout::plover());
        assert_eq!(
            chain.plugin_count(),
            1,
            "a mod with only outline_continues loads"
        );
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let join = 1 << 23;
        assert!(chain.outline_continues(kat | join));
        assert!(chain.outline_continues(join));
        assert!(!chain.outline_continues(kat));
        assert!(
            !chain.joins(kat | join),
            "it changes no grouping of its own"
        );
        assert_eq!(chain.lookup(&[kat]), None, "and it never reads a word");
    }

    #[test]
    fn a_chain_skips_a_plugin_that_does_not_compile_and_still_uses_the_one_after_it() {
        let dir =
            tmp_dir("a_chain_skips_a_plugin_that_does_not_compile_and_still_uses_the_one_after_it");
        let broken = dir.join("01-broken.rs");
        let good = dir.join("02-good.rs");
        fs::write(&broken, "this is not rust").unwrap();
        fs::write(&good, DOUBLER).unwrap();
        let chain = Chain::load_cached(&[broken, good], &dir.join("cache"), &Layout::plover());
        assert_eq!(
            chain.lookup(&[Layout::plover().parse_stroke("KAT").unwrap()]),
            Some("KKAATT".to_string()),
            "the broken plugin is skipped, the good one after it still answers"
        );
    }

    #[test]
    fn a_plugin_is_never_asked_about_unrelated_strokes_together() {
        // A window of several unrelated strokes reaching the plugin step would let it answer
        // for every one of them (it never really runs out of letters to double), swallowing
        // whatever came before into one guess instead of leaving each stroke to be judged on
        // its own.
        let dir = tmp_dir("a_plugin_is_never_asked_about_unrelated_strokes_together");
        let source = dir.join("doubler.rs");
        fs::write(&source, DOUBLER).unwrap();
        let chain = Chain::load_cached(&[source], &dir.join("cache"), &Layout::plover());
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let top = Layout::plover().parse_stroke("TOP").unwrap();
        assert_eq!(
            chain.lookup(&[kat]),
            Some("KKAATT".to_string()),
            "a single stroke still reaches the plugin"
        );
        assert_eq!(
            chain.lookup(&[kat, top]),
            None,
            "two unrelated strokes together must not reach the plugin, however short"
        );
    }

    #[test]
    fn config_join_keys_and_a_plugins_joins_are_unioned() {
        let dir = tmp_dir("config_join_keys_and_a_plugins_joins_are_unioned");
        let source = dir.join("doubler.rs");
        fs::write(&source, DOUBLER_WITH_JOIN).unwrap();
        // '~' is bit 23 (the plugin's join bit); '+' is bit 24, declared in config only
        let layout = Layout::plover_with_joins(&['~', '+'], &['+']).unwrap();
        let chain = Chain::load_cached(&[source], &dir.join("cache"), &layout);
        assert_eq!(chain.plugin_count(), 1);
        assert!(chain.joins(1 << 23), "the plugin's");
        assert!(chain.joins(1 << 24), "the config's");
        assert!(chain.joins((1 << 24) | 1), "any stroke with the key down");
        assert!(!chain.joins(1), "neither");
    }

    #[test]
    fn join_keys_must_be_extra_keys() {
        let errors = Layout::plover_with_joins(&['+'], &['~', '+']).unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("'~'") && errors[0].contains("extra_keys"),
            "{errors:?}"
        );
        assert!(Layout::plover_with_joins(&[], &['+']).is_err());
        assert_eq!(
            Layout::plover_with_joins(&['+'], &[]).unwrap(),
            Layout::plover_with(&['+']).unwrap()
        );
    }

    #[test]
    fn a_plugin_is_asked_about_a_run_the_join_key_declares_as_one_word() {
        // A join key - a theory's own, declared through `joins` (see `DOUBLER_WITH_JOIN`) -
        // exists precisely so a word that needs more keys than one stroke has can be spelled
        // across several strokes on purpose. That is still one word as far as a
        // phonetic-fallback plugin is concerned, unlike an arbitrary window the engine's own
        // search tried out of unrelated strokes.
        let dir = tmp_dir("a_plugin_is_asked_about_a_run_the_join_key_declares_as_one_word");
        let source = dir.join("doubler.rs");
        fs::write(&source, DOUBLER_WITH_JOIN).unwrap();
        let chain = Chain::load_cached(&[source], &dir.join("cache"), &Layout::plover());
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        // Bit 23 is this test's own made-up join key - keymux has no built-in notion of one.
        let joined_top = Layout::plover().parse_stroke("TOP").unwrap() | (1 << 23);
        assert!(
            chain.joins(joined_top),
            "the plugin's own joins() must be reachable through the chain"
        );
        assert_eq!(
            chain.lookup(&[kat, joined_top]),
            Some("KKAATT//TTOOPP".to_string()),
            "a join-key continuation is one word, so the plugin is still asked about it whole"
        );
    }

    /// Counts its `init` calls, and reads every outline as how many there were by then; `joins`
    /// answers whether its own instance was `init`ed. `RETURNS` is what `init` returns.
    fn counting_init(returns: &str) -> String {
        let (sig, ret) = match returns {
            "" => ("", ""),
            _ => (" -> i32", "0"),
        };
        format!(
            r#"
use std::sync::atomic::{{AtomicI32, Ordering}};
static INITS: AtomicI32 = AtomicI32::new(0);

#[no_mangle]
pub extern "C" fn init(){sig} {{
    INITS.fetch_add(1, Ordering::SeqCst);
    {ret}
}}

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {{
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}}

#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 {{
    let mut out = format!("inits={{}}", INITS.load(Ordering::SeqCst)).into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}}

#[no_mangle]
pub extern "C" fn joins(_bits: i64) -> i32 {{
    i32::from(INITS.load(Ordering::SeqCst) > 0)
}}
"#
        )
    }

    fn chain_of(test: &str, file: &str, source: &str) -> Chain {
        let dir = tmp_dir(test);
        let path = dir.join(file);
        fs::write(&path, source).unwrap();
        Chain::load_cached(&[path], &dir.join("cache"), &Layout::plover())
    }

    #[test]
    fn init_is_called_once_on_load_before_any_reading() {
        let chain = chain_of("init_once", "counting.rs", &counting_init(""));
        assert!(
            chain.is_ready(),
            "the reader is made and inited as the folder loads, not on the first stroke"
        );
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let top = Layout::plover().parse_stroke("TOP").unwrap();
        assert_eq!(
            chain.lookup(&[kat]),
            Some("inits=1".into()),
            "before the first reading"
        );
        assert_eq!(
            chain.lookup(&[top]),
            Some("inits=1".into()),
            "and never again"
        );
        assert!(
            !chain.joins(1 << 23),
            "the instance answering joins is not the one inited: they share nothing"
        );
    }

    #[test]
    fn an_init_that_returns_a_number_is_called_too() {
        let chain = chain_of("init_number", "counting.rs", &counting_init("i32"));
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        assert_eq!(chain.lookup(&[kat]), Some("inits=1".into()));
    }

    #[test]
    fn a_mod_is_inited_as_it_loads() {
        const MOD: &str = r#"
use std::sync::atomic::{AtomicBool, Ordering};
static READY: AtomicBool = AtomicBool::new(false);
#[no_mangle]
pub extern "C" fn init() {
    READY.store(true, Ordering::SeqCst);
}
#[no_mangle]
pub extern "C" fn send_early(_held: i64, _stroke: i64) -> i32 {
    i32::from(READY.load(Ordering::SeqCst))
}
"#;
        let chain = chain_of("init_mod", "mod.rs", MOD);
        assert_eq!(chain.plugin_count(), 1);
        assert!(
            chain.send_early(1, 3),
            "its init ran before it was asked anything"
        );
    }

    #[test]
    fn a_plugin_whose_init_fails_reads_nothing_and_the_chain_still_works() {
        let source = counting_init("").replace(
            "INITS.fetch_add(1, Ordering::SeqCst);",
            "INITS.fetch_add(1, Ordering::SeqCst); core::arch::wasm32::unreachable();",
        );
        let chain = chain_of("init_fails", "failing.rs", &source);
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        assert_eq!(chain.lookup(&[kat]), None);
    }

    /// Reads every outline as `(TEXT|OUTLINE)`, `TEXT` being what `context` last gave it.
    pub const ECHO_CONTEXT: &str = r#"
use std::sync::Mutex;
static CONTEXT: Mutex<String> = Mutex::new(String::new());

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn context(ptr: i32, len: i32) {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    *CONTEXT.lock().unwrap() = String::from_utf8(input.to_vec()).unwrap_or_else(|_| "BAD UTF-8".into());
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let outline = std::str::from_utf8(input).unwrap_or("");
    let mut out = format!("({}|{outline})", CONTEXT.lock().unwrap()).into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}

// Bit 23 is this test's own made-up join key
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
"#;

    #[test]
    fn a_plain_lookup_gives_context_no_text() {
        let chain = chain_of("context_plain", "echo.rs", ECHO_CONTEXT);
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        assert_eq!(chain.lookup(&[kat]), Some("(|KAT)".into()));
    }

    #[test]
    fn context_is_given_before_each_reading() {
        let chain = chain_of("context_each", "echo.rs", ECHO_CONTEXT);
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let budget = Budget::default();
        budget.start(std::time::Instant::now() + std::time::Duration::from_secs(10));
        assert_eq!(
            chain.lookup_within(
                &[kat],
                Before {
                    text: "the",
                    ..Before::default()
                },
                &budget
            ),
            Some("(the|KAT)".into())
        );
        budget.start(std::time::Instant::now() + std::time::Duration::from_secs(10));
        assert_eq!(
            chain.lookup_within(
                &[kat],
                Before {
                    text: "",
                    ..Before::default()
                },
                &budget
            ),
            Some("(|KAT)".into()),
            "an empty text is given too, never the last one left over"
        );
    }

    #[test]
    fn a_plugin_without_context_is_asked_exactly_as_before() {
        let chain = chain_of("context_none", "doubler.rs", DOUBLER);
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let budget = Budget::default();
        budget.start(std::time::Instant::now() + std::time::Duration::from_secs(10));
        assert_eq!(
            chain.lookup_within(
                &[kat],
                Before {
                    text: "the",
                    ..Before::default()
                },
                &budget
            ),
            Some("KKAATT".into())
        );
        assert!(
            chain.settled(
                &[kat],
                Before {
                    text: "anything",
                    ..Before::default()
                }
            ),
            "its questions carry no text"
        );
    }

    /// `DOUBLER` that also reports its strokes' sound groups: `T P` and `H R` of the one stroke.
    fn doubler_with_groups() -> String {
        format!(
            "{DOUBLER}{}",
            r#"
#[no_mangle]
pub extern "C" fn groups(_ptr: i32, _len: i32) -> i64 {
    let mut out: Vec<u8> = Vec::new();
    for keys in [0b1010u32, 0b110_0000, 0] {
        out.extend(keys.to_le_bytes());
    }
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#
        )
    }

    #[test]
    fn a_plugins_groups_are_kept_for_the_reading_and_used_only_when_they_fit() {
        let chain = chain_of("groups", "grouper.rs", &doubler_with_groups());
        let layout = Layout::plover();
        let budget = Budget::default();
        let read = |stroke: u32| {
            budget.start(std::time::Instant::now() + std::time::Duration::from_secs(10));
            chain
                .lookup_within(&[stroke], Before::default(), &budget)
                .unwrap()
        };
        let flower = layout.parse_stroke("TPHROUR").unwrap();
        let text = read(flower);
        assert_eq!(text, "TTPPHHRROOUURR");
        assert_eq!(
            chain.plugin_groups(&[flower], &text),
            Some(vec![vec![0b1010, 0b110_0000]])
        );
        // Not the reading it reported for, or another outline: nothing
        assert_eq!(chain.plugin_groups(&[flower], "flower"), None);
        // A report whose groups are not all in the stroke is not used
        let cat = layout.parse_stroke("KAT").unwrap();
        let text = read(cat);
        assert_eq!(chain.plugin_groups(&[cat], &text), None);
        // A plugin without `groups` reports nothing
        let plain = chain_of("groups_none", "doubler.rs", DOUBLER);
        budget.start(std::time::Instant::now() + std::time::Duration::from_secs(10));
        let text = plain
            .lookup_within(&[flower], Before::default(), &budget)
            .unwrap();
        assert_eq!(plain.plugin_groups(&[flower], &text), None);
    }

    #[test]
    fn a_groups_answer_is_split_into_strokes() {
        let bytes: Vec<u8> = [3u32, 12, 0, 48, 0]
            .iter()
            .flat_map(|k| k.to_le_bytes())
            .collect();
        assert_eq!(split_groups(&bytes), [vec![3, 12], vec![48]]);
        assert_eq!(split_groups(&[]), [Vec::<u32>::new()]);
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
