#!/usr/bin/env python3
"""The prose that ships is held to the rules the style guide states.

STYLE.md Part 5 is the half of that guide a script can enforce, and this is the script.
It reads the prose a stranger reads and fails on the rules that have one answer:

    Rule 20   a sentence over the word limit, a paragraph over the sentence limit
    Rule 21   a list without a colon lead-in, with a lowercase item, with a trailing
              comma or semicolon, or nested
    Rule 8    a semicolon, in prose or in a table cell
    Rule 9    `should`, `shall`, or `may`, and a term the profile rules out

Two more are held only once a project sets their threshold. Each rests on a number the
guide states as a default rather than as a fact. Rule 18 turns a long enough run of
parallel items into a list, and Rule 12 makes a repeated bold lead-in a heading. Both are
off at 0, and `--census` prints what either would cost at every threshold worth choosing
between.

Rule 20's counting table is what makes the length rule fair. Five things each count as one
word: backticked code, a parenthetical, a hyphenated word, a number with its unit, and a
bold label. An identifier never costs a sentence its budget.

Three things are reported and not failed, because each needs a reader. An em dash is
Rule 8's "zero, or justified" and only a person judges the second half. `would`, `could`
and `might` are the modals Rule 9 exempts inside explanatory prose. A mid-sentence `if`
is Rule 22's condition-before-command, which reads the same as an embedded question.
Each is reported with its line under `--soft`, because a count with no location is a
number a reader cannot act on.

A surface is a Markdown page or the doc comments in a source file, and the rules do not
change between them. A doc comment is prose a stranger reads and it ships with the code.
On a library it is usually the larger of the two surfaces.

`surfaces.doc_comments` names the files and the dialect to extract them with. `rust` takes
`///` and `//!`, `go` the comment run above a declaration, `python` a docstring, and
`block` the `/** ... */` shape JSDoc and kernel-doc share. `line` takes any opener the
configuration lists, for a language with no dialect here. Setting `visibility` to `public`
holds the comments on the API surface alone, which is the class STYLE.md separates.

Extraction blanks what it does not take rather than dropping it, so every report still
names the line the reader opens the file at.

Two further duties are the profile's rather than the prose's. A configured profile the
gate cannot read stops the run. Every part of that contract fails the same quiet way, and
each leaves the prose held to a weaker rule than the project asked for. Where a project
keeps an answered-questions table, the gate checks every answer it names. A rename
therefore cannot leave a row pointing at nothing. That table and the
fixed-terms table each live in the profile unless `answers.path` or `terms.path` names the
document that already owns it.

Everything this script needs to know about a particular project comes from a TOML
configuration file, so the script itself is portable. See `prose-lint.toml` for the
schema and `ADOPTING.md` for the order to introduce it in.

    prose-lint.py --config PATH             hold every surface the ratchet covers
    prose-lint.py --config PATH --all       ignore the ratchet and report every surface
    prose-lint.py --config PATH --soft      also locate what is reported and not failed
    prose-lint.py --config PATH --census    measure this corpus, rather than reading about one
    prose-lint.py --config PATH --baseline  write a ratchet recording where each surface stands
    prose-lint.py --config PATH --self-test assert the contracts the checks rest on

Requires Python 3.11 or newer, for `tomllib` in the standard library.
"""

from __future__ import annotations

import re
import sys
import tomllib
from dataclasses import dataclass, field
from fnmatch import fnmatch
from pathlib import Path

# Rule 9 bans the whole modal ladder and exempts a counterfactual inside explanatory
# prose. That exemption is a property of the use rather than of the word, which no script
# reads, so the split below is a heuristic about which words usually carry one. It is the
# default rather than a fact: a project writing to RFC 2119, where `may` is a defined term,
# moves it, and `[limits]` is where. Both lists are held to the same rule, and only the
# consequence differs.
DEFAULT_GATED_MODALS = ["should", "shall", "may"]
DEFAULT_LOOSE_MODALS = ["would", "could", "might"]

# Rule 8's em dash, and the ASCII pair that renders as one. A gate that reports only the
# character leaves the pair as a silent way past the rule, and the pair hides a sentence
# boundary exactly as the character does. Neither fires inside a `prose-lint:` marker,
# whose own syntax is `<!-- prose-lint: off -- reason -->`.
EM_DASH = re.compile(r"—|(?<= )--(?= )")

# Rule 9 bans emoji outright, which is the one clause in the guide with no judgment in
# it, so this fails rather than reports. It is the pictographic blocks and nothing else.
# An arrow and a mathematical sign are typography, and Rule 8's character table is what
# governs those. The four marks below are excluded because a table uses them as a value
# rather than as a picture in a sentence, and no other reading of them exists.
EMOJI = re.compile(
    "(?![\u2713\u2714\u2717\u2718])"
    "[\U0001F000-\U0001FAFF\u2600-\u27BF\uFE0F]"
)

# Rule 9's American spelling, as the pairs a technical corpus actually reaches for. The
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


def modals(words: list[str]) -> re.Pattern[str] | None:
    """One pattern over a list of modal verbs, or None where the list is empty.

    A project empties a list to turn its half of Rule 9 off. That is the only way to say
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
            # Rule 18 and Rule 12 are off unless a project turns them on. Both rest on a
            # threshold the guide states as a default rather than as a fact, and a corpus
            # is the only thing that says which number is right for it.
            enumeration_items=limits.get("enumeration_items", 0),
            label_block_words=limits.get("label_block_words", 0),
            labels_per_section=limits.get("labels_per_section", 3),
            units=re.compile(r"(?:" + "|".join(re.escape(u) for u in units) + r")"),
            # Rule 20's limits per surface, for a page the project holds to different
            # numbers. Each entry is `include` plus the keys it overrides, and the first
            # entry matching a surface wins.
            overrides=limits.get("override", []),
            gated=modals(limits.get("gated_modals", DEFAULT_GATED_MODALS)),
            loose=modals(limits.get("loose_modals", DEFAULT_LOOSE_MODALS)),
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
        )


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

    Rule 21 wants a list item to start with a capital, and Rule 23 exempts an identifier
    from any rule about prose. A name like `btrfs` is both lowercase and an identifier, so
    the gate cannot tell one from a missing capital by shape. The profile is the list.

    The contract is one italicized run under the configured heading, its terms separated
    by commas. PROFILE-TEMPLATE.md states it where a profile author reads it.
    """

    def unread(why: str) -> set[str]:
        raise ProfileError(
            f"the profile is configured and {why}.\n"
            "  No technical names would be in force, so Rule 21 would read a\n"
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

    The table is the project's record of which question is answered where, and Rule 9 rests
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
                    f"{config.answers_path.name}: Rule 9: `{citation}` answers "
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

    Rule 9 wants one term per concept, and the profile's third column is the half a
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
DIALECTS = {
    "rust": {"indented_code": False, "field_lists": False},
    "go": {"indented_code": True, "field_lists": False},
    "line": {"indented_code": False, "field_lists": False},
    "block": {"indented_code": False, "field_lists": True},
    "python": {"indented_code": False, "field_lists": True},
    "data": {"indented_code": False, "field_lists": False},
}


def prose_lines(surface: Surface) -> tuple[list[str], set[int]]:
    """One surface as prose, blank wherever it is not, at the lines it stands on.

    Every report names a file and a line, so a reader opens the file there and sees the
    thing. Blanking rather than dropping is what keeps that true through extraction.
    """
    lines = surface.path.read_text(encoding="utf-8", errors="replace").split("\n")
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
    lines = strip_code(lines)
    if DIALECTS.get(surface.dialect, {}).get("indented_code"):
        return strip_indented_code(lines), starts
    return strip_markdown_indented_code(lines), starts


def count_words(text: str, is_item: bool, units: re.Pattern[str]) -> int:
    """Rule 20's counting table."""
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
    # ("... else. **Every reference ...") starts it, so the markers ride along with the
    # terminator and with the opener rather than hiding either.
    # A terminator can sit inside a closing mark. `... a dead end." "Resume ...` is two
    # sentences, and reading it as one taxes prose that is already inside the limit.
    emphasis = r"(?:\*\*|\*|_|\"|\'|\))*"
    opener = emphasis + r"(?:[A-Z`\"(\[]" + (f"|(?i:{lower})\\b" if lower else "") + r")"
    # A lone capital before the stop is an initial. Two or more are an acronym, which ends
    # a sentence as readily as any other word: "... to two seconds. FAT records ..." is two
    # sentences, and reading it as one taxes a sentence that is already inside the limit.
    parts = re.split(
        r"(?<!\b[A-Z])(?<!§)([.!?]" + emphasis + r")\s+(?=" + opener + r")", s
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
    """A paragraph or one list item, with the line it starts on."""

    def __init__(self, line: int, is_item: bool, indent: int, after_heading: bool = False) -> None:
        self.line, self.is_item, self.indent = line, is_item, indent
        self.after_heading = after_heading
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
        if bare.lstrip().startswith("#"):
            current, heading = None, True
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
        # A field opens a block without being a list item. Rule 21's lead-in colon asks a
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
            heading = False
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
            found.append(current)
        current.parts.append(bare)
        current.lines.append(n)
    return found


EXEMPT_MARKER = re.compile(r"^\s*(?:<!--\s*)?prose-lint:\s*(off|on)\b")


def exempt_lines(lines: list[str]) -> set[int]:
    """Line numbers inside a `prose-lint: off` region.

    Rule 23 exempts legal boilerplate, and a license grant is recognized by shape rather
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


# Rule 18's enumeration, and Rule 12's repeated label. Both are shapes rather than counts,
# so each carries its own reasoning where it is applied.
ENUMERATION = re.compile(r"((?:[^,;:()]+,\s+){3,}(?:and|or)\s+[^,;:.()]+)")
ENUM_ITEM_WORDS = 4
BOLD_LEAD = re.compile(r"^\s*(?:(?:[-*+]|\d+\.)\s+)?\*\*([^*]+)\*\*")


def enumerated_items(sentence: str, max_words: int = ENUM_ITEM_WORDS) -> list[str]:
    """The longest run of comma-separated items a sentence carries, terminated by and/or.

    Rule 18 turns four or more parallel items into a vertical list, and the run has to be
    told from an ordinary compound sentence. Two things separate them. An enumeration ends
    on a serial comma before `and` or `or`, and its items are short. A run of
    clauses is prose that happens to have commas in it. A slash-joined member counts once,
    which is Rule 18's own instruction and the reason `ext2/ext3/ext4` does not read as
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
    # A bold run opening the block is Rule 12's label for what follows, so it is not a
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

    Rule 12 says a label a reader skims for is a heading. Where the same shape repeats down
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


# The ratchet's unit. A finding count is the wrong one: Rule 20 says a long sentence is
# split rather than compressed, and splitting a 56-word sentence into two 28-word ones
# takes the count from one finding to two. A ratchet on counts then fails the exact edit
# the guide asks for, on prose that got strictly better. Weighing a length finding by how
# far over the limit it is makes the number fall: 31 becomes 6.
WEIGHT = re.compile(r"Rule 20: (\d+) (?:words, limit|sentences in one paragraph, limit) (\d+)")


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
    """Rule 20's two numbers for one surface, after any override the project set.

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
    lines, starts = prose_lines(surface)
    skip = exempt_lines(lines)
    max_sentence, max_paragraph = limits_for(surface, config)
    fields = DIALECTS.get(surface.dialect, {}).get("field_lists", False)
    fails: list[str] = []
    soft: dict[str, list[str]] = {"emdash": [], "loose_modal": [], "mid_if": []}

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
                    f"{rel}:{block.line}: Rule 20: {words} words, limit {max_sentence}\n"
                    f"    {sentence[:140]}"
                )
        if len(sentences) > max_paragraph:
            fails.append(
                f"{rel}:{block.line}: Rule 20: {len(sentences)} sentences in one paragraph, "
                f"limit {max_paragraph}"
            )
        for sentence in sentences:
            # Rule 21 forbids a nested list, so a run inside a list item has no formatting
            # fix available and wants a reader restructuring the section instead.
            items = [] if block.is_item else enumerated_items(sentence)
            if config.enumeration_items and len(items) >= config.enumeration_items:
                fails.append(
                    f"{rel}:{block.locate(items[0])}: Rule 18: {len(items)} parallel items "
                    f"inside a sentence\n    {sentence[:140]}"
                )
        prose = re.sub(r"`[^`]*`", "", text)
        for variant, prefer in variants or []:
            if re.search(r"(?<![\w-])" + re.escape(variant) + r"(?![\w-])", prose, re.I):
                fails.append(
                    f"{rel}:{block.locate(variant)}: Rule 9: {variant!r} is ruled out by "
                    f"the profile, which writes {prefer!r}\n    {text[:140]}"
                )
        if ";" in prose:
            fails.append(f"{rel}:{block.line}: Rule 8: semicolon\n    {text[:140]}")
        for hit in config.gated.findall(prose) if config.gated else []:
            fails.append(f"{rel}:{block.line}: Rule 9: `{hit.lower()}`\n    {text[:140]}")
        for variant, prefer in spelt(prose, config):
            fails.append(
                f"{rel}:{block.locate(variant)}: Rule 9: {variant!r} is British, and the "
                f"guide writes {prefer!r}\n    {text[:140]}"
            )
        for _ in EMOJI.findall(text):
            fails.append(f"{rel}:{block.line}: Rule 9: emoji\n    {text[:140]}")
        for part, n in zip(block.parts, block.lines):
            if EXEMPT_MARKER.match(part):
                continue
            for _ in EM_DASH.findall(re.sub(r"`[^`]*`", "", part)):
                soft["emdash"].append(
                    f"{rel}:{n}: Rule 8: em dash, so zero or justified\n    {part.strip()[:140]}"
                )
        for hit in config.loose.findall(prose) if config.loose else []:
            soft["loose_modal"].append(
                f"{rel}:{block.locate(hit)}: Rule 9: `{hit.lower()}`, exempt inside a "
                f"counterfactual\n    {text[:140]}"
            )
        for hit in re.findall(r"[a-z,]\s+(?:if|when)\b", prose):
            soft["mid_if"].append(
                f"{rel}:{block.locate(hit)}: Rule 22: mid-sentence `if`/`when`, or an "
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
                fails.append(f"{rel}:{block.line}: Rule 21: item starts lowercase\n    {text[:140]}")
            if text.rstrip().endswith((",", ";")):
                fails.append(f"{rel}:{block.line}: Rule 21: item ends with a comma or semicolon")
            if block.indent >= 2:
                fails.append(f"{rel}:{block.line}: Rule 21: nested list item")

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
        cells = re.sub(r"`[^`]*`", "", raw)
        if ";" in cells:
            fails.append(f"{rel}:{n}: Rule 8: semicolon in a table cell\n    {raw.strip()[:140]}")
        for hit in config.gated.findall(cells) if config.gated else []:
            fails.append(
                f"{rel}:{n}: Rule 9: `{hit.lower()}` in a table cell\n    {raw.strip()[:140]}"
            )
        for variant, prefer in spelt(cells, config):
            fails.append(
                f"{rel}:{n}: Rule 9: {variant!r} in a table cell is British, and the guide "
                f"writes {prefer!r}\n    {raw.strip()[:140]}"
            )
        for _ in EMOJI.findall(raw):
            fails.append(f"{rel}:{n}: Rule 9: emoji in a table cell\n    {raw.strip()[:140]}")
        for variant, prefer in variants or []:
            if re.search(r"(?<![\w-])" + re.escape(variant) + r"(?![\w-])", cells, re.I):
                fails.append(
                    f"{rel}:{n}: Rule 9: {variant!r} in a table cell is ruled out by the "
                    f"profile, which writes {prefer!r}\n    {raw.strip()[:140]}"
                )

    for line_no, label, count in labels:
        fails.append(
            f"{rel}:{line_no}: Rule 12: bold lead-in {label!r} is one of {count} in its "
            f"section, so these are headings"
        )

    # Rule 21's lead-in colon: a list that follows a paragraph needs one.
    seen = blocks(lines, fields, starts)
    for i, block in enumerate(seen):
        if not block.is_item or i == 0 or block.line in skip:
            continue
        previous = seen[i - 1]
        # An indented paragraph is a list item's own continuation, so the list did not end
        # and the item below it needs no fresh lead-in. Reading it as one puts a finding on
        # every list whose items run to more than a paragraph.
        if previous.is_item or block.after_heading or previous.indent > 0:
            continue
        if not previous.text.rstrip().endswith(":"):
            fails.append(
                f"{rel}:{block.line}: Rule 21: the list lead-in does not end with a colon\n"
                f"    {previous.text[-100:]}"
            )
    return fails, soft


def self_test(config: Config) -> int:
    """The splitter's contract, asserted rather than trusted.

    Sentence length and paragraph length both rest on where one sentence ends, so a
    boundary the splitter cannot see is two defects at once. A pair read as one sentence
    fails a length rule it is inside. A paragraph of seven counted as six passes a rule it
    breaks. Both were live until the cases below were written down.
    """
    names = technical_names(config)
    # One case needs a lowercase technical name to open a sentence. The project's own is
    # the honest input, so the case is built from the profile rather than from a literal.
    name = sorted(names - {CODE_STANDIN})[0] if names - {CODE_STANDIN} else None
    cases = [
        ("an acronym ends a sentence", "clusters for FAT. So code is what df reports.", 2),
        ("an initial does not", "Filed by J. Smith and nobody else read it.", 1),
        ("a stop inside a bold run ends one", "**Record and compare.** Nothing in it varies.", 2),
        ("a bold run opens one", "It belongs to something else. **Every reference is bounded.**", 2),
        ("a version number splits nothing", "It needs a kernel of 6.1 or newer to mount it.", 1),
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
        failures.append("  a bold label opening a list item counts as one word (Rule 20)")

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
        ("a long sentence weighs its overrun", "f: Rule 20: 56 words, limit 25", 31),
        ("a paragraph weighs its overrun", "f: Rule 20: 8 sentences in one paragraph, limit 6", 2),
        ("every other finding weighs one", "f: Rule 8: semicolon", 1),
    ]
    for label, finding, want in weights:
        got = weigh(finding)
        if got != want:
            failures.append(f"  {label}: {want} expected, {got} for {finding!r}")
    if burden(["f: Rule 20: 28 words, limit 25"] * 2) >= weigh("f: Rule 20: 56 words, limit 25"):
        failures.append("  splitting a 56-word sentence must lower a surface's weight (Rule 20)")

    # Rule 18's enumeration, which has to be told from a compound sentence. Every case
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

    # Rule 8's em dash, which the ASCII pair renders as. A gate reporting only the
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

    # Rule 9's emoji clause, which is the one with no judgement in it, and the typography
    # Rule 8's character table governs instead.
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

    # Rule 23's marker has to open its line. A sentence naming it is prose about the
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
        f"block shapes over {len(shapes)}, exemption markers over {len(markers)}."
    )
    return 0


def measured_words(surface: Surface, names: set[str], config: Config) -> int:
    """How many words of prose one surface holds, counted the way Rule 20 counts them.

    This is the census denominator, and it is words rather than lines because a line means
    a different thing in every dialect. A Markdown line is part of a paragraph. A doc
    comment's is a wrapped comment, and a data value is one line however far it runs. Only
    a word count puts two surfaces of different dialects on the same scale.
    """
    lines, starts = prose_lines(surface)
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
        lines, _ = prose_lines(page)
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
        # Density is per hundred words, counted the way Rule 20 counts them. A line is
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

    print("\n# Rule 18, sentences carrying a run of N or more parallel items")
    print("  Set `limits.enumeration_items` to the N you want held, or leave it 0.")
    for threshold, count in sorted(enum_at.items()):
        print(f"  N >= {threshold}: {count}")
    if weak:
        print("\n# Answer rows confirmed by a word rather than a path")
        print("  Cite more of the path, or accept that these rows are not really guarded.")
        for line in weak:
            print(f"  {line}")
    print("\n# Rule 12, repeated bold lead-ins whose block runs over N words")
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
    print("# Surfaces not yet held to the style guide's Part 5, with the weight they stood at.")
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


def main() -> int:
    path = config_path()
    if not path.is_file():
        print(f"no configuration at {path}", file=sys.stderr)
        return 1
    config = Config.load(path)
    try:
        if "--self-test" in sys.argv:
            return self_test(config)
        names = technical_names(config)
        variants = ruled_out(config)
        stale, weak = check_answers(config)
        pages = surfaces(config)
    except ProfileError as broken:
        print(f"prose-lint: {broken}", file=sys.stderr)
        return 1
    if not pages:
        print(f"no prose surfaces found under {config.root}", file=sys.stderr)
        return 1
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
    reported: dict[str, list[str]] = {"emdash": [], "loose_modal": [], "mid_if": []}
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
    # held in full with no record of why the number moved.
    walked = {page.rel for page in pages}
    for rel in sorted(set(allowed) - walked):
        name = config.ratchet.name if config.ratchet else "the ratchet"
        ratchet.append(
            f"{name}: {rel} is listed at {allowed[rel]} and is not a surface this gate "
            f"walks. Delete the line, or add the page back to `surfaces.include`."
        )

    for line in failures + ratchet + stale:
        print(line)

    totals = {key: len(hits) for key, hits in reported.items()}
    if "--soft" in sys.argv:
        for line in reported["emdash"] + reported["loose_modal"] + reported["mid_if"]:
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
        f"Reported and not failed: {totals['emdash']} em dashes (Rule 8, "
        f"zero or justified), {totals['loose_modal']} of would/could/might "
        f"(Rule 9 exempts a counterfactual), {totals['mid_if']} mid-sentence "
        f"if/when (Rule 22 needs a reader).{where}"
    )
    for rel, now, was in sorted(still_pending):
        moved = "unchanged" if now == was else f"down from {was}"
        print(f"  pending: {rel}, weight {now} ({moved})")
    if failures or ratchet or stale:
        print(f"{len(failures) + len(ratchet) + len(stale)} findings.")
        return 1
    print("clean over every surface this gate covers.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
