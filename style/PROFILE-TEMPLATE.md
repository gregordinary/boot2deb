# Prose profile: <project>

`STYLE.md` holds the rules. This document holds what that guide asks each project to
supply, because the portable core cannot know it. Where this document is silent, the core
governs. A profile refines the core and never relaxes it.

Fill in the sections in the order given. Four of them are read by a script, and each of
those is marked with its contract. The rest are read by people, so an accurate half-filled
section beats an invented complete one. Delete a section you have nothing true to put in it.

## Readers and depth budget

Rules 1, 5 and 17 need to know who reads what. One row per surface. A surface that holds
evidence has two readers: the user of the fact, who follows the reading path, and the
checker, who reads the record. Name where the record is.

| Surface | Reading path: reader | Record: reader, and where it is | Depth |
|---|---|---|---|
| `README.md` | Someone deciding whether to use this | None | The shortest text that gets a reader running one command |
| API reference | A developer calling the interface | None | One entry per public name: what it does, what it requires, what it guarantees |
| Doc comment on a public item | The same developer, in the rendered documentation | None | The API reference's, because that is what it is |
| Doc comment on anything else | Whoever changes the code next | None | The invariant, and why the code is as it is |
| Command-line help | Someone at a prompt | None | One line per option, no rationale |
| Reference notes | A developer using the fact | A developer checking it: an Evidence section at the end of each note | As deep as the mechanism goes, with the method in the record |

`STYLE.md` names the default classes, and a profile extends them. A guide with more than
one kind of chapter is the usual reason to. A chapter explaining a mechanism, one listing
what a version supports, and one walking a reader through a task have three depth budgets.
A single "guide chapter" row hides that.

## Fixed terms

Rule 2 wants one term per concept, and the field's term over a coinage. This table is that
decision, written down. The "Not" column matters as much as the "Write" column, because a
reviewer needs to recognize the variant. The last column says whether the term is the
field's own or the project's. A coined term needs a glossary entry and the reason no
standard term would do.

Where the project already has a document that is home to this table, leave the section out
and point `terms.path` at that document. The same holds for the next section and
`answers.path`. Two copies of one table is the defect both tables exist to prevent.

| Concept | Write | Not | Standard or coined |
|---|---|---|---|
| A structure that contains a field | contains | carries, holds, has | Standard |
| | | | |

Two rows are worth having early. The first row fills one, the verb for a structure
containing a field. The other is the verb for a thing composed of parts. Rotating three
verbs across one idea is the defect Rule 2 names, and it is the easiest one to introduce
without noticing.

The fixed-terms table decides how a concept is *spelled*. The answered-questions table,
below, decides where it *lives*. Do only the first and the prose stays consistent about a
mechanism that has three implementations. Do only the second and one implementation gets
described three ways.

## House words

**A script reads this section.** The contract has three parts:

- The heading above must match `house_words_heading` in `prose-lint.toml` (the default is
  `## House words`), character for character.
- The words must be in the first table under the heading.
- The table's first column holds the forms of one word, separated by commas. The second
  holds the replacement, which the report shows. An optional third column holds phrases in
  which the word has its standard sense and is not reported, separated by commas.

A house word is a coinage where a standard term exists, or an ordinary word in a private
sense (Rule 2). The rows below are a seed: words that drift into a house sense in most
technical projects. Keep the rows that apply, delete the rest, and add the project's own.
The list is never exhaustive. A word missing from it is not thereby approved, because
Rule 2's test decides. The gate reports a listed word without ever reporting a page
"clean" of house words.

| Forms | Write instead | Standard in |
|---|---|---|
| carry, carries, carried | contains, has, applies to, performs | carry bit, carry-in |
| owns | documents, is the reference for | |
| settles, settled | confirms, determines | |
| buys, bought | gives, increases, reduces | |
| pays, spends, prices | costs, takes, uses | |
| lands, landed | is placed, measures, is | |
| rides | is programmed in, is added through | |
| the wall | wall-clock time, limit | wall-clock |
| floor, floors | minimum, threshold, fixed overhead | noise floor |
| ceiling | maximum, limit | |
| lever, levers | optimization, option | |
| knob, knobs | option, setting | |
| load-bearing | required | |

## Glossary

Rule 2 defines each project term, and each abbreviation or field term your reader does not
already have, once, in a glossary. Rule 3 links a document's first use of one to that
definition. Rule 2's subsection "The glossary" gives the entry's form. Name three things
here:

- **The location.** If the voice gate checks define-before-use or the entry form, the
  configuration names the same file.
- **The reader's boundary.** The vocabulary the reader already has, named by where they got
  it: "the SoC vendor's manual and the Linux kernel's driver interfaces". A term inside the
  boundary gets no entry. A term outside it gets one, even when the term is standard.
- **The source names.** The names the Source column uses for the documents that terms come
  from, so that one manual is not cited three ways.

A script reads the glossary file, not this section. It reads the file's first table. The
first column holds one term per row, spelled as the documents write it, and the third holds
the source.

## Genre and exemplars

Rule 4 uses the forms of the genre the reader already knows, and the portable core cannot
know which genre that is. Name it, and name two or three public exemplars a writer can open.
Candidates include a vendor's technical reference manual, a man page, an API reference and
an errata notice.

Then name the forms this project uses, and their markup:

- The callouts, and how they are written. GitHub renders `> [!NOTE]` and `> [!CAUTION]`.
- The erratum entry, its heading form and its fields.
- The Conditions block, and where it sits.
- The status values a support-status table can use.

## Record zones

**A script reads this section.** The gate's residue checks skip the record, and this
section says where the record is. Each list item's first backticked span is a heading
prefix. A heading whose text starts with it, ignoring case, opens a record zone that runs to
the next heading at its level or above. A whole file that is record is named in
`prose-lint.toml` as a `record_files` pattern instead. Start from these four zones:

- `Evidence`: the method, the controls, the run counts and the searched space behind the
  section's facts.
- `Validation`: the configurations a result was validated on, and how.
- `Revision history`: superseded figures, with their dates.
- `Appendix`: any appendix.

## Document skeletons

**A script reads this section.** Rule 3 orders a reference document by its class's
skeleton. One `###` heading per class. Under it, a line that opens with "Applies to:" names
the files by backticked glob patterns. A numbered list then names the top-level (`##`)
sections in order, each item's first backticked span being the heading prefix.

Mark an item `(optional)` when a document of the class can omit it. The gate checks a class
only once `prose-lint.toml` lists it under `skeletons_adopted`.

### Reference note

Applies to: `docs/**/*.md`

The sections, in order:

1. `Overview`
2. `Requirements` (optional)
3. `Known issues` (optional)
4. `Evidence` (optional)

## Answered questions

**A script reads this section.** Each row is a question the project answers once, and the
answer names the artifact that answers it. Rule 2 rests on this table, because reaching
for an existing answer requires being able to find it.

| The question | The answer |
|---|---|
| | |

The question is the unit, and the phrasing is not decoration. A writer about to add
something searches for the question they are answering, not for the name they would give
the answer. They have not chosen that name yet, and the existing one is rarely what they
would have picked. "What can one path component contain?" finds the answer. "Path
validation" finds it only for someone who already knows it is there.

Write each row so it can be searched and checked:

- Phrase the question the way the person who has not found the answer would ask it.
- Name the answer as a reader follows it, qualified: `store::path::components` rather
  than `components`. The gate holds a citation with a slash to that path, and every other
  citation to its last segment.
- Before adding a row, check the question is not already here under different words. Two
  rows asking one question is the defect this table exists to prevent, arriving in the
  table itself.

**A second answer is sometimes right.** When it is, it says so and says why, in the
documentation of the thing that is the second answer. An unexplained second answer is
drift even where the code is correct. The next reader cannot tell a decision from an
oversight, and the two age in opposite directions. Where the reason is a property of
the domain rather than of one call site, it belongs in a note under this table.

**A shared answer is swept for on arrival.** Where a change produces one, the change also
moves the callers that could have used it, and not only the ones it was about. An
abstraction that three callers out of eight use is worse than none: a reader learns the
mechanism and then finds it does not transfer.

**What the gate checks, and what it does not.** It confirms every citation still exists,
so a rename cannot leave a row pointing at nothing. It cannot confirm the answer still
means what the question says. A row that has quietly become the answer to a different
question is the failure a reader has to catch.

It also misses a rename whose new name is an ordinary English word, because the search then
matches any prose using it. A row is only as checkable as its answer is distinctive.

## Guards

Name what holds each of the two tables above to the code, what it can see, and what it
cannot. A rule nothing enforces is a rule nothing reports on, and this section is where a
project records which rules it has actually mechanized.

| The concept | The guard | What it cannot see |
|---|---|---|
| | | |

Three properties separate a guard worth having from a rule with a script attached:

- **One guard per concept.** Two guards over one idea disagree eventually, and the
  disagreement is discovered by whoever is unlucky rather than by whoever is responsible.
- **A guard records rather than bans.** A new site is sometimes right. A guard keyed to a
  recorded census fails on any difference from it rather than on the shape itself. That
  turns a new site into a line in a diff a reviewer can see, with its reason beside it.
- **A guard decides nothing.** What it buys is that a second answer is *chosen*. Where a
  guard is answering a design question, the question belonged in this profile.

## Protected enumerations

Rule 13 says to name the category rather than enumerate it. Say here where that rule stops.

A gate that requires an explicit list on certain pages outranks Rule 13 on those pages, and
only the project knows which pages those are. Name the gate, the list, and the surfaces it
holds. Where no such gate exists, say so and delete the rest of this section.

## Domain vocabulary

**A script reads this section.** The contract has three parts, and all three must hold:

- The heading above must match `vocabulary_heading` in `prose-lint.toml`, character for
  character.
- The words must sit in one italicized run, the first one under the heading.
- The words must be separated by commas.

Rule 29 exempts the words a field uses from any rule that would replace them with plainer
ones. The gate also reads this list to tell a lowercase technical name from a list item that
forgot its capital. That is the other reason a name belongs here. Adding a word here is how
a technical name's spelling is admitted. It does not admit a house sense of the word: Rule 2
still governs what the word means.

*extent tree, up-case table, bounds-check, streaming, idempotent, webhook.*

An `-ing` form in that list is a technical noun and stays. So does a compound verb the
codebase already uses. The list is extended by use rather than by decision, and it is never
exhaustive.

## Evidence, and the claims that have a boundary

Rule 8 requires a claim to have its evidence. The spelling is per project. Some projects
use a tag vocabulary such as `[measured]` or `[assumed]`. Others name the gate that proves
the claim. Say which, and say whether the tags appear in public documents or in internal
ones alone.

Say where the evidence goes (Rule 7): the inline tag, the Conditions block, and the record
section or document.

Then list every claim that is true only inside a boundary, with the boundary. Rule 8 says a
scoped claim keeps its scope everywhere it appears. A claim whose scope is written in one
document alone is the shape that rule exists to catch.

## Surfaces that repeat one claim

Rule 15's second-page rule needs the set. Where one sentence appears on several surfaces,
name its home and name the derivations. A wording fix goes to the home first.

This matters most where a surface freezes. A package registry keeps a description
permanently, and a published commit message cannot be corrected at all.

## Publishing constraints

What must never appear in a document that ships: internal names, planning coordinates,
personal or machine detail, addresses. Name the document that already states this rule if
one does, rather than restating it here and letting the two drift. If the voice gate
enforces it, the configuration's `internal_patterns` is the list.
