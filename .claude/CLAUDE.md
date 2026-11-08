# keymux

**keymux is a generic, Plover-like steno engine.** It could in principle host any steno theory, so
it must never hardcode one: no theory's key bits, strokes, words or behaviors in keymux code. This
includes the owner's own theory, lyk.

**Custom theory ideas go through plugins.** When a theory needs something new from the engine,
keymux gains a *generic* plugin capability that any theory could use, documented in
[`docs/plugins/README.md`](../docs/plugins/README.md) with a copyable example. So far:
- `fallback` reads a run that missed the dictionary (lyk's solver).
- `joins(bits)` names the key that continues a word.
- `send_early(held, stroke)` sends a stroke with keys still down (lyk's sticky-plus mod uses it).
- A plugin with no `alloc`/`fallback` is a mod that reads no words.
- Undo word takes back a whole run a plugin read.
- A plugin reads on its own thread, waited on until `plugin_deadline_ms`; a late reading
  replaces what its stroke typed once it lands, if that is still the last thing typed.
- The tape shows strokes in the layer's layout (extra keys too, `+` alone) and marks a line a
  plugin's reading typed with `plugin:NAME "reading"`.

The theory-specific half of each feature lives in the theory's own repo, never here.

**lyk lives elsewhere.** The custom theory is
[lyk-steno-theory](/home/fib/code/noahlyk/lyk-steno-theory): its rules, dictionary, solver plugin
(`05-lyk-solver.rs`) and mods (`06-lyk-sticky-plus.rs`), all written into keymux's config by lyk's
`deploy`. Lyk is built to work with keymux the moment it is deployed; keymux is not built around
lyk.

**Never push keymux.** Work lands on local branches only.
