# Prose and documentation style guide

How to write documentation in every form it takes: project READMEs, API references,
reference notes, procedures, handovers, and prose comments in code. The rules are portable
and apply to any project that wants the same voice. The examples come from real documents
and are kept concrete rather than generalized. A rule is easier to apply from a real
before and after than from an abstraction.

The goal is documentation that is **clear, concise, and authoritative, and only as
authoritative as the evidence allows.** A reader trusts a stated fact without knowing how
it was arrived at. They tell a proven fact from a working hypothesis at a glance.

The guide has five parts. **Substance** governs what a document claims. **Voice** governs
how it says it. **Instructional writing** covers procedures, **Format** covers how
information is laid out, and **Controlled structure** sets the countable limits that make a
page scannable.

Parts 1 to 4 need a reader to apply them. Part 5 needs a script, and that is the point of
separating it: those rules are the half a gate can enforce.

## This guide and its project profiles

The rules here are the portable core and hold for every project. What varies between
projects is not the discipline but the vocabulary. That is what the audience already
knows, what evidence tags a claim can carry, which terms are fixed, and which documents
exist and own what.

That per-project material lives in a **profile**: one document per project, kept wherever
the project keeps its internal documentation. Nothing depends on where it sits, because the
gate reads it from the path its configuration names. A profile carries only what the core
cannot state generically:

| A profile names | Because the core cannot know |
|---|---|
| The reader, and what they already have | Depth and prerequisites follow from it |
| The document inventory, and what each document owns | Rule 9's cross-linking needs an owner per fact |
| The evidence tag vocabulary | Rule 2 requires tags, and their spelling is per project |
| Fixed terms, and the wrong variants | Rule 9 wants one term per concept |
| The questions the project answers once, and where | Rule 9 needs a writer to find the existing answer |
| The guard holding each of those | A rule nothing enforces is a rule nothing reports on |
| The depth budget per document | A reference note and a README both comply at different lengths |
| Publishing constraints | What must not appear in a document that ships |
| Enumerations a gate depends on | Rule 6 yields to them, and only the project knows which they are |
| Domain vocabulary that outranks plain words | Rule 23 exempts it, and the list is per project |
| Which surfaces repeat the same claim | Rule 9's second-page rule needs to know what the set is |

A profile refines the core. It does not relax it. Where a profile is silent, the core
governs. Read both before writing, and read the profile first when the question is "what
does this project call it" rather than "how is this written".

## Type, intent, location, audience

Four questions decide what belongs in a document and how it reads. They are independent, and
answering them is the first act of writing one. A set of research notes is a useful case.
It is public in location, internal in character, and written for a developer rather than
for the user a README serves.

**Type.** What the document is: a README, an API reference, a reference note, a procedure, a
code comment, an index, a handover. Type sets the expected shape, and Part 4 turns it into a
format.

**Intent.** What it is for, stated as the reader's outcome rather than the document's
subject. "Get someone building and running." "Stop the next person repeating a dead end."
"Resume a thread after a gap." Where it is unclear whether something belongs, intent
decides.

**Location.** Internal or external. An external document is read by someone with no access
to the rest of the workspace and no context to supply, so it stands alone. It carries no
hostnames, IP addresses, credentials, private repository names, unpublished plans, or
internal tracking ids. An internal document can assume all of that, and can carry state and
dates an external one must not.

**Audience.** Whose hands it is in: a user running the software, a developer integrating it,
a maintainer changing it, or your own future self. Rule 11 applies this one sentence by
sentence.

The default classes, which a profile can extend:

| Class | Intent | Location | Audience |
|---|---|---|---|
| Project README | Build it, run it, fix the common failures | External | User |
| API reference | Call the interface correctly and know what it guarantees | External | Developer |
| Guide chapter | Understand one part well enough to use it | External | User or developer |
| Reference notes | Preserve findings and the method that established them | External | Developer |
| Code comments | Explain why this code is as it is | With the code | Maintainer |
| Procedure | Perform a task the same way every time | Internal | Developer |
| Handover | Resume a thread without re-deriving it | Internal | Future self |
| Index | Prevent a wrong start | Internal | Whoever arrives next |

Two of these carry standing obligations. A **project README** is a self-contained "1.0"
description of current capability. A reader uses the project from its own docs without
opening another repository, and it documents the feature set as it is now. **Reference
notes** can go deeper, into mechanisms and negative results. They keep the same character:
the settled fact stated plainly, tagged with how it was established.

### A doc comment is two classes, and the marker says which

A comment attached to a public item is the API reference. It ships in the rendered
documentation, a stranger reads it without opening the source, and it is held to every
rule below. A comment on anything else explains the code to whoever changes it next.

Most languages already separate the two, and the marker is the separator. Rust uses `///`
and `//!`, Go the comment run above an exported declaration, Python a docstring. A note
inside a function body is not documentation in any of them.

That boundary is worth stating because the two classes fail in opposite directions. An API
reference that reads like a note to a maintainer is Rule 11. A `SAFETY:` comment rewritten
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
| Project README | The shortest text that gets a reader building, running, and past the common failures | Mechanism a user cannot act on, or a rationale that serves the maintainer |
| API reference | One entry per public name: what it does, what it requires, what it guarantees, what it refuses | Narrative between entries, or a tutorial the README owns |
| Guide chapter | One subject, from what it is to a reader running it | A second chapter's subject, or a reference table the API documentation owns |
| Reference notes | As deep as the mechanism goes, including negatives and the method that established them | Repetition across notes, or an unowned fact restated in three files |
| Procedure | One line per action plus what the check proves | Reference material inlined into a step |
| Index | A pointer and the trap that stops a wrong start | An accreting session log |

Two depth failures recur, and they pull in opposite directions. **Under-depth in a reference
note** drops the method behind a claim, which Rule 2 then cannot tag. **Over-depth in a
README** is Rule 10 and Rule 11: rationale longer than the instruction, aimed at the wrong
reader. Neither is fixed by trimming words evenly. Both are fixed by moving material to the
class that owns it.

Every class follows every rule below. Depth and audience change. The discipline does not.
Two failures are worth watching for. Rule 11 covers the first, a README written for a
maintainer. The second is an internal fact left in a document that ships, which nothing
catches but a check before publishing.

---

# Part 1: Substance

## Rule 1. State the settled fact, not the path to it

A document describes how things behave *today*. Version control already records the
changelog. The reader wants the current truth, stated once, plainly.

| Instead of | Write |
|---|---|
| "Behavior was thought to be Y but is now understood to be Z." | "Behavior is Z." |
| "Corrected: the rule is C2 = 16 / out-bytes." | "The rule is C2 = 16 / out-bytes." |
| "An earlier note guessed C2=8, which was wrong." | (delete, and describe the C2=4 cube) |
| "First attempt failed, then we overturned it and it works." | "It works: ..." |
| "RESOLVED 2026-06-22: an identity conv supplies the feed." | "An identity conv supplies the feed." |
| "P1.4 DONE: K-accum gives +19%." | "On-NPU fp16 K-accumulation gives +19%." |

<!-- prose-lint: off -- a vocabulary list, where completeness is the point. Rule 6. -->

**Framings to cut:** *corrected, previously, used to, was thought, turned out, initially,
originally, no longer, earlier note, walked back, we had to, now understood, RESOLVED,
formerly, once thought, was broken, the old path.* Also the status verbs **DONE /
COMPLETE / WIP** and internal tracking ids (**P1.4**, **Task A**), which track a project
rather than the subject.

<!-- prose-lint: on -->

### Dates

- **Drop edit-event dates.** "Corrected 2026-06-22", "DONE 2026-06-20", "Post-2026-06-22".
  These stamp when the document changed.
- **Keep measurement provenance.** "Measured 2026-06-22 (kernel 7.1.0-1, 600 MHz)" stamps
  when a number was observed and under what conditions. That is reproducibility
  information.

The test: does the date say *when a fact was measured* (keep) or *when the document
changed* (drop)?

## Rule 2. Claim only what the evidence supports

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
| Untested | "X is Y [expected]." "Untested." Or omit the claim. |

**A negative carries its method.** "No register does X" is a result about the sweep that
looked. Where the method could not have found the positive, say what was searched, so the
boundary reads as the method's and not the subject's.

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
the thing it justifies. A sentence carried over from a sibling is true of the sibling and
false where it landed.

### A claim that is scoped somewhere carries its scope everywhere

Where an internal document has already bounded a guarantee, the public statement of that
guarantee carries the boundary. A headline property stated flat is read as unbounded, and
a reader who infers more has done the reasonable thing. The boundary is usually one clause:
the version it holds for, the configuration it holds under, the axes a certification
varied. A claim with no stated domain cannot be falsified by a later reader and cannot be
extended by one either.

## Rule 3. Keep epistemic scope, cut autobiographical hedging

These look similar and are opposites:

- **Autobiographical hedging: cut.** "We initially thought", "it took three tries", "this
  surprised us." That is Rule 1 again.
- **Scope hedging: keep.** "This is a measurement at the current operating point, not a
  proven property." "Validated at N<=32. Larger N untested." "Portable by construction,
  validated on one part."

Removing a scope caveat to sound more confident is overclaiming, which is the opposite of
the goal.

## Rule 4. Write traps forward-looking

A genuinely surprising behavior that another implementer would hit is worth documenting.
Write it as a warning to the next person.

<!-- prose-lint: off -- a specimen the rule is about, quoted as written. -->

- Instead of: "We packed int4 with int8's N-group and it broke; we fixed it."
- Write: "An int4 weight packed with int8's N-group-of-32 coincides with the correct layout
  only at K=32, so a single-K-group test passes and K>32 fails. Test weight tiles at
  K >= 2x the K-group."

<!-- prose-lint: on -->

This is the one place a "you might expect X, but it is actually Y" framing belongs. The
surprise is in the subject rather than in the edit history. Everywhere else, state the
behavior.

---

# Part 2: Voice

## Rule 5. Characterize by what a thing is and does

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
- A **cost stated next to a win**: "the flag buys headroom rather than speed."

The test: does the negative half tell the reader something they will act on, or does it
only make the positive half sound more interesting?

## Rule 6. Name the category, not an enumeration of it

An overview earns its keep by compressing. A list of five examples where one category noun
would do makes the reader assemble the abstraction that the writer left out.

| Instead of | Write |
|---|---|
| "Everything above the seam: the register encoders, the tiling, the on-NPU op library, the graph planner, and the ggml, TFLite and ONNX Runtime frontends" | "The rest of the stack ... up through the `ggml`, TFLite and ONNX Runtime frontends" |
| "device open and close, buffer allocation and teardown, cache maintenance, submit, and the capability and counter queries" | "device lifetime, buffer management, cache maintenance, submission, and the capability queries" |

Two failure modes, and both are real:

- **Enumerating instead of categorizing.** Cut the internals. Keep the items the reader
  can act on. Naming the frontends is useful because a reader wants to know theirs is
  covered. Naming the tiling layer is not.
- **A category too vague to carry the list.** "The vendor uAPI expresses several things
  differently" names nothing. "The vendor uAPI differs in buffer identity, job structure
  and submit semantics" names three categories that cover six table rows.

Lists belong where completeness is the point: a supported-model list, a chip-availability
list, a reproducibility line stating the measurement conditions. Keep those.

**This rule runs before Rule 18, not against it.** Rule 18 turns a long enough run of
parallel items into a vertical list. Reading both rules at once makes a six-item run look
like a six-item list.

The order settles it. Rule 6 asks first whether the enumeration belongs, and a category
noun ends the question there. Rule 18 then formats whatever survives. Applying them the
other way round produces the shape both rules exist to prevent, which is a vertical list
of items a single word covered.

**Where something mechanical depends on the enumeration, the enumeration wins.** A gate
that greps a page for every member of a set turns "name the category" into a red build.
The category noun is the exact wording it cannot see. A project with such a gate names the
protected enumerations in its profile, and this rule stops at them. The general form: a
rule about compression yields to a rule about coverage, because a reader can reassemble a
category and a grep cannot.

## Rule 7. Name the mechanism, not a spatial metaphor

Words like *above*, *below*, *underneath*, and *on top of* read as technical terms and are
not. Replace the metaphor with the relationship it stands for.

| Instead of | Write |
|---|---|
| "Everything above the seam compiles unchanged" | "The rest of the stack targets the silicon, so it compiles unchanged" |
| "The seam sits below the frontend, so a frontend picks the NPU up" | "A frontend links that library and inherits the route" |
| "each links the library, and the seam is below them" | "each links `libnpu`, which steps 1 through 4 have already pointed at the vendor driver" |

A term the codebase actually uses (a file name, a test name, an API name) is not a
metaphor. Keep it.

## Rule 8. Minimize the em dash, drop the semicolon

Reach for a comma, a colon, or a full stop. In technical prose an em dash often signals an
aside the sentence did not have room for. The better fix is usually to cut or to split.

The semicolon goes with it, and for a harder reason than taste. It joins two independent
clauses, which is what a full stop does with one fewer thing for a reader to parse. A
document already committed to short declaratives (Rule 20) has no use left for it.

It is also the one mark that ends a sentence where no gate can see it. Rule 20's limits
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

A paired em dash carrying a full clause is often over-explanation, and the last row of the
table is where that lands:

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

## Rule 9. Plain, dense, declarative

- **Lead with the conclusion.** The first sentence of a section is the takeaway.
- **Prefer short declaratives.** Break a sentence carrying three parenthetical asides into
  two or three sentences. Keep the precision, drop the nesting. Rule 20 puts a number on
  this.
- **Use the modal ladder:** `can`, `will`, `must`. A requirement is `must`, a possibility
  is `can`, and a recommendation is either stated as a fact or deleted. `should`, `may`,
  `might`, `could` and `would` each leave a reader deciding whether a rule is a rule. A
  model reading one treats it as optional. The ladder governs a statement of rule,
  permission or capability. A counterfactual inside explanatory prose claims nothing about
  the subject and is left alone, as in "a reader would search for this".

  All five are equal here, and the gate's split between the ones it fails and the ones it
  reports is not this rule's. The exemption is a property of the use, which no script
  reads, so the tool guesses from the word. `[limits]` is where a project corrects the
  guess, and a project defining `may` as a term of art is the usual reason to.
- **ALL-CAPS is not emphasis.** Reserve caps for register names, constants, board and
  product names, and flag values. A word capitalized to press a point becomes plain
  lowercase. The sentence has to carry the weight.
- **Bold is for what the reader must not miss.** Emphasis a passage can do without costs
  the reader nothing to drop. A paragraph where several phrases are bold has no emphasis
  left. Bold a term when acting on the wrong reading of it would break something: a
  hazard, a refusal, a default that surprises. Everything else is plain. The test is
  whether the sentence still lands with the markers removed, and where it does they were
  decoration.
- **A label heading a block is not emphasis, and a repeated label is a heading.** Bold at
  the head of a list item or a troubleshooting block names what follows and is exempt from
  the rule above. But where the same shape repeats down a section, those are headings
  written as bold, and the fix is to make them headings. Six capabilities each opening
  with a bold phrase and a dash are six headings. Rule 12 then governs them, which is
  where a label a reader skims for belongs.
- **One claim, one place.** Do not restate a finding three ways in a paragraph, or repeat
  a framing sentence in three consecutive sections.
- **Standardize at the second page, not the third.** A sentence adapted from another
  document is a decision, not a keystroke. Two wordings of one fact both read correctly on
  the day they are written, and drift apart afterwards. The drift is silent, and the only
  symptom is a reader who cannot tell which page is current. Where a claim must appear on
  more than one surface, decide which surface owns it and what the others say instead. A
  claim about the code counts the source's own documentation as a surface, and usually as
  the owner.
- **Keep numbers and identifiers exact.** Concision never costs a wrong constant, a dropped
  unit, or a lost caveat. When in doubt, keep the technical detail and cut the words around
  it.
- **Cross-link instead of repeating**, except in a project README, which stays
  self-contained: state the fact the reader needs inline. Make the link text name the
  target, so it reads as a destination rather than as "here" or "this document".
- **One term per concept, and prefer the codebase's own name.** Varying the word for
  elegance makes the reader ask whether two things are meant. Where the code calls it
  `rocket_npu.h`, the prose calls it that, not "the seam" in one paragraph and "the
  interface" in the next.
- **One verb per relationship.** The same discipline reaches the verbs. Pick one word for
  containment and one for composition, and hold them apart. A structure *carries* the field
  written inside it. A workspace *has* the crates it is made of. Rotating *carries*,
  *holds* and *takes* across one idea reads as three relationships.
- **American English.** *color, behavior, license, flavor, analyze, recognize, modeling.*
- **No emoji**, in prose, comments, or code. A check mark or a ballot cross standing as a
  value in a table is not one. Rule 8's character table governs arrows and mathematical
  signs instead.

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

A profile carries that table, and a project with no profile still benefits from asking the
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

## Rule 10. Spend explanation where the reader's next action depends on it

Long documentation is not thorough documentation. Every sentence of rationale is a
sentence the reader carries. A document that explains everything equally makes them sort
the load-bearing explanation from the incidental one. That sorting is what readers mean when
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

## Rule 11. Write for the reader's role

Ask who acts on a sentence. A project README is read by someone installing, building,
running, and troubleshooting. That reader needs what changes their commands, their
configuration, or their expectations. An explanation of *why the software is built this
way* serves a maintainer. It belongs in a source comment, an internal note, or the
research record.

This is Rule 10's sibling and not a restatement of it. Rule 10 asks whether a rationale has
earned its length for the reader it is aimed at. Rule 11 asks whether that reader is the one
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

**Where depth has to stay, name its audience in the section's first line**, so everyone
else can skip it. "Reference for readers tuning the defaults above or changing the
provider."

## Rule 12. Plain section titles, on their own line

A heading names its content. It is not the place for a thesis, a verdict, or a
reassurance.

| Instead of | Write |
|---|---|
| "## Performance: the honest envelope" | "## Performance" |
| "## Why this exists: the FOSS-not-vendor thesis" | "## Project purpose" |
| "## Capabilities and honest limitations" | "## Capabilities and limitations" |
| "### A third frontend: the detection delegate" | "### The TFLite delegate: object detection" |
| "## How it fits" | "## Interface" |

Six habits carry the rule:

- **Do not editorialize the title.** Drop the trailing characterization. The section earns
  trust from its content.
- **Honesty is the baseline, not a feature.** *Honest*, *real*, *actually*, *truthfully* in
  a heading implies the rest might not be. Cut them. The candor shows in the substance: the
  dead ends documented, the caveats kept, the losses stated next to the wins.
- **Name the subject, not the rhetorical move.** "How it fits", "How it works" and "Why it
  matters" say what a section is *doing* rather than what it is *about*. A reader scanning
  the contents learns nothing from them. Ask the question the heading begs, "fits with
  what?", and put the answer in the heading. Where the answer turns out to be framing
  rather than substance, the content belongs in the introduction and the heading goes.
- **Prefer a noun phrase**, and prefer a name over an ordinal. "A third frontend" goes stale
  when a fourth arrives. A count goes stale the same way. "Four families" is wrong on the
  day a fifth lands. The count belongs in a sentence that can be edited without breaking
  every link to the heading.
- **Prefer the term a reader would search for.** A heading is the highest-value place a
  keyword can sit, because it is indexed, linked, and rendered into a contents list. Where
  the subject has a name the project already uses, the heading takes that name. An option,
  a type or a flag beats a description of one. `Fidelity and accepted loss` finds the
  reader looking for `--accept-loss`. "What a format can and cannot keep" finds nobody.
- **A qualifier is content, not a title.** Where the true name of a section needs a clause
  to be precise, the name is the heading and the clause is the first sentence.
  `Sources and sinks`, then "A source and a sink belong to no family."

### A heading stands on its own line

A label that a reader skims for is a heading. A heading occupies a line by itself, with
its content beginning on the next one. The alternative is a bold phrase, a dash, and the
content running on from it. That puts the label inside the paragraph it introduces. There
it cannot be linked to, cannot appear in a contents list, and reads as a run-on rather
than as a structure.

| Instead of | Write |
|---|---|
| `- **Resize-safe geometry** — descriptor backups and reserved blocks, sized by ...` | `### Resize-safe geometry`, then the sentence on the next line |
| `**Connection timeouts.** The service stops with ...` | `#### Connection timeouts`, then the sentence |

The exception is a list whose items are genuinely parallel and short, and whose leading
bold term is a definition label rather than a section. A four-item family list is one, and
so is a troubleshooting block keyed by the symptom a reader sees. The test is whether a
reader would want to link to it. If yes, it is a heading.

**"Short" is the whole discriminator, and this guide puts no number on it.** A label
introducing two clauses is a label. One introducing four paragraphs is a heading that lost
its `#`. Everything between those is a judgment about the document. A project that wants
this held mechanically measures its corpus, then chooses the word count where a block
stops being a label. The guide supplies the distinction and not the threshold.

---

# Part 3: Instructional writing

Rules for procedures, READMEs, and any document a reader follows while typing.

## Rule 13. Give the command, not a description of the command

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

## Rule 14. Order the document the way the reader moves through it

- **Prerequisites go in the prerequisites.** Anything a reader must arrange before step 1
  (accounts, group membership, hardware access, installed tooling) belongs in a
  Requirements section with the command that arranges it, not discovered halfway down.
- **Number the steps once**, in one sequence, and refer back to them by number.
- **Troubleshooting goes where the failure happens.** Guidance for a failure of step 4
  belongs after step 4, not folded into step 1 where the reader has no symptom yet.
- **Split troubleshooting by symptom**, since different symptoms take opposite fixes. Head
  each block with the symptom the reader sees, as a label rather than an aside:
  "**`Permission denied`.** The node exists and refused this process."

## Rule 15. Say what a check proves

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

## Rule 16. Lead with the cause, then the fix, then the detail

A reader arrives at a troubleshooting section holding a symptom and wanting it gone. Order
the section against that reader's attention, which is shortest at the top:

1. **The likely cause, in one sentence.** "Most often the node is present and this process
   cannot open it." A reader who recognizes their situation here is already served.
2. **The fix, as commands.** Rule 13 applies: give the command, not a description of one.
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
"satisfy quickly, expand on request" shape in a format Markdown actually has. Three limits:

- Never collapse a step required to finish the task.
- Never collapse text someone would search the page for. Some viewers do not match inside
  a closed block.
- Keep blank lines around the inner content, or the Markdown inside will not render.

The same ordering applies to any section a reader reaches with a question rather than in
sequence. A verification step leads with what to look for, and explains why it matters
afterward.

## Rule 17. A document's commands are claims, so run them

Rule 2 applies to a document's own instructions. A printed command asserts that running it
produces the described result, and that assertion goes stale exactly like any other.

Check before publishing, and again whenever the surrounding code moves:

- **Run every command**, on a machine in the state the document assumes, and on each
  configuration the document claims to support (Rule 15).
- **Resolve every reference.** "Walked at these commits" must name the commits. "The three
  repositories", "the version above", "as described earlier" each need an antecedent the
  reader can actually see from where they are standing.
- **Check anchors and links.** Renaming a heading breaks every link to it, silently.
- **Re-read example output against reality.** Output pasted from an older version is a claim
  about the current one.

A command a reader cannot run costs more than the sentence would have if omitted: it sends
them to debug their own machine.

---

# Part 4: Format

Markdown is a small toolbox, and the choice among its few options is most of what a writer
controls about how a page reads.

## Rule 18. Match the format to the shape of the information

Pick the format from what the information *is*, not from what is quickest to type.

| Shape | Format |
|---|---|
| Repeated measurements over shared axes | **Table.** One row per case, axes as columns. |
| A procedure | **Numbered steps**, each with the command to run. |
| A set of independent facts about one thing | **Prose or bullets.** |
| One mechanism explained | **Prose.** A table would fragment the argument. |
| A summary of a grid too large to show | **Prose with ranges.** Do not print 15 rows to say "16-50%". |

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
have listed the same four written out. Rule 21 has the formatting.

**Four is this guide's default, and it is a default rather than a finding.** The table
above already says a set of independent facts about one thing can stay prose. A four-item
run is where that row and this one meet. Which way a given run goes depends on whether the
reader scans the items or reads through them. That varies by document more than by rule. A
project measures its own corpus and sets its own floor, and a gate holds whatever it sets.

A run inside a list item is the one case with no formatting answer. Rule 21 forbids the
nested list that would be the fix. Those want the section restructured, so a
gate is the wrong instrument and a reader is the right one.

**Avoid parallel slash lists.** "129 / 173 / 333 ms for `tiny.en` / `base.en` / `small.en`"
makes the reader count positions in two sequences and align them. Pair each value with its
label ("129 ms for `tiny.en`, 173 ms for `base.en`, 333 ms for `small.en`"), or use a table.

**Keep reference detail out of a procedure.** A step a reader is executing must carry what
they act on. The mechanism behind it belongs in a reference section they can reach when
they need it and skip when they do not. That also keeps the step numbering readable.

## Rule 19. Write tables that do not grow with the world

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

The second is shorter and states the actual rule. It stays correct when a new version
ships without changing the interface, and it does not silently claim that an unlisted
version is unsupported.

Judge this by how fast the underlying set moves. A row per frontend is fine, because
frontends arrive yearly and each row carries real content. A row per release of a
fast-moving dependency is a standing maintenance debt, and a stale one reads as a
compatibility claim.

---

# Part 5: Controlled structure

The rules above need a reader. These need a script, which is why they sit apart. They are
the half of the guide a gate can enforce, and a project that wants enforcement starts
here.

They hold every surface a stranger reads, and a doc comment on a public item is one. It
ships in the rendered documentation, and a limit that stops at the Markdown holds the
smaller half of most projects' prose.

They come from ASD-STE100, the controlled language written so that a tired reader who is
not a native English speaker cannot misread an instruction. The limits are the standard's.
The carve-outs in Rule 23 are where this guide stops following it, and the section after
it says what was refused.

## Rule 20. Hold the sentence and the paragraph to a length

- **25 words for a descriptive sentence**, the kind that explains what a thing is or does.
- **20 words for a procedural one**, a step or a warning the reader acts on.
- **Six sentences per paragraph**, one topic each.

Count the words the way the standard does, or the limit taxes precision and a writer starts
deleting the identifiers that carry the meaning:

| Counts as one word | Example |
|---|---|
| Anything in backticks | `` `mkfs.ext4 -O metadata_csum image.img` `` |
| Text inside parentheses | (typically under `/usr/local`) |
| A hyphenated word | `copy-on-write`, `byte-reproducible` |
| A number with its unit | `16 TiB`, `600 MHz`, `200 ms` |
| A bold label opening a list item | **Resize-safe geometry** |

A lead-in colon ends a sentence for counting, and each item after it carries its own
budget.

**The limit is not terseness.** Keep the articles, keep "that", keep the words a sentence
needs to be grammatical. "Ensure file exists before running" is shorter and worse than
"Make sure that the file exists before you run the command." A sentence over the limit is
split, never compressed.

## Rule 21. Format a list the same way every time

Four or more parallel items become a vertical list (Rule 18). A repeated sentence pattern
becomes a table (Rule 18). Then:

- The lead-in ends with a colon.
- Each item starts with a capital letter, unless it starts with an identifier, which is
  never edited to fit a rule about prose.
- An item takes a full stop only if it is a full sentence. Never a comma, never a
  semicolon, and the last item is not special.
- One list holds either instructions or facts, not both.
- Lists do not nest. A list that wants a sublist wants a heading and a second list.

## Rule 22. Active voice, and the condition before the command

**Active voice.** Where a sentence has an agent, the agent is the subject. Where the agent
is genuinely unknown, the passive is allowed. Where it is merely unstated, name it: the
reader, the project, or the component. "Indexes are not used on this table" becomes "This
table has no indexes" or "We do not index this table."

**Every `if` and `when` opens its sentence**, before what it governs, separated by a comma.
"Increase the timeout if the network is slow" makes a reader hold an instruction while
deciding whether it applies to them. "If the network is slow, increase the timeout" does
not.

## Rule 23. What the limits do not touch

Three things are exempt:

- **Identifiers, commands, flags, paths, quoted errors, product names, and UI labels.**
  Exact, always, even where they break a rule about capitals or word choice. Rule 13 says
  this for a code block. This is the sentence-level form of it.
- **The domain vocabulary the project already uses.** *Extent tree*, *up-case table*,
  *chunk tree*, *bounds-check*, *streaming*, *idempotent*, *webhook*. A guide that plains
  away the words a field uses has bought clarity from a reader who was not going to read
  the document. It cost the reader who was. The project's profile names the list.
- **Legal boilerplate.** A license grant or a contribution notice is recognized by shape.
  Rewriting one buys clarity nobody asked for and costs a reviewer the ability to match it
  against the original.

### What this part deliberately omits

Three further rules from the same standard were tried and refused. They are named here so
that a later reader does not adopt them thinking they were overlooked:

- **An approved-word dictionary.** It rejects *run*, *execute*, *validate*, *display* and
  *check* as verbs. Technical documentation cannot pay that.
- **A ban on `-ing` verbs.** It takes *streaming*, *addressing*, *allocating* and
  *logging*, which are the subject rather than decoration around it.
- **One new fact per sentence.** It is the rule that makes controlled prose read as
  staccato. It lengthens a document for no gain in clarity the 25-word limit has not already
  bought.

## Checklist

Before committing a documentation change:

1. Does any sentence narrate how understanding changed? Cut it.
2. Does every definitive claim carry evidence, and every prediction carry a tag, spelled
   the way the project's profile spells it?
3. Are scope caveats intact?
4. Is anything defined by what it is not, where a positive statement would do?
5. Does a list stand in for a category that could be named?
6. Does a spatial metaphor stand in for a mechanism?
7. Is any rationale longer than the instruction it supports, or unactionable where it sits?
8. Em dashes: zero, or justified. Semicolons: zero.
9. American spelling, and no emoji.
10. Does every heading name a subject rather than a rhetorical move, stand on its own
    line, and use the term a reader would search for?
11. Can a reader copy every command, in order, without reconstructing one from prose?
12. Does every troubleshooting section open with the likely cause rather than the mechanism?
13. Does any sentence explain an implementation choice rather than something the reader
    acts on?
14. Does any paragraph repeat a sentence pattern, where a table would be read faster?
15. Would any table need a new row each time the world adds an instance?
16. Has every command been run, every reference resolved, and every anchor checked?
17. For an external document: no hostnames, addresses, credentials, private repository
    names, unpublished plans, or internal tracking ids?
18. Is the document within the depth budget its class carries, and does every fact sit in
    the document that owns it?
19. Does the wording match the project's fixed terms rather than a synonym for one, and
    does one verb hold each relationship?
20. Does any sentence carry a claim about another component without naming it?
21. Does a headline claim state the scope an internal document already gave it?
22. Does every question the project already answers reach its existing answer, and does a
    second answer say, where it lives, why it is one?
23. Where this change produced a shared answer, did every caller that could use it move?

Part 5 is the mechanically checkable half. A script can answer all seven:

24. Any descriptive sentence over 25 words, or any procedural one over 20, counted by
    Rule 20's table?
25. Any paragraph over six sentences?
26. Any run of four or more parallel items still inside a sentence, slash-joined runs
    included?
27. Any list without a colon lead-in, with a lowercase non-identifier item, with a trailing
    comma or semicolon, or nested?
28. Any of `should`, `may`, `might`, `could`, `would`, or a semicolon?
29. Any `if` or `when` that is not at the start of its sentence?
30. Any repeated bold lead-in that a reader would want to link to, and is therefore a
    heading?

Items 26 and 30 rest on a threshold rather than on a count, so a gate answers them only
once a project has set one. Both are off until it does.

Three further answers are about the profile rather than about a page. A configured profile
the gate cannot read stops the run. Every answer the profile's table of questions cites is
checked to still exist, by the longest part of its path the corpus contains. A row
confirmed only by its last word is reported as the weak row it is. A variant the profile's
fixed terms rule out is reported wherever it appears. None of the three checks that a row
still means what it says.
