# The prose gate

boot2deb's documentation is held to a style guide, and Part 5 of that guide is what a
script can check. This directory carries the guide, the script, and the profile that says
what the guide cannot know about this project.

| File | What it is | Edit it |
|---|---|---|
| `STYLE.md` | The portable guide, 23 rules in five parts | No, it is vendored |
| `prose-lint.py` | The gate over Part 5, and over the profile's own contract | No, it is vendored |
| `PROFILE-TEMPLATE.md` | The document `PROFILE.md` was copied from | No, it is vendored |
| `PROFILE.md` | This project's readers, terms, vocabulary, and guards | Yes |

`prose-lint.toml` sits at the repository root rather than here, so its paths read the way
the tree does. It names the surfaces, the ratchet, the two optional rules' thresholds, and
the headings of the two `PROFILE.md` tables the gate reads.

## Running it

    python3 style/prose-lint.py --config prose-lint.toml

Python 3.11 or newer is the only requirement. CI runs that form and `--self-test`, and both
finish in seconds. Three further modes are for a person rather than for a build:

- `--census` measures this corpus: findings per rule, density per surface, and what each
  optional rule costs at every threshold it can be set to.
- `--all` prints the same corpus finding by finding, with context lines.
- `--soft` prints the three rules the gate reports and never fails, each of which needs a
  reader to judge.

`prose-pending.txt` is the ratchet, and it is empty: every surface is held in full. A
surface listed there stands at a weight it may not exceed, and comes off the list once it
reaches zero. Nothing a ratchet lists is exempt.

## Where the vendored files come from

The three vendored files are copied from the `style-kit` repository at commit `4735d78`.
They are copied rather than referenced, because a fresh clone of boot2deb has no path to
that repository. Change them upstream rather than here.

Re-run `--baseline` after taking a newer copy. A version that holds one more rule finds
more than the recorded weights allow, and every ratcheted surface then reports as having
got worse without a line of prose moving.
