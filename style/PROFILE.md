# Prose profile: boot2deb

`STYLE.md` holds the rules. This document holds what that guide asks each project to
supply, because the portable core cannot know it. Where this document is silent, the core
governs. A profile refines the core and never relaxes it.

## Readers and depth budget

| Surface | Reading path: reader | Record: reader, and where it is | Depth |
|---|---|---|---|
| `README.md` | Someone deciding whether to use boot2deb, then building the first image | None | The shortest text that gets a reader building, running, and past the common failures |
| mdBook guide chapter | A user running one part of the tool | None | One subject, from what it is to a reader running it |
| mdBook reference chapter | A user who already builds and needs the exact rule | None | One entry per name, flag, or layer |
| mdBook board chapter | Someone holding that board | A user checking a support claim: `boot2deb support-matrix`, generated from the locks | What is different about this board, and what has been booted on it |
| Doc comment on a public item in `core` or `engine` | A developer reading rustdoc | None | The API reference's: what it does, what it requires, what it guarantees |
| Doc comment on anything else | Whoever changes the code next | None | The invariant, and why the code is as it is |
| `boot2deb --help` | Someone at a prompt | None | One line per option, no rationale |
| Internal notes and handovers | A maintainer | These are the record, and none of them ships | As deep as the mechanism goes |

## Fixed terms

| Concept | Write | Not | Standard or coined |
|---|---|---|---|
| The named point a build resolves to | recipe | build preset, build config, build target | Coined |
| The file `update` writes beside a recipe | lock | lockfile, pinfile | Coined |
| A hardware config layer | layer | hardware level, hardware tier, config level, config tier | Coined |
| An independent dimension of a build point | axis | build dimension | Coined |
| An out-of-tree module layer | `kmods/` | out-of-tree driver layer, external module layer | Coined |
| A rootfs capability set | feature | addon, package bundle | Coined |
| A user tree that wins over the shipped one | overlay | config fork, custom tree | Coined |
| A structure containing a field | contains | carries a field, carries the field, has a field, has the field, holds a field, holds the field | Standard |
| A thing composed of parts | has | is made of, consists of | Standard |

Each coined term has a glossary entry, which names its nearest standard term. The words
are the field's own, and the sense each takes here is boot2deb's, which is why they count
as coined.

## House words

| Forms | Write instead | Standard in |
|---|---|---|
| carry, carries, carried, carrying | contains, has, includes, keeps, records | carry bit, carry-in |
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

The list is never exhaustive. A word missing from it is not thereby approved, because
Rule 2's test decides: would a developer who has read Debian's and the kernel's
documentation, but not this project, know what the word means here?

## Glossary

The glossary is `docs/src/reference/glossary.md`, a chapter of the book.

The reader's boundary is a Linux user who has installed Debian and built software from
source. That reader has Debian's own documentation (the Debian Policy Manual, apt, and
debootstrap), the Linux kernel's build documentation, and U-Boot's. A term inside that
boundary, such as rootfs, initramfs, kconfig, device tree, suite or pocket, gets no entry.
A term boot2deb coins, or uses in a sense of its own, gets one.

The Source column names one of: Debian Policy, Linux kernel documentation, U-Boot
documentation, depthcharge-tools, or Coined. A coined term's nearest standard term names
the project it comes from in parentheses, such as Yocto or Buildroot.

## Genre and exemplars

The book is a command-line tool's user guide and reference. Three public exemplars a
writer can open:

- The Cargo Book, for a guide and a reference over one tool's configuration files.
- The Buildroot user manual, for an image builder's board and configuration chapters.
- The `debootstrap(8)` man page, for one command's options.

The forms this project uses:

- **Callouts.** `> [!NOTE]` and `> [!CAUTION]`, which mdBook renders as labeled blocks. A
  hazard (a hang, wrong output, lost data) is a Caution, never a bold sentence.
- **Support status.** The values `boot2deb support-matrix` prints: `validated`,
  `expected`, and `experimental`, each defined on `SupportStatus` in `core::model`.

## Record zones

The public documents have no record section. The record of what has booted is
`boot2deb support-matrix`, generated from the locks, and the measurements behind a board
chapter stay in internal notes. A section with one of these headings opens a record zone,
and the gate's residue checks skip it:

- `Evidence`: the method, the controls and the run counts behind a section's facts.
- `Validation`: the configurations a result was validated on, and how.

## Answered questions

`core` is where a question gets its one answer, because the answer has to be reachable
from a host with no Linux side effects to reproduce. A row here is a question a writer
would search for before adding a second answer to it.

| The question | The answer |
|---|---|
| How do a device's hardware layers merge, and in which order? | `core::resolve::resolve_device` |
| How is an upstream tag spelled, and which version does it hold? | `core::version::parse_tag` |
| Does a solved package set satisfy its own dependencies? | `core::debdep::unsatisfied` |
| How do two Debian package versions compare? | `crates/core/src/debdep.rs`, whose `compare_versions` is dpkg's own ordering |
| What does a size written `32KiB` or `2G` mean in bytes? | `core::size::parse_size` |
| Which archive pockets does a suite publish, and from where? | `crates/core/src/distro.rs`, which lists them per suite |
| Which distribution serves a suite? | `core::loader::ConfigRoot::distro_for_suite` |
| Which git ref does `update` pin for one source axis? | `core::repin::pick_ref` |
| Is a source pin durable without a network? | `core::sources::PinForm::classify` |
| How does a fetched patch become a `git am`-ready mbox? | `core::mbox::normalize` |
| Which timestamp spelling does this project write? | `core::datetime::format_rfc3339` |
| Which ChromeOS GPT attribute bits choose the kernel partition? | `core::chromeos::kpart_flags` |
| Is an `authorized_keys` entry well-formed? | `core::authkeys::check_authorized_key` |
| Where does a config asset resolve once overlays are applied? | `core::loader::ConfigRoot` |
| What does an image's package set weigh? | `core::weight::WeightReport` |

## Guards

Each table above has a guard, and this section says what that guard reaches. A guard
listed here is one that runs, and the third column is the part a reader still owns.

| The concept | The guard | What it cannot see |
|---|---|---|
| Which board and pin has booted | `boot2deb support-matrix`, read from each recipe's lock | Whether the hardware still boots after a pin moves. The claim is retired, not re-earned |
| The complete flag surface | `cli-reference`'s staleness test, which fails when the committed page and the binary disagree | Whether a flag's description is accurate, only whether it is current |
| A public item without a doc comment | `#![warn(missing_docs)]` on `core` and `engine`, denied in CI | Anything about the comment's content |
| Every listed prose surface | `prose-lint.py` over Part 6, and the ratchet in `prose-pending.txt` | Parts 1 to 5, which need a reader |
| Candidates for Parts 1 to 5 | `prose-lint.py --voice`. `internal`, `agent` and `first_paragraph` fail on any finding, eight families hold at the counts in `voice-pending.txt`, and the rest report | Whether a candidate is a defect. `REVIEWING.md` is the review that decides |
| The house words above | The `house_word` family, held by the voice ratchet | A house sense of a word the table does not list, and a listed word used in a standard sense the third column does not name |
| The fixed-terms table above | `prose-lint.py`, which matches the third column literally per paragraph | A word this project also uses correctly in another sense. The table rules out `build config`, so a reviewer catches a *recipe* called a "config" |
| The answered-questions table above | `prose-lint.py`, which resolves every cited path against the crates | Whether a row still answers the question it asks. A citation that survives a refactor can end up naming a different mechanism |
| A bold lead-in that is really a heading | Nothing mechanical. `label_block_words` is 0, because the script counts a definition-label list as sections | Whether a reader would want to link to the label. A section of parallel bold lead-ins each introducing paragraphs is the case to catch by eye |
| A build axis called a knob | The `house_word` family lists every `knob` | Whichever sense is meant. `knob` is a settable option here as often as it is an axis, and only a reader can tell |
| An internal reference in a shipped page | The `internal` family, over `voice.internal_patterns` | A private name the patterns do not describe. The list names shapes rather than names, because it ships too |

## Protected enumerations

Rule 13 says to name the category rather than enumerate it. Two generated pages outrank
that rule, because being exhaustive is what each of them is for:

- `docs/src/reference/cli-flags.md` lists every flag of every subcommand, from the command
  tree itself. A test fails when the page and the binary disagree.
- `docs/src/reference/support-matrix.md` lists every shipped recipe, read from the locks.
  A row it does not contain is a combination nothing has built.

Rule 13 governs everywhere else, including the prose those two pages open with.

## Domain vocabulary

**A script reads this section.** The words must sit in one italicized comma-separated run,
and the heading must match `profile.vocabulary_heading` character for character. The gate
reads the list to tell a lowercase technical name from a list item that forgot its capital.
Adding a word here is how a name is admitted.

*boot2deb, u-boot, rootfs, initramfs, kconfig, kmods, apt, dpkg, ext4, btrfs, vfat, xz,
zstd, qemu-user, maskrom, vboot, depthcharge, mdbook, rustdoc, snapshot.debian.org, forky,
trixie, sid, arm64, armhf, x86_64, rkbin, rkmpp, rkvdec, rga, mpp, ffmpeg, v4l2request,
h264_rkmpp, hevc_rkmpp, jellyfin, systemd, udev, sudo, cargo, rustup, tpi, gpt, spi, emmc,
dtb, fit, npu, iommu, soc, librga, linux-libre.*

The list is extended by use rather than by decision, and it is never exhaustive.

## Evidence, and the claims that have a boundary

boot2deb does not use tag vocabulary in its public documents. A claim names the artifact
that established it, and `boot2deb support-matrix` is the generated record of what has
booted. Internal notes and handovers do use bracketed tags.

Claims that are true only inside a boundary:

- A `validated` status is per recipe and per pin. Re-pinning retires it.
- The transcode measurements are per board, per codec, and per resolution.
- An absent support claim bounds the evidence, and not the hardware.

## Surfaces that repeat one claim

| Claim | Home | Derivations |
|---|---|---|
| Which board and pin has booted | `boot2deb support-matrix`, generated from the locks | README status section, the mdBook support matrix page |
| What each shipped board demonstrates | The mdBook board chapters | The README boards table |
| How the layer axes resolve | `crates/core` rustdoc | The mdBook config model page, the README |

## Publishing constraints

The README and the mdBook ship. Neither contains a hostname, an IP address, a credential,
a private repository name, or an internal tracking id. This directory ships with them and
is held to the same constraint. The `internal` voice family enforces the part a pattern
can, and `voice.internal_patterns` in `prose-lint.toml` is that list.
