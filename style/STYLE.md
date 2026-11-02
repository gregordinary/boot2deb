# Prose and documentation style guide

How to write documentation in every form it takes: project READMEs, API references,
reference notes, procedures, handovers, and prose comments in code. The rules are portable
and apply to any project that wants the same voice. The examples come from real documents
and are kept concrete rather than generalized. A rule is easier to apply from a real
before and after than from an abstraction.

The goal is documentation that is **clear, concise, and authoritative, and only as
authoritative as the evidence allows**. It is organized for the reader's use and written in
the reader's words. A reader trusts a stated fact without knowing how it was arrived at. They
tell a proven fact from a working hypothesis at a glance. Rule 1 states the principle the
other rules follow from.

The guide has six parts. **The reader** governs whom a document serves: its order, its
terms, its forms, and where its evidence goes. **Substance** governs what a document claims.
**Voice** governs how it says it. **Instructional writing** covers procedures, **Format**
covers how information is laid out, and **Controlled structure** sets the countable limits
that make a page scannable.

Parts 1 to 5 need a reader to apply them. Part 6 needs a script, and that is the point of
separating it: those rules are the half a gate can enforce. A gate can also find candidates
for some rules in Parts 1 to 5, but a reader decides each one. A house word or a run count
in the reading path is such a candidate.

## This guide and its project profiles

The rules here are the portable core and hold for every project. What varies between
projects is not the discipline but the vocabulary. That is what the audience already
knows, what evidence tags a claim can take, and which terms are fixed. It is also which
words have drifted into a house sense, which genre the reader knows, and which documents
exist and own what.

That per-project material lives in a **profile**: one document per project, kept wherever
the project keeps its internal documentation. Nothing depends on where it sits, because the
gate reads it from the path its configuration names. A profile contains only what the core
cannot state generically:

| A profile names | Because the core cannot know |
|---|---|
| The reader of the reading path and the reader of the record, and what each already has | Depth, prerequisites and the record's location follow from them (Rules 1 and 5) |
| The document inventory, and the one document each fact belongs to | Rule 15's cross-linking needs one home per fact |
| The record zones: the headings and documents where evidence goes | Rule 7 moves process material there, and the gate skips it there |
| The evidence tag vocabulary, and where evidence is placed | Rule 8 requires tags, and their spelling is per project |
| Fixed terms, the wrong variants, and whether each is standard or coined | Rule 2 wants one term per concept, and a field term over a coinage |
| The glossary's location, the vocabulary its reader already has, and its source names | Rule 2 defines every other term once, and Rule 3 links to it at first use |
| The house words, each with its standard replacement | Rule 2 replaces them, and only the project knows its own drift |
| The genre the reader knows, with two or three public exemplars | Rule 4 uses that genre's forms |
| A document skeleton per class | Rule 3 orders a reference document by it |
| The questions the project answers once, and where | Rule 2 needs a writer to find the existing answer |
| The guard holding each of those | A rule nothing enforces is a rule nothing reports on |
| The depth budget per document | A reference note and a README both comply at different lengths |
| Publishing constraints | What must not appear in a document that ships |
| Enumerations a gate depends on | Rule 13 yields to them, and only the project knows which they are |
| Domain vocabulary that outranks plain words | Rule 29 exempts it, and the list is per project |
| Which surfaces repeat the same claim | Rule 15's second-page rule needs to know what the set is |

A profile refines the core. It does not relax it. Where a profile is silent, the core
governs. Read both before writing, and read the profile first when the question is "what
does this project call it" rather than "how is this written".

## Type, intent, location, audience

Four questions decide what belongs in a document and how it reads. They are independent, and
answering them is the first act of writing one. A set of research notes is a useful case.
It is public in location, internal in origin, and written for a developer rather than for
the user a README serves.

**Type.** What the document is: a README, an API reference, a reference note, a procedure, a
code comment, an index, a handover. Type sets the expected shape, and Part 5 turns it into a
format.

**Intent.** What it is for, stated as the reader's outcome rather than the document's
subject. "Get someone building and running." "Stop the next person repeating a dead end."
"Resume a thread after a gap." Where it is unclear whether something belongs, intent
decides.

**Location.** Internal or external. An external document is read by someone with no access
to the rest of the workspace and no context to supply, so it stands alone. It contains no
hostnames, IP addresses, credentials, private repository names, unpublished plans, or
internal tracking ids. An internal document can assume all of that, and can hold state and
dates an external one must not.

**Audience.** Whose hands it is in: a user running the software, a developer integrating it,
or a developer checking a claim before relying on it. It can also be a maintainer changing
it, or your own future self. Rule 5 applies this one sentence at a time.

The default classes, which a profile can extend:

| Class | Intent | Location | Audience |
|---|---|---|---|
| Project README | Build it, run it, fix the common failures | External | User |
| API reference | Call the interface correctly and know what it guarantees | External | Developer |
| Guide chapter | Understand one part well enough to use it | External | User or developer |
| Reference notes | State the facts a developer needs to use the subject, with the evidence for each kept in the record | External | Developer |
| Application note | Combine the mechanisms for one workload, with its constraints, accuracy and performance | External | Developer |
| Errata | Recognize a known defect and work around it | External | Developer |
| Validation record | Check how a fact was established before relying on it | External | Developer who checks |
| Code comments | Explain why this code is as it is | With the code | Maintainer |
| Procedure | Perform a task the same way every time | Internal | Developer |
| Handover | Resume a thread without re-deriving it | Internal | Future self |
| Index | Prevent a wrong start | Internal | Whoever arrives next |

Two of these have standing obligations. A **project README** is a self-contained "1.0"
description of current capability. A reader uses the project from its own docs without
opening another repository, and it documents the feature set as it is now. **Reference
notes** can go deeper, into mechanisms and negative results. They keep the same character:
the settled fact stated plainly, tagged with how it was established. Depth goes into a lower
layer (a subsection, the record, a linked note), not into the sentence.

### A doc comment is two classes, and the marker says which

A comment attached to a public item is the API reference. It ships in the rendered
documentation, a stranger reads it without opening the source, and it is held to every
rule below. A comment on anything else explains the code to whoever changes it next.

Most languages already separate the two, and the marker is the separator. Rust uses `///`
and `//!`, Go the comment run above an exported declaration, Python a docstring. A note
inside a function body is not documentation in any of them.

That boundary is worth stating because the two classes fail in opposite directions. An API
reference that reads like a note to a maintainer is Rule 5. A `SAFETY:` comment rewritten
to read like a reference has lost the invariant it existed to state.

A project's doc comments are usually the larger of its two prose surfaces, and are almost
always the one nothing enforces. `prose-lint.py` extracts them, and `prose-lint.toml`
carries the configuration.

## Depth follows the class

Depth is a property of the class, not of the writer's appetite. A reference note that runs
to a thousand lines and a README that runs to eighty can both be correct. Each becomes
wrong when it takes the other's length. Set the budget before writing, from what the reader
does with the document.

| Class | Depth | What over-length looks like |
|---|---|---|
| Project README | The shortest text that gets a reader building, running, and past the common failures | Mechanism a user cannot act on. A rationale that serves the maintainer |
| API reference | One entry per public name: what it does, what it requires, what it guarantees, what it refuses | Narrative between entries. A tutorial the README already gives |
| Guide chapter | One subject, from what it is to a reader running it | A second chapter's subject. A reference table the API documentation is home to |
| Reference notes | As deep as the mechanism goes, with the method behind each fact in the record | Repetition across notes, and a fact with no home restated in three files. Method, run history or refuted causes in the reading path, and a fact delivered inside the investigation that found it |
| Application note | The workload's constraints, method, accuracy and performance, each with its conditions | Mechanism that a reference note already documents |
| Errata | One entry per defect, in the fixed fields | The investigation that found the defect |
| Validation record | What a checker needs to repeat each measurement | A finding stated only here that the reading path needs |
| Procedure | One line per action plus what the check proves | Reference material inlined into a step |
| Index | A pointer and the trap that stops a wrong start | An accreting session log |

Three depth failures recur. **Under-depth in a reference note** drops the evidence behind a
claim, which Rule 8 then cannot tag. **Over-depth in a README** is Rule 17 and Rule 5:
rationale longer than the instruction, aimed at the wrong reader. **Evidence in the
sentence** is the reference note's own failure: the method placed between the reader and the
fact. Rule 7 moves the method to the record.

None is fixed by trimming words evenly. Each is fixed by moving material to the class, or
the zone, it belongs in.

Every class follows every rule below. Depth and audience change. The discipline does not.
Two failures are worth watching for. Rule 5 covers the first, a README written for a
maintainer. The second is an internal fact left in a document that ships, which nothing
catches but a check before publishing.

---

# Part 1: The reader

## Rule 1. Write reader-based prose: the reading path and the record

Organize a document around what its reader does with it. Use the words that reader already
knows, in the forms that reader's genre already uses. The writer's path to a fact, the
writer's vocabulary and the writer's order of discovery belong in the record, never in the
reading path.

Linda Flower named the opposite *writer-based prose* ("Writer-Based Prose: A Cognitive Basis
for Problems in Writing", *College English*, 1979). It has three features, and a rule here
answers each one:

| Writer-based feature | What it looks like | Rule |
|---|---|---|
| A narrative of discovery | The text follows the order in which the writer found things out | 6 and 7 |
| A survey of the writer's material | Organized around what the writer has, not what the reader needs | 3 |
| Privately loaded code words | Terms whose meaning the writer knows and never states | 2 |

A fourth failure has a name in rhetoric, *procatalepsis*: answering an objection before
anyone raises it. In documentation the objection is usually one the reader never had, and
Rule 10 covers it.

### The reading path and the record

A document has a **reading path**, its body: the text a reader follows to reach a fact or
perform a task. Where it holds evidence, it also has a **record**: a named section at its
end, or a separate validation document. The reading path states what is true, what is
required and what to avoid. The record states how that was established. The profile names
the record's headings and documents for each class.

| Content | Reading path | Record |
|---|---|---|
| The fact, the requirement, the behavior | Yes | |
| The evidence tag | Yes, inline and short | |
| The conditions: part, version, configuration, shape | Yes, once per table or section, as a Conditions block (Rule 4) | |
| A hazard | Yes, as a Caution callout or an erratum entry (Rule 11) | |
| An absence a reader would act on | Yes, where the reader acts (Rule 10) | |
| Support status | Yes, as a status table near the top (Rule 3) | |
| The method, the controls, the configurations compared | | Yes |
| The searched space of a negative | A summary, as "Not characterized: ..." | The full search |
| Run counts, return codes, test and harness names | | Yes |
| Excluded causes and refuted hypotheses | | Yes, as an Excluded causes list |
| Superseded measurements | | Yes, dated, in a revision table |
| Predictions and their scores | | No. The project's prediction ledger |
| Method rules and maxims | | No. The project's method guide |
| Plans, to-dos and re-measure items | | No. The tracker |
| Internal references: tracker ids, rule numbers, private document and script names | | No. The tracker and the handovers |
| Session anecdotes | | No. The measurement an anecdote held stays, restated as a result |

The record keeps the evidence a stranger can use: the method, the controls, the searched
space and the excluded causes. It also keeps the run counts and the superseded figures with
their dates.
What a stranger cannot use does not enter the document at all.

The test for a sentence: does it tell the reader what is true, or how the writer came to know
it? The first goes in the reading path, and the second goes in the record.

## Rule 2. Use the reader's terms

For a concept, use the term the field's standard documentation uses. For an identifier, use
the codebase's spelling, in backticks. A project term is admitted only where no standard
term exists, and is then defined once, in the glossary the profile names.

**Field vocabulary** is the set of terms the field's own standard documentation uses: vendor
manuals, standards, and the reference texts a practitioner has read. **House vocabulary** is
a project's own coinages, and ordinary words used in a private sense. Rule 29 exempts field
vocabulary from the rules that would replace a word with a plainer one. House vocabulary has
no exemption.

A **house word** is a coinage where a standard term exists, or an ordinary word in a private
sense. A metaphor standing in for a quantity is a house word: *the wall* for wall-clock time,
*floor* for a minimum, *buys* for "gives". Replace it. The test: would a developer who has
read the field's vendor documentation, but not this project, know what the word means here?

| Instead of | Write |
|---|---|
| "Core 1 binds and runs jobs, and buys nothing." | "Core 1 probes and executes jobs. Enabling it does not increase throughput." |
| "N buys throughput almost linearly." | "Throughput scales almost linearly with N." |
| "The defaults already carry most of the win." | "The default settings provide most of the gain." |
| "Most of the content is IP-inherent: true of the NVDLA-derived rknpu block on any Rockchip SoC that carries it." | "Most of this content applies to every Rockchip SoC that integrates the RKNPU, an NVDLA-derived design." |

Four rules keep a document's terms consistent:

- **One sense per term.** A word that names two quantities is two terms, and one of them
  needs another word. This is ASD-STE100's one-word-one-meaning principle, adopted without
  its dictionary. In one file, "floor" names a minimum batch size, a memory reserve, a fixed
  per-call overhead and a throughput limit. The word is four terms with one spelling, and the
  reader works out which one each time.
- **One term per concept.** Varying the word for elegance makes the reader ask whether two
  things are meant. Where the code calls it `rocket_npu.h`, the prose calls it that, not "the
  seam" in one paragraph and "the interface" in the next.
- **One verb per relationship, and the field's verb.** Pick one word for containment and one
  for composition, and hold them apart. A register *contains* a field, and a workspace *has*
  the crates it is made of. Rotating *contains*, *holds* and *takes* across one idea reads as
  three relationships. Where the field's documentation has a verb for the relationship, use
  that one.
- **An identifier is exact.** A function, a register, a flag or a file keeps the codebase's
  spelling, in backticks. The rule about concepts never renames an identifier.

The profile lists the project's house words with their replacements, starting from the seed
list in the kit's profile template. The list makes the common ones visible. It is never
exhaustive, and a word missing from it is not thereby approved: the test above decides.

Where a project term has to stay, the glossary defines it once, and Rule 3 links its first
use in each document to that definition.

### The unit is the question, not the term

One term per concept settles how a thing is spelled. It does not settle whether the
project has one of them. A codebase can hold three implementations of one idea while every
document about them uses the same word. The second half of the rule is one answer
per question.

The question is the unit because of who is searching. A writer about to add something has
not named it yet, and the name in the tree is rarely the one they would have picked. A
search for the name they have in mind returns nothing, so they write the second answer. A
search for the question they are answering returns the first one.

So the project's record is a list of questions, each with the artifact that answers it.
"What can one path component contain?" is a row. "Path validation" is a topic.

A profile holds that table, and a project with no profile still benefits from asking the
question in the form above before adding anything.

**A second answer is sometimes right.** When it is, it says so and says why, in the
documentation of the thing that is the second answer. The reason is usually that the two
cases differ in something the shared form would have to be told. That is the domain
speaking, not an opinion. An unexplained second answer is drift even where the code is
correct, because the next reader cannot tell a decision from an oversight. A decision
nobody recorded is one the project makes again every time it is noticed.

**A shared answer is swept for on arrival.** Where a change produces one, the change also
moves the callers that could have used it, and not only the ones it was about. An
abstraction that three callers out of eight use is worse than none: a reader learns the
mechanism, and then finds it does not transfer.

### The glossary

A reader opens the glossary from a link at a term's first use, with one question: what does
this word mean here? Each entry answers that question and nothing else. It also says where the
word comes from, because a reader who knows a term is coined does not search the vendor's
manual for it.

The glossary lists every project term, and every abbreviation or field term that the
profile's reader does not already have. The profile draws that line. A term the reader
already has gets no entry.

The glossary is a table with one row per term, in alphabetical order, ignoring case and
markup. Its columns are the term, the definition, the source, and the nearest standard term.
Symbols that one formula defines, such as a matmul's M, K and N, go in a notation table after
it. Each row follows these rules:

- **One term per row, spelled as the documents write it.** A reader's search and a gate's
  define-before-use check both find a term by its row. Related terms take a row each, and a
  definition can name its sibling.
- **An abbreviation opens with its expansion, from the term's source.** Where the source does
  not expand it, the definition opens "Not expanded." and adopts no expansion from a
  secondary source. The reader then stops looking for one. Where the source expands only
  part, the definition gives that part and names the letter it leaves.
- **Then the class, and what sets it apart.** The definition's first phrase names the kind of
  thing the term is: a block, a field, a channel, a host pass, an interval. The rest of the
  phrase distinguishes it from others of that kind, and the whole phrase could replace the
  term in a sentence. A locator, such as a register range, an ioctl or an option name,
  identifies the thing without defining it, so it comes after, in parentheses.
- **Define the term, not the facts about it.** A definition holds what stays true when a
  measurement changes. A measured value, a default, a per-part parameter, a driver version's
  behavior or a hazard's detail belongs to the note that owns it. The entry links that note.
  A constant that is part of the meaning stays: an atom is 16 bytes. The test is whether a
  re-measure, or a second part, could make the entry wrong.
- **Three sentences at most, after the expansion.** An entry that needs more has taken in
  facts about the term.
- **One sense.** Where the field uses the word in a sense the reader already knows, the entry
  says which sense the documents use. Where the documents themselves use it in two senses,
  the profile renames one. A glossary can define both senses, and the reader still has to
  work out which one each use means.
- **No undefined words.** A definition uses only words the reader already has and terms that
  have their own rows. It never uses the term it defines. A definition that needs an
  undefined term has found a missing row.
- **The source says where the term comes from.** It names the document that defines the
  term, such as a vendor manual, a standard or a driver interface, or it says Coined. A coined
  term also gives its nearest standard term where one exists, which tells the reader what to
  search for elsewhere. A term from one vendor's documents gives the other vendor's
  counterpart in the same column.

| Instead of | Write |
|---|---|
| **CNA**: "The block at `0x1xxx` that loads the input feature and the weights into the CBUF and sequences them into the MAC array." | **CNA**: "Not expanded. The convolution front end of an NPU core (registers `0x1xxx`). It loads feature data and weights into the CBUF and feeds them to the MAC array." Source: RK3588 TRM. Nearest standard term: NVDLA CDMA and CSC. |

The first opens with an address, which locates the block without saying what kind of block it
is. It never says what the letters stand for. Rockchip's manual does not expand them, and
saying so answers the question the reader arrived with.

## Rule 3. Order a reference document the way its reader learns the subject

Each section is readable with only the sections before it. Six practices apply the rule:

- **Define before use.** A term is introduced before it is used, at document scale. Where an
  earlier section cannot introduce it, link its first use to its definition.
- **Result before mechanism.** A section a reader opens to make a decision gives the result
  first, then what to set, then how the mechanism decides, then its internals. A reader who must
  cross the budget arithmetic to reach the table they came for is reading the writer's order.
  A level-2 section past a few hundred lines is a candidate for splitting. *(gate:
  `section_size`, `forward`)*
- **A table cell is scanned, not read.** A reference table's cell holds what tells its row
  from the others: what the entry does, its limits, its accuracy. Past a few sentences it is a
  section inside a table. A reader scanning the column for one row must then read a paragraph
  to get past another. Give the entry a subsection, keep the contract in the cell, and link the
  cell to the subsection. *(gate: `cell_size`)*
- **Progressive disclosure.** Each section opens with what the subject is and does, and
  detail links downward: to a subsection, to the record, or to another note.
- **Status first.** Support status goes in a table near the top, not in prose spread through
  the document.
- **Foundation before detail.** A list of register-level hazards comes after the reader has
  met the registers.

The profile names a skeleton for each class: the sequence of top-level sections the reading
path follows, and the record's heading. The default for a hardware reference follows a TRM
chapter:

1. Overview and features
2. Support status
3. Functional description
4. Programming model
5. Requirements and limits
6. Known issues
7. Performance, with its conditions
8. The record

The order a document was written in is the order the writer learned the subject, which is
rarely the order a reader can. A front page can list `DPU_RDMA`, `size_e` and the BS stage as
hazards before it has said what the DPU is. Such a page has been written in discovery order.

A procedure has its own order, which Rule 20 sets.

## Rule 4. Use the forms the reader's genre already has

A reader who knows the genre reads its forms without effort, and a home-made form costs them
the time it takes to learn it. The profile names the genre and two or three public
exemplars. The generic forms:

| Form | Use it for | Shape |
|---|---|---|
| Note | Information that changes how the reader acts, where nothing breaks | A labeled callout |
| Caution | A hazard: wrong output, a hang, lost data | A labeled callout |
| Erratum entry | A known defect and its workaround | A heading `Erratum <id>: <symptom>`, then the fields Description, Conditions, Symptom, Workaround and Status |
| Conditions block | The operating point that a section's or a table's results hold under | One labeled line, `Conditions:`, under the heading or the table |
| Not characterized | The scope a measurement did not reach | One labeled line, `Not characterized:`, under the Conditions block |
| Support-status table | What works, what is the default, what does not | One row per feature, and a Status column from a fixed set |
| Register or field table | A register's layout | Offset, field, bits, reset, function |
| Correction table | A published source that is wrong | Published value, correct value, evidence |
| Revision table | Superseded figures, in the record | Date, what changed, the earlier value |
| Glossary | The project's terms, and the abbreviations and field terms its reader lacks | One row per term, alphabetical: the term, its definition, its source, and the nearest standard term (Rule 2) |

A Conditions block states the settings its results hold under, never a result. A figure
measured under those settings belongs in the table or the text, and an older run's figure in
the record. *(gate: `conditions_result`)*

A requirement is written as a requirement, with the agent that must act: "Software must
program both fields." It is not written as the consequence of breaking it: "Set both, or
negative products come back at the wrong scale."

The labels of these forms (Note, Caution, Conditions, Not characterized, and the erratum
fields) are labels, not headings, and Rule 18's rule on bold lead-ins does not apply to them.
The markup for a callout is the profile's choice, from what its renderer supports. GitHub
renders `> [!NOTE]` and `> [!CAUTION]` as labeled callouts.

## Rule 5. Write for the reader's role: user, checker, maintainer

Ask who acts on a sentence. Three readers hold technical documentation, and each has a home:

| Reader | Wants | Reads |
|---|---|---|
| The user of the fact | What is true, what is required, what to avoid | The reading path |
| The checker | How it was established, so they can trust it or repeat it | The record |
| The maintainer | Why the software or the document is built this way, and how to change it | Source comments, the profile, a CONTRIBUTING section |

Instructions to whoever edits the document ("add a row", "promote the correction into the
subsystem note") are maintainer material. They belong in the profile, not on a reader's
page.

A project README is read by someone installing, building, running, and troubleshooting. That
reader needs what changes their commands, their configuration, or their expectations. An
explanation of *why the software is built this way* serves a maintainer. It belongs in a
source comment, an internal note, or the research record.

This is Rule 17's sibling and not a restatement of it. Rule 17 asks whether a rationale has
earned its length for the reader it is aimed at. Rule 5 asks whether that reader is the one
holding the document at all.

The distinction is not depth, it is audience. Both of these are technical. Only one is for
the README's reader:

| Maintainer material | The same fact, for a user |
|---|---|
| "That release grew `rknpu_mem_create` from 40 bytes to 48, and the ioctl request word encodes the structure size, so an older driver does not recognize the allocation command." | "The memory allocation interface changed in 0.9.6, and this provider speaks only the later form, so every allocation would fail." |
| "The driver does not validate flags against `RKNPU_MEM_MASK`, so an older one accepts the bit and ignores it." | "`RKNPU_IOVA_TIGHT` is unavailable on 0.9.6. The provider withholds it and says so at open." |
| "`iova_rcache_insert()` takes only sizes up to 32 pages, so the effect is size-selective at 128 KiB, which is why three probes found nothing." | "A board with unknown uptime can already be degraded. Check the domain before trusting an allocation-sensitive measurement on one." |

**A behavior change is user material even when its cause is not.** "This option is
unavailable below 0.9.7" is something the reader observes and plans around. "Because the
driver never checks `RKNPU_MEM_MASK`" is why the code does what it does.

The test: name the action the reader takes because of this sentence. If there is none, and
the sentence exists to justify an implementation choice, it is maintainer material.

**Relocate, do not delete.** Detail cut from a README is often the most expensive thing in
the repository to have learned. Confirm it survives in a source comment, an internal
document, or the research notes *before* removing it. Where the reader might want it, name
the file that now holds it.

**Where depth has to stay, name its audience in the section's first line** so everyone else
can skip it. For example: "Reference for readers tuning the defaults above or changing the
provider."

---

# Part 2: Substance

## Rule 6. State the settled fact, not the path to it

A document describes how things behave *today*. The reader wants the current truth, stated
once, plainly. The path to a fact has two halves, and neither belongs in the reading path:

- **Edit history**: how the document changed. Version control records it.
- **Investigation history**: the runs, the refuted readings, the candidate causes and the
  order of discovery. The record holds it, with dates, and the reading path states the
  result.

| Instead of | Write |
|---|---|
| "Behavior was thought to be Y but is now understood to be Z." | "Behavior is Z." |
| "Corrected: the rule is C2 = 16 / out-bytes." | "The rule is C2 = 16 / out-bytes." |
| "An earlier note guessed C2=8, which was wrong." | (delete, and describe the C2=4 cube) |
| "First attempt failed, then we overturned it and it works." | "It works: ..." |
| "RESOLVED 2026-06-22: an identity conv supplies the feed." | "An identity conv supplies the feed." |
| "P1.4 DONE: K-accum gives +19%." | "On-NPU fp16 K-accumulation gives +19%." |
| "The lead re-ran s8 on core 2 and s13 on all three cores with byte-identical output." | "Repeat runs on core 2 and on all three cores produced byte-identical output." |
| "Each of those three was named as the thing this run introduced, and each was excluded by the next run." | (move to the record's Excluded causes list) |

<!-- prose-lint: off -- a vocabulary list, where completeness is the point. Rule 13. -->

**Framings to cut:** *corrected, previously, used to, was thought, turned out, initially,
originally, no longer, earlier note, walked back, we had to, now understood, RESOLVED,
formerly, once thought, was broken, the old path, the first run, the next run, re-ran, was
excluded, refuted, so far, the lead, this session, the handover, needs a re-measure.* Also
the status verbs **DONE / COMPLETE / WIP** and internal tracking ids (**P1.4**, **Task A**),
which track a project rather than the subject.

<!-- prose-lint: on -->

### Dates

- **Drop edit-event dates.** "Corrected 2026-06-22", "DONE 2026-06-20", "Post-2026-06-22".
  These stamp when the document changed.
- **Keep measurement provenance.** "Measured 2026-06-22 (kernel 7.1.0-1, 600 MHz)" stamps
  when a number was observed and under what conditions. That is reproducibility
  information.

The test: does the date say *when a fact was measured* (keep) or *when the document
changed* (drop)?

### One reading per quantity

Where an older and a newer measurement of the same quantity both stand, the reading path
gives the current one with its conditions. The record gives the older one in a revision
table with its date. A latency quoted five ways across one document leaves the reader to
work out which is current.

A table row is one measurement. A row whose figures come from different runs or builds,
joined by a footnote mark, is two readings in one row. A reader who divides one figure by
another then computes a quantity nobody measured. A figure that every row repeats is stated
once, above the table. *(gate: `quantity`, `row_mix`, `column_repeat`)*

A ladder, a table whose rows each state a step and the value it reaches, takes each step from
the values it joins. A step quoted from the lever's own A/B is a second reading of the same
quantity. "+19% -> 13.38" after 11.40 is a 17% step, and a reader who multiplies down the
column reaches a number nobody measured. *(gate: `ladder`)*

## Rule 7. Keep the record out of the reading path

**Process residue** is anything that tells how a fact was established rather than what the
fact is. It includes:

- Run counts, return codes, and test and harness names
- Run narration ("the first run", "re-ran")
- Excluded causes and refuted readings
- Predictions and their scores
- Superseded figures
- Plans and re-measure items
- Citations of internal documents
- Method rules stated as maxims

Residue goes to the record. Plans go to the tracker. Method rules go to the method guide. The
reading path keeps the evidence tag and one Conditions block per table or section.

| Reading path, with residue | Reading path | Where the rest goes |
|---|---|---|
| "All gates return `rc=0` (57 of 57), the taint is unmoved, and `dmesg` shows zero WARNINGs." | (the support-status table states what works) | The record: a validated-configuration table with the gate count, the return codes, the kernel taint and the log |
| "The cap was 1.85 ms (17%) and the measurement 2.0-2.1 (19%): a hit, low by the feature term." | "The packed stem saves 2.0-2.1 ms (19%)." | The prediction ledger: the cap and its score |
| "The method point outlives these numbers." | "Note: A 5-run test does not detect the 16-20% failure rate. Use at least 25 runs." | The method guide: the general rule |

A method maxim ("A stamp read back is not a measurement") states a rule about investigating,
and a reader of the subject cannot act on it. Where the reader needs its consequence, state
the consequence as a requirement or a Note.

**A relocation is not a deletion.** A fact moved into the record must arrive there intact,
with its numbers, its conditions and its tag. Check the move across files, not within one.

## Rule 8. Claim only what the evidence supports

Be authoritative where it is earned, and only there. Tag a definitive claim with how it was
established, and tag anything unestablished as a prediction. An untagged prediction reads
exactly like a settled fact and gets inherited as a premise instead of as the question
under test.

Match the strength of the language to the strength of the evidence:

| Evidence | Language |
|---|---|
| Directly observed, reproduced | "X is Y." "Bit-exact." |
| Observed and corroborated by an authoritative source | "X is Y [measured, source-confirmed]." |
| A single run, possibly noisy | "Measured ~N at this operating point." "X appears to ..." |
| Mechanism inferred, not isolated | "The likely cause is X (inferred, not proven)." |
| Untested | "X is expected to be Y [expected]." "Untested." Or omit the claim. |

**A negative's search is recorded with it.** "No register does X" is a result about the sweep
that looked. The record states what was searched, so the boundary reads as the method's and
not the subject's. The reading path states the negative only where Rule 10 admits it, and
summarizes the scope in one line: "Not characterized: ...".

**The tag vocabulary is per project, and the requirement is not.** Every project fixes a
small set of tags in its profile and uses them literally. A reader then tells one grade of
evidence from another by pattern, rather than by reading the prose around it. A
project with no vocabulary of its own uses the four grades in the table above, spelled
`[measured]`, `[source-confirmed]`, `[inferred]` and `[expected]`. A tag invented for one
document is worse than none, because it reads as a grade the reader is expected to already
know.

**Do not inflate, and do not over-hedge either.** One unreplicated run is not definitive. A
plausible unisolated explanation is a hypothesis. Label it one. But a solid, reproduced
fact stated with "seems to" or "in our testing" only reads as unsure. Authoritative where
earned, qualified where not.

### A claim names what it is about

A sentence describing what some other component does names that component, so the claim is
checkable. "The reader rejects it" is not checkable. "The ext reader rejects it" is. This
is the prose form of a rule code documentation already needs. A justification lives with
the thing it justifies. A sentence copied from a sibling is true of the sibling and false
where it was copied to.

A ratio names its baseline in its own sentence. "38x worse" leaves the reader to guess against
what, but "raised perplexity from 25 to 964 against the per-tensor scale" does not. A loss is a
fraction or a percentage slower, never a multiplier above 1 with "loss" after it. *(gate:
`ratio`)*

### A scoped claim keeps its scope on every surface

Where an internal document has already bounded a guarantee, the public statement of that
guarantee states the boundary. A headline property stated flat is read as unbounded, and a
reader who infers more has done the reasonable thing. A claim with no stated domain cannot
be falsified by a later reader and cannot be extended by one either.

The form is fixed. In a document with a reading path and a record, the scope is one
Conditions line per table or section. On another surface, it is the clause that names the
boundary. The clause gives the version it holds for, the configuration it holds under, or
the axes a certification varied. The scope is never argued as a sentence: "This holds while
that bottleneck binds, and is not a property of the silicon."

## Rule 9. Keep epistemic scope, cut autobiographical hedging

These look similar and are opposites:

- **Autobiographical hedging: cut.** "We initially thought", "it took three tries", "this
  surprised us." That is Rule 6 again.
- **Scope hedging: keep.** "Measured at 600 MHz, resident weights, three cores." "Validated at
  N<=32. Larger N not characterized." "Portable by construction, validated on one chip."

State the conditions, and the scope follows from them. Conditions a whole document shares
are stated once, under its title, and each Conditions line then names only what differs. The
same clause repeated in every Conditions line is a refrain the reader learns to skip, and then
misses the one line that differs. *(gate: `refrain`, and `stale` for the figures a later
operating point has not re-measured)*

Do not append a disclaimer of permanence, such as "a measurement at the current operating
point, not a proven property". The disclaimer asks the reader to doubt what the Conditions
line already bounds, and repeated down a document it becomes a refrain.

Removing a scope caveat to sound more confident is overclaiming, which is the opposite of
the goal.

## Rule 10. State a negative only where the reader would act on its opposite

The set of things that are not true is unbounded, so a negative has to earn its place. State
one when all three of these hold:

1. The reader would otherwise act on the opposite. A cited source says it, a sibling part
   does it, the obvious design implies it, or the reader's next step depends on it.
2. Acting on the opposite costs something: a hang, wrong output, lost data, or wasted work.
3. It is stated where the reader would act on it, once.

Give an admitted negative its form:

| Negative | Form |
|---|---|
| It fails silently | A Caution callout, or an erratum entry |
| It is not supported | A row in the support-status table |
| A source is wrong | A correction table: published value, correct value, evidence |
| It has not been characterized | One "Not characterized" line under the Conditions block |

Do not quote a misreading in order to refute it, unless a cited source published it. Do not
list alternatives nobody proposed. Do not restate a scope boundary as a denial ("not a
property of the silicon"), because the Conditions block already states the boundary.

| Instead of | Write |
|---|---|
| "`M%4==0` is the real constraint." | "M must be a multiple of 4." |
| "There is no seventh. In particular there is no bit-reversal, no endianness flip, no field rotation." | "Each difference is one of the six edit types in Table 1." |
| "The plausible assumption is that the field belongs to a fully-connected mode." | "`FC_DATA_BANK` is active in convolution mode. Software must program it to 0." |

Rule 12 governs how a thing is defined. This rule governs which negatives a document states
at all.

## Rule 11. Write traps forward-looking, in a fixed form

A surprising behavior that another implementer would hit is worth documenting. Write it as a
warning to the next person, in the form the genre has for one: a Caution callout, or an
erratum entry (Rule 4).

**The trap test:** a reader plausibly does X, and X fails silently or costs time. A behavior
that fails loudly, or that no reader would try, is not a trap. State it as a requirement or
a limit.

<!-- prose-lint: off -- a specimen the rule is about, quoted as written. -->

- Instead of: "We packed int4 with int8's N-group and it broke; we fixed it."
- Write:

  > [!CAUTION]
  > An int4 weight packed with int8's N-group-of-32 matches the correct layout only at K=32,
  > so a single-K-group test passes and K>32 fails. Test weight tiles at K >= 2x the K-group.

<!-- prose-lint: on -->

An erratum entry takes the same content in fixed fields, so a reader can scan a list of them:

```markdown
### Erratum E1: incorrect output with two jobs executing at once

- **Description:** Two jobs that execute at the same time on the two cores corrupt each
  other's output.
- **Conditions:** Part B, any driver version measured, two jobs in flight at once.
- **Symptom:** Both jobs complete in the usual time and write incorrect values. No error is
  reported.
- **Workaround:** Software must leave core 1 unbound, or serialize submits so that one job
  executes at a time.
- **Status:** Open.
```

Inside those forms, and only there, a "you might expect X, but it is actually Y" framing
belongs. There, the surprise is in the subject and not in the edit history. Everywhere
else, state the behavior.

---

# Part 3: Voice

## Rule 12. Characterize by what a thing is and does

Define by positive identity. A definition built out of what something is *not* forces the
reader to hold the wrong model in mind while reading the correction.

| Instead of | Write |
|---|---|
| "is shaped by the silicon rather than by the driver" | "targets the silicon" |
| "`TFLITE_DIR` is a header root, not a library" | "`TFLITE_DIR` is a header root:" |
| "`GGML_CPU_ARM_ARCH` is not optional here" | "`GGML_CPU_ARM_ARCH` is required here, because ..." |
| "the NPU appears as an `rknpu` node rather than as a mainline `accel/rocket` device" | "`libnpu` drives the mainline `accel/rocket` device. Most distributions ship the BSP kernel instead, where the NPU is an `rknpu` node." |
| "a fact to read rather than a problem to fix" | (delete, and state the fact) |
| "which needs no kernel patch" | (state what it does need) |

**Keep the contrast where the contrast is the content.** Three shapes earn it:

- A **diagnostic discriminator**, where telling two outcomes apart is the reader's task.
  "A failure that names an allocation is the IOVA window, and the arithmetic is unaffected."
- A **correction against an adjacent step**, where the reader is about to repeat the wrong
  one: "`GGML_LIB_DIR` is this host's `build/ggml/src` rather than its `build/bin`."
- An **established term pair**: "host-bound rather than device-bound."
- A **cost stated next to a win**: "The flag reduces memory use and does not raise
  throughput."

The test: does the negative half tell the reader something they will act on, or does it
only make the positive half sound more interesting? Rule 10 then decides whether a negative
that is not a definition belongs in the document at all.

## Rule 13. Name the category, not an enumeration of it

An overview earns its place by compressing. A list of five examples where one category noun
would do makes the reader assemble the abstraction that the writer left out.

| Instead of | Write |
|---|---|
| "Everything above the seam: the register encoders, the tiling, the on-NPU op library, the graph planner, and the ggml, TFLite and ONNX Runtime frontends" | "The rest of the stack ... up through the `ggml`, TFLite and ONNX Runtime frontends" |
| "device open and close, buffer allocation and teardown, cache maintenance, submit, and the capability and counter queries" | "device lifetime, buffer management, cache maintenance, submission, and the capability queries" |

Two failure modes, and both are common:

- **Enumerating instead of categorizing.** Cut the internals. Keep the items the reader
  can act on. Naming the frontends is useful because a reader wants to know theirs is
  covered. Naming the tiling layer is not.
- **A category too vague to hold the list.** "The vendor uAPI expresses several things
  differently" names nothing. "The vendor uAPI differs in buffer identity, job structure
  and submit semantics" names three categories that cover six table rows.

Lists belong where completeness is the point: a supported-model list, a chip-availability
list, a reproducibility line stating the measurement conditions. Keep those.

**This rule runs before Rule 24, not against it.** Rule 24 turns a long enough run of
parallel items into a vertical list. Reading both rules at once makes a six-item run look
like a six-item list.

The order settles it. Rule 13 asks first whether the enumeration belongs, and a category
noun ends the question there. Rule 24 then formats whatever survives. Applying them the
other way round produces the shape both rules exist to prevent, which is a vertical list
of items a single word covered.

**Where something mechanical depends on the enumeration, the enumeration wins.** A gate
that greps a page for every member of a set turns "name the category" into a red build.
The category noun is the exact wording it cannot see. A project with such a gate names the
protected enumerations in its profile, and this rule stops at them. The general form: a
rule about compression yields to a rule about coverage, because a reader can reassemble a
category and a grep cannot.

## Rule 14. Name the mechanism or the quantity, not a metaphor

A metaphor reads as a technical term and is not one. Replace it with the relationship or the
quantity it stands for. Three kinds recur:

- **Spatial words**: *above*, *below*, *underneath*, *on top of*.
- **Figurative quantities**: *the wall*, *floor*, *ceiling*, *lever*, *route*, *rung*.
- **Figurative verbs**: *buys*, *pays*, *prices*, *spends*, *rides*, *poisons*, *lands*.

| Instead of | Write |
|---|---|
| "Everything above the seam compiles unchanged" | "The rest of the stack targets the silicon, so it compiles unchanged" |
| "The seam sits below the frontend, so a frontend picks the NPU up" | "A frontend links that library and inherits the route" |
| "each links the library, and the seam is below them" | "each links `libnpu`, which steps 1 through 4 have already pointed at the vendor driver" |
| "The offload buys 1.75x the speed by moving 1.48x the bytes." | "With NPU offload, prefill is 1.75x faster and moves 1.48x the DRAM bytes." |
| "Readback does not set the prefill wall." | "Readback does not determine prefill wall-clock time." |

A term the codebase uses (a file name, a test name, an API name) is not a metaphor. Keep it.
Rule 2 covers house words in general. This rule names the kind that most often passes for a
technical term.

## Rule 15. Plain, dense, declarative

- **Lead with the subject.** The first sentence of a section says what the subject is or
  does, then its key result. A key result is a fact about the subject, not a position in a
  debate.
- **Prefer short declaratives.** Break a sentence with three parenthetical asides into two
  or three sentences. Keep the precision, drop the nesting. Rule 26 puts a number on this.
- **Use the modal ladder**, below. Each modal has one sense.
- **ALL-CAPS is not emphasis.** Reserve caps for register names, constants, board and
  product names, and flag values. A word capitalized to press a point becomes plain
  lowercase. The sentence has to do the work.
- **Bold marks a term.** A hazard is a labeled callout (Caution or Note) or an erratum entry
  (Rule 4), not a bold sentence. Bold a term a reader must not misread: a refusal, or a
  default that surprises. A paragraph where several phrases are bold has no emphasis left.
  The test is whether the sentence still reads correctly with the markers removed. Where it
  does, they were decoration.
- **A label heading a block is not emphasis, and a repeated label is a heading.** Bold at
  the head of a list item or a troubleshooting block names what follows and is exempt from
  the rule above. But the same shape can repeat down a section, such as six capabilities
  each opening with a bold phrase and an em dash. Those labels are headings written as bold,
  and the fix is to make them headings. Rule 18 then governs them, which is where a label a
  reader skims for belongs.
- **A document describes its subject, not itself.** The subsection below has the rule.
- **One claim, one place.** Do not restate a finding three ways in a paragraph, or repeat
  a framing sentence in three consecutive sections.
- **Standardize at the second page, not the third.** A sentence adapted from another
  document is a decision, not a keystroke. Two wordings of one fact both read correctly on
  the day they are written, and drift apart afterwards. The drift is silent, and the only
  symptom is a reader who cannot tell which page is current. Where a claim must appear on
  more than one surface, decide which surface is its home and what the others say instead.
  A claim about the code counts the source's own documentation as a surface, and usually as
  its home.
- **Keep numbers and identifiers exact.** Concision never costs a wrong constant, a dropped
  unit, or a lost caveat. When in doubt, keep the technical detail and cut the words around
  it.
- **Delete before linking.** Before restating or linking a fact from a second place, ask
  whether that place's reader acts on it there. If not, delete it, with no link: a link is a
  cost the reader pays to decide not to follow it. Link where the reader's next step needs the
  home, at most once a section. *(gate: `restatement` lists each second statement)*
- **Cross-link instead of repeating**, except in a project README, which stays
  self-contained: state the fact the reader needs inline. Make the link text name the
  target, so it reads as a destination rather than as "here" or "this document".
- **American English.** *color, behavior, license, flavor, analyze, recognize, modeling.*
- **No emoji**, in prose, comments, or code. A check mark or a ballot cross standing as a
  value in a table is not one. Rule 16's character table governs arrows and mathematical
  signs instead.

Rule 2 governs which word names a concept. This rule governs the sentence the word sits in.

### The modal ladder

Each modal has one sense, and a sentence that states a rule, a permission or a capability
uses the one that matches:

| Sense | Write | Example |
|---|---|---|
| A requirement | *must*, with the agent that must act | "Software must program both fields." |
| A recommendation that has known exceptions | *should* | "Software should pin each pool context to its own core." |
| A possibility or a capability | *can* | "One task can write rows up to 4 KiB." |
| A plain instruction | The imperative | "Program both fields." |
| An expectation | "is expected to", with the project's tag for a prediction | "Throughput is expected to scale with N [expected]." |

<!-- prose-lint: off -- names the modals the ladder rules out, as words. Rule 15. -->

*should* never hedges a fact. *shall* and *may* are not used: *shall* belongs to contracts,
and *may* reads as both permission and possibility. *might*, *could* and *would* each leave a
reader deciding whether a rule is a rule. No requirement or recommendation is written as the
consequence of not following it.

<!-- prose-lint: on -->

The ladder governs a statement of rule, permission or capability. A counterfactual inside
explanatory prose makes no claim about the subject and is left alone. Two examples are "a
reader would search for this" and "where a positive statement would do".

<!-- prose-lint: off -- names the modal a permission model defines, as a word. Rule 15. -->

A project with a permission model needs a word for what an actor is allowed to do, apart
from what it is able to do. There *may* is permission and *can* is capability, and the
profile says so. A project that writes to RFC 2119 defines *may* the same way. A project
without either collapses *may* into *can*, as the table does.

<!-- prose-lint: on -->

The exemption and the sense are properties of the use. The gate's split between the modals
it fails and the ones it reports is therefore a heuristic about the word. The configuration
is where a project corrects it, and a permission model is the usual reason to.

### A document describes its subject, not itself

Front matter says once what a document covers. Nothing else in the body comments on the
document, its authors or its rigor, and no sentence grades a fact's importance. Words such as
*carefully*, *the single most*, *the most important*, *worth keeping*, *load-bearing* and *is
the point* assess something the reader cannot check. State the fact, and let its position
and its form show its weight.

| Instead of | Write |
|---|---|
| "This is the most important, and most counterintuitive, performance finding from the whole project." | "At 600 MHz, matmul throughput is ~460 GOP/s at fp16, int8 and int4." |
| "Negative results are recorded as carefully as positive ones." | (delete) |
| "This file states what a flat column from it does and does not mean." | "This section explains its columns." |
| "The decode is recorded here so that the next person does not reverse-engineer it again." | (delete, and put the feature's status in the support table) |

## Rule 16. Minimize the em dash, drop the semicolon

Reach for a comma, a colon, or a full stop. In technical prose an em dash often signals an
aside the sentence did not have room for. The better fix is usually to cut or to split.

The semicolon goes with it, and for a harder reason than taste. It joins two independent
clauses, which is what a full stop does with one fewer thing for a reader to parse. A
document already committed to short declaratives (Rule 26) has no use left for it.

It is also the one mark that ends a sentence where no gate can see it. Rule 26's limits
rest on where one sentence ends, and a splitter reads a full stop, a question mark and an
exclamation mark. A semicolon turns two sentences of fourteen words into one of
twenty-nine, so the length rule fires on prose already inside the limit. A writer who
obliges by compressing has made the document worse to satisfy a measurement error.
Splitting at the semicolon fixes both. That is why the semicolon is failed where the em
dash is only reported: one of them costs another rule its accuracy.

So a semicolon is not available as the replacement for an em dash. Reach past it to the
row of the table that fits.

| Function | Replacement |
|---|---|
| Introducing a definition or a list | **Colon.** "the model load: 129 / 173 / 333 ms for ..." |
| A short appositive | **Comma.** "it links into a shared object, the shape a frontend drop-in takes" |
| Two independent clauses | **Two sentences.** "Rebuild the whole consumer tree. A provider-only rebuild leaves ..." |
| A parenthetical containing its own commas | **Parentheses.** "(a `cmake --install`, typically under `/usr/local`)" |
| An aside carrying a run of items | **A list.** The dash was standing in for the structure the items wanted. |
| A trailing elaboration | **Cut it, or make it its own sentence.** |

The last row does most of the work. A paired em dash around a full clause is usually
over-explanation:

<!-- prose-lint: off -- a specimen the rule is about, quoted as written. -->

- Before: "Naming a block that fires earlier retires the job at that point — measured as
  hardware elapsed time falling from 121 to 79 microseconds with a `0x3` mask at
  64x256x256 — so the mask follows the program's terminal stage."
- After: "Naming a block that fires earlier retires the job at that point: hardware elapsed
  time falls from 121 to 79 microseconds with a `0x3` mask at 64x256x256. The mask
  therefore follows the program's terminal stage."

<!-- prose-lint: on -->

### Characters

One spelling per symbol, so that a value reads the same in a table, in prose and in a grep.

| Meaning | Write | Not |
|---|---|---|
| A numeric range | Hyphen: `39.90-41.39`, `3-33x` | An en dash |
| A multiplier or speedup | ASCII `x`: `1.10x`, `3.2x the CPU` | `1.10×` |
| Shape dimensions | `×`: `512×3840×4096` | `512x3840x4096` |
| Approximately | `~`: `~460 GOP/s` | `≈` |
| At most, at least | `<=`, `>=`, or the words | `≤`, `≥` |
| A transition or a step | `->` | `→`, except inside an established term such as `CNA→CORE→DPU` |

The multiplier and the shape separator differ on purpose: `3.2x` is a scalar a reader
compares, and `512×3840×4096` is one identifier naming a shape. Keeping them apart makes
a speedup greppable.

## Rule 17. Spend explanation, and evidence, where the reader's decision depends on it

Long documentation is not thorough documentation. Every sentence of rationale is a sentence
the reader holds in mind. A document that explains everything equally makes them sort the
necessary explanation from the incidental one. That sorting is what readers mean when
they call a document exhausting.

Default to stating what to do and what it achieves. Add the mechanism where the reader has
to **decide** something with it, **recognize** their own variant of the situation, or
would otherwise go wrong.

<!-- prose-lint: off -- a specimen the rule is about, quoted as written. -->

Before, five clauses of rationale attached to a one-line command:

> Group membership is what carries over SSH. The `+` in an `ls -l /dev/dri` listing is a
> logind ACL, and it grants the local seat session. Where the driver presents the misc
> `/dev/rknpu` node instead, its ownership follows the distribution's udev rules and is
> commonly root-only; run under `sudo -E` there, which preserves the `ROCKET_*` and
> `RKNPU_*` settings and `GGML_BACKEND_PATH` that later steps depend on.

After:

> Where the node is the misc `/dev/rknpu`, which is commonly root-only, run the steps below
> under `sudo -E`. Use `sudo -E` rather than plain `sudo` throughout: the plain form drops
> the `ROCKET_*` and `RKNPU_*` settings and `GGML_BACKEND_PATH`.

<!-- prose-lint: on -->

The logind detail is not lost. It moved to the troubleshooting entry for `Permission
denied`, where a reader is looking at that `+` and needs it. Rationale often belongs at the
point of failure rather than at the point of instruction.

Signs of over-explanation:

- The rationale runs longer than the instruction.
- It names internals the reader cannot act on ("both come from one handler and differ in
  the ioctl magic").
- The paragraph is still correct and complete with its second half deleted.
- It answers a question the reader has had no reason to ask yet.

Where a mechanism earns its length, give it a subheading so a reader can skip it, rather
than inlining it into a step.

**Evidence has the same budget.** Spend it where the reader's trust decision depends on it,
and put it in the record (Rule 7). The reading path carries the tag, and the record carries
the method, the counts and the search.

## Rule 18. Plain section titles, on their own line

A heading names its content. It is not the place for a thesis, a verdict, or a
reassurance.

| Instead of | Write |
|---|---|
| "## Performance: the honest envelope" | "## Performance" |
| "## Why this exists: the FOSS-not-vendor thesis" | "## Project purpose" |
| "## Capabilities and honest limitations" | "## Capabilities and limitations" |
| "### A third frontend: the detection delegate" | "### The TFLite delegate: object detection" |
| "## How it fits" | "## Interface" |
| "# The matmul is not MAC-bound" | "# Matmul throughput and its limits" |
| "## A rejected submit oopses the kernel, and a refusing generator is what emits one" | "## Erratum: kernel oops on a rejected submit" |

Six habits carry the rule:

- **Do not editorialize the title.** Drop the trailing characterization. The section earns
  trust from its content.
- **A heading is not a sentence.** It has no finite main verb and no ", and" clause. A
  finding's heading names its subject, and the verdict goes in the first sentence. An
  erratum's heading names its symptom: "Erratum <id>: <symptom>".
- **Honesty is the baseline, not a feature.** *Honest*, *real*, *actually*, *truthfully* in
  a heading implies the rest might not be. Cut them. The candor shows in the substance: the
  dead ends documented, the caveats kept, the losses stated next to the wins. Rule 15 holds
  body prose to the same standard.
- **Name the subject, not the rhetorical move.** "How it fits", "How it works", "Why it
  matters", "What makes it different" and "What this is worth" are headings of this kind.
  Each says what a section is *doing* rather than what it is *about*. A reader scanning the
  contents learns nothing from them. Ask the question the heading begs, "fits with what?",
  and put the answer in the heading. Where the answer turns out to be framing rather than
  substance, the content belongs in the introduction and the heading goes.
- **Prefer a noun phrase**, and prefer a name over an ordinal. "A third frontend" goes stale
  when a fourth arrives. A count goes stale the same way. "Four families" is wrong on the
  day a fifth arrives. The count belongs in a sentence that can be edited without breaking
  every link to the heading.
- **Prefer the term a reader would search for.** A heading is the highest-value place a
  keyword can sit, because it is indexed, linked, and rendered into a contents list. Where
  the subject has a name the project already uses, the heading uses that name. An option, a
  type or a flag is a better heading than a description of one. `Fidelity and accepted
  loss` finds the reader looking for `--accept-loss`. "What a format can and cannot keep"
  finds nobody.
- **A qualifier is content, not a title.** Where the true name of a section needs a clause
  to be precise, the name is the heading and the clause is the first sentence.
  `Sources and sinks`, then "A source and a sink belong to no family."

### A heading stands on its own line

A label that a reader skims for is a heading. A heading occupies a line by itself, with
its content beginning on the next one. The alternative is a bold phrase, an em dash, and
the content running on from it. That puts the label inside the paragraph it introduces.
There it cannot be linked to, cannot appear in a contents list, and reads as a run-on
rather than as a structure.

| Instead of | Write |
|---|---|
| `- **Resize-safe geometry** — descriptor backups and reserved blocks, sized by ...` | `### Resize-safe geometry`, then the sentence on the next line |
| `**Connection timeouts.** The service stops with ...` | `#### Connection timeouts`, then the sentence |

The exception is a list whose items are parallel and short, and whose leading bold term is
a definition label rather than a section. A four-item family list is one. So are a
troubleshooting block keyed by the symptom a reader sees and the fields of an erratum
entry. The test is whether a reader would want to link to it. If yes, it is a heading.

**"Short" is the whole discriminator, and this guide puts no number on it.** A label
introducing two clauses is a label. One introducing four paragraphs is a heading that lost
its `#`. Everything between those is a judgment about the document. A project that wants
this held mechanically measures its corpus, then chooses the word count where a block
stops being a label. The guide supplies the distinction and not the threshold.

---

# Part 4: Instructional writing

Rules for procedures, READMEs, and any document a reader follows while typing.

## Rule 19. Give the command, not a description of the command

A reader following a procedure must be able to copy what is on the page. Describing a
command in prose makes them reconstruct it, and that is where they get it wrong:

- Instead of: "Re-run it under `sudo -E`, and run the suite serially."
- Write:
  ```sh
  cd build && sudo -E ctest --output-on-failure -j1 --timeout 300
  ```

Three conventions keep a block pasteable:

- **No prompt character before a command.** A leading `$ ` is copied with the command and
  breaks it. Show the command alone. A `#` opening a comment line inside a shell block is a
  comment rather than a prompt, and is how to annotate.
- **Tag the fence with a language** (`sh`, `c`, `markdown`) so it highlights. A block of
  program output has no language and stays untagged.
- **Keep output out of the input block**, or mark it as a comment, so that selecting the
  block yields something a reader can paste.

Annotate a command block inline when a reader needs to know what to look for:

```sh
sudo dmesg | grep -i "Initialized rknpu"  # either "Initialized" line once it probes
ls -d /sys/bus/platform/drivers/rknpu/*   # the device it bound to, e.g. fdab0000.npu
```

## Rule 20. Order a procedure the way the reader moves through it

- **Prerequisites go in the prerequisites.** Anything a reader must arrange before step 1
  (accounts, group membership, hardware access, installed tooling) belongs in a
  Requirements section with the command that arranges it, not discovered halfway down.
- **Number the steps once**, in one sequence, and refer back to them by number.
- **Troubleshooting goes where the failure happens.** Guidance for a failure of step 4
  belongs after step 4, not folded into step 1 where the reader has no symptom yet.
- **Split troubleshooting by symptom**, since different symptoms take opposite fixes. Head
  each block with the symptom the reader sees, as a label rather than an aside:
  "**`Permission denied`.** The node exists and refused this process."

A reference document's order is Rule 3.

## Rule 21. Say what a check proves

A reader running a diagnostic needs to know what its output licenses them to conclude.
State the scope of the reading rather than only the command:

- "`modinfo rknpu` reports the driver the kernel was built with, and reads `(builtin)` on a
  BSP kernel whichever way the probe went."
- "That profile line is the evidence that the device did work. A run that prints the first
  two lines and not the third ran its encoder on the CPU."

The same discipline applies to a passing test. A gate that passes over zero cases has
proven nothing, and the document says what the gate varied.

**Verify a diagnostic against every configuration the document claims to support.** A
check that works on the common build and returns nothing on a supported variant reads as a
negative result, not as a broken check. `grep "Initialized rknpu"` matched one build's
boot message and missed the other's, which announces itself under a different string. It
reported the driver absent on a configuration the software handles.

## Rule 22. Lead with the cause, then the fix, then the detail

A reader arrives at a troubleshooting section holding a symptom and wanting it gone. Order
the section against that reader's attention, which is shortest at the top:

1. **The likely cause, in one sentence.** "Most often the node is present and this process
   cannot open it." A reader who recognizes their situation here is already served.
2. **The fix, as commands.** Rule 19 applies: give the command, not a description of one.
3. **The supporting detail, last.** Why two other checks mislead, how fast the failure
   develops, what the symptom is not. A reader who stopped after step 2 lost nothing.

Two openings that fail this test, both common:

| Opening | Problem |
|---|---|
| "The provider reports what each node answered, and the two answers take different fixes." | Describes the diagnostic instead of naming the cause. |
| "The driver maps every buffer through one shared IOMMU domain, and the kernel's rcache ..." | Mechanism before remedy. The reader wants the remedy. |

Where step 3 is long enough to push the fix off the screen, collapse it:

```markdown
<details>
<summary>Why two other common checks do not answer this</summary>

`modinfo rknpu` reports the driver the kernel was **built** with ...

</details>
```

GitHub and most renderers show that as a disclosure the reader opens on demand. It is the
"satisfy quickly, expand on request" shape in a format Markdown has. Three limits:

- Never collapse a step required to finish the task.
- Never collapse text someone would search the page for. Some viewers do not match inside
  a closed block.
- Keep blank lines around the inner content, or the Markdown inside will not render.

The same ordering applies to any section a reader reaches with a question rather than in
sequence. A verification step leads with what to look for, and explains why it matters
afterward.

## Rule 23. A document's commands are claims, so run them

Rule 8 applies to a document's own instructions. A printed command asserts that running it
produces the described result, and that assertion goes stale exactly like any other.

Check before publishing, and again whenever the surrounding code moves:

- **Run every command**, on a machine in the state the document assumes, and on each
  configuration the document claims to support (Rule 21).
- **Resolve every reference.** "Walked at these commits" must name the commits. "The three
  repositories", "the version above", "as described earlier" each need an antecedent the
  reader can see from where they are standing.
- **Check anchors and links.** Renaming a heading breaks every link to it, silently.
- **Re-read example output against reality.** Output pasted from an older version is a claim
  about the current one.

A command a reader cannot run costs more than the sentence would have if omitted: it sends
them to debug their own machine.

---

# Part 5: Format

Markdown is a small toolbox, and the choice among its few options still decides most of a
document's legibility.

## Rule 24. Match the format to the shape of the information

Pick the format from what the information *is*, not from what is quickest to type.

| Shape | Format |
|---|---|
| Repeated measurements over shared axes | **Table.** One row per case, axes as columns. |
| A procedure | **Numbered steps**, each with the command to run. |
| A set of independent facts about one thing | **Prose or bullets.** |
| One mechanism explained | **Prose.** A table would fragment the argument. |
| A summary of a grid too large to show | **Prose with ranges.** Do not print 15 rows to say "16-50%". |
| The conditions a set of results holds under | **A Conditions block**, once per table or section. |
| A hazard | **A Caution callout**, or an erratum entry. |
| What works, what is the default, what does not | **A support-status table.** |
| A register's layout | **A register or field table.** |
| Earlier figures for the same quantity | **A revision table**, in the record. |
| Project terms, and the abbreviations the reader lacks | **A glossary.** |

Rule 4 gives the genre forms in the last six rows their fields.

Prose that repeats a sentence pattern is a signal that a table is wanted. This paragraph
is a table:

<!-- prose-lint: off -- a specimen the rule is about, quoted as written. -->

> `base.en` goes from 6.75 to 3.82 CPU core-seconds on a 3 s utterance (43%), 8.37 to 4.89
> at 10 s (42%) and 9.44 to 6.17 at 30 s (35%). `tiny.en` goes from 3.03 to 1.93 (36%),
> 3.39 to 2.45 (28%) and 4.38 to 3.11 (29%).

<!-- prose-lint: on -->

Two models, three clip lengths, three measures each. As a table it is scannable down any
column. As prose the reader has to hold the pattern to parse the numbers.

**A long enough run of parallel items becomes a vertical list**, and a slash-joined run
counts as one. `ext2/ext3/ext4, FAT12/FAT16/FAT32, exFAT, and btrfs` is four items wearing
the punctuation of two. That is why it reads as prose, and survives a review that would
have listed the same four written out. Rule 27 has the formatting.

**Four is this guide's default, and it is a default rather than a finding.** The table
above already says a set of independent facts about one thing can stay prose. A four-item
run is where that row and this one meet. Which way a given run goes depends on whether the
reader scans the items or reads through them. That varies by document more than by rule. A
project measures its own corpus and sets its own floor, and a gate holds whatever it sets.

A run inside a list item is the one case with no formatting answer. Rule 27 forbids the
nested list that would be the fix. Those want the section restructured, so a
gate is the wrong instrument and a reader is the right one.

**Avoid parallel slash lists.** "129 / 173 / 333 ms for `tiny.en` / `base.en` / `small.en`"
makes the reader count positions in two sequences and align them. Pair each value with its
label ("129 ms for `tiny.en`, 173 ms for `base.en`, 333 ms for `small.en`"), or use a table.

**Keep reference detail out of a procedure.** A step a reader is executing must contain what
they act on. The mechanism behind it belongs in a reference section they can reach when
they need it and skip when they do not. That also keeps the step numbering readable.

## Rule 25. Write tables that do not grow with the world

A table of instances needs a row every time the world adds one. A table of rules needs a row
only when the rules change. Prefer the second.

Instead of one row per released version:

| Driver version | Released | Behavior |
|---|---|---|
| 0.9.6 | 2024-03-18 | Runs, without the IOVA option |
| 0.9.7 | 2024-04-24 | Full support |
| 0.9.8 | 2024-08-28 | Full support |

write the thresholds, and put the dates in a sentence below as provenance:

| Driver version | Support |
|---|---|
| 0.9.7 and later | Full. |
| 0.9.6 | Runs. The IOVA option is unavailable. |
| Below 0.9.6 | Refused at open. |

The second is shorter and states the rule itself. It stays correct when a new version
ships without changing the interface, and it does not silently claim that an unlisted
version is unsupported.

Judge this by how fast the underlying set moves. A row per frontend is fine, because
frontends arrive yearly and each row holds real content. A row per release of a
fast-moving dependency is a standing maintenance debt, and a stale one reads as a
compatibility claim.

---

# Part 6: Controlled structure

The rules above need a reader. These need a script, which is why they sit apart. They are
the half of the guide a gate can enforce, and a project that wants enforcement starts
here.

They hold every surface a stranger reads, and a doc comment on a public item is one. It
ships in the rendered documentation, and a limit that stops at the Markdown holds the
smaller half of most projects' prose.

They come from ASD-STE100, the controlled language written so that a tired reader who is
not a native English speaker cannot misread an instruction. The limits are the standard's.
The exemptions in Rule 29 are where this guide stops following it. A subsection of Rule 29
says what was refused and what was adopted instead.

## Rule 26. Hold the sentence and the paragraph to a length

- **25 words for a descriptive sentence**, the kind that explains what a thing is or does.
- **20 words for a procedural one**, a step or a warning the reader acts on.
- **Six sentences per paragraph**, one topic each.

Count the words the way the standard does, or the limit taxes precision and a writer starts
deleting the identifiers that hold the meaning:

| Counts as one word | Example |
|---|---|
| Anything in backticks | `` `mkfs.ext4 -O metadata_csum image.img` `` |
| Text inside parentheses | (typically under `/usr/local`) |
| A hyphenated word | `copy-on-write`, `byte-reproducible` |
| A number with its unit | `16 TiB`, `600 MHz`, `200 ms` |
| A bold label opening a list item | **Resize-safe geometry** |

A lead-in colon ends a sentence for counting, and each item after it has its own budget.

**The limit is not terseness.** Keep the articles, keep "that", keep the words a sentence
needs to be grammatical. "Ensure file exists before running" is shorter and worse than
"Make sure that the file exists before you run the command." A sentence over the limit is
split, never compressed.

**A split keeps its logic word.** A contrast takes *but*, a consequence takes *so* or
*therefore*, and *and* joins only two facts that point the same way. "Quantization saves RAM, and
it does not speed prefill" hides the turn the reader needs to see. Compressing a long sentence
by gluing its clauses with *and* breaks this rule and keeps the length. *(gate:
`contrast_and`)*

**Each sentence of a split names its subject.** A sentence that opens with *That*, *This*,
*So* or *Therefore* and refers to the whole previous sentence is a chain. The reader has to
rebuild its referent. Restate the subject, or restructure the run into a list or a table.

| Instead of | Write |
|---|---|
| "The cycles event counts at the memory clock. That says the PMU is running. It does not say the byte events mean bytes." | "The cycles event counts at the memory clock. The byte events are accurate to 0.4%." |

## Rule 27. Format a list the same way every time

Four or more parallel items become a vertical list (Rule 24). A repeated sentence pattern
becomes a table (Rule 24). Then:

- The lead-in ends with a colon.
- Each item starts with a capital letter, unless it starts with an identifier, which is
  never edited to fit a rule about prose.
- An item takes a full stop only if it is a full sentence. Never a comma, never a
  semicolon, and the last item is not special.
- One list holds either instructions or facts, not both.
- Lists do not nest. A list that wants a sublist wants a heading and a second list.

## Rule 28. Active voice, and the condition before the command

**Active voice.** Where a sentence has an agent, the agent is the subject. Name the agent
that acts in the subject's domain:

- In a hardware description, the agent is the block, or "software" for the programmer.
- In a library description, the agent is the function.
- The project and its authors are never agents in reference text.

Where the agent is unknown, the passive is allowed. A measured result can take the passive,
or be stated as a result with a Conditions line. "Indexes are not used on this table"
becomes "This table has no indexes."

| Instead of | Write |
|---|---|
| "The generator must keep `FC_DATA_BANK` = 0, and it does." | "Software must program `FC_DATA_BANK` to 0." |
| "This project applied the two together as one delta." | (state the result of the change) |
| "`npu_conv2d_int8()` owns all of it:" | (describe the hardware, then a separate Software support line) |

**Every `if` and `when` opens its sentence**, before what it governs, separated by a comma.
"Increase the timeout if the network is slow" makes a reader hold an instruction while
deciding whether it applies to them. "If the network is slow, increase the timeout" does
not.

## Rule 29. What the limits do not touch

Three things are exempt:

- **Identifiers, commands, flags, paths, quoted errors, product names, and UI labels.**
  Exact, always, even where they break a rule about capitals or word choice. Rule 19 says
  this for a code block. This is the sentence-level form of it.
- **Field vocabulary**: the terms the field's standard documentation uses. *Extent tree*,
  *up-case table*, *chunk tree*, *bounds-check*, *streaming*, *idempotent*, *webhook*. A
  guide that swaps a field's words for plainer ones makes a document clearer for a reader
  who was not going to read it. The cost falls on the reader who was. The project's
  profile names the list. **House vocabulary** is not exempt: a project's coinage, or an
  ordinary word in a private sense, needs a glossary entry and Rule 2's test.
- **Legal boilerplate.** A license grant or a contribution notice is recognized by shape.
  Rewriting one helps nobody and costs a reviewer the ability to match it against the
  original.

### What this part deliberately omits, and what it adopts instead

Three further rules from the same standard were tried and refused. They are named here so
that a later reader does not adopt them thinking they were overlooked:

- **An approved-word dictionary.** It rejects *run*, *execute*, *validate*, *display* and
  *check* as verbs. Technical documentation cannot afford that.
- **A ban on `-ing` verbs.** It takes *streaming*, *addressing*, *allocating* and
  *logging*, words that are the subject, not decoration around it.
- **One new fact per sentence.** It is the rule that makes controlled prose read as
  staccato. It also lengthens a document by about a third, for no gain in clarity that the
  25-word limit has not already given.

What the guide adopts from the standard instead is its other half: **one word, one
meaning.** ASD-STE100 gives each approved word one sense. This guide takes the principle
without the dictionary, and enforces it through the profile's fixed terms and house words
(Rule 2).

## Checklist

Before committing a documentation change. Items marked *(gate)* are ones a script answers,
in full or by listing candidates for a reader.

The reader:

1. Does the reading path hold process residue: run counts, harness names, refuted causes,
   predictions, plans, internal references, method maxims? Move it to the record or to its
   private home. *(gate)*
2. Is every house word replaced by its standard term, and does every remaining project term,
   and every abbreviation the reader lacks, have a glossary entry? Does each entry follow
   Rule 2's form: one term, its expansion, its class first, no facts, its source? *(gate: the
   profile's list, and the entry's form)*
3. Does any word name two quantities in one document?
4. Is every term defined, or linked to its definition, before it is used? Is support status
   in a table near the top?
5. Does the document follow its class's skeleton? *(gate, once the class adopts it)*
6. Is each hazard a callout or an erratum entry, and each requirement a *must* with its
   agent? Is each set of conditions one block per table or section?
7. Is each sentence aimed at the reader holding the page: user, checker or maintainer?

Substance:

8. Does any sentence narrate how understanding changed, in the document or in the
   investigation? Cut it, or move it to the record. *(gate)*
9. Does every definitive claim have evidence, and every prediction a tag, spelled the way the
   project's profile spells it?
10. Are scope caveats intact, and stated as conditions rather than as denials?
11. Does every stated negative pass the selection test? *(gate: candidates)*
12. Is every trap a Caution callout or an erratum entry, and does it pass the trap test?

Voice:

13. Is anything defined by what it is not, where a positive statement would do?
14. Does a list stand in for a category that could be named?
15. Does a metaphor stand in for a mechanism or a quantity?
16. Does each section's first sentence say what its subject is or does?
17. Does any sentence describe the document, its authors or its rigor, or grade a fact's
    importance? *(gate)*
18. Is any rationale longer than the instruction it supports, or unactionable where it sits?
19. Em dashes: zero, or justified. Semicolons: zero. *(gate)*
20. American spelling, and no emoji. *(gate)*
21. Does every heading name a subject rather than a verdict or a rhetorical move, and stand
    on its own line? Does it use the term a reader would search for? *(gate: candidates)*

Instructional writing and format:

22. Can a reader copy every command, in order, without reconstructing one from prose?
23. Does every troubleshooting section open with the likely cause rather than the mechanism?
24. Has every command been run, every reference resolved, and every anchor checked?
25. Does any paragraph repeat a sentence pattern, where a table would be read faster?
26. Would any table need a new row each time the world adds an instance?

Across every part:

27. For an external document: no hostnames, addresses, credentials, private repository
    names, unpublished plans, or internal tracking ids? *(gate: the project's patterns)*
28. Is the document within the depth budget its class has, and does every fact sit in the
    one document that is its home?
29. Does the wording match the project's fixed terms rather than a synonym for one, and
    does one verb hold each relationship?
30. Does any sentence make a claim about another component without naming it?
31. Does a headline claim state the scope an internal document already gave it?
32. Does every question the project already answers reach its existing answer, and does a
    second answer say, where it lives, why it is one?
33. Where this change produced a shared answer, did every caller that could use it move?

Reader cost. The scripts list candidates for each, and a reader decides:

34. Is any fact stated in a second place where that reader does not act on it? Delete it
    rather than link it. *(gate: `restatement`)*
35. Is one quantity quoted with two values, or does a table row mix runs or builds? Does a
    ladder state a step its own values do not give? *(gate: `quantity`, `row_mix`,
    `column_repeat`, `ladder`)*
36. Are the shared conditions stated once, does a Conditions line hold only settings, and does
    each figure taken at a superseded operating point say so? *(gate: `refrain`,
    `conditions_result`, `stale`)*
37. Does every ratio name its baseline, and every contrast use *but*? *(gate: `ratio`,
    `contrast_and`)*
38. Is any term coined in passing, or any sentence opened on its predicate ("Which X decides
    is ...")? *(gate: `coinage`, `inverted`)*
39. Does the reader meet a result before its mechanism, and a term before a pointer to it, and
    does every table cell scan? *(gate: `forward`, `section_size`, `cell_size`)*

Part 6 is the mechanically checkable half. A script answers all eight:

40. Any descriptive sentence over 25 words, or any procedural one over 20, counted by
    Rule 26's table?
41. Any paragraph over six sentences?
42. Any run of four or more parallel items still inside a sentence, slash-joined runs
    included?
43. Any list without a colon lead-in, with a lowercase non-identifier item, with a trailing
    comma or semicolon, or nested?
    <!-- prose-lint: off -- names the modals the ladder rules out, as words. Rule 15. -->
44. Any *shall* or *may*? Any *should* that is not a recommendation? Any semicolon?
    <!-- prose-lint: on -->
45. Any `if` or `when` that is not at the start of its sentence?
46. Any repeated bold lead-in that a reader would want to link to, and is therefore a
    heading?
47. Any run of sentences chained by *That*, *This*, *So* or *Therefore*? *(reported)*

Items 42 and 46 rest on a threshold rather than on a count, so a gate answers them only
once a project has set one. Both are off until it does.

Three further answers are about the profile rather than about a page. A configured profile
the gate cannot read stops the run. Every answer the profile's table of questions cites is
checked to still exist, by the longest part of its path the corpus contains. A row
confirmed only by its last word is reported as the weak row it is. A variant the profile's
fixed terms rule out is reported wherever it appears. None of the three checks that a row
still means what it says.
