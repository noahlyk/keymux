"""Runs strokes through the real Plover translator and formatter, and records the text after each.

Writes src/steno/testdata/plover_golden.json: the dictionaries, and for each case the strokes and
the text and key/command events after every stroke. keymux's tests replay these and must match.
"""

import json
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import logging

logging.disable(logging.CRITICAL)
from plover import system
from plover.registry import registry

registry.update()
system.setup("English Stenotype")

import plover.formatting as formatting
from plover.formatting import Formatter
from plover.steno import Stroke, normalize_steno
from plover.steno_dictionary import StenoDictionary, StenoDictionaryCollection
from plover.translation import Translator

import dicts

class Recording(dict):
    """The word list, remembering which words were asked about."""

    def __init__(self, words):
        super().__init__(words)
        self.asked = set()

    def __contains__(self, word):
        self.asked.add(word)
        return super().__contains__(word)

    def __getitem__(self, word):
        self.asked.add(word)
        return super().__getitem__(word)


OUT_PATH = sys.argv[1]
WORDS_PATH = sys.argv[2]
WORDS = Recording(system.ORTHOGRAPHY_WORDS)
system.ORTHOGRAPHY_WORDS = WORDS
KEYMUX_LEFT = ["S-", "T-", "K-", "P-", "W-", "H-", "R-", "A-", "O-"]
KEYMUX_RIGHT = ["-E", "-U", "-F", "-R", "-P", "-B", "-L", "-G", "-T", "-S", "-D", "-Z"]


def keymux_raw(rtfcre):
    """How keymux writes a stroke that has no translation: STPHA-UBGS, always with the hyphen."""
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


class MemDict(StenoDictionary):
    def _load(self, filename):
        pass

    def _save(self, filename):
        pass


def make_dict(entries):
    d = MemDict()
    d.update({normalize_steno(k): v for k, v in entries.items()})
    return d


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


CONFIGS = {
    "default": {"start_attached": False, "start_capitalized": False, "spaces_after": False},
    "attached": {"start_attached": True, "start_capitalized": False, "spaces_after": False},
    "capitalized": {"start_attached": False, "start_capitalized": True, "spaces_after": False},
    "after": {"start_attached": False, "start_capitalized": False, "spaces_after": True},
}


def run(dict_sets, strokes, config):
    """dict_sets: list of {steno: translation}, highest priority first."""
    translator = Translator()
    translator.set_dictionary(StenoDictionaryCollection([make_dict(d) for d in dict_sets]))
    formatter = Formatter()
    formatter.start_attached = config["start_attached"]
    formatter.start_capitalized = config["start_capitalized"]
    formatter.set_space_placement("After Output" if config["spaces_after"] else "Before Output")
    out = Out()
    formatter.set_output(out)
    translator.add_listener(formatter.format)
    translator.set_min_undo_length(100)
    steps = []
    for s in strokes:
        translator.translate(Stroke.from_steno(s))
        steps.append([out.text, list(out.events)])
        out.events.clear()
    return steps


def entries(*tables):
    merged = {}
    for table in tables:
        for stroke, translation in table.values():
            merged[stroke] = translation
    return merged


MAIN = entries(dicts.MAIN)
USER = entries(dicts.USER)
COMMANDS = entries(dicts.COMMANDS)

SETS = {
    # No dictionary at all: every stroke is raw
    "empty": [],
    # Every operator, with `*` undoing
    "kitchen": [COMMANDS, MAIN],
    # Layered: the user's dictionary overrides and deletes
    "layered": [USER, COMMANDS, MAIN],
}


def stroke_of(name):
    return dicts.MAIN[name][0]


def strokes_for(*parts):
    """Names from MAIN become their strokes; anything else is a stroke already."""
    out = []
    for p in parts:
        out.extend((dicts.MAIN[p][0] if p in dicts.MAIN else p).split("/"))
    return out


FEATURE_CASES = []


def case(name, set_name, *parts):
    FEATURE_CASES.append((name, set_name, strokes_for(*parts)))


# --- spacing, attach, glue
case("plain words", "kitchen", "cat", "dog", "the")
case("suffix attaches", "kitchen", "cat", "s")
case("suffix ing orthography", "kitchen", "make", "ing")
case("suffix ed orthography", "kitchen", "carry", "ed")
case("suffix s ies", "kitchen", "carry", "s")
case("suffix s es", "kitchen", "fox", "s")
case("suffix es speech", "kitchen", "speech", "s")
case("suffix y to i", "kitchen", "beauty", "ful")
case("ie to y", "kitchen", "die", "ing")
case("consonant doubling", "kitchen", "stop", "ing")
case("doubling run", "kitchen", "run", "ing")
case("red ish raw", "kitchen", "red", "ish_raw")
case("red ish ortho", "kitchen", "red", "ish")
case("able ible", "kitchen", "make", "able")
case("ation", "kitchen", "make", "ation")
case("ly", "kitchen", "happy", "ly")
case("ize", "kitchen", "cat", "ize")
case("prefix attaches", "kitchen", "re", "port")
case("prefix with braces first", "kitchen", "re2", "port")
case("unattached prefix", "kitchen", "un", "make")
case("isn't", "kitchen", "is", "n't")
case("infix", "kitchen", "day", "infix", "day")
case("infix raw", "kitchen", "day", "infix2", "day")
case("infix carry", "kitchen", "day", "infix3", "day")
case("attach meta", "kitchen", "cat", "attach", "dog")
case("attach meta suffix", "kitchen", "cat", "attach_suf")
case("attach meta ortho", "kitchen", "make", "attach_ortho")
case("attach meta prefix", "kitchen", "attach_pre", "port")
case("attach meta infix", "kitchen", "day", "attach_infix", "day")
case("attach none", "kitchen", "cat", "attach_none", "dog")
case("attach space", "kitchen", "cat", "attach_space", "dog")
case("delete space", "kitchen", "cat", "delete_space", "dog")
case("force space", "kitchen", "break", "space", "fast")
case("force space meta", "kitchen", "break", "space2", "fast")
case("break fast", "kitchen", "break", "fast")
case("retro delete space", "kitchen", "basket", "ball", "retro_delete")
case("retro insert space", "kitchen", "persuasion", "retro_insert")
case("retro insert after two strokes", "kitchen", "perfect", "situation", "retro_insert")
case("persuasion", "kitchen", "perfect", "situation")
case("retro delete =", "kitchen", "basket", "ball", "retro_delete2")
case("retro insert =", "kitchen", "perfect", "situation", "retro_insert2")
case("retro delete deprecated", "kitchen", "basket", "ball", "retro_delete3")
case("retro insert deprecated", "kitchen", "perfect", "situation", "retro_insert3")
case("retro delete with one translation", "kitchen", "retro_delete")
case("retro insert with none", "kitchen", "retro_insert")
case("retro delete then undo", "kitchen", "basket", "ball", "retro_delete", "*")
case("retro insert then undo", "kitchen", "perfect", "situation", "retro_insert", "*")
case("glue letters", "kitchen", "fs_a", "fs_b", "fs_c")
case("glue after word", "kitchen", "cat", "fs_a", "fs_b", "dog")
case("glue meta", "kitchen", "glue_meta", "fs_a", "glue_meta")
case("glue upper", "kitchen", "fs_C", "fs_a")
case("digits glue", "kitchen", "digits", "digit_word")
case("raw number strokes", "kitchen", "1", "2", "3")
case("raw number after word", "kitchen", "cat", "1", "2", "dog")
case("number stroke with hyphen", "kitchen", "-6", "-7")
case("numbers then letters", "kitchen", "12", "fs_a")
case("fingerspell caps", "kitchen", "cap", "fs_a", "fs_b")
# --- whitespace
case("newline escape", "kitchen", "newline")
case("tab escape", "kitchen", "tab")
case("newline alone", "kitchen", "cat", "nl_only", "dog")
case("newline attached", "kitchen", "cat", "nl_pre", "dog")
# --- case
case("cap next", "kitchen", "cap", "cat", "dog")
case("cap attach", "kitchen", "cat", "cap_attach", "dog")
case("cap meta", "kitchen", "cap_meta", "cat")
case("lower next", "kitchen", "cap", "lower", "cat")
case("lower meta", "kitchen", "cap", "lower_meta", "cat")
case("upper next", "kitchen", "upper", "cat", "dog")
case("upper meta", "kitchen", "upper_meta", "cat", "dog")
case("upper next with attach", "kitchen", "upper", "re", "port", "dog")
case("upper carries across attach", "kitchen", "upper", "cat", "ing")
case("retro cap", "kitchen", "cat", "retro_cap", "ville")
case("retro cap meta", "kitchen", "cat", "retro_cap_meta")
case("retro lower", "kitchen", "cap", "cat", "retro_lower")
case("retro lower meta", "kitchen", "cap", "cat", "retro_lower_meta")
case("retro upper", "kitchen", "cat", "retro_upper")
case("retro upper meta", "kitchen", "cat", "dog", "retro_upper_meta")
case("retro upper multi word", "kitchen", "multi_word", "retro_upper")
case("retro on nothing", "kitchen", "retro_cap")
case("retro cap then undo", "kitchen", "cat", "retro_cap", "*")
case("retro upper then undo", "kitchen", "cat", "retro_upper", "*", "*")
# --- carry capitalization
case("open quote then cap", "kitchen", "cap", "open_quote", "cat")
case("open quote plain", "kitchen", "open_quote", "cat", "close_quote")
case("close quote", "kitchen", "cat", "close_quote", "dog")
case("quote after period", "kitchen", "cat", "period", "open_quote", "dog")
case("carry both", "kitchen", "cat", "carry_both", "dog")
case("carry plain", "kitchen", "cap", "carry_plain", "cat")
case("open paren", "kitchen", "period", "open_paren", "cat")
case("til carry", "kitchen", "period", "til", "cat")
# --- modes
case("mode caps", "kitchen", "mode_caps", "cat", "dog")
case("mode title", "kitchen", "mode_title", "cat", "dog", "multi_word")
case("mode lower", "kitchen", "cap", "mode_lower", "cat", "caps_word")
case("mode camel", "kitchen", "mode_camel", "cat", "dog", "the")
case("mode snake", "kitchen", "mode_snake", "cat", "dog", "multi_word")
case("mode reset case", "kitchen", "mode_caps", "cat", "mode_reset_case", "dog")
case("mode reset space", "kitchen", "mode_snake", "cat", "mode_reset_space", "dog")
case("mode reset", "kitchen", "mode_camel", "cat", "mode_reset", "dog")
case("mode set space", "kitchen", "mode_set_space", "cat", "dog")
case("mode set space empty", "kitchen", "mode_set_space_empty", "cat", "dog")
case("mode set space multi", "kitchen", "mode_set_space_multi", "cat", "dog")
case("mode caps with suffix", "kitchen", "mode_caps", "make", "ing")
case("mode title with prefix", "kitchen", "mode_title", "re", "port", "dog")
case("mode camel then cap", "kitchen", "mode_camel", "cap", "cat", "dog")
case("mode caps undo", "kitchen", "mode_caps", "cat", "*", "dog")
# --- punctuation
case("period capitalizes", "kitchen", "cat", "period", "dog")
case("comma", "kitchen", "cat", "comma", "dog")
case("question", "kitchen", "cat", "question", "dog")
case("exclaim", "kitchen", "cat", "exclaim", "dog")
case("colon", "kitchen", "cat", "colon", "dog")
case("semicolon", "kitchen", "cat", "semicolon", "dog")
case("stop meta", "kitchen", "cat", "stop_meta", "dog")
case("comma meta", "kitchen", "cat", "comma_meta", "dog")
case("period undo", "kitchen", "cat", "period", "dog", "*", "*", "dog")
case("dash", "kitchen", "cat", "dash", "dog")
case("ellipsis", "kitchen", "cat", "ellipsis", "dog")
case("apostrophe", "kitchen", "cat", "apos", "s")
case("period first", "kitchen", "period", "cat")
case("two periods", "kitchen", "cat", "period", "period", "dog")
# --- currency, lookahead
case("dollars", "kitchen", "price", "dollars")
case("dollars int", "kitchen", "price_int", "dollars")
case("dollars meta", "kitchen", "price", "dollars_meta")
case("dollars from digits", "kitchen", "1", "2", "3", "4", "dollars")
case("yen", "kitchen", "number_word", "yen")
case("dollars on a word", "kitchen", "cat", "dollars")
case("dollars with nothing", "kitchen", "dollars")
case("a an vowel", "kitchen", "a_an", "example", "dog")
case("a an consonant", "kitchen", "a_an", "dog")
case("a an updates", "kitchen", "a_an", "cat")
case("a an meta", "kitchen", "a_an_meta", "example")
case("a an then undo", "kitchen", "a_an", "example", "*", "dog")
case("a an at end", "kitchen", "a_an")
case("a an twice", "kitchen", "a_an", "a_an", "example")
case("next digit", "kitchen", "next_digit", "digits")
case("next digit no", "kitchen", "next_digit", "cat")
case("next escape slash", "kitchen", "next_escape", "fs_a")
case("next word start", "kitchen", "next_word", "cat")
# --- misc actions
case("empty braces then cap", "kitchen", "cap", "empty", "cat")
case("cancel", "kitchen", "cat", "cancel", "dog")
case("cancel then undo", "kitchen", "cat", "cancel", "*", "*")
case("word end", "kitchen", "con", "word_end", "word")
case("word end meta", "kitchen", "con", "word_end_meta", "word")
case("toggle asterisk", "kitchen", "cat", "toggle_ast")
case("toggle asterisk off", "kitchen", "cat", "toggle_ast2")
case("toggle asterisk on nothing", "kitchen", "toggle_ast")
case("repeat last stroke", "kitchen", "cat", "repeat", "repeat")
case("repeat last stroke =", "kitchen", "cat", "repeat2")
case("repeat on nothing", "kitchen", "repeat")
case("undo entry", "kitchen", "cat", "dog", "undo_entry", "the")
case("undo basic", "kitchen", "cat", "dog", "*")
case("undo to empty", "kitchen", "cat", "*", "*", "*")
case("undo multi stroke", "kitchen", "perfect", "situation", "*")
case("undo multi stroke twice", "kitchen", "perfect", "situation", "*", "*")
case("undo suffix", "kitchen", "make", "ing", "*")
case("undo prefix", "kitchen", "re", "port", "*", "*")
case("undo glue", "kitchen", "fs_a", "fs_b", "*", "fs_c")
case("undo cap", "kitchen", "cap", "*", "cat")
# --- word boundary
case("word boundary prefix", "kitchen", "wb_word")
case("word boundary after word", "kitchen", "cat", "wb_word")
case("word boundary after unfinished", "kitchen", "con", "wb_word")
case("word boundary after attach", "kitchen", "re", "wb_word")
# --- key combos
case("return", "kitchen", "cat", "return", "dog")
case("left", "kitchen", "cat", "left", "dog")
case("ctrl a", "kitchen", "cat", "ctrl_a")
case("ctrl shift z", "kitchen", "ctrl_shift_z")
case("alt tab", "kitchen", "alt_tab")
case("two keys", "kitchen", "two_keys")
case("f5", "kitchen", "f5")
case("super", "kitchen", "super_l")
case("case insensitive combo", "kitchen", "case_combo")
case("named key plus", "kitchen", "named_key")
case("text then key", "kitchen", "text_then_key", "dog")
case("key then text", "kitchen", "key_then_text", "dog")
case("backspace key", "kitchen", "cat", "backspace_key")
case("combo then undo", "kitchen", "cat", "left", "*", "*")
case("combo keeps cap pending", "kitchen", "period", "left", "cat")
# --- Lapwing's plugins
case("stitch letters", "kitchen", "stitch_a", "stitch_b", "stitch_c")
case("stitch with delimiter", "kitchen", "stitch_a", "stitch_tilde", "stitch_b")
case("stitch after a word", "kitchen", "cat", "stitch_a", "stitch_b")
case("stitch last word", "kitchen", "cat", "stitch_last")
case("stitch last two words", "kitchen", "cat", "dog", "stitch_last2")
case("stitch last word default", "kitchen", "cat", "stitch_last_default")
case("stitch last word of nothing", "kitchen", "stitch_last")
case("stitch then undo", "kitchen", "cat", "stitch_last", "*")
case("repeat translation", "kitchen", "cat", "repeat_t")
case("repeat two translations", "kitchen", "cat", "dog", "repeat_t2")
case("repeat zero translations", "kitchen", "cat", "dog", "repeat_zero")
case("repeat translation after suffix", "kitchen", "make", "ing", "repeat_t")
case("repeat last word", "kitchen", "cat", "dog", "repeat_w")
case("repeat last two words", "kitchen", "perfect", "situation", "repeat_w2")
case("repeat last fragment", "kitchen", "cat", "dog", "repeat_f")
case("repeat last characters", "kitchen", "cat", "dog", "repeat_c")
case("repeat on nothing", "kitchen", "repeat_t", "repeat_w", "repeat_f", "repeat_c")
case("repeat then undo", "kitchen", "cat", "repeat_w", "*")
# --- commands
for name in ("suspend", "resume", "toggle", "add", "lookup", "suggest", "config", "focus", "quit", "set_config", "upper"):
    case(f"command {name}", "kitchen", "cat", f"cmd_{name}", "dog")
# --- literal text and escapes
case("literal left brace", "kitchen", "lbrace")
case("literal braces", "kitchen", "braces")
case("attach literal brace", "kitchen", "cat", "attach_brace")
case("text with attach inside", "kitchen", "cat", "double_atom")
case("padded text", "kitchen", "cat", "spaces_around", "dog")
case("multi atom", "kitchen", "cat", "multi_atom", "dog")
case("multi word", "kitchen", "cat", "multi_word", "dog")
case("unicode", "kitchen", "unicode", "cat")
case("dollar sign", "kitchen", "dollar", "cat")
case("dot com", "kitchen", "cat", "dot_com")
case("caps word then suffix", "kitchen", "caps_word", "s")
case("title word", "kitchen", "title_word", "s")
# --- translation behaviours
case("multi stroke prefix then complete", "kitchen", "example")
case("multi stroke partial", "kitchen", "KPA")
case("multi stroke partial then other", "kitchen", "KPA", "cat")
case("partial then undo", "kitchen", "cat", "KPA", "*")
case("catalog", "kitchen", "cat", "KAT/HROG".split("/")[1] if False else "HROG")
case("catalog full", "kitchen", "KAT", "HROG")
case("catalog three strokes", "kitchen", "KA", "TA", "HROG")
case("catalog then undo", "kitchen", "KAT", "HROG", "*")
case("catalog partial undo", "kitchen", "KA", "TA", "*", "HROG")
case("suffix folding s", "kitchen", "KATS")
case("suffix folding g", "kitchen", "KAGT")
case("suffix folding d", "kitchen", "KATD")
case("suffix folding z", "kitchen", "KATZ")
case("suffix folding two", "kitchen", "KATDZ")
case("suffix folding none", "kitchen", "TKOGS")
case("suffix folding multi", "kitchen", "KPA", "TKAOEUS")
case("suffix folding then undo", "kitchen", "KATS", "*")
case("suffix folding after word", "kitchen", "dog", "KATS", "the")
case("unmatched stroke", "kitchen", "TPHEUFR")
case("unmatched then word", "kitchen", "TPHEUFR", "cat")
case("unmatched between", "kitchen", "cat", "TPHEUFR", "dog")
case("unmatched undo", "kitchen", "cat", "TPHEUFR", "*", "dog")
case("unmatched left right", "kitchen", "STPHAUBGSZ")
case("unmatched star", "kitchen", "*EUPBS")
case("unmatched number key", "kitchen", "#T")
case("unmatched left only", "kitchen", "STKPWHR")
case("unmatched right only", "kitchen", "-FRPBLGTSDZ")
case("unmatched vowels", "kitchen", "AOEU")
case("raw digits then cap", "kitchen", "1", "cap", "cat")
case("undo on empty", "kitchen", "*")
case("undo on empty twice", "kitchen", "*", "*")
case("undo after raw", "kitchen", "TPHEUFR", "*")
case("empty dictionary raw", "empty", "KAT", "TKOG", "*", "-T")
case("empty dictionary numbers", "empty", "1", "2", "-8", "KAT")
# --- layered dictionaries
case("override", "layered", "KAT")
case("deleted entry is untranslated", "layered", "TKOG")
case("deleted entry upper case marker", "layered", "-T")
case("deleted does not hide others", "layered", "cat", "mine", "the")
case("deleted then suffix fold", "layered", "TKOGS")

SPECIAL_RAW = ["TPHEUFR", "STPHAUBGSZ", "KPA", "HROG", "*EUPBS", "-FRPBLGTSDZ"]


def pool(set_name):
    """Strokes the fuzz picks from: every entry's stroke, plus raw and suffix-folded ones."""
    entries_ = {}
    for table in SETS[set_name]:
        entries_.update(table)
    strokes = list(entries_)
    # Multi-stroke outlines contribute their strokes one by one so the fuzz can complete them
    parts = []
    for s in strokes:
        parts.extend(part for part in s.split("/") if part)
    extra = SPECIAL_RAW + ["KATS", "KAGT", "TKOGS", "1", "2", "12", "-6", "#T", "KAT", "HROG", "TKAOEUS"]
    return sorted(set(parts + extra)) + ["*"] * 60


def fuzz(set_name, count, seed, longest):
    rng = random.Random(seed)
    choices = pool(set_name)
    cases = []
    for i in range(count):
        n = rng.randint(1, longest)
        strokes = [rng.choice(choices) for _ in range(n)]
        cases.append((f"fuzz {set_name} {i}", set_name, strokes))
    return cases


ALL = []
for name, set_name, strokes in FEATURE_CASES:
    for config_name in CONFIGS:
        ALL.append((name if config_name == "default" else f"{name} [{config_name}]", set_name, strokes, config_name))
for name, set_name, strokes in fuzz("kitchen", 2200, 7, 9) + fuzz("layered", 400, 11, 7) + fuzz("empty", 80, 13, 6):
    ALL.append((name, set_name, strokes, "default"))
for name, set_name, strokes in fuzz("kitchen", 500, 17, 8):
    ALL.append((name + " [after]", set_name, strokes, "after"))
for name, set_name, strokes in fuzz("kitchen", 250, 19, 8):
    ALL.append((name + " [attached]", set_name, strokes, "attached"))

result = {
    "sets": {
        name: [{k: v for k, v in table.items()} for table in tables]
        for name, tables in SETS.items()
    },
    "cases": [],
}
bad = 0
for name, set_name, strokes, config_name in ALL:
    try:
        steps = run(SETS[set_name], strokes, CONFIGS[config_name])
    except Exception as e:  # a case Plover itself can't run is a mistake in the case
        bad += 1
        print("SKIPPED", name, strokes, repr(e), file=sys.stderr)
        continue
    result["cases"].append(
        {
            "name": name,
            "set": set_name,
            "config": CONFIGS[config_name],
            "strokes": strokes,
            "texts": [s[0] for s in steps],
            "events": [s[1] for s in steps],
        }
    )

with open(OUT_PATH, "w", encoding="utf-8") as fp:
    json.dump(result, fp, ensure_ascii=False, separators=(",", ":"))
with open(WORDS_PATH, "w", encoding="utf-8") as fp:
    for word in sorted(WORDS.asked):
        if dict.__contains__(WORDS, word):
            fp.write(f"{word} {dict.__getitem__(WORDS, word)}\n")
print(f"wrote {len(result['cases'])} cases ({bad} skipped) to {OUT_PATH}")
