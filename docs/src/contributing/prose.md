# Prose

Five surfaces are held to a style guide, and CI fails on the part of that guide a script
can check:

- This book.
- The README.
- The doc comment on every public item in `core`, `engine`, and the CLI.
- Every flag description in `crates/cli/src/args.rs`.
- The `caveats` and `description` values in the config tree, which a reader meets through
  `list-features` and the support matrix.

The guide is
[`style/STYLE.md`](https://github.com/gregordinary/boot2deb/blob/main/style/STYLE.md), and
it is portable. What it cannot know about boot2deb sits beside it in `style/PROFILE.md`.
That document names the readers, the fixed terms, the house words, and the lowercase
technical names the gate has to recognize. The terms boot2deb coins are defined in the
[glossary](../reference/glossary.md).

## Running it

```sh
python3 style/prose-lint.py --config prose-lint.toml
python3 style/prose-lint.py --config prose-lint.toml --voice
```

Python 3.11 or newer is the only requirement, and each run takes seconds. `prose-lint.toml`
at the repository root names every surface the gate walks. The first command holds the
rules a script can count, such as sentence length and the fixed terms. The second holds
the voice checks.

Three further modes are for working through a file rather than for a build:

- `--all` prints every finding with its context lines.
- `--soft` prints the rules the gate reports and never fails. Each needs a reader to judge,
  which is why none of them is in the build.
- `--census` measures the corpus: findings per rule, density per surface, and what each
  optional rule costs.

## The voice checks

`--voice` finds candidates for the rules a reader applies. A house word where the field
has a standard term is one, and so is a heading written as a sentence. Each check is a
family, and `prose-lint.toml` runs every family in one of three modes:

| Mode | The build fails when | Families |
|---|---|---|
| `hard` | The family finds anything | `internal`, `agent`, `first_paragraph` |
| `ratchet` | A page's count in the family grows | `house_word`, `residue`, `history`, `self_assessment`, `bold_hazard`, `characters`, `erratum`, `glossary` |
| `report` | Never | Every other family |

A `report` family is a worklist rather than a verdict. Run it over one file before editing
that file:

```sh
python3 style/prose-lint.py --config prose-lint.toml --voice --detail all docs/src/press.md
```

`style/REVIEWING.md` is the review that works such a list. A candidate that fires on a
correct sentence is a defect in the check, so report it and leave the sentence as it is.

## The ratchets

Two files record where a surface stood when a rule reached it. The rule over both is one
line: a listed surface never gets worse. A surface comes off the list once it reaches
zero, and the gate then holds it fully.

`prose-pending.txt` is the ratchet for the countable rules, and it is empty: every surface
the gate walks is held to them in full. The number beside a surface is a weight rather than
a count. A length finding weighs how far it runs over its limit, so splitting one long
sentence into two shorter ones lowers it. That is deliberate, because splitting is the move
the guide asks for. Never compress a sentence to get it under a limit.

`voice-pending.txt` is the ratchet for the voice families, with one count per surface and
family. Lower a count when the prose moves, and delete the line at zero.

Write each file with `--baseline`, and `--voice --baseline` for the second. Nothing a
ratchet lists is exempt. It records a starting point rather than a set of pages the rules
do not reach.

## When the gate goes red

Read the finding before editing. Rewriting a sentence is often when somebody finally reads
it. A count that no longer matches, or a cross-reference to a renamed section, surfaces
there. Fix the claim rather than the length.

A sentence that trips the gate by quoting a ruled-out word is a different case. Put the
word in backticks, or mark the block exempt with the reason in the marker. The marker has
to open its line:

    <!-- prose-lint: off -- the reason this block is exempt -->
    ...
    <!-- prose-lint: on -->

Two failures are about the configuration rather than about a page. A profile the gate
cannot read stops the run, and so does an answer the profile cites that no longer exists.
Neither is a page to go and edit.

## What is left out, and why

Two pages are generated, and a generated page is not the surface. Its sentences were
authored once somewhere else and are repeated once per row. A finding on the page is
therefore a fact about how many rows there are. The gate holds each at its source instead,
where a fix is made once:

- `reference/support-matrix.md` comes from `boot2deb support-matrix --markdown`, over the
  `caveats` in the config tree.
- `reference/cli-flags.md` comes from `boot2deb cli-reference --markdown`, over the doc
  comments in `crates/cli/src/args.rs`.

That is also why `args.rs` is the one file whose private doc comments are held. Every one
of them ships, through `--help` and through that page.
