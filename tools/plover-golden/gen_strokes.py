"""Reference outputs for steno stroke notation, from Plover's own stroke helper.

render: a set of keys and how Plover writes it. parse: a spelling and what Plover reads it as
(one canonical string per stroke), or null when Plover rejects it.
"""

import json
import logging
import random
import sys

logging.disable(logging.CRITICAL)
from plover import system
from plover.registry import registry

registry.update()
system.setup("English Stenotype")
from plover.steno import Stroke, normalize_steno

OUT = sys.argv[1]
KEYS = list(system.KEYS)
rng = random.Random(2026)

render = []
seen = set()


def add_keys(keys):
    keys = [k for k in KEYS if k in set(keys)]
    steno = Stroke.from_keys(keys).rtfcre
    if steno not in seen:
        seen.add(steno)
        render.append([keys, steno])


# Every single key and every pair
for a in KEYS:
    add_keys([a])
    for b in KEYS:
        add_keys([a, b])
# The number key with every subset of the number-capable keys, with and without extra keys
numberish = ["S-", "T-", "P-", "H-", "A-", "O-", "-F", "-P", "-L", "-T"]
others = ["K-", "W-", "R-", "*", "-E", "-U", "-R", "-B", "-G", "-S", "-D", "-Z"]
for mask in range(1 << len(numberish)):
    base = ["#"] + [k for i, k in enumerate(numberish) if mask >> i & 1]
    add_keys(base)
    for _ in range(3):
        add_keys(base + rng.sample(others, rng.randint(1, 4)))
# Random sets
for _ in range(2000):
    add_keys(rng.sample(KEYS, rng.randint(1, 12)))

ALPHABET = "STKPWHRAO*EUFRPBLGTSDZ#-0123456789"
parse = []
seen_parse = set()


def add_parse(text):
    if text in seen_parse:
        return
    seen_parse.add(text)
    try:
        parse.append([text, list(normalize_steno(text))])
    except ValueError:
        parse.append([text, None])


for _, steno in render:
    add_parse(steno)
    add_parse(steno.lower())
for text in [
    "1", "2", "5", "0", "-6", "-9", "-0", "1-", "12", "123", "1234", "12K", "1K", "K1", "#1", "#S", "#SK", "#K", "1-9", "15",
    "1590", "0-9", "-69", "S1", "1S", "#1-", "1T", "S2", "123-6", "1-6", "50", "05", "5-6", "A5", "5A", "#5", "5E", "E5", "-5",
    "#-6", "#-F", "6", "9", "#-9", "-FR", "FR", "-R", "R-", "-RPBLGTSDZ", "STKPWHRAO*EUFRPBLGTSDZ", "S-T", "*", "-*", "*-", "A-",
    "-A", "KAT", "KAT/HROG", "KA-T", "-T", "T-", "T", "AOE", "A-E", "O-E", "-EU", "EU", "AEU", "AO-EU", "#A", "1-2", "1-0",
    "12-0", "-1", "-12", "-89", "##", "#T#", "SS", "TS", "ST", "TT", "kat", "Kat", " KAT", "KAT ", "", "-", "#-", "/", "/KAT",
    "KAT/", "KAT//HROG", "#", "#*", "#-S", "1-S", "1*", "S-P", "TK-LS", "-FPLT", "TP-PL", "KW-BG", "STPH-FPLT", "STPH*FPLT",
    "#*E", "R*PT", "/S", "/", "KAT/HROG/TKOG", "1/2", "12/-6", "#/#", "*/*",
]:
    add_parse(text)
# Random spellings, mostly invalid: the parser must reject exactly what Plover rejects
for _ in range(5000):
    n = rng.randint(1, 8)
    add_parse("".join(rng.choice(ALPHABET) for _ in range(n)))
# Random spellings that are valid, built from Plover's own output with a hyphen or digit changed
for _, steno in rng.sample(render, 500):
    mutated = steno.replace("-", "") if rng.random() < 0.5 else steno
    add_parse(mutated)
    add_parse("/".join([steno, rng.choice(render)[1]]))

accepted = sum(1 for _, r in parse if r is not None)
json.dump({"render": render, "parse": parse}, open(OUT, "w"), separators=(",", ":"))
print(f"{len(render)} render cases, {len(parse)} parse cases ({accepted} accepted)")
