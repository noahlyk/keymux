//! Plugins: ordinary `.rs` files that extend the engine beyond a dictionary.
//!
//! Each lives under `<config>/plugins/`, compiled to a sandboxed WebAssembly module and asked
//! only when nothing else has an answer.
//!
//! A plugin is real Rust, compiled with the exact `rustc` this build of keymux runs on, to
//! `wasm32-unknown-unknown`. Compiling it is on keymux, the moment it is first used, exactly
//! the way a dictionary is parsed once and kept as a binary copy (see
//! [`super::dict::load_cached`]): unchanged, a plugin is never recompiled. Running it is on
//! [`wasmtime`], so a bug in a plugin cannot corrupt keymux's memory or bring the daemon down
//! with it; a crash, an infinite loop or a panic inside a plugin becomes an ordinary `Err` from
//! the call into it, nothing more.
//!
//! # The fallback interface
//!
//! The one thing a plugin can be asked right now: a reading of a stroke outline that missed
//! every dictionary outright, such as an orthographic guess from a theory's own phonetic rules.
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

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Bumped whenever the interface above changes, so a plugin compiled against an older one is
/// recompiled rather than trusted.
const ABI_VERSION: u32 = 1;

/// Directory plugins live in, under the keymux config directory: every `.rs` file in it,
/// however deeply nested, is one.
#[must_use]
pub fn plugins_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("plugins")
}

/// Where compiled plugins are cached, the same way as [`super::setup::cache_dir`].
///
/// For the usual `~/.config/keymux` it is `~/.cache/keymux/plugins`, elsewhere a hidden
/// `.cache` folder inside the plugins folder.
#[must_use]
pub fn cache_dir(config_dir: &Path) -> PathBuf {
    let usual = config_dir.file_name().is_some_and(|name| name == "keymux")
        && config_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".config");
    if usual {
        if let Some(home) = config_dir.parent().and_then(Path::parent) {
            return home.join(".cache").join("keymux").join("plugins");
        }
    }
    plugins_dir(config_dir).join(".cache")
}

/// Every `.rs` file under `dir`, in a stable order.
fn find_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(dir, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
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
    /// Plover notation (`KAT`, `KOPB/^TKEUGS`). `None` for no reading, or if the call itself
    /// failed: a bad plugin answers nothing, it cannot bring the daemon down with it.
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
        String::from_utf8(buf).ok()
    }
}

/// Every plugin found under `config_dir`'s plugins folder, compiled and loaded.
///
/// One that fails to compile or load is left out, with a warning, rather than stopping the
/// daemon: a plugins folder with nothing usable in it behaves exactly like having none.
#[must_use]
pub fn load_all(config_dir: &Path) -> Vec<Plugin> {
    let dir = plugins_dir(config_dir);
    let cache = cache_dir(config_dir);
    find_sources(&dir)
        .into_iter()
        .filter_map(|source| {
            let name = source.file_stem().map_or_else(
                || "plugin".to_string(),
                |s| s.to_string_lossy().into_owned(),
            );
            match compiled(&source, &cache).and_then(|wasm| Plugin::load(name, &wasm)) {
                Ok(plugin) => Some(plugin),
                Err(e) => {
                    tracing::warn!("plugin {} not loaded: {e:#}", source.display());
                    None
                }
            }
        })
        .collect()
}

/// The first reading any loaded plugin gives a stroke outline that missed every dictionary.
pub fn fallback(plugins: &mut [Plugin], outline: &str) -> Option<String> {
    plugins.iter_mut().find_map(|p| p.fallback(outline))
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn the_documented_example_plugin_compiles_and_gives_a_plausible_reading() {
        // The example a real plugin author would copy and adapt: keep it loadable.
        let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        let mut plugins = load_all(&config_dir);
        assert_eq!(
            plugins.len(),
            1,
            "docs/plugins/orthographic-fallback.rs should load"
        );
        assert_eq!(fallback(&mut plugins, "KAT"), Some("kat".to_string()));
        assert_eq!(fallback(&mut plugins, "STOP"), Some("stop".to_string()));
        assert_eq!(fallback(&mut plugins, "TKPWEUPB"), Some("gin".to_string()));
    }

    #[test]
    fn load_all_skips_a_plugin_that_does_not_compile_rather_than_failing() {
        let dir =
            tmp_dir().join("load_all_skips_a_plugin_that_does_not_compile_rather_than_failing");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(plugins.join("broken.rs"), "this is not rust").unwrap();
        fs::write(plugins.join("good.rs"), DOUBLER).unwrap();
        let loaded = load_all(&dir);
        assert_eq!(
            loaded.len(),
            1,
            "the good plugin loads, the broken one is skipped"
        );
        assert_eq!(loaded[0].name(), "good");
    }

    #[test]
    fn plugins_dir_and_cache_dir_match_the_steno_folder_convention() {
        assert_eq!(
            plugins_dir(Path::new("/home/a/.config/keymux")),
            PathBuf::from("/home/a/.config/keymux/plugins")
        );
        assert_eq!(
            cache_dir(Path::new("/home/a/.config/keymux")),
            PathBuf::from("/home/a/.cache/keymux/plugins")
        );
        assert_eq!(
            cache_dir(Path::new("/tmp/elsewhere")),
            PathBuf::from("/tmp/elsewhere/plugins/.cache")
        );
    }
}
