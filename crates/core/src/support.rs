//! The generated support matrix: every shipped recipe's declared support claim,
//! joined to the exact pins its lock records.
//!
//! Pure and deterministic — it reads the committed config and lock files and
//! formats them, resolving nothing upstream.
//!
//! The matrix exists so "which patch series worked with which kernel, on what
//! board" is answerable without decoding a SHA. It is *generated* on purpose: the
//! locks already hold every pin, so a table derived from them cannot drift from
//! what was actually built, and there is no second copy for someone to forget. The
//! one thing no lock can know — whether a human booted the result, and when — is the
//! recipe's [`Support`] claim, which is the only hand-written input here.

use crate::lock::Lock;
use crate::model::{ResolvedKernel, Support, SupportStatus};
use crate::{ConfigError, ConfigRoot};

/// Characters of a commit id shown in the matrix — the project's display
/// convention throughout.
const SHORT_COMMIT: usize = 12;

/// One row: a shipped recipe, the point it builds, and what is claimed about it.
///
/// Every field but [`status`](Self::status) and [`date`](Self::date) is read from
/// the recipe's resolution and its lock, so a row cannot describe a point the build
/// would not produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixRow {
    /// Recipe name — the handle `boot2deb build` takes.
    pub recipe: String,
    /// Device the recipe builds.
    pub device: String,
    /// Debian suite.
    pub suite: String,
    /// Kernel definition id (e.g. `rk3588-mainline-7.1`).
    pub kernel: String,
    /// The exact kernel the lock pins (e.g. `v7.1.1`), or [`None`] for a distro
    /// kernel — whose version rides the suite's package set rather than a commit,
    /// so the lock has none to state.
    pub kernel_ref: Option<String>,
    /// The patch series pinned for this build: profile, ref, and short commit.
    /// [`None`] where the kernel applies no series.
    pub patches: Option<PatchesCell>,
    /// The maintainer's claim.
    pub status: SupportStatus,
    /// `YYYY-MM-DD` the claim was last established.
    pub date: String,
}

/// The three facts that identify a validated patch series: which subset applied,
/// the human-legible release it came from, and the exact commit under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchesCell {
    /// Profile name selecting the series.
    pub profile: String,
    /// The release tag or branch the pin was taken at.
    pub reference: String,
    /// The exact commit, truncated for display.
    pub commit: String,
}

/// The matrix plus the recipes deliberately left out of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matrix {
    /// One row per recipe declaring a support claim, ordered by recipe name.
    pub rows: Vec<MatrixRow>,
    /// Recipes carrying no claim, ordered by name. Reported rather than silently
    /// dropped: for a locally authored recipe an absent claim is correct, but for
    /// one about to ship it is an omission, and only the caller knows which it is.
    pub unclaimed: Vec<String>,
}

/// Build the matrix from the committed recipes and locks under `root`.
///
/// Resolution supplies the declarative axes (device, kernel id, suite) so a recipe
/// that omits one and inherits the device default is still described completely;
/// the lock supplies the exact refs and commits. A recipe whose lock is missing is
/// an error, not an empty row: a shipped recipe without one is not buildable, so
/// there is no validated point to describe.
pub fn matrix(root: &ConfigRoot) -> Result<Matrix, ConfigError> {
    let mut out = Matrix::default();
    for recipe in root.list_recipes()? {
        let Some(claim) = root.recipe(&recipe)?.support else {
            out.unclaimed.push(recipe);
            continue;
        };
        let build = crate::resolve_recipe(root, &recipe, &Default::default())?;
        let lock = root.lock(&recipe)?;
        out.rows.push(row(recipe, &claim, &build.device, &build.suite, &build.kernel, &lock));
    }
    Ok(out)
}

/// Assemble one row from a recipe's claim, its resolution, and its lock.
fn row(
    recipe: String,
    claim: &Support,
    device: &str,
    suite: &str,
    kernel: &ResolvedKernel,
    lock: &Lock,
) -> MatrixRow {
    MatrixRow {
        recipe,
        device: device.to_string(),
        suite: suite.to_string(),
        kernel: kernel.id().to_string(),
        kernel_ref: lock.kernel.as_ref().map(|k| k.reference.clone()),
        patches: lock.patches.as_ref().map(|p| PatchesCell {
            profile: p.profile.clone(),
            reference: p.reference.clone(),
            commit: p.commit.chars().take(SHORT_COMMIT).collect(),
        }),
        status: claim.status,
        date: claim.date.clone(),
    }
}

impl MatrixRow {
    /// The kernel cell: the definition id, plus the pinned tag where the lock has
    /// one. A distro kernel is marked as taking its version from the suite, which is
    /// the honest reading of a lock that pins no kernel commit.
    ///
    /// Plain text, so the terminal and markdown renderings state the same thing and
    /// only their decoration differs.
    pub fn kernel_cell(&self) -> String {
        match &self.kernel_ref {
            Some(r) => format!("{} {r}", self.kernel),
            None => format!("{} (from the suite)", self.kernel),
        }
    }

    /// The patches cell: profile, release handle, and short commit, or `none` where
    /// the kernel applies no series. Plain text, like [`kernel_cell`](Self::kernel_cell).
    pub fn patches_cell(&self) -> String {
        match &self.patches {
            Some(p) => format!("{} {} ({})", p.profile, p.reference, p.commit),
            None => "none".to_string(),
        }
    }
}

/// The pinned identity of one source axis, as a label and the `ref (commit)` pair
/// that identifies it. [`None`] where the lock records no pin for that axis.
type AxisPin = (&'static str, Option<String>);

/// Every source axis a lock can pin, in report order, as `ref (short commit)`.
///
/// Ordered kernel-first because that is the axis a re-pin usually moves and the one
/// a reader checks first.
fn axis_pins(lock: &Lock) -> Vec<AxisPin> {
    let git = |r: &str, c: &str| format!("{r} ({})", c.chars().take(SHORT_COMMIT).collect::<String>());
    let mut v: Vec<AxisPin> = vec![
        ("kernel", lock.kernel.as_ref().map(|k| git(&k.reference, &k.commit))),
        ("patches", lock.patches.as_ref().map(|p| git(&p.reference, &p.commit))),
        ("u-boot", lock.uboot.as_ref().map(|u| git(&u.reference, &u.commit))),
        ("suite", Some(lock.rootfs.suite.clone())),
    ];
    if let Some(us) = &lock.userspace {
        v.push(("mpp", Some(git(&us.mpp.reference, &us.mpp.commit))));
        v.push(("librga", Some(git(&us.librga.reference, &us.librga.commit))));
        v.push(("libmali", Some(git(&us.libmali.reference, &us.libmali.commit))));
    }
    if let Some(ff) = &lock.ffmpeg {
        v.push(("ffmpeg", Some(git(&ff.base.reference, &ff.base.commit))));
        v.push(("ffmpeg-rk", Some(git(&ff.rockchip.reference, &ff.rockchip.commit))));
    }
    if let Some(b) = &lock.blobs {
        v.push(("blob atf", Some(b.atf.clone())));
        v.push(("blob tpl", Some(b.tpl.clone())));
        v.push(("blob bl32", b.bl32.clone()));
    }
    v
}

/// Describe what moved between two locks, one line per changed axis.
///
/// This is the evidence question behind a [`Validated`](SupportStatus::Validated)
/// claim: the claim rests on a build from specific pins, so a re-pin that moves any
/// of them retires the evidence — the claim would otherwise transfer, silently, to a
/// combination nobody booted. `update` holds both locks, which makes it the one
/// place the drift can be caught as it is introduced rather than audited for later.
///
/// An axis absent from one lock and present in the other is a change: gaining or
/// losing a patch series or a bootloader is exactly the kind of move that invalidates
/// a boot claim.
pub fn pin_changes(prev: &Lock, next: &Lock) -> Vec<String> {
    let (before, after) = (axis_pins(prev), axis_pins(next));
    let mut out = Vec::new();
    // Axes are keyed by label rather than zipped: the optional userspace/ffmpeg/blob
    // groups make the two vectors different lengths whenever one of those appears or
    // disappears, which is precisely a case worth reporting.
    let labels: Vec<&'static str> = before
        .iter()
        .chain(after.iter())
        .map(|(l, _)| *l)
        .fold(Vec::new(), |mut acc, l| {
            if !acc.contains(&l) {
                acc.push(l);
            }
            acc
        });
    for label in labels {
        let find = |v: &[AxisPin]| v.iter().find(|(l, _)| *l == label).and_then(|(_, p)| p.clone());
        let (was, now) = (find(&before), find(&after));
        if was != now {
            let show = |p: Option<String>| p.unwrap_or_else(|| "none".to_string());
            out.push(format!("{label} {} -> {}", show(was), show(now)));
        }
    }
    out
}

/// Header marking the rendered page generated, in the spirit of the lock banner:
/// the file is an output, and editing it is a change that the next generation
/// discards.
const PAGE_BANNER: &str = "\
<!-- Generated by `boot2deb support-matrix --markdown`; do not hand-edit.
     Regenerate after changing a recipe's [support] claim or re-pinning a lock. -->
";

/// The prose above the table. Fixed text, emitted with the table so the whole page
/// is one generated artifact — a page half generated and half hand-written has a
/// seam someone eventually edits on the wrong side.
const PAGE_INTRO: &str = "\
# Support matrix

What each shipped recipe has been taken through, and against which pins. Every
column but the last two is read from the recipe's lock — the exact pins a build
resolves — so this table cannot claim a combination that was never built.

| Status | Meaning |
|---|---|
| `validated` | An image built from this recipe booted on the hardware. |
| `expected` | Derived from a validated sibling, differing only along an axis not expected to change the outcome; never built, or built and never booted. |
| `experimental` | Under active bring-up. It may not build. |

The date is when the claim was last established: for `validated`, the day the image
booted; otherwise the day the claim was last assessed. Re-pinning a lock under a
`validated` claim is flagged by `boot2deb update`, because moving the pins retires
the evidence the claim rested on.
";

/// Render the matrix as the complete `docs/src/reference/support-matrix.md` page.
///
/// The output is byte-for-byte what the committed page must contain; a test
/// regenerates it and compares, which is what keeps the page from going stale
/// without anyone noticing.
pub fn render_markdown(matrix: &Matrix) -> String {
    let mut s = String::from(PAGE_BANNER);
    s.push('\n');
    s.push_str(PAGE_INTRO);
    s.push_str("\n| Recipe | Device | Suite | Kernel | Patches | Status | As of |\n");
    s.push_str("|---|---|---|---|---|---|---|\n");
    for r in &matrix.rows {
        // Code spans mark the values a reader would copy — an id, a tag, a commit.
        // The qualifiers around them ("from the suite", "none") are prose about the
        // build, and setting them in code would invite reading them as values.
        let kernel = match &r.kernel_ref {
            Some(rf) => format!("`{}` `{rf}`", r.kernel),
            None => format!("`{}` (from the suite)", r.kernel),
        };
        let patches = match &r.patches {
            Some(p) => format!("`{}` `{}` (`{}`)", p.profile, p.reference, p.commit),
            None => "none".to_string(),
        };
        s.push_str(&format!(
            "| `{}` | {} | {} | {kernel} | {patches} | `{}` | {} |\n",
            r.recipe, r.device, r.suite, r.status, r.date,
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row's cells describe the lock, including the two shapes a lock can take:
    /// a compiled kernel with a patch series, and a distro kernel with neither.
    #[test]
    fn cells_report_what_the_lock_pins() {
        let compiled = MatrixRow {
            recipe: "turing-rk1/forky".into(),
            device: "turing-rk1".into(),
            suite: "forky".into(),
            kernel: "rk3588-mainline-7.1".into(),
            kernel_ref: Some("v7.1.1".into()),
            patches: Some(PatchesCell {
                profile: "rk3588-accel".into(),
                reference: "main".into(),
                commit: "527d03d54ea6".into(),
            }),
            status: SupportStatus::Validated,
            date: "2026-07-14".into(),
        };
        assert_eq!(compiled.kernel_cell(), "rk3588-mainline-7.1 v7.1.1");
        assert_eq!(compiled.patches_cell(), "rk3588-accel main (527d03d54ea6)");

        // A distro kernel pins no commit — its version comes from the suite — and
        // applies no series. Neither cell may imply a pin the lock does not hold.
        let distro = MatrixRow {
            kernel: "debian-armmp".into(),
            kernel_ref: None,
            patches: None,
            ..compiled
        };
        assert_eq!(distro.kernel_cell(), "debian-armmp (from the suite)");
        assert_eq!(distro.patches_cell(), "none");
    }

    /// The rendered page carries the generated banner and one table row per matrix
    /// row, in matrix order.
    #[test]
    fn the_rendered_page_is_a_generated_artifact() {
        let page = render_markdown(&Matrix {
            rows: vec![MatrixRow {
                recipe: "asus-c201/forky".into(),
                device: "asus-c201".into(),
                suite: "forky".into(),
                kernel: "debian-armmp".into(),
                kernel_ref: None,
                patches: None,
                status: SupportStatus::Expected,
                date: "2026-07-20".into(),
            }],
            unclaimed: vec![],
        });
        assert!(page.starts_with("<!-- Generated by"));
        assert!(page.contains("| `asus-c201/forky` | asus-c201 | forky |"));
        assert!(page.contains("| `expected` | 2026-07-20 |"));
        // A distro kernel's qualifier is prose, not a value: setting "(from the
        // suite)" in a code span would read as something to copy into a config.
        assert!(page.contains("| `debian-armmp` (from the suite) | none |"));
        // Every status the enum admits is documented in the legend, so a new
        // variant cannot ship into the table undescribed.
        for status in SupportStatus::all() {
            assert!(
                PAGE_INTRO.contains(&format!("`{status}`")),
                "the page legend does not describe status '{status}'"
            );
        }
    }

    /// A lock parsed from its committed TOML form, for the pin-diff tests.
    fn lock(toml: &str) -> Lock {
        toml::from_str(toml).unwrap()
    }

    const BASE: &str = "\
[kernel]
id = \"rk3588-mainline-7.1\"
source = \"https://example.invalid/linux.git\"
ref = \"v7.1.1\"
commit = \"c9acdc466e9aa96352f658b9276aa8a45b8e817d\"

[patches]
profile = \"rk3588-accel\"
source = \"https://example.invalid/patches.git\"
ref = \"main\"
commit = \"527d03d54ea68a375b814ccb3314901530cb8b32\"

[rootfs]
suite = \"forky\"
manifest = \"r.pkgs.lock\"
";

    #[test]
    fn a_re_pin_reports_every_axis_that_moved() {
        // An unchanged lock has moved nothing: the guard must not cry wolf on a
        // routine `update` that re-resolves to the same commits.
        assert!(pin_changes(&lock(BASE), &lock(BASE)).is_empty());

        // A kernel bump names the axis and both sides of the move, so a reader can
        // see what the claim used to rest on.
        let bumped = BASE.replace("v7.1.1", "v7.1.4").replace(
            "c9acdc466e9aa96352f658b9276aa8a45b8e817d",
            "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        );
        assert_eq!(
            pin_changes(&lock(BASE), &lock(&bumped)),
            ["kernel v7.1.1 (c9acdc466e9a) -> v7.1.4 (a1b2c3d4e5f6)"]
        );

        // Losing a whole axis is a change, not an absence to skip over: a build that
        // stops applying a patch series is not the build that was validated.
        let (head, tail) = (BASE.find("[patches]").unwrap(), BASE.find("[rootfs]").unwrap());
        let no_patches = lock(&format!("{}{}", &BASE[..head], &BASE[tail..]));
        assert_eq!(
            pin_changes(&lock(BASE), &no_patches),
            ["patches main (527d03d54ea6) -> none"]
        );

        // Independent moves are reported together rather than one per run.
        let suite_and_kernel = bumped.replace("suite = \"forky\"", "suite = \"trixie\"");
        assert_eq!(
            pin_changes(&lock(BASE), &lock(&suite_and_kernel)),
            [
                "kernel v7.1.1 (c9acdc466e9a) -> v7.1.4 (a1b2c3d4e5f6)",
                "suite forky -> trixie",
            ]
        );
    }
}
