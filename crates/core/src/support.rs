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
use crate::model::{Support, SupportStatus};
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
    /// Debian suite, or [`None`] for a u-boot-only recipe, which resolves no rootfs.
    pub suite: Option<String>,
    /// Kernel definition id (e.g. `rk3588-mainline-7.1`), or [`None`] for a
    /// u-boot-only recipe, which resolves no kernel.
    pub kernel: Option<String>,
    /// The exact kernel the lock pins (e.g. `v7.1.1`), or [`None`] for a distro
    /// kernel — whose version rides the suite's package set rather than a commit,
    /// so the lock has none to state — or a u-boot-only recipe.
    pub kernel_ref: Option<String>,
    /// The kernel patch series pinned for this build: series, ref, and short commit.
    /// [`None`] where the kernel applies no series.
    pub patches: Option<PatchesCell>,
    /// The u-boot patch series pinned for this build: series, ref, and short commit.
    /// [`None`] where the boot method compiles no u-boot, or u-boot ships pristine.
    pub uboot: Option<PatchesCell>,
    /// The out-of-tree kernel modules the image ships, in lock order. Empty for a
    /// board that declares no `device_kmods`. Stated because a `.ko` built from a
    /// driver repo is shipped bytes like any other pin, and it is the one the board's
    /// Wi-Fi works or does not work by.
    pub kmods: Vec<KmodCell>,
    /// The maintainer's claim.
    pub status: SupportStatus,
    /// `YYYY-MM-DD` the claim was last established.
    pub date: String,
}

/// The three facts that identify a validated patch series: which subset applied,
/// the human-legible release it came from, and the exact commit under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchesCell {
    /// Series names selecting the series, in apply order (comma-joined for display).
    pub series: Vec<String>,
    /// The release tag or branch the pin was taken at.
    pub reference: String,
    /// The exact commit, truncated for display.
    pub commit: String,
}

/// One out-of-tree kernel module the image carries: which driver, and the commit its
/// `.ko` was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmodCell {
    /// The `device_kmods` entry name, which is also the `<name>-modules-<kver>` deb's.
    pub name: String,
    /// The branch or tag the pin was taken at.
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
        out.rows.push(row(recipe, &claim, &build, &lock));
    }
    Ok(out)
}

/// The series/ref/short-commit cell for a pinned patch series, or [`None`] when
/// the lock records no such pin.
fn patches_cell(pin: Option<&crate::lock::PatchesPin>) -> Option<PatchesCell> {
    pin.map(|p| PatchesCell {
        series: p.series.clone(),
        reference: p.reference.clone(),
        commit: p.commit.chars().take(SHORT_COMMIT).collect(),
    })
}

/// Assemble one row from a recipe's claim, its resolution, and its lock.
fn row(
    recipe: String,
    claim: &Support,
    build: &crate::model::ResolvedBuild,
    lock: &Lock,
) -> MatrixRow {
    MatrixRow {
        recipe,
        device: build.device.clone(),
        suite: build.suite.clone(),
        kernel: build.kernel.as_ref().map(|k| k.id().to_string()),
        kernel_ref: lock.kernel.as_ref().map(|k| k.reference.clone()),
        patches: patches_cell(lock.patches.as_ref()),
        uboot: patches_cell(lock.uboot_patches.as_ref()),
        kmods: lock
            .kmods
            .iter()
            .map(|k| KmodCell {
                name: k.name.clone(),
                reference: k.reference.clone(),
                commit: k.commit.chars().take(SHORT_COMMIT).collect(),
            })
            .collect(),
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
        match (&self.kernel, &self.kernel_ref) {
            (Some(k), Some(r)) => format!("{k} {r}"),
            (Some(k), None) => format!("{k} (from the suite)"),
            (None, _) => "(u-boot only)".to_string(),
        }
    }

    /// The suite cell, or an em dash for a u-boot-only recipe with no rootfs.
    pub fn suite_cell(&self) -> &str {
        self.suite.as_deref().unwrap_or("—")
    }

    /// The kernel-patches cell: series, release handle, and short commit, or `none`
    /// where the kernel applies no series. Plain text, like
    /// [`kernel_cell`](Self::kernel_cell).
    pub fn patches_cell(&self) -> String {
        Self::pin_cell(self.patches.as_ref())
    }

    /// The u-boot-patches cell, in the same shape as [`patches_cell`](Self::patches_cell).
    pub fn uboot_cell(&self) -> String {
        Self::pin_cell(self.uboot.as_ref())
    }

    /// The out-of-tree-modules cell: `name ref (commit)` per module, comma-joined, or
    /// `none` for a board that declares none. Plain text, like
    /// [`kernel_cell`](Self::kernel_cell).
    pub fn kmods_cell(&self) -> String {
        if self.kmods.is_empty() {
            return "none".to_string();
        }
        self.kmods
            .iter()
            .map(|k| format!("{} {} ({})", k.name, k.reference, k.commit))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Render a patch-pin cell as `series ref (commit)` (series comma-joined), or
    /// `none` when absent.
    fn pin_cell(pin: Option<&PatchesCell>) -> String {
        match pin {
            Some(p) => format!("{} {} ({})", p.series.join(", "), p.reference, p.commit),
            None => "none".to_string(),
        }
    }
}

/// The pinned identity of one source axis, as a label and the `ref (commit)` pair
/// that identifies it. [`None`] where the lock records no pin for that axis.
///
/// The label is owned rather than static because the per-item axes — one per
/// out-of-tree module, one per pre-built `.deb` — name the item they pin, and that
/// name is what makes two locks' axes comparable.
type AxisPin = (String, Option<String>);

/// Every source axis a lock can pin, in report order, as `ref (short commit)`.
///
/// Ordered kernel-first because that is the axis a re-pin usually moves and the one
/// a reader checks first.
///
/// **Every pin in the lock appears here.** The set is the lock's whole identity, not a
/// selection from it: an axis left out is one a re-pin can move under a
/// [`Validated`](SupportStatus::Validated) claim with nobody told, which is exactly the
/// failure [`pin_changes`] exists to prevent. A new pin in [`Lock`] therefore belongs
/// in this list as well.
fn axis_pins(lock: &Lock) -> Vec<AxisPin> {
    let git =
        |r: &str, c: &str| format!("{r} ({})", c.chars().take(SHORT_COMMIT).collect::<String>());
    let mut v: Vec<AxisPin> = vec![
        (
            "kernel".into(),
            lock.kernel.as_ref().map(|k| git(&k.reference, &k.commit)),
        ),
        (
            "patches".into(),
            lock.patches.as_ref().map(|p| git(&p.reference, &p.commit)),
        ),
        (
            "u-boot".into(),
            lock.uboot.as_ref().map(|u| git(&u.reference, &u.commit)),
        ),
        (
            "u-boot patches".into(),
            lock.uboot_patches
                .as_ref()
                .map(|p| git(&p.reference, &p.commit)),
        ),
        (
            "suite".into(),
            lock.rootfs.as_ref().map(|r| r.suite.clone()),
        ),
    ];
    // A tree the SoC does not declare has no row at all, rather than a row reading
    // "none": these axes exist only for a build whose SoC has that hardware.
    if let Some(us) = &lock.userspace {
        for (axis, pin) in [
            ("mpp", &us.mpp),
            ("librga", &us.librga),
            ("libmali", &us.libmali),
        ] {
            if let Some(p) = pin {
                v.push((axis.into(), Some(git(&p.reference, &p.commit))));
            }
        }
    }
    if let Some(ff) = &lock.ffmpeg {
        v.push((
            "ffmpeg".into(),
            Some(git(&ff.base.reference, &ff.base.commit)),
        ));
        if let Some(rk) = &ff.rockchip {
            v.push(("ffmpeg-rk".into(), Some(git(&rk.reference, &rk.commit))));
        }
    }
    // One axis per out-of-tree module, keyed by the module's own name: a board's kmods
    // move independently, and moving one changes a `.ko` the image ships. Adding or
    // dropping a module shows up as the label appearing or disappearing, which
    // `pin_changes` already reports as a move to/from `none`.
    for k in &lock.kmods {
        v.push((
            format!("kmod {}", k.name),
            Some(git(&k.reference, &k.commit)),
        ));
    }
    // Pre-built `.deb`s are content pins, so the sha256 *is* the axis — a changed hash
    // is changed shipped bytes with no ref or commit standing between. Keyed by
    // locator, the only stable name they have.
    for d in &lock.extra_debs {
        let locator = d.url.as_deref().or(d.path.as_deref()).unwrap_or("?");
        v.push((
            format!("extra deb {locator}"),
            Some(d.sha256.chars().take(SHORT_COMMIT).collect()),
        ));
    }
    if let Some(b) = &lock.blobs {
        v.push(("blob atf".into(), Some(b.atf.clone())));
        v.push(("blob tpl".into(), Some(b.tpl.clone())));
        v.push(("blob bl32".into(), b.bl32.clone()));
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
    // groups and the per-item kmod/extra-deb axes make the two vectors different
    // lengths whenever one of those appears or disappears, which is precisely a case
    // worth reporting.
    let labels: Vec<&str> = before
        .iter()
        .chain(after.iter())
        .map(|(l, _)| l.as_str())
        .fold(Vec::new(), |mut acc, l| {
            if !acc.contains(&l) {
                acc.push(l);
            }
            acc
        });
    for label in labels {
        let find = |v: &[AxisPin]| {
            v.iter()
                .find(|(l, _)| l == label)
                .and_then(|(_, p)| p.clone())
        };
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
    s.push_str(
        "\n| Recipe | Device | Suite | Kernel | Patches | U-boot | Modules | Status | As of |\n",
    );
    s.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for r in &matrix.rows {
        // Code spans mark the values a reader would copy — an id, a tag, a commit.
        // The qualifiers around them ("from the suite", "none") are prose about the
        // build, and setting them in code would invite reading them as values.
        let kernel = match (&r.kernel, &r.kernel_ref) {
            (Some(k), Some(rf)) => format!("`{k}` `{rf}`"),
            (Some(k), None) => format!("`{k}` (from the suite)"),
            (None, _) => "(u-boot only)".to_string(),
        };
        let pins = |c: &Option<PatchesCell>| match c {
            Some(p) => {
                let series = p
                    .series
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{series} `{}` (`{}`)", p.reference, p.commit)
            }
            None => "none".to_string(),
        };
        let kmods = if r.kmods.is_empty() {
            "none".to_string()
        } else {
            r.kmods
                .iter()
                .map(|k| format!("`{}` `{}` (`{}`)", k.name, k.reference, k.commit))
                .collect::<Vec<_>>()
                .join(", ")
        };
        s.push_str(&format!(
            "| `{}` | {} | {} | {kernel} | {} | {} | {kmods} | `{}` | {} |\n",
            r.recipe,
            r.device,
            r.suite_cell(),
            pins(&r.patches),
            pins(&r.uboot),
            r.status,
            r.date,
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
            suite: Some("forky".into()),
            kernel: Some("rk3588-mainline-7.1".into()),
            kernel_ref: Some("v7.1.1".into()),
            patches: Some(PatchesCell {
                series: vec!["rk3588-accel".into()],
                reference: "main".into(),
                commit: "527d03d54ea6".into(),
            }),
            uboot: None,
            kmods: vec![],
            status: SupportStatus::Validated,
            date: "2026-07-14".into(),
        };
        assert_eq!(compiled.kernel_cell(), "rk3588-mainline-7.1 v7.1.1");
        assert_eq!(compiled.patches_cell(), "rk3588-accel main (527d03d54ea6)");
        assert_eq!(compiled.uboot_cell(), "none");
        assert_eq!(compiled.kmods_cell(), "none");

        // A distro kernel pins no commit — its version comes from the suite — and
        // applies no series. Neither cell may imply a pin the lock does not hold.
        let distro = MatrixRow {
            kernel: Some("debian-armmp".into()),
            kernel_ref: None,
            patches: None,
            ..compiled
        };
        assert_eq!(distro.kernel_cell(), "debian-armmp (from the suite)");
        assert_eq!(distro.patches_cell(), "none");

        // A u-boot-only recipe has no kernel or suite; its cells say so, and the
        // u-boot pin carries the series.
        let uboot_only = MatrixRow {
            recipe: "rk3576-generic/loader".into(),
            device: "rk3576-generic".into(),
            suite: None,
            kernel: None,
            kernel_ref: None,
            patches: None,
            uboot: Some(PatchesCell {
                series: vec!["rk3576-loader".into()],
                reference: "main".into(),
                commit: "e86ef2a00000".into(),
            }),
            kmods: vec![],
            status: SupportStatus::Expected,
            date: "2026-07-21".into(),
        };
        assert_eq!(uboot_only.kernel_cell(), "(u-boot only)");
        assert_eq!(uboot_only.suite_cell(), "—");
        assert_eq!(uboot_only.uboot_cell(), "rk3576-loader main (e86ef2a00000)");

        // A board with out-of-tree modules states each one's pin: the driver `.ko` is
        // shipped bytes, and a reader asking "which AIC8800 does this image carry"
        // gets an answer instead of an omission.
        let with_kmods = MatrixRow {
            kmods: vec![
                KmodCell {
                    name: "aic8800".into(),
                    reference: "main".into(),
                    commit: "abc123def456".into(),
                },
                KmodCell {
                    name: "sunplus".into(),
                    reference: "v1.2".into(),
                    commit: "0011223344ff".into(),
                },
            ],
            ..uboot_only
        };
        assert_eq!(
            with_kmods.kmods_cell(),
            "aic8800 main (abc123def456), sunplus v1.2 (0011223344ff)"
        );
    }

    /// The rendered page carries the generated banner and one table row per matrix
    /// row, in matrix order.
    #[test]
    fn the_rendered_page_is_a_generated_artifact() {
        let page = render_markdown(&Matrix {
            rows: vec![MatrixRow {
                recipe: "asus-c201/forky".into(),
                device: "asus-c201".into(),
                suite: Some("forky".into()),
                kernel: Some("debian-armmp".into()),
                kernel_ref: None,
                patches: None,
                uboot: None,
                kmods: vec![],
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
series = [\"rk3588-accel\"]
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
        let (head, tail) = (
            BASE.find("[patches]").unwrap(),
            BASE.find("[rootfs]").unwrap(),
        );
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

    /// A lock's per-item pins — one per out-of-tree module, one per pre-built `.deb` —
    /// are part of its identity too: each names a `.ko` or a package the image ships,
    /// so moving one under a `validated` claim leaves the claim describing different
    /// bytes. Keyed by the item's own name, so a board's modules move independently.
    #[test]
    fn moving_a_kmod_or_an_extra_deb_is_a_reported_move() {
        const WITH_ITEMS: &str = "\
[[kmods]]
name = \"aic8800\"
source = \"https://example.invalid/aic8800.git\"
ref = \"main\"
commit = \"1111111111111111111111111111111111111111\"

[[kmods]]
name = \"sunplus\"
source = \"https://example.invalid/sunplus.git\"
ref = \"v1.2\"
commit = \"2222222222222222222222222222222222222222\"

[[extra_debs]]
url = \"https://example.invalid/vendor-firmware.deb\"
sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"
";
        assert!(pin_changes(&lock(WITH_ITEMS), &lock(WITH_ITEMS)).is_empty());

        // One module bumped: named individually, so the reader sees which driver moved
        // rather than "kmods changed".
        let bumped = WITH_ITEMS.replace(
            "1111111111111111111111111111111111111111",
            "3333333333333333333333333333333333333333",
        );
        assert_eq!(
            pin_changes(&lock(WITH_ITEMS), &lock(&bumped)),
            ["kmod aic8800 main (111111111111) -> main (333333333333)"]
        );

        // A content-pinned deb has no ref or commit between the claim and the bytes,
        // so the sha256 is the axis.
        let rehashed = WITH_ITEMS.replace(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        assert_eq!(
            pin_changes(&lock(WITH_ITEMS), &lock(&rehashed)),
            ["extra deb https://example.invalid/vendor-firmware.deb \
                 aaaaaaaaaaaa -> bbbbbbbbbbbb"]
        );

        // Dropping a module entirely is a move to `none`, not silence.
        let (head, _) =
            WITH_ITEMS.split_at(WITH_ITEMS.find("[[kmods]]\nname = \"sunplus\"").unwrap());
        let dropped = format!(
            "{head}{}",
            &WITH_ITEMS[WITH_ITEMS.find("[[extra_debs]]").unwrap()..]
        );
        assert_eq!(
            pin_changes(&lock(WITH_ITEMS), &lock(&dropped)),
            ["kmod sunplus v1.2 (222222222222) -> none"]
        );
    }
}
