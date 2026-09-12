# Prose profile: <project>

`STYLE.md` holds the rules. This document holds what that guide asks each project to
supply, because the portable core cannot know it. Where this document is silent, the core
governs. A profile refines the core and never relaxes it.

Fill in the sections in the order given. Only one of them is read by a script, and it is
marked. The rest are read by people, so an honest half-filled section beats an invented
complete one. Delete a section you have nothing true to put in it.

## Reader and depth budget

Rule 10 and Rule 11 need to know who reads what. One row per surface.

| Surface | Reader | Depth |
|---|---|---|
| `README.md` | Someone deciding whether to use this | The shortest text that gets a reader running one command |
| API reference | A developer calling the interface | One entry per public name: what it does, what it requires, what it guarantees |
| Doc comment on a public item | The same developer, in the rendered documentation | The API reference's, because that is what it is |
| Doc comment on anything else | Whoever changes the code next | The invariant, and why the code is as it is |
| Command-line help | Someone at a prompt | One line per option, no rationale |
| Internal notes | A maintainer, or the next person | As deep as the mechanism goes |

`STYLE.md` names the default classes, and a profile extends them. A guide with more than
one kind of chapter is the usual reason to. A chapter explaining a mechanism, one listing
what a version supports, and one walking a reader through a task have three depth budgets.
A single "guide chapter" row hides that.

## Fixed terms

Rule 9 wants one term per concept. This table is that decision, written down. The third
column matters as much as the second, because a reviewer needs to recognize the variant.

Where the project already has a document that owns this table, leave the section out and
point `terms.path` at that document. The same holds for the next section and
`answers.path`. Two copies of one table is the defect both tables exist to prevent.

| Concept | Write | Not |
|---|---|---|
| | | |

Two rows worth having early: the verb for a structure containing a field, and the verb for
a thing composed of parts. Rotating three verbs across one idea is the defect Rule 9 names,
and it is the easiest one to introduce without noticing.

This table settles how a concept is *spelled*. The next one settles where it *lives*. Do
only the first and the prose stays consistent about a mechanism that has three
implementations. Do only the second and one implementation gets described three ways.

## Answered questions

**A script reads this section.** Each row is a question the project answers once, and the
answer names the artifact that answers it. Rule 9 rests on this table, because reaching
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

Rule 6 says to name the category rather than enumerate it. Say here where that rule stops.

A gate that requires an explicit list on certain pages outranks Rule 6 on those pages, and
only the project knows which pages those are. Name the gate, the list, and the surfaces it
holds. Where no such gate exists, say so and delete the rest of this section.

## Domain vocabulary

**A script reads this section.** The contract has three parts, and all three must hold:

- The heading above must match `vocabulary_heading` in `prose-lint.toml`, character for
  character.
- The words must sit in one italicized run, the first one under the heading.
- The words must be separated by commas.

Rule 23 exempts the words a field uses from any rule that would plain them away. The gate
also reads this list to tell a lowercase technical name from a list item that forgot its
capital. That is the other reason a name belongs here. Adding a word here is how a
technical name is admitted.

*extent tree, up-case table, bounds-check, streaming, idempotent, webhook.*

An `-ing` form in that list is a technical noun and stays. So does a compound verb the
codebase already uses. The list is extended by use rather than by decision, and it is never
exhaustive.

## Evidence, and the claims that carry a boundary

Rule 2 requires a claim to carry its evidence. The spelling is per project. Some projects
use a tag vocabulary such as `[measured]` or `[assumed]`. Others name the gate that proves
the claim. Say which, and say whether the tags appear in public documents or in internal
ones alone.

Then list every claim that is true only inside a boundary, with the boundary. Rule 2 says a
scoped claim carries its scope everywhere it appears. A claim whose scope lives in one
document alone is the shape that rule exists to catch.

## Surfaces that repeat one claim

Rule 9's second-page rule needs the set. Where one sentence appears on several surfaces,
name the owner and name the derivations. A wording fix belongs at the owner first.

This matters most where a surface freezes. A package registry keeps a description
permanently, and a published commit message cannot be corrected at all.

## Publishing constraints

What must never appear in a document that ships: internal names, planning coordinates,
personal or machine detail, addresses. Name the document that owns this rule if one already
does, rather than restating it here and letting the two drift.
