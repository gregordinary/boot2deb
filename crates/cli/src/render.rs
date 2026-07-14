//! Output rendering: the one stdout contract for every command.
//!
//! A build streams [`Event`]s, rendered either for a human ([`print_event`]) or as
//! NDJSON under `--json` ([`print_event_json`]); artifact locations travel on that
//! same stream ([`emit_artifact`]) rather than as stray prints, and status lines go
//! through [`note`] so both modes carry the same facts. The remaining helpers format
//! the non-streaming commands' output.

use boot2deb_core::model::{ResolvedBoot, ResolvedBuild, ResolvedKernel};
use boot2deb_engine::event::{Event, Stream};
use boot2deb_engine::EventSink;
use std::path::Path;

/// Render one build [`Event`] to the terminal: step boundaries as `==>` headers,
/// subprocess lines indented (stderr to stderr), progress and errors called out.
pub(crate) fn print_event(event: &Event) {
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
    println!("{}", serde_json::to_string(event).expect("event serializes"));
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
pub(crate) fn note(json: bool, sink: &dyn EventSink, step: &str, line: String) {
    if json {
        sink.emit(Event::Log {
            step: step.to_string(),
            stream: Stream::Stdout,
            line,
        });
    } else {
        println!("{line}");
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
        items.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
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
pub(crate) fn print_build(b: &ResolvedBuild) {
    println!("device       : {} — {}", b.device, b.description);
    println!("arch / soc   : {} / {}", b.arch, b.soc);
    println!("boot method  : {}", b.boot_method);
    // A kernel prints only what it has: a compiled one is described by its source and
    // config inputs, a distro one by the package that installs it.
    match &b.kernel {
        ResolvedKernel::Compiled(k) => {
            println!("kernel       : {} ({}, base {})", k.id, k.flavor, k.base_defconfig);
            println!("  track      : {}", k.track.as_deref().unwrap_or("-"));
            println!("  profile    : {}", k.patch_profile.as_deref().unwrap_or("none"));
            println!("  fragments  : {}", k.config_fragments.join(", "));
        }
        ResolvedKernel::Distro(k) => {
            println!("kernel       : {} (distro-package)", k.id);
            println!("  package    : {} (version pinned in the package manifest)", k.package);
        }
    }
    println!("suite        : {}", b.suite);
    println!(
        "features     : {}",
        if b.features.is_empty() { "-".to_string() } else { b.features.join(", ") }
    );
    println!("rootfs pkgs  : {}", b.rootfs_packages.join(", "));
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
    println!("layout       : {}", b.layout);
    println!("image size   : {}", b.image_size);
    println!("hostname     : {}", b.hostname);
    println!("locale       : {} (generated: {})", b.locale, b.locales_generate.join(", "));
    println!("timezone     : {}", b.timezone);
    // A headless board has no keymap and prints none — an empty line would suggest the
    // knob exists and was left blank, when in fact Debian's default is what ships.
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
    // The boot section is the boot method's, and the two methods have nothing in
    // common to print: one compiles a bootloader out of blobs and writes it into a raw
    // gap, the other signs a kernel into a partition the firmware picks by its bits.
    match &b.boot {
        ResolvedBoot::RockchipRkbin(boot) => {
            println!("u-boot       : {} ({})", boot.uboot_ref, boot.uboot_defconfig);
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
    println!("modules      : {}", b.modules.join(", "));
    println!("cross-compile: {}", b.cross_compile);
    // Media-accel source trees print only when the build compiles the stack; a base
    // build reports it plainly instead of empty source lines.
    match (&b.userspace, &b.ffmpeg) {
        (Some(us), Some(ff)) => {
            println!(
                "mpp / librga : {} ({}) / {} ({})",
                us.mpp.git, us.mpp.git_ref, us.librga.git, us.librga.git_ref
            );
            println!("ffmpeg base  : {} ({})", ff.base.git, ff.base.git_ref);
            println!("ffmpeg rk    : {} ({})", ff.rockchip.git, ff.rockchip.git_ref);
        }
        _ => println!("media-accel  : none (no feature builds the transcode stack)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_truncates_on_character_boundaries() {
        assert_eq!(short("c9acdc466e9aa96352f658b9276aa8a45b8e817d"), "c9acdc466e9a");
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
        assert_eq!(constraint(&["arm64".to_string(), "riscv64".to_string()]), "arm64,riscv64");
    }
}
