#!/usr/bin/env python3
"""The prose that ships is held to the rules the style guide states.

STYLE.md Part 6 is the half of that guide a script can enforce, and the default mode of
this script enforces it. It reads the prose a stranger reads and fails on the rules that
have one answer:

    Rule 26   a sentence over the word limit, a paragraph over the sentence limit
    Rule 27   a list without a colon lead-in, with a lowercase item, with a trailing
              comma or semicolon, or nested
    Rule 16   a semicolon, in prose or in a table cell
    Rule 15   `shall` or `may`, and a term the profile rules out

Two more are held only once a project sets their threshold. Each rests on a number the
guide states as a default rather than as a fact. Rule 24 turns a long enough run of
parallel items into a list, and Rule 18 makes a repeated bold lead-in a heading. Both are
off at 0, and `--census` prints what either would cost at every threshold worth choosing
between.

Rule 26's counting table is what makes the length rule fair. Five things each count as one
word: backticked code, a parenthetical, a hyphenated word, a number with its unit, and a
bold label. An identifier never costs a sentence its budget.

Four things are reported and not failed, because each needs a reader. An em dash is
Rule 16's "zero, or justified" and only a person judges the second half. `would`, `could`
and `might` are the modals Rule 15 exempts inside explanatory prose. `should` is permitted
in one sense, a recommendation with known exceptions, and each use is listed so a reader
can check that sense. A mid-sentence `if` is Rule 28's condition-before-command, which reads
the same as an embedded question. Each is reported with its line under `--soft`, because a
count with no location is a number a reader cannot act on.

The voice mode (`--voice`) finds candidates for rules in Parts 1 to 5:

- Process residue and internal references in the reading path
- Self-assessment
- House words
- Sentence headings
- Rebuttal markers
- Chained openers
- Bold sentences standing in for a hazard
- The character conventions
- A document's skeleton
- An erratum's fields

The reader-cost families find what passes every count and still costs the reader:

- A phrase repeated down a page
- A claim made in a second place
- One quantity with several readings
- Measurement method in the reading path
- A correction nobody asked for
- A contrast joined by "and"
- A term coined in passing
- A sentence that opens on its predicate
- A ratio without its baseline
- A forward reference
- Re-measure debt
- A table row whose figures carry footnote marks
- A column that repeats one figure
- A section too long to scan

The voice mode knows where a document's record is, from the profile's record zones, and
holds the residue checks to the reading path alone. Each family runs as `hard`, `ratchet`,
`report` or `off`, as the configuration sets it. A page can therefore be held on one family
before it is clean on another.

A surface is a Markdown page or the doc comments in a source file, and the rules do not
change between them. A doc comment is prose a stranger reads and it ships with the code.
On a library it is usually the larger of the two surfaces.

`surfaces.doc_comments` names the files and the dialect to extract them with. `rust` takes
`///` and `//!`, `go` the comment run above a declaration, `python` a docstring, and
`block` the `/** ... */` shape JSDoc and kernel-doc share. `c` takes every comment in a C or
C++ source, for a header whose comments are its contract whatever their marker. `line` takes
any opener the configuration lists, for a language with no dialect here. Setting
`visibility` to `public` holds the comments on the API surface alone, which is the class
STYLE.md separates.

Extraction blanks what it does not take rather than dropping it, so every report still
names the line the reader opens the file at. The `c` dialect sets each comment off as its
own paragraph, and reads an indented formula as a code block. It leaves a license line out
under Rule 29. Its findings name the source line too.

Two further duties are the profile's rather than the prose's. A configured profile the
gate cannot read stops the run. Every part of that contract fails the same quiet way, and
each leaves the prose held to a weaker rule than the project asked for. Where a project
keeps an answered-questions table, the gate checks every answer it names. A rename
therefore cannot leave a row pointing at nothing. That table and the
fixed-terms table each live in the profile unless `answers.path` or `terms.path` names the
document that already owns it.

Everything this script needs to know about a particular project comes from a TOML
configuration file and the profile it names. The script itself is therefore portable. See
`prose-lint.toml` for the schema and `ADOPTING.md` for the order to introduce it in.

    prose-lint.py --config PATH                  hold every surface the ratchet covers
    prose-lint.py --config PATH --all            ignore the ratchet and report every surface
    prose-lint.py --config PATH --soft           also locate what is reported and not failed
    prose-lint.py --config PATH --quiet          list only the pending surfaces that moved
    prose-lint.py --config PATH --census         measure this corpus, rather than reading one
    prose-lint.py --config PATH --baseline       write a ratchet recording where each stands
    prose-lint.py --config PATH --self-test      assert the contracts the checks rest on
    prose-lint.py --config PATH --voice [...]    the same forms, over the voice checks
    prose-lint.py --config PATH [--voice] FILE   check the named files instead of the surfaces

With `--voice`, `--detail FAMILY[,FAMILY]` prints every finding of the named families
(`--detail all` prints every family), and `--families FAMILY[,FAMILY]` runs only those.

Requires Python 3.11 or newer, for `tomllib` in the standard library.
"""

from __future__ import annotations

import posixpath
import re
import sys
import tomllib
import unicodedata
from collections import Counter
from dataclasses import dataclass, field
from fnmatch import fnmatch
from pathlib import Path

# Rule 15's modal ladder gives each modal one sense. `shall` and `may` are not used,
# `should` is a recommendation with known exceptions, and the last three are exempt inside
# a counterfactual. The sense and the exemption are properties of the use rather than of
# the word, which no script reads, so the split below is a heuristic. It is the default
# rather than a fact. A project with a permission model writes `may` for permission, and
# one writing to RFC 2119 defines it, so `[limits]` is where each moves the word. A gated
# modal fails, and the other two lists are reported for a reader to check the sense.
DEFAULT_GATED_MODALS = ["shall", "may"]
DEFAULT_RECOMMENDATION_MODALS = ["should"]
DEFAULT_LOOSE_MODALS = ["would", "could", "might"]

# Rule 16's em dash, and the ASCII pair that renders as one. A gate that reports only the
# character leaves the pair as a silent way past the rule, and the pair hides a sentence
# boundary exactly as the character does. Neither fires inside a `prose-lint:` marker,
# whose own syntax is `<!-- prose-lint: off -- reason -->`.
EM_DASH = re.compile(r"—|(?<= )--(?= )")

# An HTML character reference ends in a semicolon that is not punctuation. A table cell
# escapes its own pipe as `&#124;`, which is the one way to write one in a cell at all.
ENTITY = re.compile(r"&(?:#[0-9]+|#[xX][0-9A-Fa-f]+|[A-Za-z][A-Za-z0-9]*);")

# Rule 15 bans emoji outright, which is the one clause in the guide with no judgment in
# it, so this fails rather than reports. It is the pictographic blocks and nothing else.
# An arrow and a mathematical sign are typography, and Rule 16's character table is what
# governs those. The four marks below are excluded because a table uses them as a value
# rather than as a picture in a sentence, and no other reading of them exists.
EMOJI = re.compile(
    "(?![\u2713\u2714\u2717\u2718])"
    "[\U0001F000-\U0001FAFF\u2600-\u27BF\u2B50\uFE0F]"
)

# Rule 15's American spelling, as the pairs a technical corpus actually reaches for. The
# list is short on purpose: every entry is a word whose British form has no other reading,
# so a match is a spelling rather than a judgment call. `[spelling]` replaces it.
DEFAULT_SPELLINGS = {
    "judgement": "judgment", "licence": "license", "honour": "honor",
    "honoured": "honored", "honours": "honors", "behaviour": "behavior",
    "behaviours": "behaviors", "colour": "color", "colours": "colors",
    "analyse": "analyze", "analysed": "analyzed", "analyses": "analyzes",
    "organise": "organize", "organised": "organized", "organisation": "organization",
    "recognise": "recognize", "recognised": "recognized",
    "normalise": "normalize", "normalised": "normalized",
    "initialise": "initialize", "initialised": "initialized",
    "serialise": "serialize", "serialised": "serialized",
    "centre": "center", "centred": "centered", "defence": "defense",
    "catalogue": "catalog", "dialogue": "dialog", "programme": "program",
    "travelled": "traveled", "labelled": "labeled", "modelling": "modeling",
    "cancelled": "canceled", "fulfil": "fulfill", "enrol": "enroll",
    "artefact": "artifact", "artefacts": "artifacts", "grey": "gray",
    "honouring": "honoring", "favour": "favor", "favours": "favors",
}
LIST_MARKER = re.compile(r"^(\s*)(?:[-*+]|\d+\.)\s+(.*)$")
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$")

# A link reference definition is addressing rather than prose, and it cannot interrupt a
# paragraph, so it is only one where a block is not already running. Rustdoc's intra-doc
# links and Go's doc links both take this shape, and a file that resolves twenty of them
# at the foot of a comment would otherwise read as twenty one-line paragraphs.
LINK_DEFINITION = re.compile(r"^\s*\[[^\]]+\]:\s*\S")

# A field in a doc comment: kernel-doc's `@name:`, JSDoc's and Doxygen's `@param`, the
# backslash spelling of the same, and Sphinx's `:param name:`. A field list is a list, and
# an item of one opens a block exactly as a bullet does. Without this the fields join the
# summary above them into one sentence, which kernel-doc hits on every comment it has,
# because its format puts the first field directly under the summary with no blank line.
FIELD_MARKER = re.compile(r"^\s*(?:[@\\]\w+|:\w[\w ]*:)")

# The stand-in every backticked run is replaced by before sentences are split. It is
# lowercase on purpose, and it is never configurable: the splitter below refuses to end a
# sentence after a lone capital, so an uppercase stand-in would make every sentence that
# ends in a code span unsplittable.
CODE_STANDIN = "code"

DEFAULT_UNITS = [
    "TiB", "GiB", "MiB", "KiB", "TB", "GB", "MB", "KB", "B",
    "bit", "bits", "byte", "bytes", "ms", "s", "MHz", "GHz",
]

# The voice checks, each with the rule it serves and the zone it reads. A `path` family
# reads the reading path only, because the same words are the record's business: a run
# count in an Evidence section is evidence, and in the body it is residue. An `all` family
# reads the whole page.
FAMILIES = {
    "internal": ("Rule 7", "internal reference", "all"),
    "agent": ("Rule 28", "the project or its authors as agent", "path"),
    "residue": ("Rule 7", "process residue", "path"),
    "history": ("Rule 6", "the path to the fact", "path"),
    "self_assessment": ("Rule 15", "self-assessment", "all"),
    "bold_hazard": ("Rule 4", "a bold sentence standing in for a hazard", "path"),
    "heading": ("Rule 18", "a heading that is a sentence or a verdict", "all"),
    "rebuttal": ("Rule 10", "a rebuttal candidate", "path"),
    "chain": ("Rule 26", "a chained opener", "path"),
    "house_word": ("Rule 2", "a house word", "all"),
    "characters": ("Rule 16", "the character conventions", "all"),
    "first_paragraph": ("Rule 7", "residue in the first paragraph", "all"),
    "skeleton": ("Rule 3", "the class skeleton", "all"),
    "erratum": ("Rule 4", "an erratum entry's form", "all"),
    "define": ("Rule 3", "a glossary term used before its definition", "all"),
    "glossary": ("Rule 2", "a glossary entry's form", "all"),
    # The reader-cost families. Each finds a pattern that passes every count above and still
    # costs the reader something: a fact to reconcile, a word to look up, a sentence to reread.
    # All are candidates; whether one is a defect needs a reader.
    "refrain": ("Rule 15", "a phrase repeated down a page", "path"),
    "restatement": ("Rule 15", "a claim made in a second place", "path"),
    "quantity": ("Rule 6", "one quantity with several readings", "path"),
    "method": ("Rule 7", "measurement method in the reading path", "path"),
    "correction": ("Rule 10", "a correction nobody asked for", "path"),
    "contrast_and": ("Rule 26", "a contrast joined by 'and'", "path"),
    "coinage": ("Rule 2", "a term coined in passing", "path"),
    "inverted": ("Rule 15", "a sentence that opens on its predicate", "path"),
    "ratio": ("Rule 8", "a ratio without its baseline, or a loss as a multiplier", "path"),
    "conditions_result": ("Rule 4", "a result inside a Conditions line", "path"),
    "forward": ("Rule 3", "a forward reference", "path"),
    "stale": ("Rule 6", "a figure the current build or operating point has not re-measured", "path"),
    "row_mix": ("Rule 6", "a table row whose figures carry footnote marks", "all"),
    "column_repeat": ("Rule 25", "a column that repeats one figure on every row", "all"),
    "section_size": ("Rule 3", "a section too long to scan", "all"),
    "cell_size": ("Rule 3", "a table cell too long to scan", "all"),
    "ladder": ("Rule 6", "a table step whose ratio its own figures do not give", "all"),
}
MODES = ("hard", "ratchet", "report", "off")


def _rx(patterns: list[str], flags: int = 0) -> list[re.Pattern[str]]:
    return [re.compile(p, flags) for p in patterns]


# Rule 6's framings: how the document changed, and how the investigation went.
HISTORY = _rx([
    r"\bcorrected\b", r"\bpreviously\b", r"\bused to be\b", r"\bwas thought\b",
    r"\bturned out\b", r"\binitially\b", r"\boriginally\b", r"\bno longer\b",
    r"\bearlier note\b", r"\bwalked back\b", r"\bnow understood\b", r"\bformerly\b",
    r"\bonce thought\b", r"\bwas broken\b", r"\bthe old path\b", r"\bthe first run\b",
    r"\bthe next run\b", r"\bre-?ran\b", r"\bwas excluded\b", r"\brefuted\b", r"\bso far\b",
    r"\bthe lead\b(?![- ]in)", r"\bthis session\b", r"\bthe next session\b",
    r"\bthe handover\b", r"\bneeds? a re-?measure\b",
], re.I) + _rx([r"\bDONE\b", r"\bCOMPLETE\b", r"\bWIP\b", r"\bTODO\b", r"\bRESOLVED\b",
                r"\bP[0-9]+\.[0-9]+\b"])
# Rule 15: the document grading itself, its rigor, or a fact's importance.
SELF_ASSESSMENT = _rx([
    r"\bthe single most\b", r"\bmost (?:important|useful|counterintuitive)\b",
    r"\bload-bearing\b", r"\bcarefully\b", r"\bworth (?:knowing|keeping|reading|noting|running)\b",
    r"\bis the point\b", r"\bhonest(?:ly)?\b", r"\bthe real story\b", r"\bactually\b",
    r"\btruthfully\b",
], re.I)
# Rule 28: the project or its authors in the agent slot.
AGENT = _rx([r"\bthis project(?:'s)?\b", r"\bwe\b", r"\bour\b", r"\bours\b"], re.I)
# Rule 7: the generic half of process residue. A profile adds its own harness names.
RESIDUE = _rx([
    r"\brc ?= ?-?\d", r"\b(\d+) of \1\b", r"\bctest \d+/\d+", r"\bcampaign\b", r"\bA/B\b",
    r"(?<![\w/])tests?/[\w.-]+\.\w+",
], re.I)
# Rule 10: phrases that usually answer a claim nobody made. Candidates only.
REBUTTAL = _rx([
    r"\bthe real (?:constraint|defect|cause|limit|question)\b", r"\bnot a property of\b",
    r"\bThere is no (?:seventh|eighth|ninth|other|\w+th)\b", r"\bstraw ?man\b",
    r"\bthe precise (?:claim|defect)\b", r"\btempting\b",
    # "Whatever its calibration" waves away alternatives. "Whatever its program is still doing"
    # and "whatever its group holds" are free relatives, the content an action works on, so a
    # verb inside the clause takes the phrase out.
    r"\bwhatever (?:their|its)\b(?!(?:\s+[\w'-]+){0,3}?\s+(?:is|are|was|were|has|had|holds?|"
    r"held|left|leaves|does|did|doing|contains?|writes?|wrote|reads?)\b)",
    r"\breads as \"", r"\bnot a (?:lever|finding|floor|wall)\b", r"\bargument is wrong\b",
    r"\bthe plausible assumption\b",
], re.I)
# Rule 26: a sentence that opens by pointing back at the whole previous one.
CHAIN = re.compile(
    r"^(?:\*\*|_)*(?:So|Therefore|Thus|Hence)\b|^(?:\*\*|_)*(?:That|This)\s+(?:is|was|makes|"
    r"means|says|gives|leaves|holds|sets|puts|matters|explains|follows|costs|rules|settles|"
    r"buys|carries|reads|does)\b"
)
# Rule 7: how a figure was measured, which belongs in the record beside the figure's source.
METHOD = _rx([
    r"\brotated (?:interleaved )?passes?\b", r"\bpaired (?:with)?in (?:a|each|the) pass\b",
    r"\b\d+(?:\.\d+)? ?sigma\b", r"\brep sd\b", r"\bstandard deviation\b",
    # "One process per stream", "per camera" and "per detector" are deployment layouts, not how a
    # figure was taken, so only a measurement noun after "per" is method. "The single-process
    # output hash" and "the single process's rate" name a baseline a result is compared with.
    r"\bsingle[- ]process\b(?!'s| (?:output|one|rate|hash|baseline)\b)",
    r"\bone process (?:per (?:configuration|arm|run|pass|cell|measurement|repetition)s?\b|each|at)\b",
    r"\brun[- ]to[- ]run\b",
    r"\bper-process spread\b", r"\bwithin-process\b", r"\bonly the direction\b",
    r"\bpooled over\b", r"\bbelow the (?:measurement's )?resolution\b",
    r"\bindistinguishable from\b", r"\bsingle measurements?\b", r"\bmeans? of (?:adjacent )?pairs\b",
    # "Runs in two passes" counts the device's passes over the data, not the measurement's.
    # So are "runs as two passes", "the op's two passes" and a chain "past two passes".
    r"(?<! in )(?<! as )(?<!'s )(?<! past )(?<! than )\b(?:two|three|four|five|six|seven|eight|nine|ten|\d+) (?:rotated )?passes\b",
], re.I)
# Rule 10: the "X, not Y" sentence that corrects a reading nobody proposed. "The mechanism is not
# established" is Rule 9's epistemic scope, which stays, so only a cause ruled out "by" a named
# alternative is a candidate.
CORRECTION = _rx([
    r",\s+not (?:a|an) [\w-]+(?: [\w-]+){0,3}[.;]", r"\bmeasures nothing\b",
    r"\bis not (?:established|explained) by\b", r"\bno evidence (?:about|of|for)\b",
    r"\brather than a (?:loss|bound|gain|regression)\b", r"\bconfirms that no\b",
    r"\bnot merely\b",
], re.I)


def admitted_negative(text: str, erratum_fields: list[str]) -> bool:
    """Whether a paragraph is in a form Rule 10 gives an admitted negative.

    The forms are a callout, which by Rule 4 holds what changes how the reader acts, an
    erratum's field, and a Not characterized line. A Conditions line is not one: a denial there
    restates the scope the line already bounds.
    """
    fields = [re.escape(f) for f in erratum_fields if f != "Conditions"] or ["Workaround"]
    return bool(re.match(r"\[!(?:NOTE|CAUTION|WARNING|IMPORTANT|TIP)\]|\*\*Not characterized:|"
                         r"\*\*(?:" + "|".join(fields) + r"):", text))
# Rule 26: a split that kept "and" where the second clause turns against the first.
CONTRAST_AND = re.compile(
    r",\s+and\s+(?:it|they|this|that|(?:its|their|the|a)\s+[\w-]+(?:\s+[\w-]+)?)\s+"
    r"(?:does|do|did|is|are|was|were|can|could|will|has|have)(?:\s+not|n't)\b"
    r"|,\s+and\s+(?:do not|never|cannot|no longer)\b", re.I)
# Rule 2: a term defined in passing, at the end of a sentence, instead of where terms are defined.
COINAGE = [
    re.compile(r"\b(?:is|are) (?:called |known as |termed )?the ([a-z][a-z-]*(?: [a-z][a-z-]*){0,2})\s*\.?\s*$"),
    re.compile(r"\b(?:we call|is called|are called|is termed|known as|referred to as) "
               r"(?:the |a |an )?[\"“']?([a-z][\w-]*(?: [a-z][\w-]*){0,2})", re.I),
]
COMMON_NOUNS = set("""default case same baseline result reason cause point goal limit rule exception
difference effect cost answer fix norm minimum maximum first last only one other rest
following latter former opposite reverse key problem practical recommended fastest slowest best
worst largest smallest main usual simplest combined preferred safest same example whole entire
complete larger smaller next-largest next-smallest""".split())
# A phrase that ends on a pro-form or an intensifier ("the narrow ones", "the defaults themselves",
# "the window sample alone") refers to something already named, and one holding a relative
# pronoun ("the variable that fits") is a clause. Neither is a term.
COINAGE_TAIL = {"one", "ones", "itself", "themselves", "alone", "exactly"}
COINAGE_CLAUSE = {"that", "which", "who"}


def value_subject(before: str, units: re.Pattern[str]) -> bool:
    """Whether the words before a copula end on an identifier, a value or a figure.

    "`toh` is the transposed output height" and "0 is the dense pass-through" say what a name
    or a figure is. "P is the swept variable" and "71 us is the completion wait" do too. Each
    says so in the sentence that uses the name. That defines the identifier, and coins no term.
    """
    toks = re.sub(r"[()\[\],:]", " ", before).split()
    if not toks:
        return False
    last = toks[-1]
    if last == CODE_STANDIN or re.fullmatch(r"[A-Z]|[~+-]?\d[\d.,]*%?", last):
        return True
    return len(toks) > 1 and bool(units.fullmatch(last)) and bool(re.fullmatch(r"~?\d[\d.,]*", toks[-2]))


def coinage(sentence: str, names: set[str], units: re.Pattern[str],
            defined: set[str], standard: list[str]) -> str | None:
    """The term a sentence coins in passing, or None.

    `defined` is the glossary's terms, lowercased: a term with an entry is defined where terms
    are defined, so a sentence that uses it coins nothing. `standard` is the configuration's
    `standard_terms`, the field's terms the glossary does not carry ("zero point"), and a phrase
    ending in one ("the input zero point") is that term. A code span inside the phrase ("the
    literal `POWER_DOWN_FREQ`") names the thing by its identifier.
    """
    for k, pattern in enumerate(COINAGE):
        m = pattern.search(sentence)
        if not m:
            continue
        phrase = m.group(1)
        lower = phrase.lower()
        words = lower.split()
        singular = re.sub(r"(?<!s)s$", "", lower)
        if (words[0] in COMMON_NOUNS or lower in names or CODE_STANDIN in words
                or words[-1] in COINAGE_TAIL or COINAGE_CLAUSE & set(words)
                or lower in defined or singular in defined
                or any(lower == t or lower.endswith(" " + t) for t in standard)
                or (k == 0 and value_subject(sentence[:m.start()], units))):
            continue
        return phrase
    return None
# Rule 15: lead with the subject, not with a clause that withholds it.
INVERTED = re.compile(
    r"^(?:\*\*|_)*(?:Which|Whether|How (?:much|many|far|long)|What) [^?]*"
    r"\b(?:is|are|depends|decides|varies|matters)\b[^?]*$")
# Rule 8 and the profile's baseline rule: a ratio names what it is a ratio of.
RATIO_BARE = re.compile(
    r"\b\d+(?:\.\d+)?x (?:worse|better|faster|slower|smaller|larger|higher|lower|more|less)\b", re.I)
RATIO_BASELINE = re.compile(
    r"\b(?:than|against|over|versus|vs\.?|relative to|compared|the same|its own|of the)\b", re.I)
RATIO_LOSS = re.compile(
    r"\b\d+(?:\.\d+)?x (?:net )?(?:loss|regression|penalty)\b"
    r"|\bcosts? [^.]{0,40}?\b\d+(?:\.\d+)?x (?:[\w-]+ )?perplexity\b", re.I)
# Rule 3: a pointer to something the reader has not met yet. "Bounded from below" and "8 GB and
# below" are a bound and a range, not a pointer.
FORWARD = re.compile(
    r"(?<!\bfrom )(?<!\band )(?<!\bor )\bbelow,"
    r"|\b(?:described|covered|explained|detailed|defined) below\b|\bsee below\b"
    r"|\blater in this (?:document|section|page)\b", re.I)
LEADS_INTO = re.compile(r"^\s*(?:`{3}|~{3}|\||(?:[-*+]|\d+\.)\s)")


def leads_into(text: str, after: str) -> bool:
    """Whether a paragraph is the lead-in of the line after it.

    A paragraph that ends in a colon and whose next line opens a code block, a table or a list
    introduces that block. Its "below" points at the next line rather than ahead in the page.
    """
    after = re.sub(r"^\s*>\s?", "", after)
    return text.rstrip().endswith(":") and bool(LEADS_INTO.match(after))
# Rule 6: a figure the document itself says no later build has re-measured.
STALE = re.compile(r"Not characterized:\**\s.*\b(?:current|later|latest|newer|this) build\b", re.I)
# What an operating point moves: a time, a rate, a ratio or a percentage, a unit standing alone
# as a table column's header does ("| t/s |"), or a performance claim made without a figure ("the
# prefill ratio is lower"). A byte count, a shape, a count of layers or a cosine is none of these,
# so a section that states only those holds a correctness result, and a clock named beside it is
# provenance rather than re-measure debt.
PERF_FIGURE = re.compile(
    r"(?<![\w.])~?\d[\d.,]*(?:-\d[\d.,]*)?\s?(?:ns|us|µs|ms|s|min|t/s|tok/s|img/s|fps|GOP/s|TOPS|"
    r"[GMK]i?B/s)(?![\w/])"
    r"|(?<![\w.])~?\d+(?:\.\d+)?(?:-\d+(?:\.\d+)?)?x\b"
    r"|(?<![\w.])[+-]?\d+(?:\.\d+)?%"
    r"|(?<![\w/])(?:t/s|tok/s|img/s|fps|GOP/s|TOPS|[GMK]i?B/s|ms|us|µs|TTFT)(?![\w/])"
    r"|\b(?:prefills?|decodes?|gains?|speedups?|faster|slower|throughput|latenc(?:y|ies)|ratios?|"
    r"wall-clock|overhead)\b", re.I)


def stale_region(block: "Block", raw_lines: list[str], headings: list[tuple[int, int, str]],
                 page_blocks: list["Block"], record: set[int] = frozenset()) -> str:
    """The text whose figures a stale pattern in `block` would date.

    A Conditions block governs its section, before it as well as after (a block placed under its
    table governs that table). The section runs to the next heading at the level of the heading
    the block sits under. A document's own Conditions line therefore governs the whole reading
    path. A paragraph governs itself, and what it introduces when it ends in a colon. Other
    Conditions and Not characterized lines in the region state settings, not results, and the
    record dates its own figures, so both are left out.
    """
    end = block.line + len(block.parts) - 1
    if CONDITIONS.match(block.text):
        above = [(m, d) for m, d, _ in headings if m < block.line]
        start, level = (above[-1][0] + 1, above[-1][1]) if above else (1, 0)
        stop = next((m for m, d, _ in headings if m > end and d <= level), len(raw_lines) + 1)
        own = ""
    else:
        own = block.text
        if not block.text.rstrip().endswith(":"):
            return own
        start = end + 1
        stop = next((m for m, _, _ in headings if m > end), len(raw_lines) + 1)
    settings = {n for b in page_blocks if start <= b.line < stop
                and (CONDITIONS.match(b.text) or GENRE_LABEL.match(b.text))
                for n in range(b.line, b.line + len(b.parts))}
    body = [raw_lines[n - 1] for n in range(start, stop) if n not in settings and n not in record]
    return own + "\n" + "\n".join(body)


def stale_finding(block: "Block", raw_lines: list[str], headings: list[tuple[int, int, str]],
                  page_blocks: list["Block"], record: set[int],
                  patterns: list[re.Pattern[str]]) -> re.Match[str] | None:
    """The re-measure debt a block states, or None.

    A Not characterized line that names a later build is debt by its own words. A configured
    stale pattern, an old operating point, is debt only where something it dates is in reach.
    A clock named beside a bit-exact result, or in a sentence about the clock itself, dates
    nothing. A block in the record is the caller's to drop, as for any reading-path family.
    """
    if (found := STALE.search(block.text)):
        return found
    found = next((m for p in patterns if (m := p.search(block.text))), None)
    if not found:
        return None
    region = re.sub(r"`[^`]*`", " ", stale_region(block, raw_lines, headings, page_blocks, record))
    for p in patterns:
        region = p.sub(" ", region)
    return found if PERF_FIGURE.search(region) else None
# Rule 4: a Conditions block states the settings a result holds under. A figure next to a result
# verb inside one is a measurement the reader cannot find in the table, and usually an older run's.
CONDITIONS = re.compile(r"\s*(?:\*\*)?Conditions:")
CONDITIONS_RESULT = re.compile(
    r"\b(?:took|takes|ran|runs? in|measured|measures?|stays? at|against|faster|slower)\b[^.;]{0,25}?"
    r"(?:(?<![\w.])~?\d[\d.,]*(?:-\d[\d.,]*)? ?(?:ms|s|t/s|img/s|GOP/s|GB/s)\b"
    r"|(?<![\w.])\d+\.\d+(?:-\d+\.\d+)?x\b|(?<![\w.])\d[\d.]*(?:-\d[\d.]*)?%)", re.I)
# A dagger-family mark only: in this workspace § cites a section and a superscript is an exponent.
FOOTNOTE_MARK = re.compile(r"[†‡¶]")
# A figure is a number that is not the tail of a name such as int16 or fp32.
ROW_FIGURE = re.compile(r"(?<![\w.])\d")
# Rule 6: a ladder row states a step and the value it reaches, "+19% -> 13.38" or "1.43x -> 11.40".
# A target that is itself a ratio, "3.4x -> 2.6x", is one ratio under two configurations, not a step.
LADDER_STEP = re.compile(r"(?:([+-]?\d+(?:\.\d+)?)%|(\d+(?:\.\d+)?)x)\s*(?:->|→)\s*(~?)(\d+(?:\.\d+)?)(?![\d.]*\s*[x%])")
LADDER_VALUE = re.compile(r"(~?)(\d+(?:\.\d+)?)(?!.*\d)")


def _half_ulp(figure: str) -> float:
    """Half the last printed digit of a figure: the rounding its printed form allows."""
    return 0.5 * 10 ** -(len(figure.split(".")[1]) if "." in figure else 0)


def ladder_off(previous: str, cell: str) -> str | None:
    """The step a ladder cell states, when the two values it joins cannot give it.

    The interval is what the printed precision allows. Each value is within half its last
    digit, twice that for one marked `~`. The stated step is within half its own last digit.
    A step outside it is a ratio from another measurement, and a reader who multiplies down
    the column reaches a number nobody measured.
    """
    step, before = LADDER_STEP.search(cell), LADDER_VALUE.search(previous)
    if not step or not before:
        return None
    pct, mult, approx, after = step.groups()
    v0, v1 = float(before.group(2)), float(after)
    if v0 <= 0:
        return None
    e0 = _half_ulp(before.group(2)) * (2 if before.group(1) else 1)
    e1 = _half_ulp(after) * (2 if approx else 1)
    lo, hi = (v1 - e1) / (v0 + e0), (v1 + e1) / (v0 - e0)
    if pct is not None:
        stated, slack = 1 + float(pct) / 100, _half_ulp(pct) / 100
    else:
        stated, slack = float(mult), _half_ulp(mult)
    if stated + slack < lo or stated - slack > hi:
        implied = v1 / v0
        shown = f"{(implied - 1) * 100:+.0f}%" if pct is not None else f"{implied:.2f}x"
        return f"{step.group(0)} after {before.group(0)}: the two values give {shown}"
    return None
RANGE_CELL = re.compile(r"\d[\d.]*\s*[-–]\s*\d")
STOPWORDS = set("""a an the and or but of to in on at by for with from as is are was were be been
being it its this that these those which who whom whose than then so not no nor if when while
into onto over under per via each every any all both either neither has have had does do did
can cannot will would could might may must should there their they them also only more most
less same such other another""".split())

HAZARD_WORDS = re.compile(
    r"\b(?:hangs?|hung|corrupt\w*|wrong|poison\w*|wedge\w*|silently|never|garbage|lost|"
    r"loses|fails?|crash\w*|oops\w*|deadlock\w*)\b", re.I
)
FINITE_VERBS = set("""
is are was were has have had does do did can cannot must will isn't aren't doesn't won't
computes carries needs gives breaks fails buys pays owns settles lands ships poisons stays
goes beats refuses admits decides depends means belongs applies survives sits hangs wedges
oopses emits raises scales follows explains inverts transfers rounds saturates misses tells
says proves outlives rides prices spends keeps reaches makes takes holds
""".split())
EDITORIAL = re.compile(r"\b(honest|honestly|the real story|actually|truthfully|what actually works)\b", re.I)
HEAD_RHET = re.compile(
    r"^(How it (works|fits)|Why it matters|Why this exists|What makes it different|"
    r"What this is worth|What's here|What it provides)$", re.I
)


def modals(words: list[str]) -> re.Pattern[str] | None:
    """One pattern over a list of modal verbs, or None where the list is empty.

    A project empties a list to turn its half of Rule 15 off. That is the only way to say
    "this corpus uses `may` as a defined term" without turning the whole rule off.
    """
    if not words:
        return None
    return re.compile(r"\b(" + "|".join(re.escape(w) for w in words) + r")\b", re.I)


@dataclass
class Config:
    """Everything the gate needs that the style guide cannot state generically."""

    root: Path
    include: list[str]
    exclude: list[str]
    doc_comments: list[dict]
    profile: Path | None
    vocabulary_heading: str
    ratchet: Path | None
    max_sentence: int
    max_paragraph: int
    enumeration_items: int
    label_block_words: int
    labels_per_section: int
    units: re.Pattern[str]
    overrides: list[dict] = field(default_factory=list)
    gated: re.Pattern[str] | None = None
    recommendation: re.Pattern[str] | None = None
    loose: re.Pattern[str] | None = None
    spellings: dict[str, str] = field(default_factory=dict)
    terms_heading: str = ""
    terms_path: Path | None = None
    answers_heading: str = ""
    answers_path: Path | None = None
    answers_root: Path | None = None
    answers_include: list[str] = field(default_factory=list)
    answers_exclude: list[str] = field(default_factory=list)
    extra_names: set[str] = field(default_factory=set)
    voice_ratchet: Path | None = None
    house_words_heading: str = "## House words"
    record_zones_heading: str = "## Record zones"
    skeletons_heading: str = "## Document skeletons"
    voice: dict = field(default_factory=dict)

    @classmethod
    def load(cls, path: Path) -> "Config":
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        here = path.parent

        def resolve(value: str | None) -> Path | None:
            return (here / value).resolve() if value else None

        surfaces = data.get("surfaces", {})
        profile = data.get("profile", {})
        ratchet = data.get("ratchet", {})
        limits = data.get("limits", {})
        counting = data.get("counting", {})
        answers = data.get("answers", {})
        terms = data.get("terms", {})

        root = resolve(surfaces.get("root", "."))
        assert root is not None
        units = counting.get("units", DEFAULT_UNITS)
        # The two profile tables default to the profile, and each may name its own
        # document. A project whose answered-questions table already has an owner points
        # at that owner rather than copying the table into the profile, because the copy
        # is the drift the table exists to prevent, arriving in the guard for it.
        profile_path = resolve(profile.get("path"))
        # A modal sits on one rung. A project that gates `should` keeps the default
        # recommendation list, and the word fails there without being reported here too.
        gated = limits.get("gated_modals", DEFAULT_GATED_MODALS)
        recommended = [
            w for w in limits.get("recommendation_modals", DEFAULT_RECOMMENDATION_MODALS)
            if w.lower() not in {g.lower() for g in gated}
        ]
        loose = [
            w for w in limits.get("loose_modals", DEFAULT_LOOSE_MODALS)
            if w.lower() not in {g.lower() for g in gated + recommended}
        ]
        return cls(
            root=root,
            include=surfaces.get("include", ["**/*.md"]),
            exclude=surfaces.get("exclude", []),
            doc_comments=surfaces.get("doc_comments", []),
            profile=profile_path,
            vocabulary_heading=profile.get("vocabulary_heading", ""),
            ratchet=resolve(ratchet.get("path")),
            max_sentence=limits.get("sentence_words", 25),
            max_paragraph=limits.get("paragraph_sentences", 6),
            # Rule 24 and Rule 18 are off unless a project turns them on. Both rest on a
            # threshold the guide states as a default rather than as a fact, and a corpus
            # is the only thing that says which number is right for it.
            enumeration_items=limits.get("enumeration_items", 0),
            label_block_words=limits.get("label_block_words", 0),
            labels_per_section=limits.get("labels_per_section", 3),
            units=re.compile(r"(?:" + "|".join(re.escape(u) for u in units) + r")"),
            # Rule 26's limits per surface, for a page the project holds to different
            # numbers. Each entry is `include` plus the keys it overrides, and the first
            # entry matching a surface wins.
            overrides=limits.get("override", []),
            gated=modals(gated),
            recommendation=modals(recommended),
            loose=modals(loose),
            spellings={
                k.lower(): v
                for k, v in data.get("spelling", {}).get("write", DEFAULT_SPELLINGS).items()
            },
            terms_heading=terms.get("heading", ""),
            terms_path=resolve(terms.get("path")) or profile_path,
            answers_heading=answers.get("heading", ""),
            answers_path=resolve(answers.get("path")) or profile_path,
            answers_root=resolve(answers.get("root")) or root,
            answers_include=answers.get("include", []),
            answers_exclude=answers.get("exclude", []),
            extra_names={n.lower() for n in counting.get("names", [])},
            voice_ratchet=resolve(ratchet.get("voice_path")),
            house_words_heading=profile.get("house_words_heading", "## House words"),
            record_zones_heading=profile.get("record_zones_heading", "## Record zones"),
            skeletons_heading=profile.get("skeletons_heading", "## Document skeletons"),
            voice=data.get("voice", {}),
        )


def profile_section(config: Config, heading: str) -> str | None:
    """The body of one profile section, or None when the profile or the heading is absent."""
    if not config.profile or not heading or not config.profile.is_file():
        return None
    text = config.profile.read_text(encoding="utf-8")
    parts = re.split(r"^" + re.escape(heading) + r"[ \t]*$", text, maxsplit=1, flags=re.M)
    if len(parts) < 2:
        return None
    level = len(heading) - len(heading.lstrip("#"))
    end = re.search(r"^#{1,%d}\s" % level, parts[1], flags=re.M)
    return parts[1][: end.start()] if end else parts[1]


class ProfileError(Exception):
    """A configured profile the gate cannot read.

    This stops the run rather than warning past it. Every part of the profile contract
    fails the same quiet way. Each leaves the gate holding the prose to a weaker rule than
    the project asked for, while still reporting success. The ways it breaks are a missing
    file, a heading that does not match, and an italicized run that is not there.

    That is a broken configuration rather than a style finding. The two want opposite
    outcomes: a finding is something to go and fix in the prose, and this is something
    that makes the findings untrustworthy. A project that wants no profile leaves
    `profile.path` empty.
    """


def technical_names(config: Config) -> set[str]:
    """The words the project's profile admits as technical names.

    Rule 27 wants a list item to start with a capital, and Rule 29 exempts an identifier
    from any rule about prose. A name like `btrfs` is both lowercase and an identifier, so
    the gate cannot tell one from a missing capital by shape. The profile is the list.

    The contract is one italicized run under the configured heading, its terms separated
    by commas. PROFILE-TEMPLATE.md states it where a profile author reads it.
    """

    def unread(why: str) -> set[str]:
        raise ProfileError(
            f"the profile is configured and {why}.\n"
            "  No technical names would be in force, so Rule 27 would read a\n"
            "  lowercase identifier as a list item that forgot its capital."
        )

    names = set(config.extra_names)
    if not config.profile or not config.vocabulary_heading:
        return names
    if not config.profile.is_file():
        return unread(f"there is no file at {config.profile}")
    text = config.profile.read_text(encoding="utf-8")
    section = text.split(config.vocabulary_heading, 1)
    if len(section) < 2:
        return unread(f"it has no heading matching {config.vocabulary_heading!r}")
    body = section[1].split("\n#", 1)[0]
    # A lone `*...*` run, never a `**bold**` one. Without the guards a bold lead-in above
    # the list is captured instead, the gate runs with that sentence as its whole
    # vocabulary, and nothing warns, because a run *was* found.
    listed = re.search(r"(?<!\*)\*(?!\*)([^*]+?)\*(?!\*)", body)
    if not listed:
        return unread("its vocabulary section holds no italicized run")
    for term in listed.group(1).replace("\n", " ").split(","):
        first = term.strip().strip(".").split()
        if first:
            names.add(first[0].lower())
    return names


def answered_questions(config: Config) -> list[tuple[str, str]]:
    """The rows of the answered-questions table, as (question, answer) pairs.

    One row per question the project answers once, and the answer names the artifact that
    answers it. The table is read the same way a reader reads it, so what the gate checks
    and what a reviewer checks are the same list.

    The table lives in the profile unless `answers.path` names another document. Where a
    project already has an owner for it, that owner is the one to read. A second copy in
    the profile is the drift the table exists to prevent.
    """
    if not config.answers_heading or not config.answers_path:
        return []
    rows = [
        (cells[0], cells[1])
        for cells in read_table(
            config.answers_path, config.answers_heading, 2, "the answers table"
        )
        if cells[0].lower() not in {"the question", "question"}
    ]
    if not rows:
        raise ProfileError(
            f"the answers table is configured and the section under "
            f"{config.answers_heading!r} in {config.answers_path.name} holds no table rows"
        )
    return rows


def suffixes(citation: str) -> list[str]:
    """Every trailing form of a cited answer, longest first.

    A row cites the qualified path a reader follows, and the source spells any suffix of
    it. The definition site writes the last segment alone, and a call site often writes two
    or three. Trying the longest first confirms a row by the most of its path the corpus
    actually contains, which is what makes the check worth running. Matching the
    last segment alone would pass `store::gone` on any prose using the word "gone".

    A trailing macro bang or call parentheses belong to the citation rather than to the
    name, so they come off first.
    """
    stem = citation.strip().rstrip("!").removesuffix("()")
    for separator in ("::", "->", "."):
        if separator in stem:
            parts = stem.split(separator)
            return [separator.join(parts[i:]) for i in range(len(parts))]
    return [stem]


def needle(answer: str) -> str:
    """The last segment of a cited answer, which is the weakest form it can be found by."""
    return suffixes(answer)[-1]


def check_answers(config: Config) -> tuple[list[str], list[str]]:
    """Every answer the profile names, checked to still exist.

    The table is the project's record of which question is answered where, and Rule 2 rests
    on it. A writer reaches for the existing answer rather than writing a second one.

    A renamed artifact leaves the row pointing at nothing. The row then documents a
    mechanism that is gone, which is worse than no row at all. A reader trusts it and
    writes the second answer the table existed to prevent.

    Returns the rows that fail and the rows that pass weakly. A weak row is one whose
    citation is qualified and whose corpus holds only its last segment. The check confirmed
    a word rather than a path. That is not a defect in the prose and does not
    fail the run. It is the difference between a row that is guarded and one that looks
    guarded, so it is counted and `--census` lists them.

    Nothing here confirms the answer still means what the question says. A row that has
    quietly become the answer to a different question is the failure a reader has to catch.
    """
    rows = answered_questions(config)
    if not rows or not config.answers_root:
        return [], []
    excluded = {
        q.resolve()
        for pattern in config.answers_exclude
        for q in config.answers_root.glob(pattern)
    }
    corpus, searched = [], 0
    for pattern in config.answers_include:
        for path in sorted(config.answers_root.glob(pattern)):
            if not path.is_file() or path.resolve() in excluded:
                continue
            corpus.append(path.read_text(encoding="utf-8", errors="replace"))
            searched += 1
    if not searched:
        raise ProfileError(
            f"the answers table is configured and `answers.include` matched no file "
            f"under {config.answers_root}"
        )
    haystack = "\n".join(corpus)
    missing: list[str] = []
    weak: list[str] = []
    for question, answer in rows:
        for citation in re.findall(r"`([^`]+)`", answer):
            matched, how = resolves(citation, haystack, config)
            if matched is None:
                missing.append(
                    f"{config.answers_path.name}: Rule 15: `{citation}` answers "
                    f'"{question}" and no longer exists\n    {how}'
                )
            elif "::" in citation and "::" not in matched:
                weak.append(
                    f"{config.answers_path.name}: `{citation}` was found only as {matched!r}, "
                    f'so the row for "{question}" is confirmed by a word rather than a path'
                )
    return missing, weak


def occurs(text: str, haystack: str) -> bool:
    """Whether `text` appears in the corpus as a whole name rather than inside a longer one."""
    return re.search(r"(?<![\w-])" + re.escape(text) + r"(?![\w-])", haystack) is not None


def resolves(
    citation: str, haystack: str, config: Config
) -> tuple[str | None, str]:
    """The longest form of one cited answer that the corpus contains, or None.

    A citation naming a location is held to that location, and never to a suffix of it. The
    last segment of `tools/idioms.toml` is `toml`, which occurs in every manifest in the
    tree and would pass the row whatever happened to the file. A located citation is
    matched as a path suffix rather than from the root. A row cites the path a reader
    recognizes, and the tree holds it several directories down.
    """
    assert config.answers_root is not None
    if "/" in citation:
        stem = citation.rstrip("/")
        for pattern in (stem, f"{stem}.*", f"**/{stem}", f"**/{stem}.*"):
            if next(config.answers_root.glob(pattern), None) is not None:
                return stem, ""
        return None, f"no path under {config.answers_root} ends with {stem!r}"
    for form in suffixes(citation):
        if form and occurs(form, haystack):
            return form, ""
    return None, f"no suffix of {citation!r} occurs in the files `answers.include` matches"


def read_table(path: Path, heading: str, columns: int, what: str) -> list[list[str]]:
    """The rows of one table under one heading, as lists of cell text.

    Two of the profile's sections are tables a script reads. Both are read the way a reader
    reads them, so what the gate holds and what a reviewer holds are one document. Either
    can name its own document instead of the profile, so the failure names the file it was
    looking in. A blank template row is not a row.
    """
    if not heading:
        return []
    if not path.is_file():
        raise ProfileError(f"{what} is configured and there is no file at {path}")
    section = path.read_text(encoding="utf-8").split(heading, 1)
    if len(section) < 2:
        raise ProfileError(
            f"{what} is configured and {path.name} has no heading matching {heading!r}"
        )
    rows: list[list[str]] = []
    for line in section[1].split("\n#", 1)[0].splitlines():
        line = line.strip()
        if not line.startswith("|") or set(line) <= set("| -:"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) < columns or not any(cells):
            continue
        rows.append(cells)
    return rows


def ruled_out(config: Config) -> list[tuple[str, str]]:
    """The variants the project's fixed-terms table rules out, each with its replacement.

    Rule 15 wants one term per concept, and the profile's third column is the half a
    reviewer needs. Recognizing the wrong variant is what the rule actually asks of a
    reader. A variant is only findable mechanically, so this is the half worth automating,
    and the reason that column is not decoration.
    """
    out: list[tuple[str, str]] = []
    if not config.terms_heading or not config.terms_path:
        return out
    for cells in read_table(
        config.terms_path, config.terms_heading, 3, "the fixed-terms table"
    ):
        if cells[0].lower() == "concept":
            continue
        prefer = cells[1].strip("`\"' ")
        for variant in cells[2].split(","):
            variant = variant.strip().strip("`\"' ").strip(".")
            if variant and prefer:
                out.append((variant, prefer))
    return out


@dataclass(frozen=True)
class Surface:
    """One prose surface: a Markdown page, or the doc comments in one source file.

    A doc comment is prose that ships. It is read by the same stranger the README is
    written for, and on many projects it is the larger of the two. Only the extraction
    differs, so a surface carries the dialect to extract it with and is held to the same
    rules afterward.
    """

    path: Path
    rel: str
    dialect: str = "markdown"
    prefixes: tuple[str, ...] = ()
    keys: tuple[str, ...] = ()
    public_only: bool = False


def matching(root: Path, include: list[str], exclude: list[str]) -> list[Path]:
    """Files under `root` matching `include` and not `exclude`, in the order given.

    Include patterns are honored in order and sorted within each one. A project therefore
    controls the order a report walks its pages in, rather than inheriting the
    filesystem's.
    """
    excluded = {p.resolve() for pattern in exclude for p in root.glob(pattern)}
    found: list[Path] = []
    seen: set[Path] = set()
    for pattern in include:
        for path in sorted(root.glob(pattern)):
            resolved = path.resolve()
            if path.is_file() and resolved not in excluded and resolved not in seen:
                seen.add(resolved)
                found.append(path)
    return found


def surfaces(config: Config) -> list[Surface]:
    """Every surface this gate walks: the pages first, then the doc comments.

    The pages come first because they are the order a reader meets the project in, and a
    report is read from the top. A source file appears once per configuration entry that
    names it, and never twice.
    """
    found = [
        Surface(path, str(path.relative_to(config.root)))
        for path in matching(config.root, config.include, config.exclude)
    ]
    claimed = {surface.path.resolve() for surface in found}
    for entry in config.doc_comments:
        dialect = entry.get("dialect", "")
        if dialect not in DIALECTS:
            raise ProfileError(
                f"`surfaces.doc_comments` names the dialect {dialect!r}, and this gate "
                f"extracts {', '.join(sorted(DIALECTS))}.\n"
                "  A language whose doc comments open with a fixed marker takes the\n"
                "  `line` dialect and lists those markers in `prefixes`."
            )
        prefixes = tuple(entry.get("prefixes", ()))
        keys = tuple(entry.get("keys", ()))
        if dialect == "data" and not keys:
            raise ProfileError(
                "`surfaces.doc_comments` configures the `data` dialect and lists no "
                "`keys`.\n"
                "  Nothing would be extracted, and the gate would report every data file\n"
                "  clean while holding none of the prose in it."
            )
        seen_for = entry.get("visibility", "all")
        if seen_for not in ("all", "public"):
            raise ProfileError(
                f"`surfaces.doc_comments` sets visibility to {seen_for!r}, and this gate "
                "reads `all` or `public`.\n"
                "  `public` holds the comments STYLE.md calls the API reference and drops\n"
                "  the rest, which it calls a different class with a different reader."
            )
        if dialect == "line" and not prefixes:
            raise ProfileError(
                "`surfaces.doc_comments` configures the `line` dialect and lists no "
                "`prefixes`.\n"
                "  Nothing would be extracted, and the gate would report every source\n"
                "  file clean while holding none of their prose."
            )
        root = (config.root / entry["root"]).resolve() if entry.get("root") else config.root
        for path in matching(root, entry.get("include", []), entry.get("exclude", [])):
            if path.resolve() in claimed:
                continue
            claimed.add(path.resolve())
            found.append(
                Surface(
                    path,
                    str(path.relative_to(config.root)),
                    dialect,
                    prefixes,
                    keys,
                    seen_for == "public",
                )
            )
    return found


def named_files(config: Config, args: list[str]) -> list[Surface]:
    """Files named on the command line: relative to the surfaces' root, or to the cwd.

    A file the configuration already walks keeps the dialect it is configured with. Any
    other is a page, unless its suffix says it is C or C++ source.
    """
    configured = {surface.path.resolve(): surface for surface in surfaces(config)}
    out = []
    for arg in args:
        under_root = config.root / arg
        path = under_root if under_root.is_file() else Path(arg)
        if not path.is_file():
            print(f"prose-lint: no file at {arg}", file=sys.stderr)
            continue
        dialect = "c" if path.suffix.lower() in C_SUFFIXES else "markdown"
        out.append(configured.get(path.resolve(), Surface(path, relative(config, path), dialect)))
    return out


def relative(config: Config, page: Path) -> str:
    try:
        return str(page.resolve().relative_to(config.root))
    except ValueError:
        return str(page)


def strip_code(lines: list[str]) -> list[str]:
    """Blank every fenced block, keeping the line count so a report names the right line."""
    out, fence = [], None
    for line in lines:
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if fence is None and marker:
            fence, out = marker.group(1)[0] * 3, out + [""]
            continue
        if fence is not None:
            out.append("")
            if marker and marker.group(1)[0] * 3 == fence:
                fence = None
            continue
        out.append(line)
    return out


# A source file in one of these is read as the prose of its comments. Rule 29 exempts
# legal boilerplate, and a licence line is recognized by shape.
C_SUFFIXES = {".c", ".h", ".cc", ".cpp", ".cxx", ".hh", ".hpp"}
LEGAL = re.compile(r"SPDX-License-Identifier|^\s*Copyright\b", re.I)
# A licence grant is legal text, not the file's prose: the GNU notice runs from "This file is
# part of" or "is free software" to its line naming gnu.org/licenses.
GRANT_START = re.compile(r"\bThis file is part of\b|\bis free software\b")
GRANT_END = re.compile(r"gnu\.org/licenses")


def c_comments(text: str) -> list[tuple[int, bool, str, list[str]]]:
    """Every comment in a C or C++ source, as (first line, trailing, kind, raw lines).

    A comment is trailing when code precedes it on its first line. String and character
    literals are skipped, so a `/*` inside one opens nothing.
    """
    out: list[tuple[int, bool, str, list[str]]] = []
    i, n, line, code_on_line = 0, len(text), 1, False
    while i < n:
        c = text[i]
        if c == "\n":
            line, code_on_line, i = line + 1, False, i + 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            out.append((line, code_on_line, "line", [text[i:end]]))
            i = end
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            body = text[i:end]
            out.append((line, code_on_line, "block", body.split("\n")))
            line, i = line + body.count("\n"), end
        elif c in "\"'":
            j = i + 1
            while j < n and text[j] not in (c, "\n"):
                j += 2 if text[j] == "\\" else 1
            line += text.count("\n", i, min(j, n))
            i, code_on_line = (j + 1 if j < n and text[j] == c else j), True
        else:
            code_on_line = code_on_line or not c.isspace()
            i += 1
    return out


def comment_view(text: str) -> tuple[list[str], list[int]]:
    """A C or C++ source as the prose of its comments, and the source line of each.

    Code is left out, so the rules read only what was written to be read. Each comment is
    set off by blank lines, so two never join into one sentence. A trailing comment stands
    alone, and a run of `//` lines on consecutive lines is one comment, as a block comment
    is.

    Inside a comment the decoration goes (the markers and a leading `*`). A line indented
    four or more after a blank line is a code block, as in Markdown. A list indented under
    the prose is brought to the margin. A comment line reading `prose-lint: off` or `on`
    marks an exempt region, as the HTML comment does on a page.

    The legal lines are left out too: an SPDX tag, a copyright line, and a GNU license
    grant. A grant runs from "This file is part of" to its gnu.org/licenses line.
    """
    groups: list[tuple[int, list[str]]] = []
    joinable = False
    for first, trailing, kind, raw in c_comments(text):
        if kind == "line":
            body = [re.sub(r"^//[/!]?\s?", "", raw[0]).rstrip()]
            if joinable and not trailing and groups[-1][0] + len(groups[-1][1]) == first:
                groups[-1][1].extend(body)
                continue
            joinable = not trailing
        else:
            raw = list(raw)
            raw[0] = re.sub(r"^/\*[*!]?\s?", "", raw[0])
            raw[-1] = re.sub(r"\s*\*/$", "", raw[-1])
            body = [raw[0].rstrip()] + [
                (re.sub(r"^\s*\*\s?", "", l) if re.match(r"^\s*\*", l) else l.strip()).rstrip()
                for l in raw[1:]
            ]
            joinable = False
        groups.append((first, body))

    view: list[str] = []
    where: list[int] = []
    for first, body in groups:
        lines, after_blank, in_code, grant = [], True, False, False
        for l in body:
            marker = re.search(r"prose-lint:\s*(off|on)\b", l)
            indent = len(l) - len(l.lstrip())
            if not grant and GRANT_START.search(l) and any(GRANT_END.search(b) for b in body):
                grant = True
            if grant:
                grant = not GRANT_END.search(l)
                l = ""
            elif LEGAL.search(l):
                l = ""
            elif marker:
                l = f"<!-- prose-lint: {marker.group(1)} -->"
            elif l.strip() and indent >= 4 and (after_blank or in_code):
                l, in_code = "", True
            elif l.strip():
                in_code = False
            after_blank = not l.strip() and not in_code
            lines.append(l)
        items = [len(l) - len(l.lstrip()) for l in lines if LIST_MARKER.match(l)]
        base = min(items) if items else 0
        lines = [l[base:] if l[:base].isspace() else l for l in lines]
        view.append("")
        where.append(first)
        for k, l in enumerate(lines):
            view.append(l)
            where.append(first + k)
        view.append("")
        where.append(first + len(lines) - 1)
    return view, where


def strip_markdown_indented_code(lines: list[str]) -> list[str]:
    """Blank every indented code block in Markdown, and no continuation paragraph.

    An indented run is a code block only where a code block can start. It cannot interrupt
    a paragraph, and inside a list the same indentation is the item's own continuation,
    which is prose. Blanking on indentation alone would drop that prose from every rule
    while reporting the surface clean, so both conditions are tracked.
    """
    out, open_list, paragraph = [], False, False
    for line in lines:
        if not line.strip():
            out.append(line)
            paragraph = False
            continue
        indented = line.startswith("    ") or line.startswith("\t")
        if not indented or LIST_MARKER.match(line):
            open_list = bool(LIST_MARKER.match(line))
            out.append(line)
            paragraph = True
            continue
        out.append("" if not open_list and not paragraph else line)
        paragraph = paragraph or open_list
    return out


def strip_indented_code(lines: list[str]) -> list[str]:
    """Blank every indented code block, for a dialect whose code blocks are indented.

    This is not safe over Markdown. A continuation paragraph under a list item is indented
    to the same depth as a code block and is prose. It is right for a doc-comment format
    that has no fences, where an indented span is the only way to write code. A list item's
    own continuation is still indented, so a run following a list item is kept, and only
    one following ordinary prose is blanked.
    """
    out, in_list = [], False
    for line in lines:
        if not line.strip():
            out.append(line)
            continue
        if re.match(r"^\s{0,3}[-*+•]\s", line):
            in_list = True
            out.append(line)
            continue
        if re.match(r"^[ \t]", line):
            out.append(line if in_list else "")
            continue
        in_list = False
        out.append(line)
    return out


# A rustdoc comment, and never an ordinary one. `////` is a rule of slashes rather than a
# doc comment, and rustdoc reads it as neither.
# One space comes off, and never any other whitespace. A doc comment's own indentation is
# content: it is what makes a list continuation a continuation, and in a format whose code
# blocks are indented it is the whole difference between prose and code.
RUST_DOC = re.compile(r"^\s*//(?:/(?!/)|!) ?(.*)$")

# What a Go doc comment has to be adjacent to. A comment run is documentation when a
# declaration follows it with no blank line between, which is go/doc's own rule. The
# second form is an exported struct field or interface method, indented inside its type.
GO_DECLARATION = re.compile(r"^(?:package|func|type|var|const|import)\b|^\s+\w+[\s(*\[]")

# What Go calls exported: a declared name whose first letter is upper case. A method's
# receiver comes between `func` and the name, so it is skipped over rather than read.
GO_EXPORTED = re.compile(
    r"^(?:package\b"
    r"|func\s+(?:\([^)]*\)\s*)?[A-Z]"
    r"|(?:type|var|const)\s+\(?\s*[A-Z])"
    r"|^\s+[A-Z]\w*[\s(*\[]"
)


# What follows a Rust doc comment, and what it says about who reads it. `pub(crate)` and
# its relatives are not public: they are the crate's own vocabulary, which is the internal
# class. A block opened by `pub enum` or `pub trait` makes every item inside it public,
# because a variant and a trait method carry no keyword of their own.
RUST_ATTR = re.compile(r"^\s*#!?\[")
RUST_PUB = re.compile(r"^\s*pub(?!\s*\(\s*(?:crate|super|in)\b)\b")
RUST_OPENS_ALL_PUBLIC = re.compile(r"^\s*pub(?!\s*\(\s*(?:crate|super|in)\b)[^;]*\b(?:enum|trait)\b")
RUST_PRIVATE_MOD = re.compile(r"^\s*(?!pub\b)(?:\w+\s+)*mod\b")


def rust_doc_comments(lines: list[str], public_only: bool = False) -> list[str]:
    """The rustdoc in one Rust file, blank elsewhere, at the lines it stands on.

    `///` and `//!` are the whole surface. An ordinary `//` comment is a note to whoever is
    changing the code rather than prose a stranger reads. A `SAFETY:` comment is the
    clearest case. It states an invariant for a reviewer, and the compiler's own lint holds
    it rather than this one.

    `public_only` keeps the comments on public items and drops the rest. STYLE.md calls the
    first the API reference, and the second a different class with a different reader. What
    it can see is the `pub` keyword, the enclosing block, and nothing else. It
    reads `//!` as public, because a module's own front page is what a reader of that
    module meets. It cannot see a private item re-exported into the public API by a
    `pub use` somewhere else, and reports that item as private.
    """
    out = [""] * len(lines)
    # One entry per open block: True where every item inside is public regardless of its
    # own keyword, False where nothing inside can be, and None where each item speaks for
    # itself. The file's own top level is the last of those.
    scope: list[bool | None] = [None]
    n = 0
    while n < len(lines):
        found = RUST_DOC.match(lines[n])
        if not found:
            depth = lines[n].count("{") - lines[n].count("}")
            if depth > 0:
                inherited = False if scope[-1] is False else None
                if RUST_OPENS_ALL_PUBLIC.match(lines[n]) and scope[-1] is not False:
                    inherited = True
                elif RUST_PRIVATE_MOD.match(lines[n]):
                    inherited = False
                scope.extend([inherited] * depth)
            elif depth < 0:
                del scope[max(1, len(scope) + depth) :]
            n += 1
            continue
        # A run of doc comments, then whatever it documents.
        start, inner = n, lines[n].lstrip().startswith("//!")
        while n < len(lines) and (RUST_DOC.match(lines[n]) or RUST_ATTR.match(lines[n])):
            n += 1
        target = n
        while target < len(lines) and (
            not lines[target].strip() or RUST_ATTR.match(lines[target])
        ):
            target += 1
        if scope[-1] is False:
            public = False
        elif inner or scope[-1] is True:
            public = True
        else:
            public = target < len(lines) and bool(RUST_PUB.match(lines[target]))
        if public or not public_only:
            for i in range(start, n):
                line = RUST_DOC.match(lines[i])
                if line:
                    out[i] = line.group(1)
    return out


def go_doc_comments(lines: list[str], public_only: bool = False) -> list[str]:
    """The doc comments in one Go file, blank elsewhere, at the lines they stand on.

    Go marks a doc comment by position rather than by syntax. It is the comment run
    immediately above a declaration. A run separated from what follows it by a blank line
    is an ordinary comment, and so is one inside a function body. This tells them apart by
    what the run is adjacent to, rather than by parsing the file.

    `public_only` keeps the comments on exported declarations, which STYLE.md calls the API
    reference, and drops the rest. Go states that in the name itself, so this reads the
    declared identifier's first letter and needs nothing else. It cannot see that an
    exported name inside an unexported type is unreachable, and reports that one as
    exported.
    """
    out = [""] * len(lines)
    n = 0
    while n < len(lines):
        if not re.match(r"^\s*//", lines[n]):
            n += 1
            continue
        start, body = n, []
        while n < len(lines) and re.match(r"^\s*//", lines[n]):
            body.append(re.sub(r"^\s*//", "", lines[n]).removeprefix(" "))
            n += 1
        if n < len(lines) and GO_DECLARATION.match(lines[n]):
            if not public_only or GO_EXPORTED.match(lines[n]):
                out[start : start + len(body)] = body
    return out


def prefixed_doc_comments(lines: list[str], prefixes: tuple[str, ...]) -> list[str]:
    """The doc comments a language marks with a fixed opener, at the lines they stand on.

    The generic case, for a language this gate has no dialect of. It takes the openers
    from the configuration, so admitting one is data rather than a change here.
    """
    ordered = sorted(prefixes, key=len, reverse=True)
    out = []
    for line in lines:
        stripped = line.lstrip()
        for prefix in ordered:
            if stripped.startswith(prefix):
                out.append(stripped[len(prefix) :].removeprefix(" "))
                break
        else:
            out.append("")
    return out


BLOCK_OPEN = re.compile(r"^\s*/\*[*!]")
BLOCK_CLOSE = re.compile(r"\*/")
BLOCK_MARGIN = re.compile(r"^\s*\* ?")

# A Python docstring, and what it has to sit under to be documentation. A module's is the
# file's first statement; every other one follows a `def` or a `class`.
PY_DEF = re.compile(r"^\s*(?:async\s+)?(?:def|class)\s+(\w+)")
PY_QUOTE = re.compile(r"^(\s*)[rRbBuU]{0,2}(\"\"\"|\'\'\')")
PY_SKIP = re.compile(r"^\s*(#|from\s|import\s|$)")


def block_doc_comments(lines: list[str], public_only: bool = False) -> list[str]:
    """The `/** ... */` doc comments in one file, at the lines they stand on.

    The shape JSDoc, kernel-doc and Doxygen share. `/*` alone opens an ordinary comment
    and is left, because only the doubled marker means documentation in any of the three.
    A continuation's leading `*` is a margin rather than content, so it comes off.

    `public_only` has nothing to read here. These are comment conventions rather than
    languages, and the visibility of what one documents is the host language's to state.
    A project holding only its public surface in one of them names those files directly.
    """
    out, inside = [], False
    for line in lines:
        if not inside and BLOCK_OPEN.match(line):
            inside = True
            body = line[line.index("/*") + 3 :]
            if BLOCK_CLOSE.search(line):
                inside = False
                body = body[: body.index("*/")] if "*/" in body else body
            out.append(body.strip())
            continue
        if inside:
            if BLOCK_CLOSE.search(line):
                inside = False
                out.append(BLOCK_MARGIN.sub("", line[: line.index("*/")]).rstrip())
                continue
            out.append(BLOCK_MARGIN.sub("", line).rstrip())
            continue
        out.append("")
    return out


def python_doc_comments(lines: list[str], public_only: bool = False) -> list[str]:
    """The docstrings in one Python file, at the lines they stand on.

    A docstring is a string literal in a documentation position, which is the module's
    first statement or the line after a `def` or a `class`. A string anywhere else is a
    value, so position is what tells the two apart, the same way Go's adjacency does.

    `public_only` keeps the module docstring and the ones on names not opening with an
    underscore. That leading underscore is what Python states about who a name is for.
    """
    out = [""] * len(lines)
    n, opened = 0, 0
    while opened < len(lines) and PY_SKIP.match(lines[opened]):
        opened += 1
    while n < len(lines):
        # Where this pass started, so every path out of the loop advances. A documentation
        # position that holds no string is the case that would otherwise spin here.
        here = n
        target = None
        found = PY_DEF.match(lines[n])
        if found:
            # A signature can run to several lines, so the docstring follows the line the
            # colon lands on rather than the `def` itself.
            end = n
            while end < len(lines) and not lines[end].rstrip().endswith(":"):
                end += 1
            target, n = (found.group(1), end + 1) if end < len(lines) else (None, n + 1)
        elif n == opened:
            target = ""
        if target is None:
            n = max(n, here + 1)
            continue
        while n < len(lines) and not lines[n].strip():
            n += 1
        quote = PY_QUOTE.match(lines[n]) if n < len(lines) else None
        if not quote:
            n = max(n, here + 1)
            continue
        if public_only and target.startswith("_") and not target.startswith("__"):
            # Skipped, but the run is still walked so the scan resumes past it.
            keep = False
        else:
            keep = True
        mark, start = quote.group(2), n
        rest = lines[n][quote.end() :]
        if mark in rest:
            if keep:
                out[n] = rest[: rest.index(mark)].strip()
            n += 1
            continue
        if keep:
            out[n] = rest.strip()
        n += 1
        while n < len(lines) and mark not in lines[n]:
            if keep:
                out[n] = lines[n][len(quote.group(1)) :].rstrip()
            n += 1
        if n < len(lines):
            if keep:
                out[n] = lines[n][: lines[n].index(mark)].strip()
            n += 1
        del start
    return out


# TOML, JSON and YAML hold prose in the values of named keys, and a key's value is where
# a project keeps a caveat, a summary, or a description that a generator later renders. A
# quoted run is what carries it in all three, so this reads the runs rather than the
# format, and keeps the line each one stands on.
DATA_KEY = r"^(\s*)(?:-\s*)?[\"\']?(?:%s)[\"\']?\s*[=:]"

# YAML's block scalars, which are the shape an author reaches for when the value is a
# paragraph. `|` keeps the line breaks and `>` folds them into one run, and both take an
# optional indentation indicator and chomping marker. A key holding one is the most likely
# place for prose in a workflow, a chart, or an API description.
DATA_BLOCK = re.compile(r"[|>]([0-9]*)([+-]?)\s*$")
DATA_STRING = re.compile(r"\"\"\"|\'\'\'|\"((?:[^\"\\\\]|\\\\.)*)\"|\'((?:[^\'\\\\]|\\\\.)*)\'")


def data_values(lines: list[str], keys: tuple[str, ...]) -> tuple[list[str], set[int]]:
    """The prose in the named keys of one data file, at the line each value starts on.

    A value is one unit whatever it spans. A run broken over several lines is joined onto
    the line it opens, and the rest are blanked. A reader opens the file there and sees the
    whole thing, and two values never join into one sentence.

    YAML's block scalars are read as well, because `|` and `>` are what an author reaches
    for when the value is a paragraph. A folded run lands on one line. A literal one keeps
    its own line breaks, so a blank line inside it still separates two paragraphs.

    What it cannot see is a bare unquoted scalar, which YAML allows and the other two do
    not. A key naming a value that is not prose is the project's to leave out of `keys`.
    A workflow's `run` is the example worth stating, because it is a script.
    """
    opener = re.compile(DATA_KEY % "|".join(re.escape(k) for k in keys))
    out = [""] * len(lines)
    starts: set[int] = set()
    n = 0
    while n < len(lines):
        found = opener.match(lines[n])
        if not found:
            n += 1
            continue
        # A block scalar: every following line indented past the key belongs to it.
        block = DATA_BLOCK.search(lines[n][found.end() :])
        if block:
            indent, body, end = len(found.group(1)), [], n + 1
            while end < len(lines) and (
                not lines[end].strip()
                or len(lines[end]) - len(lines[end].lstrip()) > indent
            ):
                body.append(lines[end])
                end += 1
            margin = min(
                (len(line) - len(line.lstrip()) for line in body if line.strip()),
                default=0,
            )
            folded = lines[n][found.end() :].lstrip().startswith(">")
            if folded:
                # A folded run is one paragraph, so it lands on the line it opens.
                joined = " ".join(line.strip() for line in body if line.strip())
                if joined and body:
                    out[n + 1] = joined
                    starts.add(n + 2)
            else:
                for offset, line in enumerate(body):
                    out[n + 1 + offset] = line[margin:].rstrip()
                if body:
                    starts.add(n + 2)
            n = end
            continue
        # The value runs until its brackets balance, which is the whole of an array and
        # the rest of the line for a scalar.
        depth, first = 0, True
        while n < len(lines):
            text = lines[n]
            if first:
                text = text[text.index("=") + 1 :] if "=" in text else text.split(":", 1)[-1]
                first = False
            for value in strings(text, lines, n):
                out[value[0]] = (out[value[0]] + " " + value[1]).strip()
                starts.add(value[0] + 1)
                n = max(n, value[2])
            depth += lines[n].count("[") + lines[n].count("{")
            depth -= lines[n].count("]") + lines[n].count("}")
            n += 1
            if depth <= 0:
                break
    return out, starts


def strings(text: str, lines: list[str], n: int) -> list[tuple[int, str, int]]:
    """Every quoted run starting on one line, as (line it opens on, text, line it ends on)."""
    found: list[tuple[int, str, int]] = []
    for match in DATA_STRING.finditer(text):
        if match.group(0) in ('\"\"\"', "\'\'\'"):
            mark, body, end = match.group(0), [text[match.end() :]], n
            while end + 1 < len(lines) and mark not in lines[end + 1]:
                end += 1
                body.append(lines[end].strip())
            if end + 1 < len(lines):
                end += 1
                body.append(lines[end][: lines[end].index(mark)].strip())
            found.append((n, " ".join(part for part in body if part).strip(), end))
            break
        found.append((n, (match.group(1) or match.group(2) or "").strip(), n))
    return found


# Every dialect this gate extracts, and whether its code blocks are indented rather than
# fenced. Rustdoc content is Markdown. Go's is Go's own doc-comment format, which has
# headings, lists and links but no fences and no tables. Python's is neither: the shape a
# docstring takes is the project's, and Markdown is the closest reading of most of them.
# `field_lists` opens a block on a doc-comment field. Where a value begins is the data
# dialect's to say, line by line, because a value runs to as many lines as it needs.
# `nesting` is whether the dialect can express a nested list at all. gofmt indents every
# doc-comment list item by the same three spaces and flattens a deeper one, so in Go an
# indent says nothing about depth, and Rule 27's nesting finding would have no fix.
DIALECTS = {
    "rust": {"indented_code": False, "field_lists": False},
    "go": {"indented_code": True, "field_lists": False, "nesting": False},
    "line": {"indented_code": False, "field_lists": False},
    "block": {"indented_code": False, "field_lists": True},
    "python": {"indented_code": False, "field_lists": True},
    "data": {"indented_code": False, "field_lists": False},
    "c": {"indented_code": False, "field_lists": False},
}


def surface_view(surface: Surface) -> tuple[list[str], set[int], list[int] | None]:
    """One surface as the prose a reader meets, with its fenced code still in place.

    Every dialect but `c` blanks in place, so a view line is the source line and the map is
    None. The `c` dialect sets each comment off with blank lines of its own. Its view
    therefore carries the source line of every view line, and a report translates through
    it. The voice checks read this view, because a prompt character inside a fenced block is
    one of their findings.
    """
    text = surface.path.read_text(encoding="utf-8", errors="replace")
    if surface.dialect == "c":
        view, where = comment_view(text)
        return view, set(), where
    lines = text.split("\n")
    starts: set[int] = set()
    if surface.dialect == "rust":
        lines = rust_doc_comments(lines, surface.public_only)
    elif surface.dialect == "go":
        lines = go_doc_comments(lines, surface.public_only)
    elif surface.dialect == "block":
        lines = block_doc_comments(lines, surface.public_only)
    elif surface.dialect == "python":
        lines = python_doc_comments(lines, surface.public_only)
    elif surface.dialect == "data":
        lines, starts = data_values(lines, surface.keys)
    elif surface.dialect == "line":
        lines = prefixed_doc_comments(lines, surface.prefixes)
    return lines, starts, None


def prose_lines(surface: Surface) -> tuple[list[str], set[int], list[int] | None]:
    """One surface as prose, blank wherever it is not, with the map back to its source.

    Every report names a file and a line, so a reader opens the file there and sees the
    thing. Blanking rather than dropping is what keeps that true through extraction. The
    `c` dialect reads its own indented code while it builds the view, so nothing more is
    blanked here.
    """
    lines, starts, where = surface_view(surface)
    lines = strip_code(lines)
    if surface.dialect == "c":
        return lines, starts, where
    if DIALECTS.get(surface.dialect, {}).get("indented_code"):
        return strip_indented_code(lines), starts, where
    return strip_markdown_indented_code(lines), starts, where


def to_source(where: list[int] | None, n: int) -> int:
    """The source line a view line came from, or the line itself where the view is the source."""
    return where[n - 1] if where and 0 < n <= len(where) else n


def relocate(findings: list[str], rel: str, where: list[int] | None) -> list[str]:
    """Findings reported at view lines, moved to the source lines a reader opens."""
    if not where:
        return findings
    head = re.compile(r"^" + re.escape(rel) + r":(\d+):")
    return [head.sub(lambda m: f"{rel}:{to_source(where, int(m.group(1)))}:", f, count=1)
            for f in findings]


def count_words(text: str, is_item: bool, units: re.Pattern[str]) -> int:
    """Rule 26's counting table."""
    s = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    if is_item:
        s = re.sub(r"^\s*\*\*[^*]+\*\*", "LABEL", s)
    s = re.sub(r"`[^`]*`", "CODE", s)
    s = re.sub(r"\([^)]*\)", "PAREN", s)
    s = re.sub(r"\b\d[\d.,]*\s+" + units.pattern + r"\b", "NUM", s)
    s = s.replace("**", "").replace("*", "").replace("_", "")
    return len([w for w in s.split() if w.strip(".,:")])


def split_sentences(text: str, names: set[str]) -> list[str]:
    """Split on terminal punctuation, leaving abbreviations and version numbers alone.

    A technical name is a proper noun that happens to be lowercase, and one opens a
    sentence as readily as any other word. Without the profile's list here, "checksum.
    btrfs stores ..." reads as one long sentence and the length rule fires on prose that
    is already two short ones.
    """
    s = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    s = re.sub(r"`[^`]*`", CODE_STANDIN, s)
    lower = "|".join(
        sorted((re.escape(n) for n in names | {CODE_STANDIN}), key=len, reverse=True)
    )
    # Emphasis is not a sentence boundary either way round. A stop inside a bold run
    # ("... can hold.** A name ...") ends its sentence, and a run that opens the next one
    # ("... else. **Every reference ...") starts it, so the markers stay with the
    # terminator and with the opener rather than hiding either.
    # A terminator can sit inside a closing mark. `... a dead end." "Resume ...` is two
    # sentences, and reading it as one taxes prose that is already inside the limit.
    emphasis = r"(?:\*\*|\*|_|\"|\'|\))*"
    opener = emphasis + r"(?:[A-Z`\"(\[]" + (f"|(?i:{lower})\\b" if lower else "") + r")"
    # A lone capital before the stop is an initial. Two or more are an acronym, which ends
    # a sentence as readily as any other word: "... to two seconds. FAT records ..." is two
    # sentences, and reading it as one taxes a sentence that is already inside the limit.
    # A lone capital is also a variable, and "... tiles both N and K. The gain ..." is two
    # sentences too. An initial is followed by a name, so the stop ends a sentence when the
    # next word is a function word that opens one ("The", "It", "A"), unless that word is
    # itself an initial ("J. A. Smith").
    function = "|".join(sorted((w.capitalize() for w in STOPWORDS), key=len, reverse=True))
    after_variable = r"(?<=\b[A-Z])(?=[.!?]" + emphasis + r"\s+" + emphasis + r"(?:" + function + r")[\s,])"
    parts = re.split(
        r"(?:(?<!\b[A-Z])|" + after_variable + r")(?<!§)([.!?]" + emphasis + r")\s+(?=" + opener + r")", s
    )
    out, buf = [], ""
    for index, chunk in enumerate(parts):
        buf += chunk
        if index % 2:
            out.append(buf.strip())
            buf = ""
    if buf.strip():
        out.append(buf.strip())
    return [p for p in out if len(p.split()) >= 3]


class Block:
    """A paragraph or one list item, with the line it starts on.

    A continuation is an indented paragraph after a blank line inside a list item. It is
    held to the sentence rules as prose. It belongs to the list, so the item after it needs
    no lead-in of its own.
    """

    def __init__(self, line: int, is_item: bool, indent: int, after_heading: bool = False) -> None:
        self.line, self.is_item, self.indent = line, is_item, indent
        self.after_heading = after_heading
        self.continuation = False
        self.parts: list[str] = []
        self.lines: list[int] = []

    @property
    def text(self) -> str:
        return " ".join(p.strip() for p in self.parts).strip()

    def locate(self, literal: str) -> int:
        """The line one of this block's own source lines carries `literal` on.

        A block is several lines joined, and a rule that matches the joined text has no
        line of its own. Searching the parts back gives the reported line the property
        that matters: a reader opens the file there and sees the thing. Where the match
        straddles a line break it is on neither part, and the block's first line is the
        honest answer.
        """
        for n, part in zip(self.lines, self.parts):
            if literal in part:
                return n
        return self.line


def blocks(
    lines: list[str], fields: bool = False, starts: set[int] | None = None
) -> list[Block]:
    """The paragraphs and list items in one surface.

    `fields` opens a block on a doc-comment field, for a dialect whose convention is a
    field list. `starts` names the lines that begin a value, for a dialect whose lines are
    separate values rather than one flowing paragraph. Two values never join into one
    sentence, and a value that runs to several lines is still measured as what it is.
    """
    found: list[Block] = []
    current: Block | None = None
    heading = False
    in_list = False
    comment = False
    for n, raw in enumerate(lines, 1):
        line = raw.rstrip()
        bare = re.sub(r"^\s*>\s?", "", line)
        # An HTML comment runs to its terminator, and its body is not prose. Reading the
        # second line of one as a paragraph gives the list below it a lead-in that never
        # ends in a colon, which is a finding about a comment nobody renders.
        if comment:
            comment = "-->" not in bare
            current = None
            continue
        # A heading is one to six marks and a space. `#include` in a comment and `#123` in
        # an issue reference are prose, and reading either as a heading drops its sentence.
        if re.match(r"\s{0,3}#{1,6}(?:\s|$)", bare):
            current, heading, in_list = None, True, False
            continue
        if not bare.strip() or bare.lstrip().startswith("|"):
            current = None
            continue
        # Markup opens a block, and only where no block is already running. A wrapped
        # continuation beginning with `<` is prose, and reading it as markup drops it
        # from every length rule: "Its lines are `DATA <member>" / "<size> SHA512 <hex>`
        # and there is no field naming the package" is one sentence. An HTML comment is
        # never a continuation, so it ends a block wherever it sits.
        if bare.lstrip().startswith("<!--"):
            comment = "-->" not in bare
            current = None
            continue
        if current is None and (
            bare.lstrip().startswith("<") or LINK_DEFINITION.match(bare)
        ):
            continue
        # A field opens a block without being a list item. Rule 27's lead-in colon asks a
        # paragraph above a list to end in one, and a summary above a field list is not
        # that shape: the fields are the item's own parts rather than a list it introduces.
        if (starts and n in starts) or (fields and FIELD_MARKER.match(bare)):
            current = Block(n, False, len(bare) - len(bare.lstrip()), heading)
            heading = False
            current.parts.append(bare.strip())
            current.lines.append(n)
            found.append(current)
            continue
        marker = LIST_MARKER.match(bare)
        if marker:
            current = Block(n, True, len(marker.group(1)), heading)
            heading, in_list = False, True
            current.parts.append(marker.group(2))
            current.lines.append(n)
            found.append(current)
            continue
        if current is None:
            # The indent is kept for a paragraph too, because it is what tells a list
            # item's continuation paragraph from a paragraph at the top level. The two
            # are the same shape and only one of them ends a list.
            current = Block(n, False, len(bare) - len(bare.lstrip()), heading)
            heading = False
            if in_list and len(bare) - len(bare.lstrip()) >= 2:
                current.continuation = True
            else:
                in_list = False
            found.append(current)
        current.parts.append(bare)
        current.lines.append(n)
    return found


EXEMPT_MARKER = re.compile(r"^\s*(?:<!--\s*)?prose-lint:\s*(off|on)\b")


def exempt_lines(lines: list[str]) -> set[int]:
    """Line numbers inside a `prose-lint: off` region.

    Rule 29 exempts legal boilerplate, and a license grant is recognized by shape rather
    than read for clarity. In Markdown the marker is an HTML comment. It renders as nothing
    and still says in the file itself why those paragraphs are not held to the rules.

    A doc comment in a language whose renderer shows an HTML comment verbatim writes the
    marker bare instead. Both forms have to open the line, because a sentence naming the
    marker is prose about it rather than a use of it.
    """
    off, spans = None, set()
    for n, line in enumerate(lines, 1):
        found = EXEMPT_MARKER.match(line)
        if found and found.group(1) == "off":
            off = n
        elif found and off is not None:
            spans |= set(range(off, n + 1))
            off = None
    if off is not None:
        spans |= set(range(off, len(lines) + 1))
    return spans


# Rule 24's enumeration, and Rule 18's repeated label. Both are shapes rather than counts,
# so each carries its own reasoning where it is applied.
ENUMERATION = re.compile(r"((?:[^,;:()]+,\s+){3,}(?:and|or)\s+[^,;:.()]+)")
ENUM_ITEM_WORDS = 4
BOLD_LEAD = re.compile(r"^\s*(?:(?:[-*+]|\d+\.)\s+)?\*\*([^*]+)\*\*")


def enumerated_items(sentence: str, max_words: int = ENUM_ITEM_WORDS) -> list[str]:
    """The longest run of comma-separated items a sentence carries, terminated by and/or.

    Rule 24 turns four or more parallel items into a vertical list, and the run has to be
    told from an ordinary compound sentence. Two things separate them. An enumeration ends
    on a serial comma before `and` or `or`, and its items are short. A run of
    clauses is prose that happens to have commas in it. A slash-joined member counts once,
    which is Rule 24's own instruction and the reason `ext2/ext3/ext4` does not read as
    three.
    """
    found = ENUMERATION.search(sentence)
    if not found:
        return []
    items = [i.strip() for i in found.group(1).split(", ") if i.strip()]
    # Only the run's last member carries the conjunction, and splitting on every `and`
    # instead would cut "resolves the toolchain and build dependencies" into two short
    # items and read a clause as an enumeration.
    items[-1] = re.sub(r"^(?:and|or)\s+", "", items[-1])
    # A bold run opening the block is Rule 18's label for what follows, so it is not a
    # member of any enumeration inside it.
    if items and re.match(r"^\*\*[^*]+\*\*", items[0]):
        items = items[1:]
    if any(len(item.split()) > max_words for item in items):
        return []
    return items


def repeated_labels(
    lines: list[str], skip: set[int], block_words: int, per_section: int
) -> list[tuple[int, str, int]]:
    """Bold lead-ins that repeat down one section, with the section's count.

    Rule 18 says a label a reader skims for is a heading. Where the same shape repeats down
    a section, those are headings written as bold. It exempts a list whose items are
    genuinely parallel and short, where the bold term is a definition label rather than a
    section. Length is what tells the two apart here: a label introducing a paragraph of
    prose is a section, and a four-word gloss is a label.
    """
    out: list[tuple[int, str, int]] = []
    section: list[tuple[int, str]] = []

    def close() -> None:
        if len(section) >= per_section:
            out.extend((n, label, len(section)) for n, label in section)
        section.clear()

    n, total = 0, len(lines)
    while n < total:
        line = lines[n]
        n += 1
        if line.lstrip().startswith("#"):
            close()
            continue
        if n in skip:
            continue
        found = BOLD_LEAD.match(line)
        if not found:
            continue
        # The block this label introduces, so a long one reads as a section and a short
        # one as a definition label. A blank line ends it.
        body, cursor = [line], n
        while cursor < total and lines[cursor].strip():
            body.append(lines[cursor])
            cursor += 1
        if len(" ".join(body).split()) > block_words:
            section.append((n, found.group(1)))
    close()
    return out


# The ratchet's unit. A finding count is the wrong one: Rule 26 says a long sentence is
# split rather than compressed, and splitting a 56-word sentence into two 28-word ones
# takes the count from one finding to two. A ratchet on counts then fails the exact edit
# the guide asks for, on prose that got strictly better. Weighing a length finding by how
# far over the limit it is makes the number fall: 31 becomes 6.
WEIGHT = re.compile(r"Rule 26: (\d+) (?:words, limit|sentences in one paragraph, limit) (\d+)")


def weigh(finding: str) -> int:
    """How much one finding counts toward a surface's ratchet.

    A length finding weighs its overrun, so any split lowers it and no split raises it.
    Every other finding weighs one, because it is present or it is not.
    """
    over = WEIGHT.search(finding)
    return max(1, int(over.group(1)) - int(over.group(2))) if over else 1


def burden(findings: list[str]) -> int:
    """The weight of every finding on one surface."""
    return sum(weigh(f) for f in findings)


def limits_for(surface: Surface, config: Config) -> tuple[int, int]:
    """Rule 26's two numbers for one surface, after any override the project set.

    The limits are global unless a project says otherwise. A sentence is as long in a
    reference note as in a README, and a reader gives up at the same place. What does vary
    is a surface whose shape is not prose at the sentence level, and an override is how a
    project says which. The first entry matching a surface wins.
    """
    for entry in config.overrides:
        for pattern in entry.get("include", []):
            if fnmatch(surface.rel, pattern):
                return (
                    entry.get("sentence_words", config.max_sentence),
                    entry.get("paragraph_sentences", config.max_paragraph),
                )
    return config.max_sentence, config.max_paragraph


def spelt(text: str, config: Config) -> list[tuple[str, str]]:
    """Every British spelling in one run of prose, with what the guide writes instead."""
    if not config.spellings:
        return []
    return [
        (found, config.spellings[found.lower()])
        for found in re.findall(r"[A-Za-z]+", text)
        if found.lower() in config.spellings
    ]


def check(
    surface: Surface,
    names: set[str],
    config: Config,
    variants: list[tuple[str, str]] | None = None,
) -> tuple[list[str], dict[str, list[str]]]:
    rel = surface.rel
    lines, starts, where = prose_lines(surface)
    skip = exempt_lines(lines)
    max_sentence, max_paragraph = limits_for(surface, config)
    fields = DIALECTS.get(surface.dialect, {}).get("field_lists", False)
    fails: list[str] = []
    soft: dict[str, list[str]] = {
        "emdash": [], "loose_modal": [], "recommendation": [], "mid_if": []
    }

    for block in blocks(lines, fields, starts):
        if block.line in skip:
            continue
        text = block.text
        sentences = split_sentences(text, names)
        for sentence in sentences:
            words = count_words(
                sentence, block.is_item and sentence is sentences[0], config.units
            )
            if words > max_sentence:
                fails.append(
                    f"{rel}:{block.line}: Rule 26: {words} words, limit {max_sentence}\n"
                    f"    {sentence[:140]}"
                )
        if len(sentences) > max_paragraph:
            fails.append(
                f"{rel}:{block.line}: Rule 26: {len(sentences)} sentences in one paragraph, "
                f"limit {max_paragraph}"
            )
        for sentence in sentences:
            # Rule 27 forbids a nested list, so a run inside a list item has no formatting
            # fix available and wants a reader restructuring the section instead.
            items = [] if block.is_item else enumerated_items(sentence)
            if config.enumeration_items and len(items) >= config.enumeration_items:
                fails.append(
                    f"{rel}:{block.locate(items[0])}: Rule 24: {len(items)} parallel items "
                    f"inside a sentence\n    {sentence[:140]}"
                )
        prose = ENTITY.sub("", re.sub(r"`[^`]*`", "", text))
        for variant, prefer in variants or []:
            if re.search(r"(?<![\w-])" + re.escape(variant) + r"(?![\w-])", prose, re.I):
                fails.append(
                    f"{rel}:{block.locate(variant)}: Rule 15: {variant!r} is ruled out by "
                    f"the profile, which writes {prefer!r}\n    {text[:140]}"
                )
        if ";" in prose:
            fails.append(f"{rel}:{block.line}: Rule 16: semicolon\n    {text[:140]}")
        for hit in config.gated.findall(prose) if config.gated else []:
            fails.append(f"{rel}:{block.line}: Rule 15: `{hit.lower()}`\n    {text[:140]}")
        for variant, prefer in spelt(prose, config):
            fails.append(
                f"{rel}:{block.locate(variant)}: Rule 15: {variant!r} is British, and the "
                f"guide writes {prefer!r}\n    {text[:140]}"
            )
        for _ in EMOJI.findall(text):
            fails.append(f"{rel}:{block.line}: Rule 15: emoji\n    {text[:140]}")
        for part, n in zip(block.parts, block.lines):
            if EXEMPT_MARKER.match(part):
                continue
            for _ in EM_DASH.findall(re.sub(r"`[^`]*`", "", part)):
                soft["emdash"].append(
                    f"{rel}:{n}: Rule 16: em dash, so zero or justified\n    {part.strip()[:140]}"
                )
        for hit in config.loose.findall(prose) if config.loose else []:
            soft["loose_modal"].append(
                f"{rel}:{block.locate(hit)}: Rule 15: `{hit.lower()}`, exempt inside a "
                f"counterfactual\n    {text[:140]}"
            )
        for hit in config.recommendation.findall(prose) if config.recommendation else []:
            soft["recommendation"].append(
                f"{rel}:{block.locate(hit)}: Rule 15: `{hit.lower()}`, a recommendation "
                f"with known exceptions only\n    {text[:140]}"
            )
        for hit in re.findall(r"[a-z,]\s+(?:if|when)\b", prose):
            soft["mid_if"].append(
                f"{rel}:{block.locate(hit)}: Rule 28: mid-sentence `if`/`when`, or an "
                f"embedded question\n    {text[:140]}"
            )

        if block.is_item:
            body = re.sub(r"^\s*(?:\*\*|__)?", "", text)
            first = body[:1]
            opener = re.split(r"[\s,.:]", body, 1)[0].strip("`*_[]()")
            identifier = (
                opener.lower() in names
                or any(c.isdigit() for c in opener)
                or any(c.isupper() for c in opener[1:])
                or body[:1] in "`[<"
            )
            if first and first.isalpha() and first.islower() and not identifier:
                fails.append(f"{rel}:{block.line}: Rule 27: item starts lowercase\n    {text[:140]}")
            if text.rstrip().endswith((",", ";")):
                fails.append(f"{rel}:{block.line}: Rule 27: item ends with a comma or semicolon")
            nesting = DIALECTS.get(surface.dialect, {}).get("nesting", True)
            if nesting and block.indent >= 2:
                fails.append(f"{rel}:{block.line}: Rule 27: nested list item")

    labels = (
        repeated_labels(lines, skip, config.label_block_words, config.labels_per_section)
        if config.label_block_words
        else []
    )
    # A table row is not a sentence, so no length rule reaches it. A semicolon, a gated
    # modal, and a term the profile rules out are each wrong in a cell for the same reason
    # they are wrong in a paragraph, and a table is where a project's densest prose often
    # is. A feature matrix is also where a second name for one concept survives longest,
    # because a cell is read on its own and never beside the paragraph that named it.
    for n, raw in enumerate(lines, 1):
        if n in skip or not raw.lstrip().startswith("|") or set(raw.strip()) <= set("| -:"):
            continue
        cells = ENTITY.sub("", re.sub(r"`[^`]*`", "", raw))
        if ";" in cells:
            fails.append(f"{rel}:{n}: Rule 16: semicolon in a table cell\n    {raw.strip()[:140]}")
        for hit in config.gated.findall(cells) if config.gated else []:
            fails.append(
                f"{rel}:{n}: Rule 15: `{hit.lower()}` in a table cell\n    {raw.strip()[:140]}"
            )
        for variant, prefer in spelt(cells, config):
            fails.append(
                f"{rel}:{n}: Rule 15: {variant!r} in a table cell is British, and the guide "
                f"writes {prefer!r}\n    {raw.strip()[:140]}"
            )
        for _ in EMOJI.findall(raw):
            fails.append(f"{rel}:{n}: Rule 15: emoji in a table cell\n    {raw.strip()[:140]}")
        for variant, prefer in variants or []:
            if re.search(r"(?<![\w-])" + re.escape(variant) + r"(?![\w-])", cells, re.I):
                fails.append(
                    f"{rel}:{n}: Rule 15: {variant!r} in a table cell is ruled out by the "
                    f"profile, which writes {prefer!r}\n    {raw.strip()[:140]}"
                )

    for line_no, label, count in labels:
        fails.append(
            f"{rel}:{line_no}: Rule 18: bold lead-in {label!r} is one of {count} in its "
            f"section, so these are headings"
        )

    for block, previous in missing_leadins(blocks(lines, fields, starts), skip):
        fails.append(
            f"{rel}:{block.line}: Rule 27: the list lead-in does not end with a colon\n"
            f"    {previous.text[-100:]}"
        )
    return relocate(fails, rel, where), {k: relocate(v, rel, where) for k, v in soft.items()}


# Rule 4's labeled lines sit directly under a heading or a table and qualify what follows,
# so a list after one is introduced by the heading, not by the label.
GENRE_LABEL = re.compile(r"^\*\*(?:Conditions|Not characterized):\*\*")


def missing_leadins(seen: list[Block], skip: set[int]) -> list[tuple[Block, Block]]:
    """Rule 27's lead-in colon: a list that follows a paragraph needs one.

    An indented paragraph is a list item's own continuation, so the list did not end and
    the item below it needs no fresh lead-in. Reading it as one puts a finding on every
    list whose items run to more than a paragraph.
    """
    out = []
    for i, block in enumerate(seen):
        if not block.is_item or i == 0 or block.line in skip:
            continue
        previous = seen[i - 1]
        if previous.is_item or previous.continuation or previous.indent > 0:
            continue
        if block.after_heading:
            continue
        if GENRE_LABEL.match(previous.text) and previous.after_heading:
            continue
        if not previous.text.rstrip().endswith(":"):
            out.append((block, previous))
    return out


# ---------------------------------------------------------------------------------------
# The voice mode.


@dataclass
class Voice:
    """What the voice checks read from the profile and the configuration."""

    house: list[tuple[str, re.Pattern[str], list[str]]]
    record_prefixes: list[str]
    record_files: list[str]
    skeletons: dict[str, tuple[list[str], list[tuple[str, bool]]]]
    adopted: list[str]
    internal: list[re.Pattern[str]]
    provenance_exempt: list[re.Pattern[str]]
    residue: list[re.Pattern[str]]
    erratum_fields: list[str]
    glossary: Path | None
    glossary_terms: list[str]
    define_files: list[str]
    modes: dict[str, str]
    problems: list[str]


def first_table(body: str) -> list[list[str]]:
    """The rows of the first markdown table in a section, header and separator dropped."""
    rows, seen_sep = [], False
    for line in body.splitlines():
        s = line.strip()
        if not s.startswith("|"):
            if rows or seen_sep:
                break
            continue
        # An escaped pipe is a character inside a cell, which a regular expression needs.
        cells = [c.strip().replace("\\|", "|")
                 for c in re.split(r"(?<!\\)\|", s.strip().strip("|"))]
        if all(re.fullmatch(r":?-{3,}:?", c) for c in cells if c):
            seen_sep = True
            continue
        if seen_sep:
            rows.append(cells)
    return rows


def first_code(text: str) -> str | None:
    found = re.search(r"`([^`]+)`", text)
    return found.group(1) if found else None


def house_hits(text: str, house: list[tuple[str, re.Pattern[str], list[str]]]) -> list[tuple[str, str]]:
    """Each house word in a text, as (the form found, the listed word).

    A phrase in a word's third column is its standard sense and is removed first. A pipe escaped
    in prose, "gate\\|up", reads as the table cell's "gate|up" does.
    """
    text = text.replace("\\|", "|")
    out = []
    for word, pattern, exempt in house:
        bare = text
        for phrase in exempt:
            bare = re.sub(re.escape(phrase), " ", bare, flags=re.I)
        out += [(m.group(0), word) for m in pattern.finditer(bare)]
    return out


def load_voice(config: Config) -> Voice:
    """Read the house words, the record zones and the skeletons from the profile.

    Each section has the same contract as the vocabulary: a heading the configuration
    names, and a shape the template states. A configured section that cannot be read is
    reported, because it fails quietly otherwise: the gate runs, and finds nothing.
    """
    problems: list[str] = []
    house: list[tuple[str, re.Pattern[str], list[str]]] = []
    body = profile_section(config, config.house_words_heading)
    if body is None:
        problems.append(f"no {config.house_words_heading!r} section in the profile")
    else:
        for row in first_table(body):
            cell = row[0].strip().strip("`").strip()
            exempt = []
            if len(row) >= 3:
                exempt = [e.strip().strip("`*").strip() for e in row[2].split(",") if e.strip()]
            # A cell written between slashes is one regular expression, for a word that is a
            # house word only in context. Anything else is a comma-separated list of forms.
            if len(cell) > 2 and cell.startswith("/") and cell.endswith("/"):
                try:
                    pattern = re.compile(cell[1:-1], re.I)
                except re.error as error:
                    problems.append(f"house word {cell} is not a regular expression: {error}")
                    continue
                house.append((cell, pattern, [e for e in exempt if e]))
                continue
            forms = [f.strip().strip("`*").strip() for f in cell.split(",") if f.strip()]
            forms = [f for f in forms if f]
            if not forms:
                continue
            pattern = re.compile(
                r"\b(?:" + "|".join(re.escape(f) for f in sorted(forms, key=len, reverse=True))
                + r")\b", re.I,
            )
            house.append((forms[0], pattern, [e for e in exempt if e]))

    record_prefixes: list[str] = []
    body = profile_section(config, config.record_zones_heading)
    if body is None:
        problems.append(f"no {config.record_zones_heading!r} section in the profile")
    else:
        for line in body.splitlines():
            if LIST_MARKER.match(line):
                code = first_code(line)
                if code:
                    record_prefixes.append(code.lower())

    skeletons: dict[str, tuple[list[str], list[tuple[str, bool]]]] = {}
    body = profile_section(config, config.skeletons_heading)
    if body is not None:
        for chunk in re.split(r"^###\s+", body, flags=re.M)[1:]:
            name, _, rest = chunk.partition("\n")
            globs: list[str] = []
            items: list[tuple[str, bool]] = []
            for line in rest.splitlines():
                if line.lower().startswith("applies to"):
                    globs += re.findall(r"`([^`]+)`", line)
                elif re.match(r"^\s*\d+\.\s", line):
                    code = first_code(line)
                    if code:
                        items.append((code.lower(), "(optional)" in line.lower()))
            skeletons[name.strip()] = (globs, items)

    voice = config.voice
    glossary = (config.root / voice["glossary"]) if voice.get("glossary") else None
    terms: list[str] = []
    if glossary is not None:
        if glossary.is_file():
            for row in first_table(glossary.read_text(encoding="utf-8")):
                term = row[0].strip().strip("*`").strip()
                if term:
                    terms.append(term)
        else:
            problems.append(f"the configured glossary {glossary} does not exist")

    if voice.get("subjects") and not voice.get("quantity_unit"):
        problems.append("[voice] lists `subjects` and no `quantity_unit`, so the quantity "
                        "family reads nothing")

    modes = {family: "report" for family in FAMILIES}
    for family, mode in voice.get("modes", {}).items():
        if family not in FAMILIES or mode not in MODES:
            problems.append(f"unknown family or mode in [voice.modes]: {family} = {mode!r}")
            continue
        modes[family] = mode

    return Voice(
        house=house,
        record_prefixes=record_prefixes,
        record_files=voice.get("record_files", []),
        skeletons=skeletons,
        adopted=voice.get("skeletons_adopted", []),
        internal=_rx(voice.get("internal_patterns", [])),
        provenance_exempt=_rx(voice.get("provenance_exempt", [])),
        residue=RESIDUE + _rx(voice.get("residue_patterns", []), re.I),
        erratum_fields=voice.get("erratum_fields", ["Conditions", "Symptom", "Workaround", "Status"]),
        glossary=glossary,
        glossary_terms=terms,
        define_files=voice.get("define_before_use", []),
        modes=modes,
        problems=problems,
    )


def heading_text(raw: str) -> str:
    found = HEADING.match(raw)
    return found.group(2) if found else ""


# A relative clause after a noun keeps a heading a noun phrase: "Refusal for pools whose divisor
# can lag" names a refusal. "That" is left out, because "Check that the clock is set" is a
# sentence, and a heading that opens on the pronoun ("Where the time goes") is a question.
RELATIVE = re.compile(r"(?<=\w )\b(?:whose|which|who|whom|where)\b", re.I)


def heading_words(bare: str) -> list[str]:
    """A heading's words, less a trailing relative clause holding the one verb of its own.

    A clause with two finite verbs ("The path which fails is the int8 one") hides the
    heading's own verb, so it stays and the heading reads as a sentence.
    """
    words = re.findall(r"[A-Za-z']+", bare.lower())
    found = RELATIVE.search(bare)
    if found:
        clause = re.findall(r"[A-Za-z']+", bare[found.end():].lower())
        if sum(w in FINITE_VERBS for w in clause) == 1:
            return re.findall(r"[A-Za-z']+", bare[:found.start()].lower())
    return words


def record_lines(lines: list[str], prefixes: list[str]) -> set[int]:
    """Line numbers inside the record: a heading that opens with a record-zone name, to
    the next heading at its level or above."""
    inside: set[int] = set()
    level = None
    for n, line in enumerate(lines, 1):
        found = HEADING.match(line)
        if found:
            depth = len(found.group(1))
            if level is not None and depth <= level:
                level = None
            title = re.sub(r"^[\dA-Z]+[.)]\s+", "", found.group(2)).strip().lower()
            if level is None and any(title.startswith(p) for p in prefixes):
                level = depth
        if level is not None:
            inside.add(n)
    return inside


def hits(patterns: list[re.Pattern[str]], text: str) -> list[str]:
    return [m.group(0) for p in patterns for m in p.finditer(text)]


def without_brackets(text: str) -> str:
    """The text with every bracketed span that is not a link label removed."""
    return re.sub(r"\[[^\[\]]*\](?!\()", " ", text)


def md_cells(row: str) -> list[str]:
    return [c.strip().replace("\\|", "|") for c in re.split(r"(?<!\\)\|", row.strip().strip("|"))]


def md_tables(lines: list[str]) -> list[tuple[int, list[str], list[tuple[int, list[str]]]]]:
    """Each table on a page, as (header line, header cells, [(row line, row cells)])."""
    out = []
    n = 0
    while n < len(lines):
        head = lines[n].strip()
        sep = lines[n + 1].strip() if n + 1 < len(lines) else ""
        if head.startswith("|") and sep.startswith("|") and "-" in sep and re.fullmatch(r"[|:\-\s]+", sep):
            rows = []
            m = n + 2
            while m < len(lines) and lines[m].strip().startswith("|"):
                rows.append((m + 1, md_cells(lines[m])))
                m += 1
            out.append((n + 1, md_cells(head), rows))
            n = m
            continue
        n += 1
    return out


def tokens(text: str) -> list[str]:
    """Lowercase word tokens, with link targets dropped and code spans kept as words."""
    s = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text).replace("`", "").lower()
    return re.findall(r"[a-z0-9~][a-z0-9_.~%/+-]*[a-z0-9%]|[a-z0-9]", s)


def stem(word: str) -> str:
    for suffix in ("ing", "ed", "es", "s"):
        if len(word) > len(suffix) + 3 and word.endswith(suffix):
            return word[: -len(suffix)]
    return word


def content_words(text: str) -> set[str]:
    return {stem(w) for w in tokens(text) if w not in STOPWORDS and (len(w) >= 3 or w[0].isdigit())}


def quantity_tests(config: Config):
    """The test names a text gives, by the `quantity_test` pattern.

    A figure is compared only with figures of the same subject, test and precision, so the
    test is what keeps two configurations apart. An empty pattern, the default, makes every
    figure of a subject one test, for a project whose configurations have no name. Each
    subject's readings are then listed together, and the reviewer tells a second reading of
    one configuration from a reading of another.
    """
    pattern = config.voice.get("quantity_test", "")
    if not pattern:
        return lambda text: ["any test"]
    rx = re.compile(pattern)
    return lambda text: [m.group(0) for m in rx.finditer(text)]
@dataclass(frozen=True)
class Roles:
    """Whose figure a reading is, from `[voice]`.

    `baseline` names the party a project compares against, such as a reference
    implementation, and `device` labels everything else. `other` matches the words that make
    a figure another quantity entirely, which then reads as no reading at all. Empty, a
    `baseline` puts every figure on the device, and an empty `other` excludes nothing.
    """

    baseline: str = ""
    device: str = "device"
    other: re.Pattern[str] | None = None

    @classmethod
    def load(cls, voice: dict) -> "Roles":
        other = voice.get("quantity_exclude", "")
        return cls(voice.get("quantity_baseline", ""), voice.get("quantity_device", "device"),
                   re.compile(other, re.I) if other else None)


def quantity_role(before: str, after: str, roles: Roles = Roles()) -> str | None:
    """Whose figure one is: the baseline's when the words just around it name the baseline.

    `before` and `after` are the text between the figure and its neighboring figures. None for
    a figure `roles.other` marks as another quantity. A figure in a relative clause after the
    baseline ("4.0x a CPU backend ..., which runs at 5.47 t/s") is the baseline's. A figure
    followed by a ratio against the baseline is the device's.
    """
    clause = re.split(r",|;|\band\b", after, maxsplit=1)[0]
    if roles.other and (roles.other.search(before[-30:]) or roles.other.search(clause[:30])):
        return None
    if not roles.baseline:
        return roles.device
    named = re.compile(r"\b" + re.escape(roles.baseline) + r"\b")
    against = re.compile(r"^[^.;]*?\d(?:\.\d+)?x\s+(?:(?:a|an|the)\s+)?(?:[\w-]+\s+)?"
                         + re.escape(roles.baseline) + r"\b")
    if re.search(r",\s+which\b", before) and named.search(before):
        return roles.baseline
    near = before[-25:] + " " + ("" if against.search(after[:40]) else after[:25])
    return roles.baseline if named.search(near) else roles.device


def figure_owner(sentence: str, start: int, mentions: list[tuple[int, int, str]],
                 heading: str | None, baseline: str = "") -> str | None:
    """The subject whose figure the one at `start` is, or None.

    `mentions` is each configured subject named in the sentence, as (start, end, name), and
    `heading` the subject the nearest heading names. A figure right after a possessive is the
    possessor's: "against Llama-3.2-3B's 61.79 t/s" is Llama's. Likewise, "against resident
    fp16's 15.1 t/s" is another configuration's, so it is no reading of the sentence's subject.
    Otherwise a subject named only in a possessive after the figure is the party it is compared
    with. The figure then falls to the sentence's other subject or to the heading's. The
    baseline's figure stays the subject's, and its role names the baseline.
    """
    for s, e, name in mentions:
        if re.fullmatch(r"['’]s\s+~?", sentence[e:start]):
            return name
    held = re.search(r"([\w.-]+)['’]s\s+~?$", sentence[:start])
    if held and held.group(1) != baseline:
        return None
    eligible = {name for s, e, name in mentions if s < start or sentence[e:e + 2] not in ("'s", "’s")}
    if len(eligible) == 1:
        return eligible.pop()
    return heading if not eligible else None
# A datatype names what a sentence is about as readily as a model does: "tf32's MAC rate is ..."
# and "bf16's MAC rate is ..." are two facts in one template, not one fact stated twice.
DATATYPE = re.compile(r"\b(?:u?int(?:4|8|16|32|64)|fp(?:8|16|32|64)|bf16|tf32|F16|BF16|F32|"
                      r"I?Q\d_\w+|MXFP4)\b")
MD_LINK = re.compile(r"\[([^\]]*)\]\(([^)\s]*)[^)]*\)")
EVIDENCE_TAG = re.compile(r"\[[A-Za-z][^\[\]]*\](?!\()")


def slug(title: str) -> str:
    """GitHub's heading anchor: lowercase, punctuation dropped, spaces to hyphens."""
    t = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", title)
    t = re.sub(r"<[^>]+>", "", t)
    t = "".join(c for c in t.strip().lower()
                if c in "-_" or not unicodedata.category(c).startswith(("P", "S")))
    return t.replace(" ", "-")


def sentence_links(text: str, rel: str, names: set[str]) -> list[tuple[str, set[tuple[str, str]]]]:
    """A paragraph's sentences, each with the (page, anchor) of every relative link it carries.

    The splitter reads a link as its label. Each link is therefore placed in the first
    sentence, from the previous link's on, that holds its label. A page is relative to the
    surfaces' root, and a bare `#anchor` names the page the paragraph is on.
    """
    sentences = split_sentences(text, names)
    out: list[tuple[str, set[tuple[str, str]]]] = [(s, set()) for s in sentences]
    at = 0
    # A paragraph or item that opens on a linked bold label, "**[Clock](perf/clock.md).**", is an
    # index entry, and every sentence after the label summarizes the page it links.
    label_link = re.match(r"\*\*\[[^\]]*\]\(", text)
    for m in MD_LINK.finditer(text):
        label = re.sub(r"`[^`]*`", CODE_STANDIN, m.group(1)).strip()
        path, _, anchor = m.group(2).partition("#")
        if not label or re.match(r"^[a-z][\w+.-]*:", path):
            continue
        page = posixpath.normpath(posixpath.join(posixpath.dirname(rel), path)) if path else rel
        if label_link and m.start() == 2:
            for _, links in out:
                links.add((page, anchor.lower()))
            continue
        for i in range(at, len(sentences)):
            if label in sentences[i]:
                out[i][1].add((page, anchor.lower()))
                at = i
                break
    return out


def sentence_subject(sentence: str, subjects: list[re.Pattern[str]]) -> tuple[frozenset, frozenset]:
    """The configured subjects and the datatypes a sentence names, as two sets.

    The two are kept apart. "Llama-3.2-3B decodes at 11.09 t/s at Q4_K_M" and "Q4_K_M decodes
    at 11.09 t/s" then share their datatype, whatever the second leaves to its heading.
    """
    models = frozenset(p.pattern for p in subjects if p.search(sentence))
    types = frozenset({"f16": "fp16", "f32": "fp32"}.get(t, t)
                      for t in (m.group(0).lower() for m in DATATYPE.finditer(sentence)))
    return (models, types)


def same_home(a: tuple, b: tuple, glossary: str | None) -> bool:
    """Whether two similar sentences are a form the guide permits rather than a restatement.

    Each side is (page, links, lead, sections, subject).

    Four forms are permitted. A sentence that links the page the other sits on is the
    one-sentence summary that points at its home. Two that link the same page are two such
    summaries. A page's lead, the text above its first level-2 heading, summarizes its own body.
    Two sentences that each name a datatype, or a model, the other never names are one template
    stated for two subjects ("tf32's rate is ..." and "bf16's rate is ..."). One that only adds
    a name to the other ("an int8 matmul that takes the int32 K split" against "a matmul that
    takes the int32 K split") is still the same claim.

    On one page a link counts only when its anchor names a section holding the other sentence.
    Every sentence on the page shares the page, and two on one page that link a third page are
    the page saying it twice.
    """
    page_a, links_a, lead_a, sections_a, subject_a = a
    page_b, links_b, lead_b, sections_b, subject_b = b
    if page_a == page_b and (lead_a or lead_b):
        return True
    if any(x - y and y - x for x, y in zip(subject_a, subject_b)):
        return True
    pages_a = {p for p, _ in links_a} - {glossary}
    pages_b = {p for p, _ in links_b} - {glossary}
    if page_a != page_b:
        return page_b in pages_a or page_a in pages_b or bool(pages_a & pages_b)
    anchors_a = {anchor for p, anchor in links_a if p == page_a and anchor}
    anchors_b = {anchor for p, anchor in links_b if p == page_b and anchor}
    return bool(anchors_a & sections_b or anchors_b & sections_a)


def reading_claims(
    surface: Surface, names: set[str], config: Config, voice: Voice
) -> tuple[list[tuple[int, str, tuple]], list[tuple[str, str, str, float, int, str, int]]]:
    """A page's reading-path sentences, and its readings of a named quantity.

    A sentence is (line, text, side), where side is what `same_home` reads. The side is the
    page, the links the sentence carries, and whether it is in the page's lead. It ends with
    the anchors of the sections holding the sentence, and the sentence's subject. An
    erratum's Workaround restates its requirement by the erratum's form, so it is not a claim
    here.

    A reading is (subject, test, precision, value, line, text): a figure in the configured unit
    that a sentence or a table cell attaches to one configured subject and one test. A
    sentence names its subject, or inherits it from the nearest heading that names exactly one.
    """
    rel = surface.rel
    raw_lines, _, source_line = surface_view(surface)
    if any(fnmatch(rel, g) for g in voice.record_files):
        return [], []
    lines = strip_code(raw_lines)
    skip = exempt_lines(lines)
    record = record_lines(lines, voice.record_prefixes)
    subjects = _rx(config.voice.get("subjects", []))

    def at(n: int) -> int:
        return source_line[n - 1] if source_line and 0 < n <= len(source_line) else n

    # The lead runs from the title to the first level-2 heading, on a page that has both.
    levels = [(n, len(m.group(1)), m.group(2)) for n, l in enumerate(lines, 1) if (m := HEADING.match(l))]
    first_h2 = next((n for n, d, _ in levels if d == 2), None)
    lead_end = first_h2 if first_h2 and levels and levels[0][1] == 1 else 0
    sections: dict[int, frozenset[str]] = {}
    open_: list[tuple[int, str]] = []
    for n, depth, title in levels:
        open_ = [s for s in open_ if s[0] < depth] + [(depth, slug(title))]
        sections[n] = frozenset(s for _, s in open_)

    def section_of(n: int) -> frozenset[str]:
        above = [h for h, *_ in levels if h <= n]
        return sections[above[-1]] if above else frozenset()

    sentences: list[tuple[int, str, tuple]] = []
    for block in blocks(lines):
        if block.line in skip or block.line in record or GENRE_LABEL.match(block.text):
            continue
        if re.match(r"(?:\*\*)?Workaround:", block.text):
            continue
        lead = block.line < lead_end
        for sentence, links in sentence_links(block.text, rel, names):
            # A document's default-conditions sentence repeats in every document by design.
            if "Conditions line" not in sentence:
                side = (rel, frozenset(links), lead, section_of(block.line),
                        sentence_subject(sentence, subjects))
                sentences.append((at(block.line), sentence, side))

    readings: list[tuple[str, str, str, float, int, str, int]] = []
    unit_text = config.voice.get("quantity_unit", "")
    if not subjects or not unit_text:
        return sentences, readings
    unit = re.compile(r"(?<![\w.])~?(\d+(?:\.\d+)?)\s*" + unit_text)
    find_tests = quantity_tests(config)

    def named(text: str) -> list[str]:
        return sorted({p.pattern for p in subjects if p.search(text)})

    # The subject a heading names is inherited by its subsections, until a subsection names
    # its own; a heading that names several subjects leaves the subject open.
    context: dict[int, str | None] = {}
    stack: list[tuple[int, list[str]]] = []
    current: str | None = None
    for n, line in enumerate(lines, 1):
        found = HEADING.match(line)
        if found:
            depth = len(found.group(1))
            stack = [s for s in stack if s[0] < depth] + [(depth, named(found.group(2)))]
            hit = next((s[1] for s in reversed(stack) if s[1]), [])
            current = hit[0] if len(hit) == 1 else None
        context[n] = current

    roles = Roles.load(config.voice)
    precisions = config.voice.get("quantity_precision", "")
    precision_rx = re.compile(precisions) if precisions else None

    def role(before: str, after: str) -> str | None:
        return quantity_role(before, after, roles)

    def precision(*texts: str) -> str:
        if precision_rx is None:
            return "?"
        for text in texts:
            hit = {m.group(1).upper() for m in precision_rx.finditer(text)}
            if len(hit) == 1:
                return hit.pop()
        return "?"

    for block in blocks(lines):
        if block.line in skip or block.line in record:
            continue
        for sentence in split_sentences(block.text.replace("`", ""), names):
            tests = find_tests(sentence)
            spans = list(unit.finditer(sentence))
            if not spans or "->" in sentence:
                continue
            mentions = [(m.start(), m.end(), p.pattern) for p in subjects for m in p.finditer(sentence)]
            # Each figure's role reads the words between it and its neighbours only.
            values = []
            for k, m in enumerate(spans):
                before = sentence[spans[k - 1].end() if k else 0:m.start()]
                after = sentence[m.end():spans[k + 1].start() if k + 1 < len(spans) else len(sentence)]
                values.append((float(m.group(1)), role(before, after),
                               figure_owner(sentence, m.start(), mentions, context.get(block.line),
                                            roles.baseline)))
            if len(tests) == 1:
                pairs = [(tests[0], v) for v in values]
            elif len(tests) == len(values) > 1:
                pairs = list(zip(tests, values))
            else:
                continue
            for test, (value, who, subject) in pairs:
                if who is None or subject is None:
                    continue
                readings.append((subject, test, f"{precision(sentence)} {who}", value, at(block.line), sentence, 0))
    # A column is read when its header names the unit, or matches `quantity_column` (a speedup
    # column titled "Speedup" whose cells carry the unit). A header that names the baseline holds
    # the baseline's figure, unless it is a ratio against it ("Speedup over the CPU EP").
    column_text = config.voice.get("quantity_column", "") or unit_text

    def cpu_column(title: str) -> bool:
        if not roles.baseline:
            return False
        name = re.escape(roles.baseline)
        return bool(re.search(r"\b" + name + r"\b", title)) and not re.search(
            r"\b(?:over|against|than|vs\.?) (?:the )?" + name + r"\b", title)

    for head_line, header, rows in md_tables(lines):
        if head_line in skip or head_line in record:
            continue
        # A table with several device columns compares configurations, and each column is one;
        # a table with a single device column is the one comparable with prose.
        device_cols = [c for c, t in enumerate(header)
                       if re.search(column_text, t.replace("`", "")) and not cpu_column(t)]
        for col, title in enumerate(header):
            plain = title.replace("`", "")
            if not re.search(column_text, plain):
                continue
            head_tests = find_tests(plain)
            for line, cells in rows:
                if col >= len(cells) or "->" in cells[col]:
                    continue
                tests = head_tests or find_tests(cells[0].replace("`", ""))
                if len(tests) != 1:
                    continue
                value = re.search(r"(?<![\w.])(\d+(?:\.\d+)?)", cells[col])
                hit = named(" ".join(cells[:1])) or named(plain)
                subject = hit[0] if len(hit) == 1 else (context.get(line) if not hit else None)
                if value and subject:
                    if cpu_column(plain):
                        who = roles.baseline
                    else:
                        who = roles.device if len(device_cols) == 1 else f"{roles.device} [{plain.strip()}]"
                    readings.append((subject, tests[0], f"{precision(cells[col], cells[0], plain)} {who}",
                                     float(value.group(1)), at(line), " | ".join(cells), at(head_line)))
    return sentences, readings


def cross_page_findings(
    claims: dict[str, tuple[list[tuple[int, str, tuple]], list[tuple[str, str, str, float, int, str, int]]]],
    config: Config, voice: Voice,
) -> dict[str, list[tuple[str, int, str, str]]]:
    """Restatement and quantity findings, which need every page at once."""
    out: dict[str, list[tuple[str, int, str, str]]] = {}

    if voice.modes.get("restatement") != "off":
        least = int(config.voice.get("restatement_words", 7))
        threshold = float(config.voice.get("restatement_jaccard", 0.6))
        glossary = relative(config, voice.glossary) if voice.glossary is not None else None
        # A bracketed evidence tag is provenance, not the claim: two sentences that share only
        # "[HW sweep, board A, 2026-09-27]" state two results, so the comparison leaves
        # tags out. The length floor still reads the whole sentence, as it always has. A tag
        # opens on a word, and a bit range such as [31:24] is part of the claim and stays.
        items = [(rel, line, sentence, content_words(EVIDENCE_TAG.sub(" ", sentence)), side)
                 for rel, (sentences, _) in claims.items() for line, sentence, side in sentences
                 if len(content_words(sentence)) >= least]
        items = [i for i in items if len(i[3]) >= 4]
        df: Counter[str] = Counter(w for _, _, _, words, _ in items for w in words)
        common = max(20, len(items) // 20)
        index: dict[str, list[int]] = {}
        for k, (_, _, _, words, side) in enumerate(items):
            shared: Counter[int] = Counter()
            for w in words:
                if df[w] <= common:
                    shared.update(index.get(w, []))
            # A tie goes to the earlier statement, the one a reader meets first. Without the
            # rule the winner is whichever the set of shared words happens to yield first,
            # and the same text names a different home from one run to the next.
            best = None
            for j, count in shared.items():
                if count < 4:
                    continue
                other = items[j][3]
                score = len(words & other) / len(words | other)
                if score >= threshold and (best is None or (-score, j) < (-best[0], best[1])) \
                        and not same_home(side, items[j][4], glossary):
                    best = (score, j)
            if best is not None:
                rel, line, sentence, _, _ = items[k]
                orel, oline, _, _, _ = items[best[1]]
                out.setdefault(rel, []).append(
                    ("restatement", line, f"restates {orel}:{oline} ({best[0]:.2f})", sentence[:140]))
            for w in words:
                if df[w] <= common:
                    index.setdefault(w, []).append(k)

    if voice.modes.get("quantity") != "off":
        groups: dict[tuple[str, str, str], list[tuple[float, str, int, str, int]]] = {}
        for rel, (_, readings) in claims.items():
            for subject, test, prec, value, line, text, table in readings:
                groups.setdefault((subject, test, prec), []).append((value, rel, line, text, table))
        for (subject, test, prec), found in groups.items():
            # Readings within 1.5% of each other are one figure quoted at two precisions.
            clusters: list[list[tuple[float, str, int, str, int]]] = []
            for reading in sorted(found):
                if clusters and reading[0] <= clusters[-1][0][0] * 1.015:
                    clusters[-1].append(reading)
                else:
                    clusters.append([reading])
            if len(clusters) < 2:
                continue
            # A table's rows are configurations by construction, so readings that all come from
            # one table are a comparison, not one quantity read several ways.
            if len({(r[1], r[4]) for r in found}) == 1 and found[0][4]:
                continue
            listing = ", ".join(f"{c[0][0]:g} ({c[0][1]}:{c[0][2]})" for c in clusters)
            name = re.sub(r"\\b|\(\?.*?\)|\\", "", subject)
            first = min(found, key=lambda r: (r[1], r[2]))
            out.setdefault(first[1], []).append(
                ("quantity", first[2], f"{name} {test} {prec}: {len(clusters)} readings: {listing}",
                 first[3][:140]))
    return out


def voice_check(
    surface: Surface, names: set[str], config: Config, voice: Voice
) -> list[tuple[str, int, str, str]]:
    """Every voice finding on one page, as (family, line, detail, context)."""
    rel = surface.rel
    raw_lines, _, source_line = surface_view(surface)
    lines = strip_code(raw_lines)
    skip = exempt_lines(lines)
    whole_record = any(fnmatch(rel, g) for g in voice.record_files)
    record = set(range(1, len(lines) + 1)) if whole_record else record_lines(lines, voice.record_prefixes)
    out: list[tuple[str, int, str, str]] = []

    def add(family: str, line: int, detail: str, context: str) -> None:
        if voice.modes.get(family, "report") == "off":
            return
        if FAMILIES[family][2] == "path" and line in record:
            return
        out.append((family, line, detail, context[:140]))

    # Characters, line by line, including the prompt character inside a code block.
    fence = None
    for n, line in enumerate(raw_lines, 1):
        if n in skip:
            continue
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            fence = None if fence else marker.group(1)[0]
            continue
        if fence:
            if re.match(r"^\s*\$ ", line):
                add("characters", n, "a prompt character in a code block", line.strip())
            continue
        prose = re.sub(r"`[^`]*`", "", line)
        for char, what in (("—", "an em dash"), ("–", "an en dash"), ("≈", "use ~"),
                           ("≤", "use <="), ("≥", "use >=")):
            if char in prose:
                add("characters", n, what, line.strip())
        if "→" in prose and "CNA→CORE→DPU" not in re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", prose):
            add("characters", n, "use ->", line.strip())
        for m in re.finditer(r"(?<![\w.])\d+(?:\.\d+)?×(?!\d)", prose):
            add("characters", n, f"a multiplier is x: {m.group(0)}", line.strip())

    # Headings, and an erratum's fields.
    headings = [(n, len(m.group(1)), m.group(2)) for n, l in enumerate(lines, 1)
                if (m := HEADING.match(l))]
    for index, (n, depth, title) in enumerate(headings):
        if n in skip:
            continue
        bare = re.sub(r"`[^`]*`", CODE_STANDIN, title)
        words = heading_words(bare)
        if HEAD_RHET.match(bare.strip()):
            add("heading", n, "names a rhetorical move, not a subject", title)
        elif "—" in bare or EDITORIAL.search(bare):
            add("heading", n, "characterizes its content", title)
        elif ", and " in bare or any(w in FINITE_VERBS for w in words):
            add("heading", n, "reads as a sentence", title)
        if re.match(r"^Erratum\b", title):
            if not re.match(r"^Erratum(?: [\w.-]+)?: \S", title):
                add("erratum", n, "the heading is not 'Erratum <id>: <symptom>'", title)
            end = next((m for m, d, _ in headings[index + 1:] if d <= depth), len(lines) + 1)
            body = "\n".join(lines[n:end - 1])
            for name in voice.erratum_fields:
                if not re.search(r"(?m)^\s*(?:[-*]\s+)?(?:\*\*)?" + re.escape(name) + r":", body):
                    add("erratum", n, f"no {name} field", title)
        for found, word in house_hits(bare, voice.house):
            add("house_word", n, f"{found} (listed: {word})", title)

    # The paragraph after the title is the page's front door.
    first = None
    title_seen = False
    for n, line in enumerate(lines, 1):
        if HEADING.match(line):
            if title_seen and first is None:
                break
            title_seen = True
            continue
        if title_seen and line.strip() and not line.lstrip().startswith(("|", "<", ">")):
            first = n
            break
    first_block = next((b for b in blocks(lines) if first is not None and b.line == first), None)
    page_blocks = blocks(lines)
    defined = {t.lower() for t in voice.glossary_terms}
    standard = [t.lower() for t in config.voice.get("standard_terms", [])]
    stale_patterns = _rx(config.voice.get("stale_patterns", []))

    for block in page_blocks:
        if block.line in skip:
            continue
        n = block.line
        raw = block.text
        prose = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", raw)
        prose = re.sub(r"`[^`]*`", CODE_STANDIN, prose)
        exposed = without_brackets(raw)
        for hit in hits(voice.internal, raw):
            if any(p.search(hit) for p in voice.provenance_exempt) and hit not in exposed:
                continue
            add("internal", n, hit, raw)
        for hit in hits(AGENT, prose):
            add("agent", n, hit, raw)
        for hit in hits(voice.residue, raw):
            add("residue", n, hit, raw)
        for hit in hits(HISTORY, prose):
            add("history", n, hit, raw)
        for hit in hits(SELF_ASSESSMENT, prose):
            add("self_assessment", n, hit, raw)
        for hit in hits(REBUTTAL, prose):
            add("rebuttal", n, hit, raw)
        for hit in hits(METHOD, prose):
            add("method", n, hit, raw)
        if not admitted_negative(raw, voice.erratum_fields):
            for hit in hits(CORRECTION, prose):
                add("correction", n, hit, raw)
        end = n + len(block.parts) - 1
        after = next((l for l in raw_lines[end:end + 2] if l.strip()), "")
        for m in FORWARD.finditer(prose):
            if "below" in m.group(0).lower() and leads_into(raw, after):
                continue
            add("forward", n, m.group(0), raw)
        if (stale := stale_finding(block, raw_lines, headings, page_blocks, record, stale_patterns)):
            add("stale", n, stale.group(0)[:60], raw)
        if CONDITIONS.match(raw) and (m := CONDITIONS_RESULT.search(" ".join(raw.split()))):
            add("conditions_result", n, m.group(0), raw)
        for sentence in split_sentences(raw, names):
            if CHAIN.match(sentence):
                add("chain", n, sentence.split()[0].strip("*_") + " ...", sentence)
            if (m := CONTRAST_AND.search(sentence)):
                add("contrast_and", n, m.group(0).lstrip(", "), sentence)
            if (term := coinage(sentence, names, config.units, defined, standard)):
                add("coinage", n, term, sentence)
            if INVERTED.match(sentence):
                add("inverted", n, " ".join(sentence.split()[:3]) + " ...", sentence)
            if (m := RATIO_BARE.search(sentence)) and not RATIO_BASELINE.search(sentence):
                add("ratio", n, f"no baseline: {m.group(0)}", sentence)
            if (m := RATIO_LOSS.search(sentence)):
                add("ratio", n, f"a loss as a multiplier: {m.group(0)}", sentence)
        for m in re.finditer(r"\*\*([^*]+)\*\*(\S?)", raw):
            span, after = m.group(1), m.group(2)
            sentence_like = span.rstrip().endswith((".", "!", "?")) or after in (".", "!", "?")
            if sentence_like and len(span.split()) >= 6 and HAZARD_WORDS.search(span):
                add("bold_hazard", n, "a bold sentence that names a hazard", span)
        for found, word in house_hits(prose, voice.house):
            add("house_word", n, f"{found} (listed: {word})", raw)
        if first_block is not None and block is first_block:
            for family, found in (("residue", hits(voice.residue, raw)),
                                  ("internal", hits(voice.internal, raw)),
                                  ("agent", hits(AGENT, prose)),
                                  ("self_assessment", hits(SELF_ASSESSMENT, prose)),
                                  ("history", hits(HISTORY, prose))):
                for hit in found:
                    add("first_paragraph", n, f"{family}: {hit}", raw)

    # A phrase that recurs down the page: refrain_words words in refrain_min or more paragraphs
    # of the reading path. A run of overlapping repeats with the same paragraphs is one finding.
    # A C header is not read down: each declaration's comment states its contract in full, as
    # zlib.h and sqlite3.h do, and the caller reads the one at the call site. A return convention
    # repeated over declarations is that form, so the family reads pages only.
    path_blocks = [] if source_line is not None else \
        [b for b in blocks(lines) if b.line not in skip and b.line not in record]
    size = int(config.voice.get("refrain_words", 6))
    least = int(config.voice.get("refrain_min", 5))
    grams: dict[tuple[str, ...], set[int]] = {}
    toks = []
    for index, block in enumerate(path_blocks):
        t = tokens(block.text)
        toks.append(t)
        for i in range(len(t) - size + 1):
            grams.setdefault(tuple(t[i:i + size]), set()).add(index)
    done: set[tuple[str, ...]] = set()
    refrains: list[tuple[int, int, int, str]] = []    # (block, start token, count, phrase)
    for index, t in enumerate(toks):
        i = 0
        while i <= len(t) - size:
            g = tuple(t[i:i + size])
            where = grams.get(g, set())
            if len(where) >= least and g not in done and min(where) == index:
                j = i
                while j + 1 <= len(t) - size and grams.get(tuple(t[j + 1:j + 1 + size])) == where:
                    j += 1
                done.update(tuple(t[k:k + size]) for k in range(i, j + 1))
                phrase = " ".join(t[i:j + size])
                if not all(w in STOPWORDS for w in phrase.split()):
                    refrains.append((index, i, len(where), phrase))
                i = j + 1
                continue
            i += 1
    # Overlapping repeats that start in one paragraph are one refrain: keep the most frequent.
    for index, start, count, phrase in refrains:
        rivals = [r for r in refrains if r[0] == index and abs(r[1] - start) < size and r[2] > count]
        if not rivals:
            add("refrain", path_blocks[index].line, f"in {count} paragraphs: '{phrase}'",
                path_blocks[index].text)

    # Tables: a row is one measurement, and a figure every row repeats is stated once.
    for head_line, header, rows in md_tables(lines):
        if head_line in skip:
            continue
        cell_limit = int(config.voice.get("cell_words", 80))
        for index, (line, cells) in enumerate(rows):
            marked = [c for c in cells[1:] if ROW_FIGURE.search(c) and FOOTNOTE_MARK.search(c)]
            if marked:
                add("row_mix", line, f"{len(marked)} figure(s) with a footnote mark", " | ".join(cells))
            for c in cells:
                if len(c.split()) > cell_limit:
                    add("cell_size", line, f"{len(c.split())} words, limit {cell_limit}", c)
            if index:
                for col in range(1, len(cells)):
                    prev = rows[index - 1][1]
                    if col < len(prev) and (off := ladder_off(prev[col], cells[col])):
                        add("ladder", line, off, " | ".join(cells))
        for col in range(1, len(header)):
            values = [cells[col] for _, cells in rows if col < len(cells)]
            if len(values) >= 3 and len(set(values)) == 1 and RANGE_CELL.search(values[0]):
                add("column_repeat", head_line, f"'{header[col]}' is {values[0]} on every row", header[col])

    # A level-2 section longer than a reader scans before finding what they came for.
    limit = int(config.voice.get("section_lines", 300))
    for index, (n, depth, title) in enumerate(headings):
        if depth != 2:
            continue
        end = next((m for m, d, _ in headings[index + 1:] if d <= 2), len(lines) + 1)
        if end - n > limit:
            add("section_size", n, f"{end - n} lines, limit {limit}", title)

    # The class skeleton, for the classes the configuration has adopted.
    h2 = [(n, t.lower()) for n, d, t in headings if d == 2]
    for name in voice.adopted:
        if name not in voice.skeletons:
            continue
        globs, items = voice.skeletons[name]
        if not any(fnmatch(rel, g) for g in globs):
            continue
        last, last_name = -1, None
        for prefix, optional in items:
            where = next((i for i, (_, t) in enumerate(h2) if t.startswith(prefix)), None)
            if where is None:
                if not optional:
                    add("skeleton", 1, f"{name}: no section '{prefix}'", rel)
                continue
            if where < last:
                add("skeleton", h2[where][0], f"{name}: '{prefix}' comes before '{last_name}'", rel)
            last, last_name = max(last, where), prefix

    # A glossary term used before the page links it to its glossary entry (Rule 3).
    # A sentence-case row ("Kick") is capitalized only because it opens a row, so its first
    # letter matches either case; an acronym ("CBUF") matches exactly. A link to the plural
    # ("[regcmds](GLOSSARY.md)") counts as a link to the term. The plural of a one-word term is
    # not a use, because "runs" and "decodes" are as often verbs; that of a phrase or an
    # acronym ("machine parameters", "BOs") is.
    if voice.glossary is not None and any(fnmatch(rel, g) for g in voice.define_files):
        target = r"\]\([^)]*" + re.escape(voice.glossary.name)
        for term in voice.glossary_terms:
            body = re.escape(term)
            if term[:1].isupper() and term[1:] == term[1:].lower():
                body = f"[{term[0]}{term[0].lower()}]" + re.escape(term[1:])
            plural = r"(?:e?s)?" if (" " in term or term.isupper()) else ""
            use = re.compile(r"(?<![\w-])" + body + plural + r"(?![\w-])")
            link = re.compile(r"\[[^\]]*(?<![\w-])" + body + r"(?:e?s)?(?![\w-])[^\]]*" + target)
            link_line = next((n for n, l in enumerate(lines, 1) if link.search(l)), None)
            for n, line in enumerate(lines, 1):
                if HEADING.match(line) and n == 1:
                    continue
                if not use.search(re.sub(r"`[^`]*`", " ", line)):
                    continue
                if link_line is None or link_line > n:
                    add("define", n, f"'{term}' before its glossary link", line.strip())
                break
    if source_line:
        out = [(f, source_line[n - 1] if 0 < n <= len(source_line) else n, d, c)
               for f, n, d, c in out]
    return out


def glossary_findings(
    text: str, names: set[str], max_words: int, units: re.Pattern[str]
) -> list[tuple[int, str, str]]:
    """Rule 2's entry form over a glossary's first table, as (line, detail, context).

    Only the parts of the form a script can see. They are one term per row, a Source column
    that is filled, alphabetical order, no repeats, and the sentence budget. Whether a
    definition names its class first, or holds a fact that belongs in a note, needs a reader.
    """
    out: list[tuple[int, str, str]] = []
    header: list[str] | None = None
    seen_sep, previous = False, ""
    keys: set[str] = set()
    for n, line in enumerate(text.split("\n"), 1):
        s = line.strip()
        if not s.startswith("|"):
            if seen_sep:
                break
            header = None
            continue
        cells = [c.strip().replace("\\|", "|") for c in re.split(r"(?<!\\)\|", s.strip("|"))]
        if header is None:
            header = cells
            if len(cells) < 3:
                out.append((n, "no Source column: term, definition, source, nearest standard term", s))
            continue
        if not seen_sep:
            seen_sep = all(re.fullmatch(r":?-{3,}:?", c) for c in cells if c)
            continue
        term = cells[0].strip("*`").strip()
        definition = cells[1] if len(cells) > 1 else ""
        if re.search(r",\s|\s(?:and|or)\s|/|\(", term):
            out.append((n, f"'{term}': one term per row, and the expansion in the definition", s))
        if len(cells) < 3 or not cells[2]:
            out.append((n, f"'{term}': no source", s))
        # Order ignores case and markup. A repeat does not: an identifier keeps its exact
        # spelling, so `rknpu` the driver and RKNPU the NPU are two terms.
        key = re.sub(r"[`*_]", "", term).casefold()
        if term in keys:
            out.append((n, f"'{term}': a second row for the term", s))
        elif key < previous:
            out.append((n, f"'{term}': out of alphabetical order", s))
        keys.add(term)
        previous = max(previous, key)
        # An abbreviation's definition opens with its expansion, or "Not expanded.", and the
        # budget of three sentences starts after it.
        first = re.match(r"[^.]*\.\s*", definition)
        if (first and len(re.findall(r"[A-Z]", term.split()[0] if term else "")) >= 2
                and count_words(first.group(0), False, units) <= 8):
            definition = definition[first.end():]
        sentences = split_sentences(definition, names)
        if len(sentences) > 3:
            out.append((n, f"'{term}': {len(sentences)} sentences, limit 3 after the expansion", s))
        for sentence in sentences:
            words = count_words(sentence, False, units)
            if words > max_words:
                out.append((n, f"'{term}': a sentence of {words} words, limit {max_words}", sentence))
    return out


def voice_pending(config: Config) -> dict[tuple[str, str], int]:
    if "--all" in sys.argv or not config.voice_ratchet or not config.voice_ratchet.is_file():
        return {}
    out: dict[tuple[str, str], int] = {}
    for line in config.voice_ratchet.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        rest, _, count = line.rpartition(" ")
        name, _, family = rest.rpartition(" ")
        out[(name.strip(), family.strip())] = int(count)
    return out


def option(flag: str) -> str | None:
    if flag in sys.argv:
        index = sys.argv.index(flag)
        if index + 1 < len(sys.argv):
            return sys.argv[index + 1]
    return None


def voice_main(config: Config, pages: list[Surface], names: set[str]) -> int:
    voice = load_voice(config)
    for problem in voice.problems:
        print(f"prose-lint: {problem}.", file=sys.stderr)
    only = option("--families")
    if only:
        wanted = set(only.split(","))
        for family in FAMILIES:
            if family not in wanted:
                voice.modes[family] = "off"
    detail = option("--detail") or ""
    shown = set(FAMILIES) if detail == "all" else set(detail.split(",")) - {""}

    results: dict[str, list[tuple[str, int, str, str]]] = {}
    for page in pages:
        results[page.rel] = voice_check(page, names, config, voice)
    # Restatement and quantity compare pages with each other, so they run over the whole set.
    if voice.modes.get("restatement") != "off" or voice.modes.get("quantity") != "off":
        claims = {page.rel: reading_claims(page, names, config, voice)
                  for page in pages}
        for rel, found in cross_page_findings(claims, config, voice).items():
            results.setdefault(rel, []).extend(found)
    # The glossary is held to its form whether or not it is a surface.
    if voice.glossary is not None and voice.glossary.is_file() and voice.modes["glossary"] != "off":
        rel = relative(config, voice.glossary)
        found = glossary_findings(voice.glossary.read_text(encoding="utf-8"), names,
                                  config.max_sentence, config.units)
        results.setdefault(rel, []).extend(("glossary", n, what, context[:140])
                                           for n, what, context in found)

    if "--baseline" in sys.argv:
        print("# Surfaces not yet held to the voice checks, by family, with the count they stood at.")
        print("#")
        print("# A ratchet, not an exemption: a surface's count in a family may not grow. Only the")
        print("# families the configuration runs as `ratchet` are held to it.")
        for rel, found in results.items():
            for family, count in sorted(Counter(f for f, *_ in found).items()):
                print(f"{rel} {family} {count}")
        return 0

    allowed = voice_pending(config)
    failures: list[str] = []
    totals: Counter[str] = Counter()
    per_file: dict[str, Counter[str]] = {}
    for rel, found in results.items():
        counts = Counter(f for f, *_ in found)
        per_file[rel] = counts
        totals.update(counts)
        for family, count in counts.items():
            mode = voice.modes[family]
            rule, label, _ = FAMILIES[family]
            listed = [f for f in found if f[0] == family]
            failing = mode == "hard" or (mode == "ratchet" and count > allowed.get((rel, family), 0))
            if failing:
                if mode == "ratchet" and (rel, family) in allowed:
                    failures.append(f"{rel}: {rule}: {label}: {count}, up from the "
                                    f"{allowed[(rel, family)]} recorded. It may not grow.")
                for _, line, what, context in listed:
                    failures.append(f"{rel}:{line}: {rule}: {label}: {what}\n    {context}")
            elif family in shown:
                for _, line, what, context in listed:
                    print(f"{rel}:{line}: {rule}: {label}: {what}\n    {context}")

    for line in failures:
        print(line)
    print(f"\nVoice checks over {len(pages)} surfaces:")
    for family, (rule, label, zone) in FAMILIES.items():
        mode = voice.modes[family]
        if mode == "off":
            continue
        top = sorted(((c[family], r) for r, c in per_file.items() if c[family]), reverse=True)[:3]
        where = ", ".join(f"{r} {c}" for c, r in top)
        print(f"  {family:16s} {mode:8s} {totals[family]:5d}  {rule}: {label}"
              + (f"  [{where}]" if where else ""))
    if failures:
        print(f"{len(failures)} voice findings fail.")
        return 1
    print("no voice check fails.")
    return 0


def self_test(config: Config) -> int:
    """The splitter's and the voice parser's contracts, asserted rather than trusted.

    Sentence length and paragraph length both rest on where one sentence ends, so a
    boundary the splitter cannot see is two defects at once. A pair read as one sentence
    fails a length rule it is inside. A paragraph of seven counted as six passes a rule it
    breaks. Both were live until the cases below were written down. The voice checks rest
    on where the record starts, which the zone cases hold.
    """
    names = technical_names(config)
    # One case needs a lowercase technical name to open a sentence. The project's own is
    # the faithful input, so the case is built from the profile rather than from a literal.
    name = sorted(names - {CODE_STANDIN})[0] if names - {CODE_STANDIN} else None
    cases = [
        ("an acronym ends a sentence", "clusters for FAT. So code is what df reports.", 2),
        ("an initial does not", "Filed by J. Smith and nobody else read it.", 1),
        ("a stop inside a bold run ends one", "**Record and compare.** Nothing in it varies.", 2),
        ("a bold run opens one", "It belongs to something else. **Every reference is bounded.**", 2),
        ("a version number splits nothing", "It needs a kernel of 6.1 or newer to mount it.", 1),
        ("a variable before a function word ends one", "It tiles both N and K. The gain is the tile count.", 2),
        ("an initial before another initial does not", "Filed by J. A. Smith and nobody else read it.", 1),
        ('a stop inside a closing quote ends one', 'It said "one thing." "Another" follows it.', 2),
        ("a stop inside a closing paren ends one", "It holds (for now.) Nothing else does.", 2),
    ]
    if name:
        cases.append(
            (
                "a technical name opens one",
                f"It verifies the checksum. {name} reads it back.",
                2,
            )
        )
    failures = []
    for label, text, want in cases:
        got = len(split_sentences(text, names))
        if got != want:
            failures.append(f"  {label}: {want} sentences expected, {got} found\n    {text}")

    item = "**A file's contents, while it is placed.** How long that is depends."
    if count_words(split_sentences(item, names)[0], True, config.units) != 1:
        failures.append("  a bold label opening a list item counts as one word (Rule 26)")

    # An item's second paragraph is part of the list, so the next item needs no lead-in.
    # A paragraph back at the margin ends the list, so the item after it does.
    # A wrapped line that opens with an issue number is prose, not a heading.
    page = ["Two entries:", "", "- First.", "", "  More of the first, and the issue", "  #163 entry.",
            "", "- Second.", "", "A paragraph at the margin.", "", "- Third."]
    got = [b.line for b, _ in missing_leadins(blocks(page), set())]
    if got != [12]:
        failures.append(f"  an item's continuation paragraph keeps its list open: {got}, want [12]")

    # A Conditions line under a heading qualifies the list after it, and is not its lead-in.
    page = ["## Performance", "", "**Conditions:** Board A, 600 MHz.", "", "- One.", "", "Text.", "",
            "**Conditions:** not under a heading.", "", "- Two."]
    got = [b.line for b, _ in missing_leadins(blocks(page), set())]
    if got != [11]:
        failures.append(f"  a Conditions line under a heading needs no colon: {got}, want [11]")

    # The record starts at a record-zone heading and ends at the next heading at its level.
    zoned = ["# Title", "", "Body.", "", "## Evidence", "", "57 of 57.", "", "### Detail", "",
             "More.", "", "## Next", "", "Body again."]
    got_zone = sorted(record_lines(zoned, ["evidence"]))
    if got_zone != list(range(5, 13)):
        failures.append(f"  the record runs from its heading to the next at its level: {got_zone}")

    # A heading with a finite verb is a sentence, and a noun phrase is not.
    for title, want in (("The matmul is not MAC-bound", True), ("Matmul throughput and its limits", False)):
        words = re.findall(r"[A-Za-z']+", title.lower())
        if any(w in FINITE_VERBS for w in words) != want:
            failures.append(f"  heading verb detection on {title!r}")

    # A chained opener points back at a whole sentence, and a determiner names a subject.
    for sentence, want in (("That says the PMU is running.", True), ("That register holds it.", False),
                           ("So the mask follows the stage.", True)):
        if bool(CHAIN.match(sentence)) != want:
            failures.append(f"  chain detection on {sentence!r}")

    # A house-word table: forms in the first column, exempt phrases in the third.
    table = "| Word | Write | Not when |\n|---|---|---|\n| floor, floors | minimum | noise floor |\n"
    table += "| `/(?-i:arms?)\\|cells?/` | configuration | |\n"
    rows = first_table(table)
    if rows != [["floor, floors", "minimum", "noise floor"], ["`/(?-i:arms?)|cells?/`", "configuration", ""]]:
        failures.append(f"  the first table's rows: {rows}")

    # A glossary: two terms in a row, a missing source, and an order break are each found,
    # and an abbreviation's expansion does not count against the sentence budget.
    glossary = ("| Term | Definition | Source | Nearest |\n|---|---|---|---|\n"
                "| CNA | Not expanded. The front end. It loads data. It feeds the array. | TRM | |\n"
                "| Kick, kicks | One start of the program counter. | | |\n"
                "| Atom | Sixteen bytes of channels at one position. | NVDLA | |\n")
    got = [what.split(":")[1].strip() for _, what, _ in glossary_findings(glossary, names, 25, config.units)]
    want = ["one term per row, and the expansion in the definition", "no source",
            "out of alphabetical order"]
    if got != want:
        failures.append(f"  the glossary's form: {got}, want {want}")

    # A C source is read as its comments. Two trailing comments stay two paragraphs, a
    # comment marker inside a string opens nothing, an indented formula is code, a list is
    # brought to the margin, a run of `//` lines is one comment, and a licence line is exempt.
    source = ("// SPDX-License-Identifier: MIT\n"
              "int a; /* trailing one */\n"
              "int b; /* trailing two */\n"
              "const char *s = \"/* not a comment */\";\n"
              "/*\n"
              " * A block comment that wraps\n"
              " * onto a second line.\n"
              " *\n"
              " *     x = y + z\n"
              " *\n"
              " *   - An item\n"
              " */\n"
              "// A line comment\n"
              "// in two parts.\n")
    view, where = comment_view(source)
    got_c = [(where[b.line - 1], b.text, b.indent) for b in blocks(view)]
    want_c = [(2, "trailing one", 0), (3, "trailing two", 0),
              (6, "A block comment that wraps onto a second line.", 0), (11, "An item", 0),
              (13, "A line comment in two parts.", 0)]
    if got_c != want_c:
        failures.append(f"  a C source's comment paragraphs: {got_c}, want {want_c}")

    # The reader-cost families: each pattern fires on the case it was written for, and not on
    # the nearest sentence that is fine.
    bench = Roles("CPU", "device", re.compile(r"\b(?:decod\w*|stream|tg\d+)\b", re.I))
    reader_cases = [
        ("contrast_and", lambda s: bool(CONTRAST_AND.search(s)),
         "Quantization saves RAM, and it does not speed prefill.", True),
        ("contrast_and", lambda s: bool(CONTRAST_AND.search(s)),
         "It loads the model, and it runs the graph.", False),
        ("coinage", lambda s: bool((m := COINAGE[0].search(s)) and m.group(1) == "ingest"),
         "This one-time conversion is the ingest.", True),
        ("inverted", lambda s: bool(INVERTED.match(s)), "Which minimum decides is per architecture.", True),
        ("inverted", lambda s: bool(INVERTED.match(s)), "Which limit binds first?", False),
        ("ratio", lambda s: bool(RATIO_BARE.search(s)) and not RATIO_BASELINE.search(s),
         "They are 38x worse end to end on one model.", True),
        ("ratio", lambda s: bool(RATIO_BARE.search(s)) and not RATIO_BASELINE.search(s),
         "It is 1.9x faster than the CPU backend.", False),
        ("correction", lambda s: bool(hits(CORRECTION, s)), "It is a boundary between two points, not a bound.", True),
        ("forward", lambda s: bool(FORWARD.search(s)), "Native-quant experts, below, describes it.", True),
        ("method", lambda s: bool(hits(METHOD, s)), "The figures are three rotated passes.", True),
        ("method", lambda s: bool(hits(METHOD, s)), "For throughput, run one process per stream.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "Frigate runs one process per detector.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "Each process returned the single-process output hash.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "Each latency is one process per configuration.", True),
        ("method", lambda s: bool(hits(METHOD, s)), "A single-process A/B does not settle the sign.", True),
        ("method", lambda s: bool(hits(METHOD, s)), "SiLU runs in two passes, a sigmoid LUT and then a multiply.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "Measured over three passes with the order rotated.", True),
        ("method", lambda s: bool(hits(METHOD, s)), "GELU runs as two passes, the LUT and a multiply.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "No test shape goes past two passes.", False),
        ("method", lambda s: bool(hits(METHOD, s)), "The op's two passes share one cube.", False),
        # quantity_role: the text before and after a figure, split at @@, under a
        # configuration that compares a device against a CPU and sets decode rates aside.
        ("quantity", lambda s: quantity_role(*s.split("@@"), bench) == "device",
         "At pp2048, Q4_K_M prefills at @@ t/s, 4.0x a CPU backend with llama.cpp's weight repack on", True),
        ("quantity", lambda s: quantity_role(*s.split("@@"), bench) == "CPU",
         " t/s, 4.0x a CPU backend with its weight repack on, which runs at @@ t/s.", True),
        ("quantity", lambda s: quantity_role(*s.split("@@"), bench) is None,
         " t/s F16 prefill at pp2048 (4.2x) and a @@ t/s Q4_K_M stream at 4 threads.", True),
        ("quantity", lambda s: quantity_role(*s.split("@@"), bench) == "device",
         "It prefills at @@ t/s and decodes at", True),
        ("quantity", lambda s: quantity_role(*s.split("@@")) == "device",
         " t/s, 4.0x a CPU backend, which runs at @@ t/s.", True),
        ("quantity", lambda s: quantity_role(*s.split("@@"), bench) is None,
         "Decode measures @@ t/s at four threads.", True),
        ("stale", lambda s: bool(STALE.search(s)), "**Not characterized:** the mAP on this build.", True),
        ("stale", lambda s: bool(STALE.search(s)), "**Not characterized:** RF-DETR base.", False),
        ("conditions_result", lambda s: bool(CONDITIONS.match(s) and CONDITIONS_RESULT.search(s)),
         "**Conditions:** Board A, warm. It ran faster than 8 threads (1343 against 1580 ms).", True),
        ("conditions_result", lambda s: bool(CONDITIONS.match(s) and CONDITIONS_RESULT.search(s)),
         "**Conditions:** Board A, an 11 s clip, the stock 50 ms autosuspend delay.", False),
        ("conditions_result", lambda s: bool(CONDITIONS.match(s) and CONDITIONS_RESULT.search(s)),
         "**Conditions:** Board B, 786 MHz. At 594 MHz, with a newer kernel, the four measure 12-21% slower.", True),
        ("conditions_result", lambda s: bool(CONDITIONS.match(s) and CONDITIONS_RESULT.search(s)),
         "**Conditions:** Board A, CPU governor `performance`, 100% of the cores online.", False),
    ]

    # The 2026-11-01 review's check defects: each fix's false positive, and the nearest true
    # positive, which must still fire. The units are fixed here, so the cases hold under any
    # configuration's counting table.
    u = re.compile(r"(?:ms|us|s)")
    reader_cases += [
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None,
         "A task's extents sit in fixed-width fields, and for a convolution the CNA's are the narrow ones.", True),
        ("coinage", lambda s: coinage(s, names, u, {"tile"}, []) is None,
         "The notch is one row of the full C, and the destination is the tile.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), ["zero point"]) is None,
         "Each band runs over a sub-input whose halo is the input zero point.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None,
         "A complete result needs code, where code is the transposed output height.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None,
         "Of the layer's remaining 0.12 ms, 71 us is the completion wait.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None, "Pool depth P is the swept variable.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None, "The one-job batching is the whole gain.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None, "The chunk depth is the variable that fits.", True),
        ("coinage", lambda s: coinage(s, names, u, set(), []) is None, "Its GELU is the exact code formula.", True),
        ("coinage", lambda s: coinage(s, names, u, {"tile"}, ["zero point"]) == "ingest",
         "This one-time conversion is the ingest.", True),
        ("splitter", lambda s: len(split_sentences(s, names)) == 2,
         "The rate is half at single-pass K. It splits at two.", True),
    ]

    def side(page: str, links=(), lead=False, sections=(), text="") -> tuple:
        return (page, frozenset(links), lead, frozenset(sections),
                sentence_subject(text, [re.compile(r"Llama-3\.2-3B")]))
    tf32, bf16 = "On a tf32 matmul only 7 produces a correct result.", "On a bf16 matmul only 3 produces a correct result."
    wedge = "An int8 matmul that takes the int32 K split can wedge the device."
    llama, quant = "Llama-3.2-3B decodes at 11.09 t/s at Q4_K_M.", "Q4_K_M decodes at 11.09 t/s."
    labelled = sentence_links("**[Clock](perf/clock.md).** It boots at 198 MHz. It runs faster above that.",
                              "README.md", names)
    linked = sentence_links("It is fast on every shape. See [the note](../perf/a.md) for its cost.",
                            "encodings/x.md", names)
    reader_cases += [
        ("restatement", lambda s: same_home(side("a.md", lead=True), side("a.md"), None),
         "a page's lead summarizes its own body", True),
        ("restatement", lambda s: same_home(side("a.md", lead=True), side("b.md"), None),
         "another page's lead is no summary of this one", False),
        ("restatement", lambda s: same_home(side("a.md", [("b.md", "")]), side("b.md"), None),
         "one sentence plus a link to the page the other is on", True),
        ("restatement", lambda s: same_home(side("a.md", [("c.md", "")]), side("b.md", [("c.md", "")]), None),
         "two summaries that link one home", True),
        ("restatement", lambda s: same_home(side("a.md", [("c.md", "")]), side("a.md", [("c.md", "")]), None),
         "one page that says it twice, linking a third", False),
        ("restatement", lambda s: same_home(side("a.md", [("a.md", "pinning")]), side("a.md", (), False, ["pinning"]), None),
         "a step that links the section restating it", True),
        ("restatement", lambda s: same_home(side("a.md", [("G.md", "")]), side("b.md", [("G.md", "")]), "G.md"),
         "two sentences that link only the glossary", False),
        ("restatement", lambda s: same_home(side("a.md", text=tf32), side("a.md", text=bf16), None),
         "one template for two datatypes", True),
        ("restatement", lambda s: same_home(side("a.md", text=wedge), side("b.md", text=wedge.replace("An int8", "A")), None),
         "a restatement that only adds a datatype", False),
        ("restatement", lambda s: same_home(side("a.md", text=llama), side("b.md", text=quant), None),
         "one model and quant named in two orders", False),
        ("restatement", lambda s: [len(links) for _, links in labelled] == [1, 1],
         "a linked bold label links every sentence after it", True),
        ("restatement", lambda s: [sorted(links) for _, links in linked] == [[], [("perf/a.md", "")]],
         "a link stays on the sentence that carries it", True),
        ("restatement", lambda s: "[31:24]" in EVIDENCE_TAG.sub(" ", s) and "sweep" not in EVIDENCE_TAG.sub(" ", s),
         "Bits [31:24] are the kernel width [HW sweep, Board A].", True),
    ]

    # The stale family over one page: a Conditions line over a bit-exact result, one placed under
    # its table, a sentence about the clock, a performance claim with no figure, and the record.
    page = ["# Title", "", "## Exactness", "", "**Conditions:** Board A, ~816 MHz.", "",
            "Every output is bit-exact over 24 shapes.", "", "## Speed", "", "| shape | t/s |",
            "|---|---|", "| a | 12.1 |", "", "**Conditions:** Board A, ~816 MHz.", "", "## Clock", "",
            "The 600 MHz request runs at ~816 MHz at a 0.80 V rail.", "",
            "On the ~816 MHz sweep the prefill ratio is lower.", "", "## Evidence", "",
            "At ~816 MHz the run took 5 ms."]
    page_blocks = blocks(page)
    heads = [(n, len(m.group(1)), m.group(2)) for n, l in enumerate(page, 1) if (m := HEADING.match(l))]
    zone = record_lines(page, ["evidence"])

    def stale_at(line: int) -> bool:
        block = next(b for b in page_blocks if b.line == line)
        return bool(stale_finding(block, page, heads, page_blocks, zone, [re.compile("~816 MHz")]))
    reader_cases += [
        ("stale", lambda s: stale_at(5), "a Conditions line over a bit-exact result", False),
        ("stale", lambda s: stale_at(15), "a Conditions line under its t/s table", True),
        ("stale", lambda s: stale_at(19), "a sentence about the clock itself", False),
        ("stale", lambda s: stale_at(21), "a performance claim with no figure", True),
        ("stale", lambda s: 25 in zone, "a dated line under Evidence is the record's", True),
    ]

    gemma = "On Gemma at pp2048, resident int8 prefill is ~9.1 t/s against resident fp16's ~15.1 t/s."
    sweep = "On that sweep F16 measures 98.98 t/s at pp512, against Llama-3.2-3B's 61.79 t/s."

    def owners(text: str, name: str, heading: str | None) -> list[str | None]:
        mentions = [(m.start(), m.end(), name) for m in re.finditer(re.escape(name), text)]
        return [figure_owner(text, m.start(), mentions, heading, "CPU")
                for m in re.finditer(r"~?\d+(?:\.\d+)?(?= t/s)", text)]
    gate = [("gate", re.compile(r"\bgates?\b", re.I), ["gate|up"])]
    reader_cases += [
        ("quantity", lambda s: owners(s, "Llama-3.2-3B", "SmolVLM2") == ["SmolVLM2", "Llama-3.2-3B"], sweep, True),
        ("quantity", lambda s: owners(s, "Llama-3.2-3B", None) == ["Llama-3.2-3B"],
         "Llama-3.2-3B's F16 prefill is 61.79 t/s.", True),
        ("quantity", lambda s: owners(s, "Gemma", None) == ["Gemma", None], gemma, True),
        ("quantity", lambda s: owners(s, "Phi", "Phi") == ["Phi", "Phi"],
         "It prefills at 30 t/s, against the CPU's 7.2 t/s.", True),
        ("house_word", lambda s: house_hits(s, gate) == [], "A group (Q\\|K\\|V, gate\\|up) is one matmul.", True),
        ("house_word", lambda s: house_hits(s, gate) == [("gate", "gate")], "The gate passes every shape.", True),
        ("row_mix", lambda s: bool(FOOTNOTE_MARK.search(s)), 'M 1-8 along the width (§"The matmul")', False),
        ("correction", lambda s: admitted_negative(s, ["Workaround"]),
         "[!NOTE] It is an absolute deadline, not a relative timeout.", True),
        ("correction", lambda s: admitted_negative(s, ["Conditions", "Workaround"]),
         "**Conditions:** Board A, one sample, not a precise cost.", False),
        ("correction", lambda s: bool(hits(CORRECTION, s)), "The mechanism is not established.", False),
        ("correction", lambda s: bool(hits(CORRECTION, s)), "The spread is not explained by the clock.", True),
        ("forward", lambda s: bool(FORWARD.search(s)), "That minimum bounds its colmax from below, and holds.", False),
        ("forward", lambda s: bool(FORWARD.search(s)), "At 8 GB and below, the cache stays small.", False),
        ("forward", lambda s: leads_into(s, "```c"), "The strides were measured at four shapes, below:", True),
        ("forward", lambda s: leads_into(s, "| a | b |"), "It runs with the lowering below, which closes joins.", False),
        ("heading", lambda s: any(w in FINITE_VERBS for w in heading_words(s)),
         "Chained-run refusal for pools whose divisor can lag", False),
        ("heading", lambda s: any(w in FINITE_VERBS for w in heading_words(s)), "The path which fails is the int8 one", True),
        ("heading", lambda s: any(w in FINITE_VERBS for w in heading_words(s)), "Where the time goes", True),
        ("rebuttal", lambda s: bool(hits(REBUTTAL, s)),
         "It fences a job at the blind settle, whatever its program is still doing.", False),
        ("rebuttal", lambda s: bool(hits(REBUTTAL, s)), "It holds whatever its last writer left.", False),
        ("rebuttal", lambda s: bool(hits(REBUTTAL, s)), "An int8 build scores 0.03-0.08 whatever its calibration.", True),
    ]
    for family, test, text, want in reader_cases:
        if test(text) != want:
            failures.append(f"  {family} on {text!r}: {not want} expected {want}")
    ladder = [("7.98 t/s", "1.43x -> 11.40", None), ("11.40", "+19% -> 13.38", "+17%"),
              ("13.38", "+7% -> ~14.5", "+8%"), ("~14.5", "+6% -> ~15.1", None),
              ("~14.5", "+12% -> ~15.1", "+4%"),
              ("1.9x", "3.4x -> 2.6x", None), ("1.15x", "0.923x (35.4030% -> 38.3480%)", None),
              ("~14.5", "+4% -> ~15.1", None), ("1.43x -> 11.40", "+17% -> 13.38", None)]
    for before, cell, want in ladder:
        got = ladder_off(before, cell)
        if bool(got) != bool(want) or (want and want not in got):
            failures.append(f"  ladder {before!r} then {cell!r}: {got!r}, expected {want!r}")
    table = ["| Model | t/s |", "|---|---:|", "| A | 56.0 † |", "| B | 1.1-1.2x |",
             "| C | 1.1-1.2x |", "| D | 1.1-1.2x |"]
    parsed = md_tables(table)
    if len(parsed) != 1 or len(parsed[0][2]) != 4 or not FOOTNOTE_MARK.search(parsed[0][2][0][1][1]):
        failures.append(f"  a table's rows and a footnote mark: {parsed}")

    # The answers guard rests on one derivation: a row cites the qualified path a reader
    # follows, and the source declares only its tail. A tail derived wrongly is a guard
    # that passes over a renamed answer, which is the one failure it exists to catch.
    # The answers guard rests on this ladder. A row is confirmed by the longest form of its
    # citation the corpus contains, so a ladder that skips straight to the last segment
    # would pass `store::gone` on any prose using the word "gone".
    tails = [
        ("a qualified path yields every suffix", "store::version::compare",
         ["store::version::compare", "version::compare", "compare"]),
        ("a macro's bang is citation, not name", "wire::wire_enum!",
         ["wire::wire_enum", "wire_enum"]),
        ("call parentheses come off", "host::probe()", ["host::probe", "probe"]),
        ("an underscored name survives whole", "read_full_or_eof", ["read_full_or_eof"]),
        ("a constant keeps its case", "store::DB_DIR", ["store::DB_DIR", "DB_DIR"]),
        ("a hyphenated package name is one word", "some-package-testkit",
         ["some-package-testkit"]),
        ("a dotted name splits on the dot", "Config.load", ["Config.load", "load"]),
    ]
    for label, citation, want in tails:
        got = suffixes(citation)
        if got != want:
            failures.append(f"  {label}: {want!r} expected, {got!r} derived from {citation!r}")

    # The ratchet's weight, which is what makes a split lower a surface's number.
    weights = [
        ("a long sentence weighs its overrun", "f: Rule 26: 56 words, limit 25", 31),
        ("a paragraph weighs its overrun", "f: Rule 26: 8 sentences in one paragraph, limit 6", 2),
        ("every other finding weighs one", "f: Rule 16: semicolon", 1),
    ]
    for label, finding, want in weights:
        got = weigh(finding)
        if got != want:
            failures.append(f"  {label}: {want} expected, {got} for {finding!r}")
    if burden(["f: Rule 26: 28 words, limit 25"] * 2) >= weigh("f: Rule 26: 56 words, limit 25"):
        failures.append("  splitting a 56-word sentence must lower a surface's weight (Rule 26)")

    # Rule 24's enumeration, which has to be told from a compound sentence. Every case
    # below was a false positive found by running the check over real prose.
    runs = [
        ("a short run is an enumeration", "It takes a, b, c, and d today.", 4),
        ("a run of clauses is not",
         "It fetches the release, resolves the dependencies, and downloads them.", 0),
        ("a bold label is not a member", "**A rule applies here**, one, two, and three.", 3),
        ("three items are under the floor", "It takes one, two, and three.", 0),
        ("a slash-joined member counts once", "It reads a/b/c, d, e, and f here.", 4),
    ]
    for label, text, want in runs:
        got = len(enumerated_items(text))
        if got != want:
            failures.append(f"  {label}: {want} items expected, {got} found\n    {text}")

    # Doc comments are prose that ships, and extracting them is the one step that can
    # lose a whole surface without saying so. A dialect that extracts nothing reports
    # every file clean, which is the same silent success the profile contract stops.
    extractions = [
        (
            "rustdoc takes `///` and `//!` and no other comment",
            "rust",
            ["//! A module.", "/// An item.", "// An ordinary note.", "//// A rule."],
            ["A module.", "An item.", "", ""],
        ),
        (
            "rustdoc keeps a doc comment's own indentation",
            "rust",
            ["///   indented under a list item."],
            ["  indented under a list item."],
        ),
        (
            "a Go comment run is documentation when a declaration follows it",
            "go",
            ["// Store keeps records.", "type Store struct {"],
            ["Store keeps records.", ""],
        ),
        (
            "a Go comment run separated from the declaration is not",
            "go",
            ["// A note to whoever is editing.", "", "type Store struct {"],
            ["", "", ""],
        ),
        (
            "a Go doc comment's tab-indented code keeps its indent, so it blanks",
            "go",
            ["// Use it like this:", "//", "//\ts := Open()", "func Open() {}"],
            ["Use it like this:", "", "", ""],
        ),
        (
            "the line dialect takes the openers it is given",
            "line",
            ["/// A summary.", "; not this one"],
            ["A summary.", ""],
        ),
    ]
    extractions += [
        (
            "a block comment opens on the doubled marker and not on a plain one",
            "block",
            ["/** A summary", " * that wraps. */", "/* An ordinary note. */"],
            ["A summary", "that wraps.", ""],
        ),
        (
            "a Python docstring is the statement under a def",
            "python",
            ['def open_it():', '    """Opens it."""', '    return 1'],
            ["", "Opens it.", ""],
        ),
        (
            "a Python string that is not in a documentation position is a value",
            "python",
            ['x = 1', 'y = """Not a docstring."""'],
            ["", ""],
        ),
    ]
    # Visibility, which decides whether a comment is the API reference or the other class.
    # Getting it wrong drops prose from every rule while reporting the surface clean.
    visibility = [
        ("rustdoc on a public item is the reference", "rust",
         ["/// Kept.", "pub fn f() {}"], ["Kept.", ""]),
        ("rustdoc on a private item is not", "rust",
         ["/// Dropped.", "fn f() {}"], ["", ""]),
        ("`pub(crate)` is the crate's own vocabulary", "rust",
         ["/// Dropped.", "pub(crate) fn f() {}"], ["", ""]),
        ("an attribute between the two is not the item", "rust",
         ["/// Kept.", "#[inline]", "pub fn f() {}"], ["Kept.", "", ""]),
        ("a variant of a public enum carries no keyword of its own", "rust",
         ["pub enum E {", "    /// Kept.", "    A,", "}"], ["", "Kept.", "", ""]),
        ("a variant of a private enum is not public either", "rust",
         ["enum E {", "    /// Dropped.", "    A,", "}"], ["", "", "", ""]),
        ("a module's own front page is what a reader of it meets", "rust",
         ["//! Kept."], ["Kept."]),
        ("an exported Go declaration is the reference", "go",
         ["// Kept.", "func Exported() {}"], ["Kept.", ""]),
        ("an unexported one is not", "go",
         ["// Dropped.", "func unexported() {}"], ["", ""]),
        ("a method's receiver is skipped over to reach the name", "go",
         ["// Kept.", "func (s *S) Exported() {}"], ["Kept.", ""]),
        ("an underscored Python name is not the reference", "python",
         ['def _helper():', '    """Dropped."""'], ["", ""]),
    ]
    for label, dialect, source, want in visibility:
        run = {"rust": rust_doc_comments, "go": go_doc_comments, "python": python_doc_comments}
        got = run[dialect](source, True)
        if got != want:
            failures.append(f"  {label}: {want!r} expected, {got!r} extracted")

    # A doc-comment field list, and a data file's values. Both are units a reader edits one
    # at a time, and joining them into one paragraph puts the finding on prose nobody can
    # fix: kernel-doc puts its first field directly under the summary, so a seven-word
    # summary reported as fifty-nine words at the summary's own line.
    shapes_by_dialect = [
        (
            "a kernel-doc field opens a block",
            ["A summary.", "@one: the first.", "@two: the second."],
            {"fields": True},
            3,
        ),
        (
            "a Sphinx field does too",
            ["A summary.", ":param one: the first."],
            {"fields": True},
            2,
        ),
        (
            "a wrapped field stays with its field",
            ["@one: the first,", "      continued here."],
            {"fields": True},
            1,
        ),
        (
            "a field is not a list item, so it asks no lead-in colon",
            ["A summary.", "@one: the first."],
            {"fields": True},
            2,
        ),
        (
            "Markdown is untouched by it",
            ["A paragraph", "@mention continuing it."],
            {},
            1,
        ),
        (
            "two data values never join into one sentence",
            ["First value.", "Second value."],
            {"starts": {1, 2}},
            2,
        ),
        (
            "a value running to several lines is one block",
            ["A value that", "wraps over lines."],
            {"starts": {1}},
            1,
        ),
    ]
    for label, source, how, want in shapes_by_dialect:
        got = len(blocks(source, how.get("fields", False), how.get("starts")))
        if got != want:
            failures.append(f"  {label}: {want} blocks expected, {got} found\n    {source}")

    # The data dialect, which reads a value rather than a format. A value broken over
    # several lines is one unit, so it lands on the line it opens.
    values = [
        (
            "a scalar lands on its own line",
            ['summary = "Held here."'],
            ["Held here."],
        ),
        (
            "a YAML key reads the same way",
            ['  summary: "Held here."'],
            ["Held here."],
        ),
        (
            "an array gives one line per element",
            ["caveats = [", '  "First.",', '  "Second.",', "]"],
            ["", "First.", "Second.", ""],
        ),
        (
            "a run over several lines is one unit on the line it opens",
            ['description = """', "First half", 'second half."""'],
            ["First half second half.", "", ""],
        ),
        (
            "a key nobody named is not prose",
            ['name = "overlay"'],
            [""],
        ),
    ]
    values += [
        (
            "a YAML literal block keeps its own line breaks",
            ["description: |", "  First line.", "", "  Second paragraph."],
            ["", "First line.", "", "Second paragraph."],
        ),
        (
            "a YAML folded block is one run on the line it opens",
            ["summary: >", "  First half", "  second half."],
            ["", "First half second half.", ""],
        ),
        (
            "a block scalar ends where the indentation does",
            ["description: |", "  Held.", "other: value"],
            ["", "Held.", ""],
        ),
    ]
    for label, source, want in values:
        got, opened = data_values(source, ("caveats", "description", "summary"))
        if got != want:
            failures.append(f"  {label}: {want!r} expected, {got!r} extracted")
        for line in opened:
            if not (1 <= line <= len(source)):
                failures.append(f"  {label}: value start {line} is outside the file")

    # Rule 16's em dash, which the ASCII pair renders as. A gate reporting only the
    # character leaves the pair as a silent way past the rule.
    dashes = [
        ("the character is reported", "A clause — and another.", 1),
        ("the spaced ASCII pair is too", "A clause -- and another.", 1),
        ("a long flag is not a dash", "Run it with --all-features today.", 0),
        ("a range is not a dash", "Lines 3--4 of the file.", 0),
    ]
    for label, text, want in dashes:
        got = len(EM_DASH.findall(text))
        if got != want:
            failures.append(f"  {label}: {want} expected, {got} found in {text!r}")

    # Rule 15's emoji clause, which is the one with no judgement in it, and the typography
    # Rule 16's character table governs instead.
    pictographs = [
        ("a pictograph is emoji", "Ship it \U0001F680 today.", 1),
        ("an arrow is typography", "The step reads a \u2192 b here.", 0),
        ("a mathematical sign is typography", "It holds x \u2264 y always.", 0),
        ("a check mark is a table value", "| yes \u2713 | no \u2717 |", 0),
        ("a dingbat is emoji", "Done \u2705 already.", 1),
    ]
    for label, text, want in pictographs:
        got = len(EMOJI.findall(text))
        if got != want:
            failures.append(f"  {label}: {want} expected, {got} found in {text!r}")

    for label, dialect, source, want in extractions:
        if dialect == "rust":
            got = rust_doc_comments(source)
        elif dialect == "go":
            got = go_doc_comments(source)
        elif dialect == "block":
            got = block_doc_comments(source)
        elif dialect == "python":
            got = python_doc_comments(source)
        else:
            got = prefixed_doc_comments(source, ("///",))
        got = strip_code(got)
        if DIALECTS[dialect]["indented_code"]:
            got = strip_indented_code(got)
        if got != want:
            failures.append(f"  {label}: {want!r} expected, {got!r} extracted")
        if len(got) != len(source):
            failures.append(f"  {label}: extraction moved the line numbers")

    # Indented code in Markdown, which has two conditions because getting either wrong
    # drops prose from every rule while still reporting the surface clean.
    indents = [
        ("an indented run after a lead-in is code", ["Run this:", "", "    a --flag"], ""),
        (
            "an indented continuation under a list item is prose",
            ["- An item.", "", "    Its second paragraph."],
            "    Its second paragraph.",
        ),
        (
            "an indented line cannot interrupt a paragraph",
            ["A sentence that", "    wraps indented."],
            "    wraps indented.",
        ),
    ]
    for label, source, want in indents:
        got = strip_markdown_indented_code(source)[-1]
        if got != want:
            failures.append(f"  {label}: {want!r} expected, {got!r} kept")

    # What a line opens, which decides whether it is prose at all. Each case below was a
    # surface the gate walked and did not hold.
    shapes = [
        (
            "a wrapped line opening with `<` is prose, not markup",
            ["Its lines are `DATA <member>", "<size> SHA512 <hex>` and there is no field."],
            1,
        ),
        ("an HTML block opening a block is markup", ["<div>", "", "A paragraph."], 1),
        (
            "an HTML comment's body is not prose",
            ["<!-- A note", "     that wraps. -->", "", "A paragraph."],
            1,
        ),
        ("a link reference definition is not a paragraph", ["[`Foo`]: crate::Foo"], 0),
        (
            "a list item's continuation paragraph keeps its indent",
            ["- An item.", "", "  Its second paragraph.", "", "- Another item."],
            3,
        ),
        ("a link inside a paragraph still is one", ["A sentence.", "[`Foo`]: crate::Foo"], 1),
    ]
    for label, source, want in shapes:
        got = len(blocks(source))
        if got != want:
            failures.append(f"  {label}: {want} blocks expected, {got} found\n    {source}")

    # Rule 29's marker has to open its line. A sentence naming it is prose about the
    # marker, and reading it as a use of one exempts the rest of the file in silence.
    markers = [
        ("a marker opening a line exempts", ["<!-- prose-lint: off -->", "Held? No."], 2),
        ("a bare marker exempts, for a renderer that shows HTML verbatim",
         ["prose-lint: off", "Held? No."], 2),
        ("a sentence naming the marker does not",
         ["Legal text has a marker: `<!-- prose-lint: off -->`.", "Held? Yes."], 0),
    ]
    for label, source, want in markers:
        got = len(exempt_lines(source))
        if got != want:
            failures.append(f"  {label}: {want} lines exempt expected, {got} found")

    # A Go doc-comment list is indented by gofmt, and its items are not nested.
    nested = [
        ("go", "// It has duties:\n//   - One thing.\n//   - Another.\nfunc Thing() {}\n", 0),
        ("markdown", "It has duties:\n\n- One thing.\n  - Another.\n", 1),
    ]
    for dialect, source, want in nested:
        lines = strip_code(go_doc_comments(source.split("\n")) if dialect == "go" else source.split("\n"))
        items = [b for b in blocks(lines) if b.is_item and b.indent >= 2]
        got = len(items) if DIALECTS.get(dialect, {}).get("nesting", True) else 0
        if got != want:
            failures.append(f"  nesting in {dialect}: {want} nested items expected, {got} found")

    # A character reference is markup, and its semicolon is not Rule 16's. A cell escapes
    # its own pipe as one, so a table quoting |err| would otherwise fail on every row.
    entities = [
        ("an escaped pipe in a cell", "| median &#124;err&#124; | 3 |", 0),
        ("a named reference in prose", "It runs at 4&nbsp;GHz and stops.", 0),
        ("a real semicolon beside one", "It runs at 4&nbsp;GHz; it stops.", 1),
    ]
    for label, text, want in entities:
        got = ENTITY.sub("", text).count(";")
        if got != want:
            failures.append(f"  {label}: {want} semicolons expected, {got} found in {text!r}")

    for line in failures:
        print(line)
    if failures:
        print(f"{len(failures)} contract failures.")
        return 1
    print(
        f"contracts hold: splitter over {len(cases) + 1} cases, answer suffixes over "
        f"{len(tails)}, enumerations over {len(runs)}, weights over {len(weights) + 1}, "
        f"extraction over {len(extractions)}, visibility over {len(visibility)}, "
        f"em dashes over {len(dashes)}, emoji over {len(pictographs)}, "
        f"field and value shapes over {len(shapes_by_dialect)}, "
        f"data values over {len(values)}, "
        f"indented code over {len(indents)}, "
        f"block shapes over {len(shapes)}, exemption markers over {len(markers)}, "
        f"character references over {len(entities)}, nesting over {len(nested)}, "
        f"the voice and reader-cost parsers over {11 + len(reader_cases) + len(ladder)}."
    )
    return 0


def measured_words(surface: Surface, names: set[str], config: Config) -> int:
    """How many words of prose one surface holds, counted the way Rule 26 counts them.

    This is the census denominator, and it is words rather than lines because a line means
    a different thing in every dialect. A Markdown line is part of a paragraph. A doc
    comment's is a wrapped comment, and a data value is one line however far it runs. Only
    a word count puts two surfaces of different dialects on the same scale.
    """
    lines, starts, _ = prose_lines(surface)
    fields = DIALECTS.get(surface.dialect, {}).get("field_lists", False)
    skip = exempt_lines(lines)
    total = 0
    for block in blocks(lines, fields, starts):
        if block.line in skip:
            continue
        for sentence in split_sentences(block.text, names):
            total += count_words(sentence, False, config.units)
    return total


def census(
    config: Config, pages: list[Surface], names: set[str], weak: list[str] | None = None
) -> int:
    """What this corpus actually looks like, so a project stops inheriting other numbers.

    Every threshold in this kit is a default rather than a measurement. A claim about what
    "a corpus usually" returns is a claim about whichever corpus the claim was written on.
    This prints the one in front of it, at every threshold worth choosing between. It gives
    findings per rule, density per surface, and the two shapes the optional rules key on.
    A project reads its own table and sets its own numbers.
    """
    per_rule: dict[str, int] = {}
    rows: list[tuple[str, int, int]] = []
    enum_at: dict[int, int] = {n: 0 for n in range(3, 9)}
    label_at: dict[int, int] = {n: 0 for n in (15, 25, 40, 60)}
    for page in pages:
        rel = page.rel
        fails, _ = check(page, names, config)
        lines, _, _ = prose_lines(page)
        skip = exempt_lines(lines)
        for line in fails:
            rule = re.search(r"(Rule \d+)", line)
            if rule:
                per_rule[rule.group(1)] = per_rule.get(rule.group(1), 0) + 1
        for block in blocks(lines):
            if block.line in skip or block.is_item:
                continue
            for sentence in split_sentences(block.text, names):
                found = len(enumerated_items(sentence))
                for threshold in enum_at:
                    if found >= threshold:
                        enum_at[threshold] += 1
        for words in label_at:
            label_at[words] += len(repeated_labels(lines, skip, words, config.labels_per_section))
        # Density is per hundred words, counted the way Rule 26 counts them. A line is
        # not comparable across dialects: a Markdown line is part of a paragraph, and a
        # data value is one line however far it runs, so a file of long values reads as
        # the densest thing in the corpus on a measure that is really about wrapping.
        rows.append((rel, measured_words(page, names, config), len(fails)))

    print("# Findings per rule")
    for rule, count in sorted(per_rule.items(), key=lambda kv: -kv[1]):
        print(f"  {rule:10} {count}")
    print("\n# Density per surface, worst first, per hundred words of prose")
    print(f"  {'surface':50} {'words':>6} {'findings':>9} {'per 100':>8}")
    for rel, lines_count, found in sorted(
        rows, key=lambda r: -(r[2] / r[1] * 100 if r[1] else 0)
    ):
        rate = found / lines_count * 100 if lines_count else 0
        print(f"  {rel:50} {lines_count:6} {found:9} {rate:8.1f}")
    total_lines = sum(r[1] for r in rows) or 1
    total_found = sum(r[2] for r in rows)
    print(f"  {'TOTAL':50} {total_lines:6} {total_found:9} {total_found / total_lines * 100:8.1f}")

    print("\n# Rule 24, sentences carrying a run of N or more parallel items")
    print("  Set `limits.enumeration_items` to the N you want held, or leave it 0.")
    for threshold, count in sorted(enum_at.items()):
        print(f"  N >= {threshold}: {count}")
    if weak:
        print("\n# Answer rows confirmed by a word rather than a path")
        print("  Cite more of the path, or accept that these rows are not really guarded.")
        for line in weak:
            print(f"  {line}")
    print("\n# Rule 18, repeated bold lead-ins whose block runs over N words")
    print(
        f"  At {config.labels_per_section} or more per section. "
        "Set `limits.label_block_words`, or leave it 0."
    )
    for words, count in sorted(label_at.items()):
        print(f"  N >  {words}: {count}")
    return 0


RATCHET_MARKER = "# prose-lint ratchet format: weight"


def pending(config: Config) -> dict[str, int]:
    """Surfaces not yet brought to the rules, with the weight they stood at.

    A ratchet, not an exemption. A surface here never gets worse, and comes off the list
    when it reaches zero. The file is the domain this gate covers, stated rather than
    implied: an allowance nobody wrote down is the shape a skipped gate takes.

    A file written before weights existed records finding counts, and the two numbers are
    not comparable. Reading one as weights silently gives every listed surface a budget far
    tighter than it was granted. The marker is required, and its absence says what to do.
    """
    if "--all" in sys.argv:
        return {}
    if not config.ratchet or not config.ratchet.is_file():
        return {}
    text = config.ratchet.read_text(encoding="utf-8")
    if RATCHET_MARKER not in text:
        raise ProfileError(
            f"the ratchet at {config.ratchet} records finding counts, and this gate\n"
            "  records weights, which are not comparable. Re-run with --baseline to\n"
            "  rewrite it. A weight is how far a length finding runs over its limit, so\n"
            "  splitting a long sentence lowers a surface's number instead of raising it."
        )
    out: dict[str, int] = {}
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        name, _, count = line.rpartition(" ")
        out[name.strip()] = int(count)
    return out


def baseline(
    config: Config, pages: list[Surface], names: set[str], variants: list[tuple[str, str]]
) -> int:
    """Write a ratchet recording where every surface stands today.

    An existing corpus does not comply on the day the gate is introduced. A gate that
    fails on every page is one somebody switches off. The baseline turns the current state
    into the thing the gate holds. No surface gets worse, and each comes off the list when
    it reaches zero.
    """
    print("# Surfaces not yet held to the style guide's Part 6, with the weight they stood at.")
    print("#")
    print("# A ratchet, not an exemption: a surface here may not get worse, and comes off the")
    print("# list when it reaches zero. Delete a line once its surface is clean.")
    print("#")
    print("# The number is a weight rather than a finding count. A length finding weighs how")
    print("# far it runs over its limit, so splitting one long sentence into two shorter ones")
    print("# lowers it. Every other finding weighs one.")
    print(RATCHET_MARKER)
    total = 0
    for page in pages:
        fails, _ = check(page, names, config, variants)
        if fails:
            print(f"{page.rel} {burden(fails)}")
            total += burden(fails)
    print(f"# {total} weight over {len(pages)} surfaces at baseline.", file=sys.stderr)
    return 0


def config_path() -> Path:
    """The configuration this run reads, from `--config` or from beside this script."""
    if "--config" in sys.argv:
        return Path(sys.argv[sys.argv.index("--config") + 1]).resolve()
    return Path(__file__).resolve().parent / "prose-lint.toml"


def positional() -> list[str]:
    """Arguments that are neither a flag nor a flag's value."""
    out, skip = [], False
    for arg in sys.argv[1:]:
        if skip:
            skip = False
            continue
        if arg in ("--config", "--detail", "--families"):
            skip = True
            continue
        if not arg.startswith("--"):
            out.append(arg)
    return out


def main() -> int:
    path = config_path()
    if not path.is_file():
        print(f"no configuration at {path}", file=sys.stderr)
        return 1
    config = Config.load(path)
    named = positional()
    try:
        if "--self-test" in sys.argv:
            return self_test(config)
        names = technical_names(config)
        variants = ruled_out(config)
        stale, weak = check_answers(config)
        pages = named_files(config, named) if named else surfaces(config)
    except ProfileError as broken:
        print(f"prose-lint: {broken}", file=sys.stderr)
        return 1
    if not pages:
        print(f"no prose surfaces found under {config.root}", file=sys.stderr)
        return 1
    if "--voice" in sys.argv:
        return voice_main(config, pages, names)
    if "--census" in sys.argv:
        return census(config, pages, names, weak)
    if "--baseline" in sys.argv:
        return baseline(config, pages, names, variants)
    try:
        allowed = pending(config)
    except ProfileError as broken:
        print(f"prose-lint: {broken}", file=sys.stderr)
        return 1

    failures: list[str] = []
    ratchet: list[str] = []
    reported: dict[str, list[str]] = {
        "emdash": [], "loose_modal": [], "recommendation": [], "mid_if": []
    }
    still_pending: list[tuple[str, int, int]] = []
    for page in pages:
        rel = page.rel
        fails, soft = check(page, names, config, variants)
        for key in reported:
            reported[key] += soft[key]
        if rel in allowed:
            was = allowed[rel]
            now = burden(fails)
            still_pending.append((rel, now, was))
            if now > was:
                name = config.ratchet.name if config.ratchet else "the ratchet"
                ratchet.append(
                    f"{rel}: weight {now}, up from the {was} recorded in "
                    f"{name}. A pending surface may not get worse."
                )
            continue
        failures += fails

    # A ratchet line naming a surface the gate never walked is an allowance for nothing.
    # It survives a rename or a deletion silently, and the file it once covered is then
    # held in full with no record of why the number moved. A run over named files walks
    # only those, so it leaves the rest of the ratchet alone.
    walked = {page.rel for page in pages}
    for rel in [] if named else sorted(set(allowed) - walked):
        name = config.ratchet.name if config.ratchet else "the ratchet"
        ratchet.append(
            f"{name}: {rel} is listed at {allowed[rel]} and is not a surface this gate "
            f"walks. Delete the line, or add the page back to `surfaces.include`."
        )

    for line in failures + ratchet + stale:
        print(line)

    totals = {key: len(hits) for key, hits in reported.items()}
    if "--soft" in sys.argv:
        for key in ("emdash", "loose_modal", "recommendation", "mid_if"):
            for line in reported[key]:
                print(line)
    covered = len(pages) - len(still_pending)
    where = "" if "--soft" in sys.argv else " Pass --soft to locate them."
    if weak:
        print(
            f"{len(weak)} answer rows are confirmed by a word rather than a path. "
            "Run --census to list them."
        )
    print(
        f"\n{covered} of {len(pages)} surfaces held to the rules. "
        f"Reported and not failed: {totals['emdash']} em dashes (Rule 16, "
        f"zero or justified), {totals['loose_modal']} of would/could/might "
        f"(Rule 15 exempts a counterfactual), {totals['recommendation']} of should "
        f"(Rule 15: a recommendation only), {totals['mid_if']} mid-sentence "
        f"if/when (Rule 28 needs a reader).{where}"
    )
    # A build log wants the surfaces that moved. The full listing is what a person working
    # through the ratchet reads, which is a different moment, so `--quiet` leaves it out.
    for rel, now, was in sorted(still_pending):
        if now == was and "--quiet" in sys.argv:
            continue
        moved = (
            "unchanged" if now == was else f"down from {was}" if now < was else f"up from {was}"
        )
        print(f"  pending: {rel}, weight {now} ({moved})")
    if failures or ratchet or stale:
        print(f"{len(failures) + len(ratchet) + len(stale)} findings.")
        return 1
    print("clean over every surface this gate covers.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
