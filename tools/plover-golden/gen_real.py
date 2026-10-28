"""Reference outputs from Plover's own shipped dictionaries, for a local-only check.

Random sequences of real entries (single strokes, whole multi-stroke outlines, punctuation,
suffixes) plus undo, run through Plover's translator and formatter. Not committed: main.json is
4 MB. Run keymux's `real_plover_dictionary_matches_plover` test against the output.
"""

import json
import logging
import os
import random
import sys

logging.disable(logging.CRITICAL)
from plover import system
from plover.registry import registry

registry.update()
system.setup("English Stenotype")
import plover.formatting as formatting
from plover.dictionary.json_dict import JsonDictionary
from plover.formatting import Formatter
from plover.steno import Stroke
from plover.steno_dictionary import StenoDictionaryCollection
from plover.translation import Translator

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
KEYMUX_LEFT = ["S-", "T-", "K-", "P-", "W-", "H-", "R-", "A-", "O-"]
KEYMUX_RIGHT = ["-E", "-U", "-F", "-R", "-P", "-B", "-L", "-G", "-T", "-S", "-D", "-Z"]


def keymux_raw(rtfcre):
    keys = set(Stroke.from_steno(rtfcre).steno_keys)
    left = "".join(k[0] for k in KEYMUX_LEFT if k in keys)
    right = "".join(k[1] for k in KEYMUX_RIGHT if k in keys)
    return ("#" if "#" in keys else "") + left + "-" + ("*" if "*" in keys else "") + right


_orig_raw = formatting._raw_to_actions


def _raw(stroke, ctx):
    if stroke.replace("-", "", 1).isdigit():
        return _orig_raw(stroke, ctx)
    return _orig_raw(keymux_raw(stroke), ctx)


formatting._raw_to_actions = _raw

assets = sys.argv[1]
out_path = sys.argv[2]
count = int(sys.argv[3]) if len(sys.argv) > 3 else 1500
commands = JsonDictionary.load(os.path.join(assets, "commands.json"))
main = JsonDictionary.load(os.path.join(assets, "main.json"))
rng = random.Random(4242)

entries = list(main.items())
single = [(k, v) for k, v in entries if len(k) == 1 and isinstance(v, str)]
multi = [(k, v) for k, v in entries if 2 <= len(k) <= 3]
words = [(k, v) for k, v in single if v.isalpha() and v.islower()]
punctuation = [(k, v) for k, v in single if v in ("{.}", "{,}", "{?}", "{!}", "{:}", "{;}", "{-|}", "{^}", "{^ing}", "{^ed}", "{^s}", "{^er}", "{^ly}", "{>}", "{<}")]
suffix_strokes = [k for k, v in single if v.startswith("{^") and len(v) < 10]
specials = [(k, v) for k, v in single if v.startswith("{") and v not in [p[1] for p in punctuation]]


def pick():
    r = rng.random()
    if r < 0.45:
        return [rng.choice(words)[0][0]]
    if r < 0.60:
        return list(rng.choice(multi)[0])
    if r < 0.78:
        return [rng.choice(punctuation)[0][0]]
    if r < 0.86:
        return [rng.choice(specials)[0][0]]
    if r < 0.92:
        return [rng.choice(suffix_strokes)[0]]
    return ["*"]


def run(strokes, config):
    translator = Translator()
    translator.set_dictionary(StenoDictionaryCollection([commands, main]))
    formatter = Formatter()
    formatter.start_attached = config["start_attached"]
    formatter.set_space_placement("After Output" if config["spaces_after"] else "Before Output")

    class Out:
        def __init__(self):
            self.text = ""
            self.events = []

        def send_backspaces(self, n):
            self.text = self.text[: len(self.text) - n] if n <= len(self.text) else ""

        def send_string(self, s):
            self.text += s

        def send_key_combination(self, combo):
            self.events.append(["combo", combo])

        def send_engine_command(self, command):
            self.events.append(["command", command])

    output = Out()
    formatter.set_output(output)
    translator.add_listener(formatter.format)
    translator.set_min_undo_length(100)
    steps = []
    for s in strokes:
        translator.translate(Stroke.from_steno(s))
        steps.append([output.text, list(output.events)])
        output.events.clear()
    return steps


cases = []
skipped = 0
configs = [
    {"start_attached": True, "start_capitalized": False, "spaces_after": False},
    {"start_attached": False, "start_capitalized": False, "spaces_after": False},
    {"start_attached": True, "start_capitalized": False, "spaces_after": True},
]
for i in range(count):
    strokes = []
    for _ in range(rng.randint(3, 14)):
        strokes.extend(pick())
    config = configs[i % len(configs)]
    try:
        steps = run(strokes, config)
    except Exception:
        skipped += 1
        continue
    cases.append({"config": config, "strokes": strokes, "texts": [s[0] for s in steps], "events": [s[1] for s in steps]})
json.dump({"cases": cases}, open(out_path, "w"), ensure_ascii=False)
print(f"{len(cases)} cases, {skipped} skipped")
