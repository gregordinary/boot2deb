//! Produce image files from a build's artifacts: the streaming write behind
//! `boot2deb press`, the verification of what was written, and the per-unit seed
//! partition.
//!
//! boot2deb does not write devices — flashing is a flasher's job, and any dd-like
//! tool can take the file `press` emits. What lives here is everything up to that
//! file: [`mod@write`] streams a (compressed) artifact into a destination with a
//! digest tap; [`verify`] re-reads what was written and holds it to the digest
//! and to the GPT the artifact carries; [`seed`] regenerates the per-unit
//! personalization partition inside the file. Re-assembly with tree additions —
//! the other way `press` produces a file — is the image node's
//! [`press_image`](crate::image::press_image), fed by the [`additions`] model
//! defined here — whose `.tmpl` entries are expanded against the image's own
//! identity by [`mod@template`].
//!
//! Everything runs against ordinary files, so the whole path is unit-tested on
//! any host with no root and no device.

pub mod additions;
pub mod seed;
pub mod template;
pub mod verify;
pub mod write;

#[cfg(test)]
mod tests {
    //! The whole press path end to end: a compressed artifact with a seed
    //! partition streams into an output file, verifies, personalizes, and reads
    //! back through the same parser the device hook mirrors.

    use crate::event::{Event, Step};
    use boot2deb_core::press::{SEED_PARTITION_BYTES, SEED_PARTLABEL};
    use gpt::disk::LogicalBlockSize;
    use gpt::{partition_types, GptConfig};
    use std::io::Write as _;
    use std::path::Path;

    /// A miniature but structurally real image: GPT with `b2d-seed` + `rootfs`,
    /// the seed FAT spliced in, xz-compressed like a build artifact.
    fn artifact(dir: &Path) -> std::path::PathBuf {
        let raw = dir.join("mini.img");
        let size: u64 = 8 << 20;
        let seed_first: u64 = 2048;
        let seed_len = SEED_PARTITION_BYTES / 512;
        {
            let f = std::fs::File::create(&raw).unwrap();
            f.set_len(size).unwrap();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&raw)
                .unwrap();
            let mut disk = GptConfig::new()
                .writable(true)
                .logical_block_size(LogicalBlockSize::Lb512)
                .create_from_device(file, Some(uuid::Uuid::from_bytes([0x11; 16])))
                .unwrap();
            disk.add_partition_at(
                SEED_PARTLABEL,
                1,
                seed_first,
                seed_len,
                partition_types::BASIC,
                0,
            )
            .unwrap();
            disk.add_partition_at(
                "rootfs",
                2,
                seed_first + seed_len,
                (size / 512) - (seed_first + seed_len) - 33,
                partition_types::LINUX_FS,
                0,
            )
            .unwrap();
            disk.write().unwrap();
        }
        // The built-in empty seed, exactly as the image node splices it.
        let seed_bytes = super::seed::partition_image(
            &super::seed::SeedKeys::default(),
            0x0b2d_0001,
            1_700_000_000,
            u32::try_from(seed_first).unwrap(),
        )
        .unwrap();
        {
            use std::io::{Seek, SeekFrom};
            let mut f = std::fs::OpenOptions::new().write(true).open(&raw).unwrap();
            f.seek(SeekFrom::Start(seed_first * 512)).unwrap();
            f.write_all(&seed_bytes).unwrap();
        }
        let xz = dir.join("mini.img.xz");
        {
            let f = std::fs::File::create(&xz).unwrap();
            let mut w =
                lzma_rust2::XzWriter::new(f, lzma_rust2::XzOptions::with_preset(1)).unwrap();
            w.write_all(&std::fs::read(&raw).unwrap()).unwrap();
            w.finish().unwrap();
        }
        std::fs::remove_file(&raw).unwrap();
        xz
    }

    #[test]
    fn the_press_round_trip_streams_verifies_and_personalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = artifact(tmp.path());
        let dest = tmp.path().join("card.img");

        let sink = |_: Event| {};
        let step = Step::start(&sink, "press");

        // The whole-image size read out of the compressed artifact's own GPT:
        // 8 MiB.
        assert_eq!(
            super::verify::planned_image_bytes(&artifact).unwrap(),
            8 << 20
        );

        // Write, exactly as a plain press does.
        let written = {
            let mut f = std::fs::File::create(&dest).unwrap();
            let written = super::write::stream_image(&artifact, &mut f, &step).unwrap();
            f.sync_all().unwrap();
            written
        };
        assert_eq!(written.bytes, 8 << 20);

        // Verify: digest re-read, then table against the artifact's own.
        super::verify::verify_digest(&dest, &written, &step).unwrap();
        let planned = super::verify::planned_table(&artifact).unwrap();
        let read_back = super::verify::read_back_table(&dest).unwrap();
        super::verify::compare_tables("t", &planned, &read_back).unwrap();
        assert_eq!(planned.len(), 2, "seed + rootfs");

        // Personalize, exactly as `boot2deb seed` (and `press --hostname`) does
        // after the write.
        let keys = super::seed::SeedKeys {
            hostname: Some("rk1-07".into()),
            authorized_keys: vec!["ssh-ed25519 KEY op@x".into()],
            ..super::seed::SeedKeys::default()
        };
        super::seed::rewrite_seed(&dest, &keys, 1_700_000_000).unwrap();

        // Read back through ferrosys's FAT reader + the parser the device hook
        // mirrors: the operator-visible file says what was asked.
        let entry = super::verify::read_back_table(&dest)
            .unwrap()
            .into_iter()
            .find(|e| e.name == SEED_PARTLABEL)
            .unwrap();
        let bytes = std::fs::read(&dest).unwrap();
        let start = usize::try_from(entry.first_lba * 512).unwrap();
        let end = start + usize::try_from(SEED_PARTITION_BYTES).unwrap();
        let mut reader =
            ferrosys::fat::Reader::open(std::io::Cursor::new(bytes[start..end].to_vec())).unwrap();
        let node = reader.lookup(super::seed::SEED_FILE.as_bytes()).unwrap();
        let text = String::from_utf8(reader.read_data(&node).unwrap()).unwrap();
        assert_eq!(super::seed::parse(&text), keys);
        step.finish();
    }
}
