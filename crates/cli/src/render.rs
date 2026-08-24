//! Output rendering: the one stdout contract for every command.
//!
//! A build streams [`Event`]s, rendered either for a human ([`print_event_at`], at the
//! caller's [`Verbosity`]) or as NDJSON under `--json` ([`print_event_json`]); artifact
//! locations travel on that same stream ([`emit_artifact`]) rather than as stray
//! prints, and status lines go through [`note`] so both modes carry the same facts. The
//! remaining helpers format the non-streaming commands' output — [`print_columns`]
//! sizing every `list-*` table from its own data.

use boot2deb_core::model::{ResolvedBoot, ResolvedBuild, ResolvedKernel};
use boot2deb_engine::event::{Event, LogOrigin, Stream};
use boot2deb_engine::EventSink;
use std::path::Path;

/// How much of the [`Event`] stream a human rendering shows.
///
/// The levels exist because the stream carries two very different volumes: what a
/// stage *decided* is tens of lines, and what its subprocesses *printed* is tens of
/// thousands. Showing both by default made a documented tens-of-minutes kernel
/// compile unreadable, and showing neither would hide the one thing a stuck build has
/// to say — so the split is on [`LogOrigin`], not on a line budget.
///
/// `--json` is unaffected: NDJSON is the whole stream by definition, and filtering it
/// would hand a scripted consumer a partial record of the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Verbosity {
    /// `--quiet`: artifacts and errors only — the result of the build and nothing
    /// about its progress. For a scripted caller that wants the paths.
    Quiet,
    /// The default: step boundaries, coarse progress, each stage's own decisions, the
    /// artifacts, and errors. Enough to follow a build, without the compile chatter.
    #[default]
    Normal,
    /// `--verbose`: the above plus every relayed subprocess line — what `make`, `git`,
    /// and `dpkg-buildpackage` actually printed. The level to reach for when a stage
    /// fails or hangs.
    Verbose,
}

impl Verbosity {
    /// Resolve the two flags into a level. They are mutually exclusive at the clap
    /// layer, so both set cannot reach here.
    pub(crate) fn from_flags(quiet: bool, verbose: bool) -> Self {
        match (quiet, verbose) {
            (true, _) => Verbosity::Quiet,
            (_, true) => Verbosity::Verbose,
            _ => Verbosity::Normal,
        }
    }

    /// Whether this level renders `event`.
    ///
    /// Errors and artifacts pass at every level: an error is why the command failed,
    /// and an artifact path is the thing the command was run to produce.
    fn shows(self, event: &Event) -> bool {
        match event {
            Event::Error { .. } | Event::Artifact { .. } => true,
            Event::Log {
                origin: LogOrigin::Subprocess,
                ..
            } => self == Verbosity::Verbose,
            _ => self != Verbosity::Quiet,
        }
    }
}

/// Render one build [`Event`] to the terminal: step boundaries as `==>` headers,
/// log lines indented (stderr to stderr), progress and errors called out — showing
/// only what `verbosity` admits.
pub(crate) fn print_event_at(verbosity: Verbosity, event: &Event) {
    if !verbosity.shows(event) {
        return;
    }
    match event {
        Event::StepStarted { step } => println!("==> [{step}] started"),
        Event::Progress { step, pct } => println!("--> [{step}] {pct}%"),
        Event::Log { stream, line, .. } => match stream {
            Stream::Stdout => println!("    {line}"),
            Stream::Stderr => eprintln!("    {line}"),
        },
        Event::StepFinished { step } => println!("==> [{step}] done"),
        Event::Artifact { role, path, .. } => println!("{role:<14}: {path}"),
        Event::Error { step, context } => eprintln!("==> [{step}] error: {context}"),
    }
}

/// Emit one event as a line of NDJSON on stdout — the `--json` wire form
/// ([`Event`]'s serde tagging is the schema).
pub(crate) fn print_event_json(event: &Event) {
    // Event serialization cannot fail (string/enum fields only).
    println!(
        "{}",
        serde_json::to_string(event).expect("event serializes")
    );
}

/// Report one produced artifact on the build stream ([`Event::Artifact`]): the
/// human sink renders it as a `role : path` summary line, the `--json` sink as
/// a structured event — either way the location is part of the one stdout
/// contract rather than a stray print.
pub(crate) fn emit_artifact(sink: &dyn EventSink, step: &str, role: &str, path: &Path) {
    sink.emit(Event::Artifact {
        step: step.to_string(),
        role: role.to_string(),
        path: path.display().to_string(),
    });
}

/// A build status line: printed for a human, or carried on the `--json` stream
/// as a stdout-tagged [`Event::Log`] under `step` — scripted consumers see the
/// same facts without stdout mixing plain text into the NDJSON.
///
/// `verbosity` gates the human side only; the `--json` stream carries the line
/// regardless, since it is part of the record of the build. Tagged
/// [`LogOrigin::Stage`], because it is the command speaking rather than a subprocess.
pub(crate) fn note(
    json: bool,
    verbosity: Verbosity,
    sink: &dyn EventSink,
    step: &str,
    line: String,
) {
    if json {
        sink.emit(Event::Log {
            step: step.to_string(),
            stream: Stream::Stdout,
            origin: LogOrigin::Stage,
            line,
        });
    } else if verbosity != Verbosity::Quiet {
        println!("{line}");
    }
}

/// Print `rows` as aligned columns, each width taken from the widest cell in it.
///
/// Every column but the last is padded and separated by two spaces; the last is
/// written bare, so no line carries trailing whitespace into a reader's terminal or
/// their copy-paste. A row shorter than the widest is padded with empty cells rather
/// than truncating the table.
///
/// Widths come from the data because names do not fit a constant: a hardcoded
/// `{:<24}` renders `asus-chromebit-cs10/forky` and `turing-rk1/media-accel-forky`
/// pushed out of their column, which is precisely when a listing is hardest to read.
///
/// Character counts rather than byte lengths, so a description with a non-ASCII
/// character still aligns.
pub(crate) fn print_columns(rows: &[Vec<String>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    for row in rows {
        let mut line = String::new();
        for i in 0..row.len() {
            let cell = &row[i];
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                let pad = widths[i].saturating_sub(cell.chars().count());
                line.push_str(cell);
                line.extend(std::iter::repeat_n(' ', pad + 2));
            }
        }
        println!("{}", line.trim_end());
    }
}

/// Finish one `list-*` command: under `--json`, print the collected rows as one
/// JSON array (unreadable entries ride along as `{name, error}` objects); in
/// human mode, surface unreadable entries via [`warn_unreadable`].
pub(crate) fn finish_listing(
    json: bool,
    rows: Vec<serde_json::Value>,
    kind: &str,
    broken: &[(String, String)],
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        warn_unreadable(kind, broken);
    }
    Ok(())
}

/// After a `list-*` render, surface unreadable entries on stderr so a corrupt
/// layer file is not lost in a long listing. The listing itself stays
/// usable and the exit code stays 0 — a warning, not a failure.
pub(crate) fn warn_unreadable(kind: &str, broken: &[(String, String)]) {
    if broken.is_empty() {
        return;
    }
    let plural = if broken.len() == 1 { "y" } else { "ies" };
    eprintln!("warning: {} {kind} entr{plural} unreadable:", broken.len());
    for (name, err) in broken {
        eprintln!("  {name}: {err}");
    }
}

/// First 12 characters of a commit id for display. Truncates on a character
/// boundary so a malformed (non-hex, hand-edited) value renders short instead
/// of panicking on a byte slice.
pub(crate) fn short(commit: &str) -> &str {
    match commit.char_indices().nth(12) {
        Some((i, _)) => &commit[..i],
        None => commit,
    }
}

/// Render a feature compatibility list for `list-features`: the values comma-joined,
/// or `"any"` when empty — an empty `requires_soc`/`requires_arch` means the feature
/// imposes no constraint, which reads better as "any" than as a blank.
pub(crate) fn constraint<T: std::fmt::Display>(items: &[T]) -> String {
    if items.is_empty() {
        "any".to_string()
    } else {
        items
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Render a byte count as a short human string (`1.5 GiB`, `812 MiB`, `4.0 KiB`).
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

/// Render a [`ResolvedBuild`] as the `resolve` command's human report: every axis
/// of the build point, in the order a reader checks them (device, hardware, kernel,
/// rootfs, image, sources).
///
/// A u-boot-only build reports the axes it does not have once, in words, and then
/// omits their lines entirely. Resolution carries device defaults in those fields
/// that no node ever reads, so printing them would show an image size and a hostname
/// for an artifact that has neither — and a blank locale line reads as a bug rather
/// than as an axis that does not exist here.
pub(crate) fn print_build(b: &ResolvedBuild) {
    let image = b.produces_image();
    println!("device       : {} — {}", b.device, b.description);
    println!("arch / soc   : {} / {}", b.arch, b.soc);
    println!("boot method  : {}", b.boot_method);
    if !image {
        println!("deliverable  : u-boot only — no kernel, suite, rootfs, or image");
    }
    // A kernel prints only what it has: a compiled one is described by its source and
    // config inputs, a distro one by the package that installs it.
    match &b.kernel {
        Some(ResolvedKernel::Compiled(k)) => {
            println!(
                "kernel       : {} ({}, base {})",
                k.id, k.flavor, k.base_defconfig
            );
            println!("  track      : {}", k.track.as_deref().unwrap_or("-"));
            println!(
                "  series     : {}",
                if k.patch_series.is_empty() {
                    "none".to_string()
                } else {
                    k.patch_series.join(", ")
                }
            );
            println!("  fragments  : {}", k.config_fragments.join(", "));
        }
        Some(ResolvedKernel::Distro(k)) => {
            println!("kernel       : {} (distro-package)", k.id);
            println!(
                "  package    : {} (version pinned in the package manifest)",
                k.package
            );
        }
        // Stated once above; a second "(none)" here would be noise.
        None => {}
    }
    if let Some(s) = &b.suite {
        println!("suite        : {s}");
    }
    // Which suite's `dpkg` archives this build's `.deb`s. Printed only where it is not
    // the image's own — that is the common case and would just restate the line above.
    // Where it differs, it is the only suite this build has, and it decides both which
    // root gets provisioned and the artifact-cache key of every `.deb` archived in it.
    if b.suite.as_deref() != Some(b.packaging_suite.as_str()) {
        println!(
            "packaged by  : {} (the device default — this deliverable resolves no suite \
             of its own)",
            b.packaging_suite
        );
    }
    if image {
        println!(
            "features     : {}",
            if b.features.is_empty() {
                "-".to_string()
            } else {
                b.features.join(", ")
            }
        );
        println!("rootfs pkgs  : {}", b.rootfs_packages.join(", "));
    }
    if !b.apt_sources.is_empty() {
        println!(
            "apt sources  : {}",
            b.apt_sources
                .iter()
                .map(|s| format!("{} ({})", s.name, s.uri))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !b.extra_debs.is_empty() {
        println!(
            "extra debs   : {}",
            b.extra_debs
                .iter()
                .map(|d| format!("{} ({})", d.locator_label(), short(&d.sha256)))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // The layout is read on both paths: it decides whether the bootloader is written
    // into the image's raw gap or emitted as its own file.
    println!("layout       : {}", b.layout);
    if image {
        println!("image size   : {}", b.image_size);
        println!("hostname     : {}", b.hostname);
        println!(
            "locale       : {} (generated: {})",
            b.locale,
            b.locales_generate.join(", ")
        );
        println!("timezone     : {}", b.timezone);
        // A headless board has no keymap and prints none — an empty line would suggest
        // the knob exists and was left blank, when in fact Debian's default is what
        // ships.
        if let Some(k) = &b.keymap {
            let mut km = k.layout.clone();
            if !k.variant.is_empty() {
                km.push_str(&format!(" ({})", k.variant));
            }
            println!("keymap       : {km} [{}]", k.model);
        }
        println!("dtb          : {}", b.kernel_dtb);
        // Only a board carrying its own (not-yet-upstream) device tree has sources to
        // show; an upstream-DTB board would print an empty line for nothing.
        if !b.device_dts.is_empty() {
            println!("device dts   : {}", b.device_dts.join(", "));
        }
        // Extra kernel arguments only where the board declares them.
        if !b.kernel_cmdline.is_empty() {
            println!("cmdline extra: {}", b.kernel_cmdline);
        }
    }
    // The boot section is the boot method's, and the two methods have nothing in
    // common to print: one compiles a bootloader out of blobs and writes it into a raw
    // gap, the other signs a kernel into a partition the firmware picks by its bits.
    match &b.boot {
        ResolvedBoot::RockchipRkbin(boot) => {
            println!(
                "u-boot       : {} ({})",
                boot.uboot_ref, boot.uboot_defconfig
            );
            println!(
                "u-boot series: {}",
                boot.uboot_series.as_deref().unwrap_or("none (pristine)")
            );
            println!("rkbin atf    : {}", boot.rkbin.atf);
            println!("rkbin tpl    : {}", boot.rkbin.tpl);
            if let Some(bl32) = &boot.rkbin.bl32 {
                println!("rkbin bl32   : {bl32}");
            }
            println!(
                "offsets      : idbloader {}, u-boot.itb {}, rootfs {}",
                boot.offsets.idbloader, boot.offsets.uboot_itb, boot.offsets.rootfs
            );
        }
        ResolvedBoot::Depthcharge(boot) => {
            println!("board profile: {}", boot.board);
            println!(
                "kernel part  : {} @ {} (priority {} tries {} successful {} -> flags {:#018x})",
                boot.kpart.size,
                boot.kpart.offset,
                boot.kpart.priority,
                boot.kpart.tries,
                boot.kpart.successful,
                boot.kpart.flags
            );
            println!("cmdline      : {} (root= derived from fstab)", boot.cmdline);
            println!("offsets      : rootfs {}", boot.rootfs_offset);
        }
    }
    // The SoC's initramfs module list is a kernel-axis value, so it has no meaning
    // for a build that compiles no kernel.
    if image {
        println!("modules      : {}", b.modules.join(", "));
    }
    println!("cross-compile: {}", b.cross_compile);
    if !image {
        return;
    }
    // Media-accel source trees print only when the build compiles the stack; a base
    // build reports it plainly instead of empty source lines.
    match (&b.userspace, &b.ffmpeg) {
        (Some(us), Some(ff)) => {
            // One line per tree the SoC declares — which trees appear is itself the
            // useful signal, since it says what this SoC's stack is made of.
            for (label, src) in [
                ("mpp          ", &us.mpp),
                ("librga       ", &us.librga),
                ("libmali      ", &us.libmali),
            ] {
                if let Some(s) = src {
                    println!("{label}: {} ({})", s.git, s.git_ref);
                }
            }
            println!("ffmpeg base  : {} ({})", ff.base.git, ff.base.git_ref);
            if let Some(rk) = &ff.rockchip {
                println!("ffmpeg rk    : {} ({})", rk.git, rk.git_ref);
            }
        }
        _ => println!("media-accel  : none (no feature builds the transcode stack)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_splits_the_stream_on_who_wrote_the_line() {
        let stage = Event::Log {
            step: "kernel".into(),
            stream: Stream::Stdout,
            origin: LogOrigin::Stage,
            line: "reusing kernel tree".into(),
        };
        let subprocess = Event::Log {
            step: "kernel".into(),
            stream: Stream::Stdout,
            origin: LogOrigin::Subprocess,
            line: "  CC drivers/foo.o".into(),
        };
        let started = Event::StepStarted {
            step: "kernel".into(),
        };
        let artifact = Event::Artifact {
            step: "image".into(),
            role: "compressed".into(),
            path: "/out/x.img.xz".into(),
        };
        let error = Event::Error {
            step: "kernel".into(),
            context: "make failed".into(),
        };

        // The default is the whole build minus the compile chatter — which is the
        // distinction the level exists to draw.
        assert!(Verbosity::Normal.shows(&stage));
        assert!(!Verbosity::Normal.shows(&subprocess));
        assert!(Verbosity::Normal.shows(&started));

        // Verbose adds the relayed output and takes nothing away.
        for e in [&stage, &subprocess, &started, &artifact, &error] {
            assert!(Verbosity::Verbose.shows(e), "{e:?}");
        }

        // Quiet keeps exactly what the command produced and why it stopped: an
        // artifact path is the reason it was run, and an error is why it failed.
        assert!(Verbosity::Quiet.shows(&artifact));
        assert!(Verbosity::Quiet.shows(&error));
        for e in [&stage, &subprocess, &started] {
            assert!(!Verbosity::Quiet.shows(e), "{e:?}");
        }
    }

    #[test]
    fn the_two_flags_resolve_to_one_level() {
        assert_eq!(Verbosity::from_flags(false, false), Verbosity::Normal);
        assert_eq!(Verbosity::from_flags(true, false), Verbosity::Quiet);
        assert_eq!(Verbosity::from_flags(false, true), Verbosity::Verbose);
        // clap rejects both, so this pairing never reaches here; resolving it to the
        // quieter level keeps the function total rather than panicking.
        assert_eq!(Verbosity::from_flags(true, true), Verbosity::Quiet);
    }

    #[test]
    fn short_truncates_on_character_boundaries() {
        assert_eq!(
            short("c9acdc466e9aa96352f658b9276aa8a45b8e817d"),
            "c9acdc466e9a"
        );
        assert_eq!(short("abc"), "abc");
        // Multibyte input truncates by characters, not bytes.
        assert_eq!(short("ééééééééééééééé"), "éééééééééééé");
    }

    #[test]
    fn human_size_scales_to_the_largest_fitting_unit() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(4096), "4.0 KiB");
        assert_eq!(human_size(1_610_612_736), "1.5 GiB");
    }

    #[test]
    fn constraint_renders_an_empty_list_as_any() {
        assert_eq!(constraint::<String>(&[]), "any");
        assert_eq!(
            constraint(&["arm64".to_string(), "riscv64".to_string()]),
            "arm64,riscv64"
        );
    }
}
