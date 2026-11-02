# The prose gate

boot2deb's documentation is held to a style guide. Part 6 of that guide is what a script
can check, and the gate fails a build on it. The gate also lists candidates for rules in
Parts 1 to 5, and fails on the few of those that admit no legitimate use. This directory
contains the guide, the script, the review that covers what the script cannot read, and
the profile that says what the guide cannot know about this project.

| File | What it is | Edit it |
|---|---|---|
| `STYLE.md` | The portable guide, 29 rules in six parts | No, it is vendored |
| `prose-lint.py` | The gate over Part 6, over the profile's own contract, and the voice checks | No, it is vendored |
| `REVIEWING.md` | The review a person does for what the gate cannot read | No, it is vendored |
| `PROFILE-TEMPLATE.md` | The document `PROFILE.md` was copied from | No, it is vendored |
| `PROFILE.md` | This project's readers, terms, house words, vocabulary, and guards | Yes |

`prose-lint.toml` sits at the repository root rather than here, so its paths read the way
the tree does. It names the surfaces, the two ratchets, the optional rules' thresholds,
the voice families' modes, and the headings of the `PROFILE.md` sections the gate reads.

## Running it

    python3 style/prose-lint.py --config prose-lint.toml
    python3 style/prose-lint.py --config prose-lint.toml --voice

Python 3.11 or newer is the only requirement. CI runs both forms and `--self-test`, and
each finishes in seconds. Four further modes are for a person rather than for a build:

- `--census` measures this corpus: findings per rule, density per surface, and what each
  optional rule costs at every threshold it can be set to.
- `--all` prints the same corpus finding by finding, with context lines.
- `--soft` prints the rules the gate reports and never fails, each of which needs a reader
  to judge.
- `--voice --detail all FILE` prints every voice candidate in one file, which is the
  worklist `REVIEWING.md` starts from.

## The ratchets

`prose-pending.txt` is the Part 6 ratchet, and it is empty: every surface is held in full.
A surface listed there stands at a weight it may not exceed, and comes off the list once
it reaches zero. Nothing a ratchet lists is exempt.

`voice-pending.txt` is the voice ratchet: one count per surface and family. Only the
families `prose-lint.toml` runs as `ratchet` are held to it. Each of those counts may fall
and may not grow. Lower a count, or delete its line, once the prose moves.

## Where the vendored files come from

The four vendored files are copied from the `style-kit` repository at commit `99fe2b5`.
They are copied rather than referenced, because a fresh clone of boot2deb has no path to
that repository. Change them upstream rather than here.

Re-run both baselines after taking a newer copy. A version that holds one more rule finds
more than the recorded counts allow, and every ratcheted surface then reports as having
got worse without a line of prose moving.
