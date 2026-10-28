# Plugins

A plugin is one ordinary `.rs` file, real Rust, nothing else: no separate crate, no `Cargo.toml`
to write, no build step to run by hand. Drop it into `~/.config/keymux/plugins/` (any name
ending in `.rs`, any depth of folder) and keymux compiles and loads it, the moment it's first
needed and again whenever the file changes, the same way a dictionary reloads live.

What a plugin extends today: what happens when a stroke misses every dictionary outright. With
no plugin, keymux shows the stroke's own keys, as it always has (`KAT` with no entry in any
dictionary just shows `KAT`). With one loaded, it's asked first, and only falls back to the raw
keys if it has no answer either.

`orthographic-fallback.rs` in this folder is a complete, working example: a phonetic best guess
read straight off a stroke's own keys, close enough to be read, not real spelling (a stroke
alone can't say how a word is actually spelled). Copy it into your plugins folder to try it, or
read it as a template for a plugin of your own.

## Why WebAssembly

A plugin is compiled to [WebAssembly](https://webassembly.org/) (`wasm32-unknown-unknown`), not
to a native library, and run through [`wasmtime`](https://wasmtime.dev/). That is a deliberate
choice, not an obstacle to work around:

- **A bad plugin cannot bring keymux down.** A panic, an infinite loop, an out-of-bounds read:
  all of it is trapped inside the sandbox. The call into it returns an ordinary error; the
  daemon keeps running.
- **No ABI to get wrong.** A native plugin loaded as a shared library has to be compiled with
  the exact same Rust compiler as the host, forever, or its layout can silently disagree with
  what the host expects. A WebAssembly module has none of that: it stays loadable by any
  compliant runtime, including a newer keymux built with a newer Rust, indefinitely.
- **The cost is a boundary, not a rewrite.** A plugin is still real Rust. It just can't reach
  the filesystem, threads or anything else keymux doesn't explicitly hand it, which for a
  function that turns stroke keys into a guessed spelling is not a loss.

Building one requires the `wasm32-unknown-unknown` target:

```
rustup target add wasm32-unknown-unknown
```

keymux compiles plugins with its own `rustc`, so this only needs doing once, not something a
plugin's own build does.

## Writing one

A plugin exports exactly two functions, in plain Rust with no attributes beyond `#[no_mangle]`
and `extern "C"`:

```rust
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 { /* ... */ }

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 { /* ... */ }
```

`alloc` is asked for `len` bytes of memory and returns where they start (as an address inside
the plugin's own sandboxed memory, not a real pointer keymux can read directly), or `0` to
decline. keymux writes the stroke outline there, in Plover notation (`KAT`, or `KOPB/TKEUGS` for
several strokes), encoded as UTF-8.

`fallback` is given that address and length. It answers with its own reading of the outline,
written into memory it allocates the same way, as a packed pointer and length
(`(ptr as i64) << 32 | len as i64`), or any negative number for no reading at all.

`orthographic-fallback.rs` does all of this; the bottom of the file, under "the interface keymux's
plugin loader calls", is every line that interface needs; everything above it is the plugin's
own logic and can be replaced freely.

A stroke that carries the join key (see `src/steno/layout.rs`) is never given to a plugin on its
own: it already declared itself a continuation of the word before it, so being asked to read it
in isolation would be asking the wrong question.

## Where things are cached

A compiled plugin lives in `~/.cache/keymux/plugins/`, named after its own source's content
hash. Delete that folder to force every plugin to recompile; nothing is ever lost from it being
gone, since it is rebuilt automatically.
