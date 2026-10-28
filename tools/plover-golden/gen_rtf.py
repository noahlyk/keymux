"""Reference outputs for reading RTF/CRE dictionaries, from Plover's own parser.

Writes a JSON list of {"name", "rtf" (hex of the file's bytes), "entries" ([outline, translation])}:
files Plover itself writes from a dictionary that uses every operator of the translation
language, and hand-written files with the corners of the format.
"""

import json
import logging
import os
import sys
import tempfile

logging.disable(logging.CRITICAL)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from plover import system
from plover.registry import registry

registry.update()
system.setup("English Stenotype")
from plover.dictionary.rtfcre_dict import RtfDictionary
from plover.dictionary.rtfcre_parse import parse_rtfcre
from plover.steno import normalize_steno

import dicts

OUT = sys.argv[1]
cases = []


def add(name, raw: bytes):
    text = raw.decode("cp1252")
    try:
        entries = [[s, t] for s, t in parse_rtfcre(text)]
    except Exception as e:  # a file Plover refuses: keymux must refuse it too
        cases.append({"name": name, "rtf": raw.hex(), "error": True})
        return
    cases.append({"name": name, "rtf": raw.hex(), "entries": entries})


# Files Plover writes
everything = {stroke: translation for stroke, translation in dicts.MAIN.values()}
extras = {
    "TPH-PLTD": "café ñ 日本 😀",
    "TKPWAO": "line one\nline two\n\nparagraph",
    "TA*B": "col\tcol",
    "TPH-PL": "brace \\{ and \\} and back\\\\slash",
    "KAT/TKOG": "  leading and trailing  ",
    "KW-BG": "{,}",
    "TKO*PBT": "100% [sure] (really) #1 & more",
    "A*EU": "'",
    "STPH": "a b c",
    "STPH*": "{^ ^}",
}
everything.update(extras)
for chunk in range(0, len(everything), 40):
    items = list(everything.items())[chunk : chunk + 40]
    d = RtfDictionary()
    d.update({normalize_steno(k): v for k, v in items})
    with tempfile.NamedTemporaryFile(suffix=".rtf", delete=False) as fp:
        path = fp.name
    d._save(path)
    add(f"written by plover {chunk // 40}", open(path, "rb").read())
    os.unlink(path)

# Hand-written files
HEADER = b"{\\rtf1\\ansi{\\*\\cxrev100}\\cxdict{\\*\\cxsystem Test}{\\stylesheet{\\s0 Normal;}{\\s1 Contin;}}\r\n"
hand = {
    "simple": b"{\\rtf1\\ansi\\cxdict\r\n{\\*\\cxs KAT}cat\r\n{\\*\\cxs TKOG}dog\r\n}",
    "no body": b"{\\rtf1\\ansi\\cxdict\r\n}",
    "comments are skipped": HEADER + b"{\\*\\cxs KAT}cat{\\*\\cxcomment note}\r\n{\\*\\cxs TKOG}dog{\\*\\cxdictflags 5}\r\n}",
    "cxds forms": HEADER + b"{\\*\\cxs A}\\cxds ing\r\n{\\*\\cxs B}re\\cxds \r\n{\\*\\cxs C}\\cxds -\\cxds \r\n{\\*\\cxs D}\\cxds\r\n}",
    "punctuation groups": HEADER + b"{\\*\\cxs A}{\\cxp. }\r\n{\\*\\cxs B}{\\cxp, }\r\n{\\*\\cxs C}{\\cxp\\'}\r\n{\\*\\cxs D}{\\cxp-}\r\n{\\*\\cxs E}{\\cxp\"}\r\n}",
    "force case": HEADER + b"{\\*\\cxs A}\\cxfc word\r\n{\\*\\cxs B}\\cxfl Word\r\n}",
    "fingerspelling": HEADER + b"{\\*\\cxs A}{\\cxfing a}\r\n{\\*\\cxs B}\\cxfing b\r\n}",
    "undo stroke": HEADER + b"{\\*\\cxs A}\\cxdstroke\r\n}",
    "plover meta and macro": HEADER + b"{\\*\\cxs A}{{\\*\\cxplovermeta :stitch:A}}\r\n{\\*\\cxs B}{{\\*\\cxplovermacro repeat_last_stroke}}\r\n}",
    "stenovations flags": HEADER + b"{\\*\\cxs A}word{{\\*\\cxsvatdictflags N}}\r\n{\\*\\cxs B}word{{\\*\\cxsvatdictflags X}}\r\n}",
    "paragraphs and lines": HEADER + b"{\\*\\cxs A}one\\par two\\line three\\tab four\r\n{\\*\\cxs B}x\\\r\ny\r\n}",
    "styles": HEADER + b"{\\*\\cxs A}one\\par\\s1 two\r\n{\\*\\cxs B}three\\s0 four\r\n}",
    "escapes": HEADER + b"{\\*\\cxs A}\\{x\\} \\\\ \\-\\_\r\n}",
    "unicode": HEADER + b"{\\*\\cxs A}caf\\u233? x\\u-3913?y\r\n{\\*\\cxs B}{\\uc0\\u8364 }\r\n{\\*\\cxs C}\\u55357?\\u56832?\r\n{\\*\\cxs D}{\\uc2\\u8364 ab}c\r\n}",
    "cp1252 bytes": HEADER + b"{\\*\\cxs A}\x93quoted\x94 \x80 \xe9\r\n}",
    "number sign": HEADER + b"{\\*\\cxs #S}one\r\n{\\*\\cxs 1-9}x\r\n}",
    "missing close braces": HEADER + b"{\\*\\cxs KAT}cat\r\n{\\*\\cxs TKOG}dog",
    "new entry before the last closes": HEADER + b"{\\*\\cxs KAT}cat{\\*\\cxs TKOG}dog\r\n}",
    "extra close brace": HEADER + b"{\\*\\cxs KAT}cat}\r\n}",
    "leading and trailing spaces": HEADER + b"{\\*\\cxs A}   spaced   \r\n{\\*\\cxs B}.\r\n{\\*\\cxs C}. \r\n}",
    "empty translation": HEADER + b"{\\*\\cxs A}\r\n{\\*\\cxs B}x\r\n}",
    "unknown groups": HEADER + b"{\\*\\cxs A}a{\\foo bar}b{\\*\\bar baz}c\r\n}",
    "nested groups": HEADER + b"{\\*\\cxs A}a{b{c}d}e\r\n}",
    "space after control word": HEADER + b"{\\*\\cxs A}a\\par b\\par  c\r\n}",
}
for name, raw in hand.items():
    add(name, raw)
add("not rtf", b"{\"KAT\": \"cat\"}")
add("bad header", b"{\\rtf2 x}")

json.dump(cases, open(OUT, "w"), separators=(",", ":"))
print(f"{len(cases)} files")
