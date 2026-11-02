# Reviewing documentation

The gate holds what a script can count. A page can pass every check and still cost its reader.
Its table rows can mix two builds, or it can quote one quantity three ways. It can describe a
default from the branch that turns it off. This document is the review that finds those,
done as three passes, and the step that turns what each pass finds into a check.

It assumes the gate is already running, as `ADOPTING.md` sets it up. The review reads with
the gate's output beside the page, and it is the part of `STYLE.md` that only a reader can
apply.

## The three passes

Each pass asks one question and changes one kind of thing:

| Pass | Question | What it changes | What it leaves alone |
|---|---|---|---|
| Style pass | Does each page serve the reader holding it? | Where content sits, which words name it, the genre's forms, the voice | Every fact: numbers, identifiers, claims, fenced code, link targets |
| Fact pass | Is each statement still true? | Contradictions and stale claims, each settled from evidence | Everything its listed items do not touch |
| Reader-cost review | Does a true, well-written page still cost its reader? | Second statements, mixed readings, defaults and limits the prose gets wrong | The style, unless a finding needs it |

Run them in that order, and keep each pass's diff to its own kind of change. A style edit that
also changes a number cannot be reviewed as either. The style pass does not resolve the
contradictions it meets. It lists them, and the list is the fact pass's input.

A project can run one pass alone. The reader-cost review is the one worth repeating, after any
change to the code a document describes.

## Before any pass

Four things are prepared once, by whoever runs the pass, and handed to every reviewer:

- **The profile.** It names the readers, the fixed terms, the house words, the record zones
  and the home of each fact. Every reviewer reads it in full first.
- **The facts that moved.** List what changed since the documents were last edited: a new
  default, a removed option, a re-measure, a renamed function. Each entry names its source, a
  commit or a record. Every reviewer searches their files for each one.
- **The worklist.** Run the gate and the voice checks over each file, and save the output per
  file. The review starts from that list, not from the page.
- **Who owns which file.** Split the files among reviewers, and give each file one owner. A
  reviewer edits only the files they own and changes nothing in version control.

The worklist for one file comes from two commands:

    python3 prose-lint.py --config prose-lint.toml --all FILE
    python3 prose-lint.py --config prose-lint.toml --voice --detail all FILE

A fix that belongs in another reviewer's file is reported rather than made. The report gives
the file and the exact old and new text, and whoever runs the pass applies it. That keeps two
reviewers from editing one file, and it keeps every cross-file change visible in one place.

## The style pass

The style pass gives each reader their part of the page. Most documents written while the work
happened put the result and the history of finding it in one place, in the order the work took.
The pass separates the two, and makes four kinds of change, in this order of importance:

1. **Move the record out of the reading path** (Rules 1, 6 and 7). Run counts, harness names,
   refuted causes, older figures and method go to the record, which the profile names. The body
   keeps the fact, its evidence tag, and one Conditions block per table or section.
2. **Use the reader's terms** (Rule 2). Replace each house word where it is used in its house
   sense. A term the glossary defines stays.
3. **Use the genre's forms** (Rules 3, 4 and 11). A bold warning becomes a Caution callout or
   an erratum entry. A requirement stated as its consequence becomes "Software must ...".
4. **Fix the voice** (Rules 10, 15, 18, 26 and 28). Delete self-assessment and rebuttals of
   claims nobody made. Rename a heading that is a sentence or a verdict.

### Preserved content

The style pass changes where a fact sits and how it is worded, never the fact:

- **Every number, identifier and measured value.** Do not round, recompute or correct one. A
  value that looks wrong goes in the report.
- **Every claim.** A claim can move to the record, and arrives there intact, with its numbers,
  its conditions and its tag. Nothing is added, strengthened or weakened.
- **Fenced code blocks**, verbatim, and every link target. Link text can change.
- **A heading another document cites.** Search every document for its exact title and its
  anchor before renaming it. Where a citation sits in a file the reviewer does not own, the
  heading stays and the report lists it.

The deletions allowed are edit-history narration, self-assessment, a rebuttal the selection
test refuses, and references a stranger cannot use. Each one is named in the report.

### The fact-preservation check

Compare each edited file against its last committed version, token by token. The tokens are
numbers, code spans, link targets, evidence tags and fenced blocks. Every token the comparison
lists as lost is accounted for in the report, as a moved fact or an allowed deletion. Anything
else lost is a fact the pass dropped, and it goes back.

A script that extracts those tokens from both versions and diffs the two sets is a few dozen
lines. It is the check that makes a large style pass reviewable, because the reviewer of the
diff reads the accounting instead of every sentence.

## The fact pass

The fact pass resolves the contradictions and stale claims the style pass listed, and the facts
that moved. Its bar is evidence, not plausibility. A number changed without a source in the
report is a defect the pass added.

### Evidence sources

Work through the sources cheapest first, and stop at the first one that decides the item:

1. **Whether an earlier edit caused it.** The diff against the last commit shows what the style
   pass changed. A figure moved away from its condition is fixed by restoring the condition.
2. **The figure's history.** Version control finds the commit that entered each figure, and its
   message usually gives the date, the configuration and the build. A later commit with a
   competing figure for the same quantity usually supersedes the earlier one.
3. **The record.** The document's evidence section, a validation document, or the raw data.
4. **The code.** A default, a refusal, an option's existence or a limit is decided by the code
   the document describes, at its current state. Cite the function, not a line number.
5. **The sources the document cites.** A specification, a reference manual, a vendor's driver.
6. **Private records.** An issue tracker or a decision log often names the measurement that
   superseded a figure. Use one to decide, and never cite it in a document that publishes.

### Outcomes

Each item ends in one of five outcomes, and the report names it:

| Outcome | When | What the document gets |
|---|---|---|
| Scoped | The figures measure different conditions, and both stand | Each figure's conditions, stated where the figure is |
| Superseded | A newer measurement of the same configuration exists | The current figure in the body, and the older one in the record with its date and conditions |
| Corrected | The text is wrong against the code, a source or its own record | The fix, with the evidence in the report |
| Needs a measurement | Only a new run decides it | No change, and the report names the run that would decide it |
| Cross-file | The fix belongs in a file the reviewer does not own | No change here, and the exact old and new text in the report |

Needs a measurement is a complete outcome, and the common mistake is avoiding it. Do not
guess, average two figures, or pick the one that looks more plausible.

Leave everything the listed items do not touch. A fact pass is not a style pass, but its edits
follow the style. A superseded figure goes to the record rather than into a "was X, now Y"
sentence.

## The reader-cost review

The reader-cost review reads a page that is true and well written for what it still costs its
reader. It starts from the worklist of the reader-cost families:

    python3 prose-lint.py --config prose-lint.toml --voice \
      --families refrain,restatement,quantity,method,correction,contrast_and,coinage,inverted,ratio,conditions_result,forward,stale,row_mix,column_repeat,section_size,cell_size,ladder \
      --detail all

Each finding is a candidate, never a verdict. Four habits decide how a reviewer works them:

- **Work `restatement` by deleting, not linking.** For each second statement, ask whether that
  page's reader acts on the fact there. If not, delete it with no link. Keep one link only
  where the reader's next step needs the fact's home, at most once a section.
- **Work `stale` by evidence, not by age.** A figure stated with its operating point is
  correct. Where a newer measurement of the same configuration exists, it replaces the old one
  in the body. Otherwise the figure is re-measure debt, listed in the report.
- **Fix a wrong check, not the prose.** A family that fires on a correct sentence is a defect in
  the family. Report the sentence, and leave it as it is. Rewording a correct sentence to quiet
  a candidate trains the gate to miss the next real one.
- **Check that a check that reports nothing ran.** Compare the count a check read against a
  count taken another way, such as a search or a declaration total. A check that silently read
  nothing reports the same zero as a clean page.

### Review steps for a reader

These are the defects a clean gate passed, each found on a real corpus. A reviewer works
through the list for each page:

- **A gate described in prose is read off the code that decides it.** List every condition
  the deciding function tests, then check that the prose names each one.
- **One option, one rule.** Before writing a recommendation for an option, find every other
  sentence that mentions it. Two recommendations for one option are replaced by one rule and
  the observable condition that decides it.
- **A recommendation that differs from a default says why the default stays**, or the default
  changes.
- **A summary range is recomputed from the table it summarizes.**
- **Every pronoun with two candidates names its referent.** "This route" and "its row" are
  checked against the paragraph, and where two are in view, the sentence names the one meant.
- **A link targets the subsection that holds the fact**, not the chapter around it. An anchor
  check proves the target exists, not that it is the right one.
- **A baseline is the baseline's best configuration on the current build.** Check each setting
  that changes it before quoting a speedup against it.
- **Every refusal the code makes is a limit the user meets.** List the errors a supported case
  can return at run time as well as the reasons it is declined. The support table or a Caution
  names each.
- **An option's documentation is read off its parser, in both directions.** A switch parsed as
  an integer turns off for `on` and `true`. Every component that reads the option is named.
- **A defined term keeps its definition.** Search for each defined term before the review ends,
  and check every use means what the definition says.
- **A support row names the artifact it was validated on.** For a model, that is the checkpoint
  and the export that produced the file. A row that reads as one artifact is checked against
  what ran.
- **One configuration, one reading.** Before quoting a figure in a second table, find the other
  runs of that configuration. Quote one, or say what differed between them.
- **A figure under a Conditions line was taken under it.** An untagged figure inherits its
  section's conditions, so its run is checked against them.
- **A step the reader takes gives its command** (Rule 19). A figure a tool reproduces gives the
  command line, with every option that differs from the tool's defaults.
- **A version bound is read off the newest run that tested it.** A claim about another project
  is read off that project's own documentation.
- **When a default changes, every option and test that depended on the old one is followed.**
  A test named for a path the new default never reaches passes while testing nothing.

## The report

Every pass ends in a report to whoever ran it. The report states facts and omits the diff. Its
sections, in this order:

1. **Changed.** One line per change, with its outcome and the evidence that decided it: a
   commit and its date, a function, a record section.
2. **Cross-file.** The file, and the exact old and new text.
3. **Needs a measurement.** What to run, on what, under which conditions.
4. **Re-measure debt.** Figures in the body at a superseded operating point with no newer
   measurement, one line per section.
5. **Check defects.** Each candidate that fired on a correct sentence, with the sentence and its
   family.
6. **Finding classes.** Each kind of defect the pass found that no check caught, named in one
   sentence. Each says how a check or a review step catches it next time.
7. **The gate results** for every file the pass edited.

A style pass adds the token accounting from "The fact-preservation check". It also lists the
headings left unrenamed, and each house word kept in a standard sense.

## A finding becomes a check

The sixth section of the report is the reason the review is worth repeating. A finding class
becomes one of two things, and the person who runs the pass decides which:

- **A check**, where a pattern finds it with few false positives. It becomes a family in the
  gate, and the sentence that showed it becomes a self-test case. A check defect from the fifth
  section takes the same path. The pattern is fixed, and the correct sentence it fired on
  becomes a case that holds the fix.
- **A review step**, where only a reader can judge it. It joins "Review steps for a reader",
  in the project's own brief or in this document.

A class that applies beyond one project belongs in the kit. Send it upstream with the sentence
that showed it, the way an adoption report does.

## Briefing a reviewer

A reviewer who is another person or an agent knows only what the brief says. A brief that
leaves a decision open gets it made differently by each reviewer. Give every brief the same
parts:

```markdown
# Brief: <pass> on <set of files>

<What the pass is for, in two sentences, and what an earlier pass already did.>

You own only: <files>. Other reviewers are editing other files at the same time.
Edit nothing else, and make no version-control writes.

## Read first
1. The profile, in full.
2. The sections of STYLE.md this pass uses: <rules>.
3. Each file you own, in full, before editing it.

## The facts that moved
<One entry per fact, each with its source.>

## The worklist
<Where each file's gate and voice output is saved.>

## Preserved content
<The style pass's list, or "everything your items do not touch".>

## After editing
<The gate commands, and any project check, per file.>

## Report
<The sections of "The report", in order.>
```

Run a pilot on one file before handing out the rest. The pilot's report shows which decisions
the brief left open. Add each decision to the brief as a rule before the other reviewers start,
so every file gets the same answer.
