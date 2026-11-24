# Plugins

A plugin is one ordinary `.rs` file, real Rust, nothing else: no separate crate, no `Cargo.toml`
to write, no build step to run by hand. It lives in your steno folder exactly where a dictionary
file would (`~/.config/keymux/steno/`, any name ending in `.rs`, any depth of folder), and keeps
its place in that same priority order: see `src/steno/source.rs` for how a number prefix, a
plain file or a plain folder rank against each other. keymux compiles and loads it in the
background as the steno folder loads, and again whenever the file changes, the same way a
dictionary reloads live.

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

A plugin exports two required functions, in plain Rust with no attributes beyond `#[no_mangle]`
and `extern "C"`:

```rust
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 { /* ... */ }

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 { /* ... */ }
```

`alloc` is asked for `len` bytes of memory and returns where they start (as an address inside
the plugin's own sandboxed memory, not a real pointer keymux can read directly), or `0` to
decline. keymux writes one word's outline there, in steno notation (`KAT`, `KOPB`, `KAT/+-G`),
encoded as UTF-8 — one stroke, or several where every one after the first continues the word
before it on purpose (see `joins` below), declaring itself a continuation of the same word. It
never writes a run of otherwise-unrelated strokes: the engine's own search for the longest
dictionary match tries windows of many strokes together regardless, and a plugin that answered
those too would win every one of them, swallowing unrelated strokes into one guess instead of
leaving them as separate words. A real multi-stroke brief is the dictionary's job.

`fallback` is given that address and length. It answers with its own reading of the outline,
written into memory it allocates the same way, as a packed pointer and length
(`(ptr as i64) << 32 | len as i64`), or any negative number for no reading at all.

`orthographic-fallback.rs` does all of this; the bottom of the file, under "the interface keymux's
plugin loader calls", is every line that interface needs; everything above it is the plugin's
own logic and can be replaced freely.

A join-key stroke with nothing before it to glue onto is never given to a plugin on its own: it
already declared itself a continuation of the word before it, so being asked to read it in
isolation would be asking the wrong question.

### `joins` (optional)

keymux itself has no notion of any key meaning "continue the previous word" - every stroke key,
standard or a theory's own extra one, is just a bit to the engine. A theory that wants one of
its keys to behave that way exports a third function:

```rust
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 { /* ... */ }
```

Called with one stroke's bits (the same ones a dictionary key or the tape would carry),
answering non-zero if this stroke should glue onto whatever came before it rather than start a
word of its own. A plugin that has no use for this leaves it out entirely; a stroke is then
never treated as a continuation by anything in keymux. `orthographic-fallback.rs` includes a
working example - see its own comment for the bit it assumes.

A plugin is no longer needed for this: the steno layer can declare the join key in its config
(`extra_keys: ['+'], join_keys: ['+']`, see the README's steno Config section). If both are
present, they are unioned: a stroke continues the word if either says so.

### `send_early` (optional)

A stroke is sent when its last key comes up. A plugin can send one sooner, with keys still down:

```rust
#[no_mangle]
pub extern "C" fn send_early(held: i64, stroke: i64) -> i32 { /* ... */ }
```

Asked each time a stroke key comes up while others are still down, with the bits of the keys
still down (`held`) and of every key pressed for the stroke so far (`stroke`). Non-zero sends the
stroke now, the same way a `{:firstup}` entry does: the keys still down stay in the chord and go
with the next key pressed, and letting go of them alone sends nothing. `sticky-key.rs` uses it to
make one key sticky: hold it, and each stroke goes as its other keys come up, the held key part of
every one.

### `outline_continues` (optional)

`{:outline:next}` and `{:outline:explain}` follow the next translation as the engine groups it: a
multi-stroke entry (`KPA/TKAOEU`) or a joined run is named whole, and the first stroke that starts
a new translation ends them. A plugin can make some strokes always continue the outline shown:

```rust
#[no_mangle]
pub extern "C" fn outline_continues(stroke: i64) -> i32 { /* ... */ }
```

Asked with one stroke's bits, and only for a stroke struck while one of those commands is
following an outline (never otherwise, so it costs nothing the rest of the time). Non-zero makes
the stroke part of the outline even where the engine starts a new translation with it; the entry
shown is then each translation's entry, in order, separated by spaces. It changes nothing about
how strokes translate. `outline-join.rs` makes a theory's join key `+` continue every outline.

### `init` (optional)

```rust
#[no_mangle]
pub extern "C" fn init() { /* ... */ }   // `-> i32` is accepted too; the number is ignored
```

Called exactly once on the instance that reads words, on its own thread, right after it is made
and before any `context` or `fallback`: the place to build tables up front instead of on the first
reading. It runs as the steno folder loads, and again for the new instance whenever a plugin
file changes; the load waits for it (up to 2 seconds) before the new dictionaries are used, so the
first stroke never pays for it. A stroke that needs the plugin while `init` is still running is
late (see below), never held up. A mod's one instance is `init`ed as it loads. The second instance
that answers `joins` and `send_early` for a plugin that reads words is never `init`ed, so those two
must not depend on it. An `init` that traps leaves the plugin reading nothing (a mod is left out).

### `context` (optional)

```rust
#[no_mangle]
pub extern "C" fn context(ptr: i32, len: i32) { /* ... */ }
```

Called immediately before every `fallback`, on the same instance, with the text typed just before
the outline being asked about, written as UTF-8 at an address `alloc` returned (as `fallback`'s
input is; an empty text is `alloc(0)` and `len` 0; if `alloc` declines a non-empty one, `context`
is skipped for that reading). It is meant only to inform the `fallback` that follows.

- **What text:** what keymux knows is on screen before the outline's first stroke: steno output and
  text typed by hand (Enter and Tab included). For a joined run it is the text before the run's
  first stroke, never what the run typed so far.
- **How much:** at most the last 256 bytes, the cut moved forward to a character boundary, so it
  can be 253 to 256 bytes when there is more.
- **Walls:** empty after anything that leaves keymux not knowing what precedes the cursor: a key
  that moves the caret (an arrow, Delete, Escape), a shortcut, a window change, or a stroke whose
  key combination moves the cursor or deletes (a barrier). Only what was typed after the last
  wall is given.
- **Questions:** the same outline after different text is a different question, read again. A
  reading that is late keeps the text it was asked with.
- `context` is not called before `joins` or `send_early`, nor by anything but the reading
  instance. A plugin without it is asked exactly as before.

### History: the strokes before a word (optional)

A plugin may import two host functions (module `keymux`) to read back the strokes struck before
the run it is being asked about, one at a time. Nothing to export; a plugin that does not import
them is asked exactly as before, and one that does still loads everywhere (the host defines them
for every instance).

```rust
#[link(wasm_import_module = "keymux")]
extern "C" {
    fn history_len() -> i32;
    fn history_stroke(k: i32) -> i64;
}
```

- `history_len() -> i32`: how many strokes of history there are before the run (since the last
  wall), at most `i32::MAX`.
- `history_stroke(k: i32) -> i64`: the stroke `k` steps back. `k = 0` is the stroke right before
  the run's first stroke, `k = 1` the one before that, and so on. The value is the stroke's key
  bits, the layout `joins(bits)` and `send_early` receive, as a non-negative `i64`; `-1` if `k` is
  negative, or past the oldest stroke or the wall.

Both answer `0` / `-1` anywhere but inside a `fallback` call: in `joins`, `send_early`, `init`,
`context` and in `alloc`, and in a `fallback` for a lookup that carries no history (for example
a reverse lookup). Each call is O(1); reading `k` strokes is `k` calls, whatever the history's
length, and nothing is copied.

- **What is in it:** the strokes of the translations the engine keeps, in order: every stroke
  struck that left a translation, including ones whose text is empty, ones that matched nothing,
  and ones consumed into a multi-stroke entry (both strokes of `KAT/TOP` are there). The strokes
  of the run being asked about are not; neither are strokes that made no translation (a
  transparent stroke, an engine command such as undo itself).
- **Undo:** taking a translation back removes its strokes (a multi-stroke entry gives back the
  strokes it replaced as they were). A question never sees strokes that are no longer there: if
  undo cuts into the history while the plugin reads, the calls answer `0` / `-1` (that answer is
  thrown away anyway).
- **Walls:** the history starts over empty at the same walls `context` does (a caret-moving key,
  a shortcut, a window change, `start_fresh`, a barrier stroke, which is itself before the wall),
  and also when text is typed by hand.
- **Questions:** the same outline after different strokes is a different question, read again.
  A late reading keeps the history it was asked with.
- **Limit:** the newest 1,048,576 strokes (4 MiB) at least are kept; the oldest go a block at a
  time (up to 65,536 over the limit before they do). Read as far back as you need, not to the end.

A plugin checking whether the previous stroke was a given one:

```rust
const PREVIOUS: i64 = 1 << 23; // whatever bits your theory cares about

fn previous_was_marked() -> bool {
    unsafe { history_stroke(0) == PREVIOUS }
}
```

## A slow reading never holds up typing

A plugin that reads words does it on a thread of its own, so a `fallback` that takes a while
never stalls the keyboard. The stroke waits for the reading only until a deadline, set per steno
layer (`plugin_deadline_ms`, 30 by default):

- **In time:** the reading is used exactly as if it had been asked in place.
- **Late:** the stroke is typed as if the plugin had no reading (as its keys, or whatever the
  dictionaries make of it), and typing goes on. When the reading lands, if that stroke is still
  the last thing typed, it is translated again with the reading and what it typed is replaced
  (backspaces, then the new text). If anything came first (another stroke, an undo, text typed by
  hand), the reading is dropped.
- **Newest first:** a newer stroke's question drops whatever earlier strokes asked that has not
  started yet, so the thread always works toward the run being typed now. A run of joined
  strokes asks about the whole run on every stroke, so the reading that lands is the whole run's.
  A reading that has started cannot be stopped; it finishes and is kept for a moment, in case it
  is asked for again.

`joins`, `send_early` and `outline_continues` are answered by a second instance of the same module, on the engine's
own thread, so they never wait behind a reading. The two instances share no memory: keep
whatever `fallback` needs from call to call (a cache of the run read last, say) in what
`fallback` itself sees (it, `init` and `context` share the reading instance), and never count on
`joins` or `send_early` to see it, or to have set it.

## On the tape

A stroke's tape line (`keymux steno tape`) shows the stroke in the layer's own layout, extra keys
included: a joined stroke is `+-G`, a stroke of the join key alone is `+`. When a plugin's reading
is what a stroke typed, the line ends with `plugin:NAME "reading"` (`NAME` is the plugin file's
stem, `05-solver` for `05-solver.rs`). A late reading that replaced what its stroke typed is a
line of its own, `late`, with what the replacement typed and the same mark; the overlay shows it
too, as a line with no stroke, and greys what the stroke typed without it. A stroke that matched
nothing is `?` followed by what it typed (`? "KA-T"`), so a reader can follow an undo of it. A
theory can score its fallback on real typing from these lines.

## Mods: a plugin that reads no words

`alloc` and `fallback` are only required of a plugin that reads words. One that only changes how
the engine behaves (`joins`, `send_early`, `outline_continues`) leaves them out and is a *mod*: it
sits in the steno folder like any plugin, is never asked for a word, and must export `fallback`,
`joins`, `send_early` or `outline_continues` (`init` and `context` alone are not enough).
`sticky-key.rs` and `outline-join.rs` are complete ones.

## Where things are cached

A compiled plugin lives under the same cache root a dictionary's own binary copy is kept (see
`src/steno/setup.rs`), in a `plugins` folder next to it — for the usual `~/.config/keymux`, that
is `~/.cache/keymux/steno/plugins/`, named after its own source's content hash. Delete that
folder to force every plugin to recompile; nothing is ever lost from it being gone, since it is
rebuilt automatically.
