# Plover reference outputs

keymux's tests compare it with the real Plover. These scripts run Plover's own translator,
formatter, orthography, stroke code and RTF parser, and write what they do into
`src/steno/testdata/`. keymux's tests replay the same inputs and must give the same answers.

| Script | Writes | What it checks |
|---|---|---|
| `gen.py` | `plover_golden.json`, `plover_golden_words.txt` | the text, key combinations and commands after every stroke: every operator of the translation language, undo, and thousands of random sequences, in each start mode, with spaces before and after, including Lapwing's stitching and repeat plugins |
| `gen_strokes.py` | `stroke_golden.json` | how a set of keys is written, and how a spelling is read (number keys, hyphens, `/S`) |
| `gen_ortho.py` | `orthography_golden.json`, `orthography_words.txt` | a suffix added to a word (`make` + `ing`) |
| `gen_rtf.py` | `rtf_golden.json` | RTF/CRE files Plover writes, and hand-written ones with the corners of the format |
| `gen_real.py` | a local file only | the same check over Plover's shipped `main.json` and `commands.json` (too big to keep): run `plovers_own_dictionaries_type_what_plover_types` with `PLOVER_ASSETS` and `PLOVER_REAL_GOLDEN` set |

Regenerate (needs Python 3 and network access; nothing here is needed to build or test keymux):

```sh
python3 -m venv /tmp/plover-venv
/tmp/plover-venv/bin/pip install plover-stroke rtf-tokenize platformdirs appdirs setuptools evdev psutil xkbcommon
/tmp/plover-venv/bin/pip install --no-deps plover plover-stitching plover-last-translation
/tmp/plover-venv/bin/python tools/plover-golden/gen.py src/steno/testdata/plover_golden.json src/steno/testdata/plover_golden_words.txt
/tmp/plover-venv/bin/python tools/plover-golden/gen_strokes.py src/steno/testdata/stroke_golden.json
/tmp/plover-venv/bin/python tools/plover-golden/gen_ortho.py src/steno/testdata/orthography_golden.json src/steno/testdata/orthography_words.txt
/tmp/plover-venv/bin/python tools/plover-golden/gen_rtf.py src/steno/testdata/rtf_golden.json
```

Strokes with no translation are written the way keymux writes them (`STPHA-UBGS`) instead of
Plover's own spelling, which is the one difference by design.
