"""Reference outputs for attaching a suffix to a word, from Plover's own orthography code.

Writes two files: the cases (word, suffix, result), and the part of Plover's word list those
cases looked at, so keymux's tests need only a few KB instead of the 4 MB list.
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
from plover.orthography import add_suffix


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


OUT_CASES, OUT_WORDS = sys.argv[1], sys.argv[2]
real_words = dict(system.ORTHOGRAPHY_WORDS)
rng = random.Random(99)

PICKED = """artistic humble questionable triple statute confirm supervise frequent establish speech cherry die metallurgy
beauty write Minnesota ceremony spaghetti pretty fantastic epistomology oratory radical complementary disproportional perform
restore token conditional spectacular category custom fantasy criminal similar free narrate defer make stop carry red lie agree
sit run happy try fox box carry cat dog fast basket ball perfect situation break day word mine hello world go do see be
play stay city lady baby copy story study marry bury fly cry apply deny rely supply reply
nice love have come give move live use make take like hope ride write bite hate state create
fat hot big sad run swim win begin forget regret refer prefer occur admit commit permit submit
picnic panic mimic traffic politic magic logic tragic comic
sharp calm brown sun moon star light dark night day""".split()
SUFFIXES = """ing ed s es er est ly ish able ible ability ibility ation ize ise ity ive ist ical ial ify ification ness
ment ful y en tion ry tory ary cy ologist ization ified ier iest ally ably ery ory ist ists ism isms ize izes ized izing
ality ilities ties ies ied age al ance ant ar ate ation ative ic ical ish less like ous ious ual ure ward wise y
ing-ed s's er's 's n't ing-in ee eer eers""".split()

# A random sample of real words, so the rules meet the words people actually use
rank_sorted = sorted(real_words, key=lambda w: real_words[w])
sample = rng.sample(rank_sorted[:6000], 260) + rng.sample(rank_sorted[6000:60000], 140)
all_words = PICKED + sample + [w.capitalize() for w in rng.sample(PICKED, 25)] + [w.upper() for w in rng.sample(PICKED, 10)]

words = Recording(real_words)
system.ORTHOGRAPHY_WORDS = words
cases = []
for word in all_words:
    for suffix in rng.sample(SUFFIXES, 5):
        cases.append([word, suffix, add_suffix(word, suffix)])
# Every picked word with every suffix, since those are the interesting ones
for word in PICKED[:60]:
    for suffix in SUFFIXES:
        cases.append([word, suffix, add_suffix(word, suffix)])
# Suffixes with something after a space, which is kept as it is
for word, suffix in [("make", "ing the"), ("carry", "s and"), ("cat", "s "), ("stop", "ed  x")]:
    cases.append([word, suffix, add_suffix(word, suffix)])
# Odd words
for word, suffix in [("", "ing"), ("a", "ing"), ("e", "ed"), ("y", "s"), ("ie", "ing"), ("ch", "s"), ("qu", "ing"), ("x", "s")]:
    cases.append([word, suffix, add_suffix(word, suffix)])

json.dump(cases, open(OUT_CASES, "w", encoding="utf-8"), ensure_ascii=False, separators=(",", ":"))
with open(OUT_WORDS, "w", encoding="utf-8") as fp:
    for word in sorted(words.asked):
        if dict.__contains__(words, word):
            fp.write(f"{word} {dict.__getitem__(words, word)}\n")
print(f"{len(cases)} cases; {len(words.asked)} words asked about, {sum(1 for w in words.asked if dict.__contains__(words, w))} in the list")
