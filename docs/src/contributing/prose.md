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
it is portable. What it cannot know about this project sits beside it in
`style/PROFILE.md`. That document carries the readers, the fixed terms, and the
lowercase technical names the gate has to recognize.

## Running it

```sh
python3 style/prose-lint.py --config prose-lint.toml
```

Python 3.11 or newer is the only requirement, and the run takes seconds. `prose-lint.toml`
at the repository root names every surface the gate walks. Three further modes are worth
knowing:

- `--all` prints every finding with its context lines, which is how one file gets worked
  through.
- `--soft` prints the three rules the gate reports and never fails. Each needs a reader to
  judge, which is why none of them is in the build.
- `--census` measures the corpus: findings per rule, density per surface, and what each
  optional rule costs.

## The ratchet

`prose-pending.txt` is the ratchet, and it is empty: every surface the gate walks is held
to the rules in full.

It stays as the mechanism a corpus that does not yet comply is brought in through. A
surface listed there carries the weight it stands at. The rule over that file is one line:
**a listed surface never gets worse.** A surface comes off the list once it reaches zero,
and the gate then holds it fully. Write the file with `--baseline`.

Nothing a ratchet lists is exempt. It records a starting point rather than a set of pages
the rules do not reach.

The number beside a surface is a weight rather than a count. A length finding weighs how
far it runs over its limit, so splitting one long sentence into two shorter ones lowers it.
That is deliberate, because splitting is the move the guide asks for. Never compress a
sentence to get it under a limit.

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
where a fix lands once:

- `reference/support-matrix.md` comes from `boot2deb support-matrix --markdown`, over the
  `caveats` in the config tree.
- `reference/cli-flags.md` comes from `boot2deb cli-reference --markdown`, over the doc
  comments in `crates/cli/src/args.rs`.

That is also why `args.rs` is the one file whose private doc comments are held. Every one
of them ships, through `--help` and through that page.
