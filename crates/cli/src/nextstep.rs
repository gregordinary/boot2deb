//! What to do with a finished build: the `next:` block printed after a build that
//! wrote an image.
//!
//! Derived from what the run actually produced — the image files themselves and the
//! layout that decided how many there are — rather than from a template, so the
//! paths in it are paths that exist and the destinations match the layout. A `split`
//! build names both media, since that layout exists precisely because they are two.
//!
//! The device node is a placeholder. Naming a real one would be a command that
//! overwrites a disk if pasted on the wrong host, and the build has no way to know
//! which disk is meant.

use boot2deb_engine::image::{CompressedImage, ImageCompression, ImageOutput};
use std::path::PathBuf;

/// Placeholder device node. Not a real one anywhere: an operator has to substitute
/// it, which is the point.
const PLACEHOLDER: &str = "/dev/sdX";

/// One image a build produced, and what medium it is written to.
pub(crate) struct Flashable {
    /// The file to write — the preferred compressed form where the run compressed,
    /// else the raw image.
    pub(crate) path: PathBuf,
    /// The container [`path`](Self::path) is in, or `None` when it is the raw image.
    /// Decides whether the write command needs a decompressing pipe.
    pub(crate) format: Option<ImageCompression>,
    /// What the file is, which decides the destination it is written to.
    pub(crate) role: Role,
}

/// What an image file is within its build's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// A whole disk: bootloader in the raw gap, GPT, rootfs. The `combined` layout.
    Whole,
    /// Bootloader payloads only, for the boot medium. The `split` layout's first half,
    /// and what a `--stage uboot` run emits on its own.
    Bootloader,
    /// GPT + rootfs, for the OS disk. The `split` layout's second half.
    Rootfs,
}

impl Role {
    /// What the destination is, in words — the half of a `split` install that is not
    /// obvious from the file name.
    fn destination(self) -> &'static str {
        match self {
            Role::Whole => "the boot medium",
            Role::Bootloader => "the boot medium (the bootloader alone)",
            Role::Rootfs => "the OS disk",
        }
    }
}

/// Pair a layout's images with their compressed counterparts into the list of things
/// to flash.
///
/// Matching is by
/// [`CompressedImage::source`](boot2deb_engine::image::CompressedImage::source), not
/// by position: a run may ask for more than one container, so there is not one
/// compressed file per raw image to zip against. Within an image the first match
/// wins, which is the first container the operator asked for — the build emits them
/// in request order.
///
/// `compressed` is empty when the run was told not to compress. Where a raw image
/// was deleted after compression only the containers exist, which is why a
/// compressed path wins whenever there is one.
pub(crate) fn flashables(output: &ImageOutput, compressed: &[CompressedImage]) -> Vec<Flashable> {
    let roles: &[Role] = match output {
        ImageOutput::Combined { .. } => &[Role::Whole],
        ImageOutput::Split { .. } => &[Role::Bootloader, Role::Rootfs],
    };
    output
        .images()
        .iter()
        .zip(roles)
        .map(|(raw, role)| {
            let preferred = compressed.iter().find(|c| c.source == *raw);
            Flashable {
                path: preferred.map_or_else(|| raw.to_path_buf(), |c| c.path.clone()),
                format: preferred.map(|c| c.format),
                role: *role,
            }
        })
        .collect()
}

/// The `next:` block for a finished build, as lines ready to print.
///
/// Empty when the run produced no image at all, which is every `--stage` short of the
/// image node: there is nothing to write, so a hint would be an instruction to flash
/// a file that does not exist.
///
/// A compressed image is decompressed on the way through the pipe rather than to a
/// file, so the command needs no scratch space the size of the image.
pub(crate) fn hint(flashables: &[Flashable]) -> Vec<String> {
    if flashables.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "next: write {} to {PLACEHOLDER} — confirm the device with `lsblk` first, \
         since dd overwrites it whole",
        if flashables.len() == 1 {
            "the image"
        } else {
            "each image"
        }
    )];
    for f in flashables {
        // The destination is named only where the layout has more than one, since with
        // a single image it would restate the line above.
        if flashables.len() > 1 {
            lines.push(format!("      # {}", f.role.destination()));
        }
        lines.push(format!("      {}", write_command(f)));
    }
    lines
}

/// The command that writes one image file to the placeholder device: piped through
/// the container's decompressor where there is one, a plain `dd` for a raw image.
///
/// The decompressor comes from the recorded [`ImageCompression`] rather than from
/// sniffing the file name, so a `.gz` image is never handed to `xzcat`.
fn write_command(flashable: &Flashable) -> String {
    let path = flashable.path.display();
    match flashable.format {
        Some(format) => format!(
            "{} {path} | sudo dd of={PLACEHOLDER} bs=4M status=progress conv=fsync",
            format.decompressor()
        ),
        None => format!("sudo dd if={path} of={PLACEHOLDER} bs=4M status=progress conv=fsync"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combined(stem: &str) -> ImageOutput {
        ImageOutput::Combined {
            image: PathBuf::from(format!("/out/{stem}.img")),
        }
    }

    fn split(stem: &str) -> ImageOutput {
        ImageOutput::Split {
            bootloader: PathBuf::from(format!("/out/{stem}-boot.img")),
            rootfs: PathBuf::from(format!("/out/{stem}-rootfs.img")),
        }
    }

    /// A compressed artifact of `raw`, as the image node would report it.
    fn compressed(raw: &str, format: ImageCompression) -> CompressedImage {
        CompressedImage {
            source: PathBuf::from(raw),
            path: PathBuf::from(format!("{raw}.{}", format.extension())),
            format,
        }
    }

    #[test]
    fn a_combined_build_names_one_image_and_no_destination() {
        let xz = [compressed(
            "/out/turing-rk1-forky.img",
            ImageCompression::Xz,
        )];
        let lines = hint(&flashables(&combined("turing-rk1-forky"), &xz));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("the image"), "{lines:?}");
        assert_eq!(
            lines[1].trim(),
            "xzcat /out/turing-rk1-forky.img.xz | sudo dd of=/dev/sdX bs=4M \
             status=progress conv=fsync"
        );
        // One image, so there is nothing to disambiguate and no comment line.
        assert!(!lines.iter().any(|l| l.contains('#')), "{lines:?}");
    }

    #[test]
    fn a_split_build_names_both_media_in_layout_order() {
        let xz = [
            compressed("/out/turing-rk1-forky-boot.img", ImageCompression::Xz),
            compressed("/out/turing-rk1-forky-rootfs.img", ImageCompression::Xz),
        ];
        let lines = hint(&flashables(&split("turing-rk1-forky"), &xz));
        // Header, then a destination comment and a command for each half.
        assert_eq!(lines.len(), 5);
        assert!(lines[1].contains("bootloader"), "{lines:?}");
        assert!(lines[2].contains("-boot.img.xz"), "{lines:?}");
        assert!(lines[3].contains("OS disk"), "{lines:?}");
        assert!(lines[4].contains("-rootfs.img.xz"), "{lines:?}");
    }

    #[test]
    fn a_gz_build_pipes_through_zcat() {
        // The pipe is read off the recorded container, so a `.gz` image has to reach
        // the operator with the reader that can actually open it — handing a `.gz`
        // to `xzcat` produces a command that fails at the worst moment.
        let gz = [compressed(
            "/out/asus-c201-libreboot-mainline-forky.img",
            ImageCompression::Gz,
        )];
        let lines = hint(&flashables(
            &combined("asus-c201-libreboot-mainline-forky"),
            &gz,
        ));
        assert_eq!(
            lines[1].trim(),
            "zcat /out/asus-c201-libreboot-mainline-forky.img.gz | sudo dd \
             of=/dev/sdX bs=4M status=progress conv=fsync"
        );
    }

    #[test]
    fn an_uncompressed_image_is_written_without_a_pipe() {
        let lines = hint(&flashables(&combined("x"), &[]));
        assert_eq!(
            lines[1].trim(),
            "sudo dd if=/out/x.img of=/dev/sdX bs=4M status=progress conv=fsync"
        );
    }

    #[test]
    fn a_run_that_produced_no_image_hints_nothing() {
        assert!(hint(&[]).is_empty());
    }

    #[test]
    fn a_gz_image_is_piped_through_zcat_not_xzcat() {
        // The decompressor comes from the recorded format, so the one container
        // u-boot can read is never handed to the xz tool.
        let gz = [compressed("/out/x.img", ImageCompression::Gz)];
        let lines = hint(&flashables(&combined("x"), &gz));
        assert_eq!(
            lines[1].trim(),
            "zcat /out/x.img.gz | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync"
        );
    }

    #[test]
    fn the_first_container_requested_is_the_one_hinted() {
        // `--compress gz,xz` emits both for the same raw image; the hint names the
        // one asked for first rather than whichever sorts or lands first.
        let both = [
            compressed("/out/x.img", ImageCompression::Gz),
            compressed("/out/x.img", ImageCompression::Xz),
        ];
        let flash = flashables(&combined("x"), &both);
        assert_eq!(flash.len(), 1, "one raw image yields one flashable");
        assert_eq!(flash[0].format, Some(ImageCompression::Gz));
        assert!(hint(&flash)[1].contains("zcat /out/x.img.gz"));
    }

    #[test]
    fn a_split_build_matches_each_half_to_its_own_container() {
        // Matching is by source, not position: the rootfs half must not pick up the
        // bootloader half's artifact when the lists are not one-to-one.
        let arts = [
            compressed("/out/s-rootfs.img", ImageCompression::Gz),
            compressed("/out/s-boot.img", ImageCompression::Xz),
        ];
        let flash = flashables(&split("s"), &arts);
        assert_eq!(flash[0].path, PathBuf::from("/out/s-boot.img.xz"));
        assert_eq!(flash[1].path, PathBuf::from("/out/s-rootfs.img.gz"));
    }
}
