# Plugins

A plugin is one ordinary `.rs` file, real Rust, nothing else: no separate crate, no `Cargo.toml`
to write, no build step to run by hand. It lives in your steno folder exactly where a dictionary
file would (`~/.config/keymux/steno/`, any name ending in `.rs`, any depth of folder), and keeps
its place in that same priority order: see `src/steno/source.rs` for how a number prefix, a
plain file or a plain folder rank against each other. keymux compiles and loads it the moment
it's first needed and again whenever the file changes, the same way a dictionary reloads live.

A plugin is asked in its own turn, exactly like a dictionary file: if nothing ahead of it in the
folder's priority order answered, it gets a chance before whatever comes after. Numbered ahead
of your dictionaries, it can override them outright; numbered after, or left with no number, it
is a fallback for whatever nothing earlier in the folder could answer — typically, what happens
when a stroke misses every dictionary outright. With no plugin at all, keymux shows the stroke's
own keys, as it always has (`KAT` with no entry anywhere just shows `KAT`).

`orthographic-fallback.rs` in this folder is a complete, working example: a phonetic best guess
read straight off a stroke's own keys, close enough to be read, not real spelling (a stroke
alone can't say how a word is actually spelled). Copy it into your steno folder to try it, giving
it a number after your dictionaries so it only answers what they miss, or read it as a template
for a plugin of your own.

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

A compiled plugin lives under the same cache root a dictionary's own binary copy is kept (see
`src/steno/setup.rs`), in a `plugins` folder next to it — for the usual `~/.config/keymux`, that
is `~/.cache/keymux/steno/plugins/`, named after its own source's content hash. Delete that
folder to force every plugin to recompile; nothing is ever lost from it being gone, since it is
rebuilt automatically.
