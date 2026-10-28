# Plover reference outputs

keymux's tests compare it with the real Plover. These scripts run Plover's own translator,
formatter and stroke code and write what it does into `src/steno/testdata/`:

- `plover_golden.json`: for each case, the strokes typed and the text and key/command events after
  every one (`gen.py`; dictionaries in `dicts.py` exercise every operator of the translation language)
- `stroke_golden.json`: how Plover writes a set of keys and reads a spelling (`gen_strokes.py`)

Regenerate (needs Python 3 and network access; nothing here is needed to build or test keymux):

```sh
python3 -m venv /tmp/plover-venv
/tmp/plover-venv/bin/pip install plover-stroke rtf-tokenize platformdirs appdirs setuptools
/tmp/plover-venv/bin/pip install --no-deps plover
/tmp/plover-venv/bin/python tools/plover-golden/gen.py src/steno/testdata/plover_golden.json
/tmp/plover-venv/bin/python tools/plover-golden/gen_strokes.py src/steno/testdata/stroke_golden.json
```

Strokes with no translation are written the way keymux writes them (`STPHA-UBGS`) instead of
Plover's own spelling, which is the one difference by design.
