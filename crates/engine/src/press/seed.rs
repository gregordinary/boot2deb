//! The per-unit seed partition: a 1 MiB FAT12 volume holding `seed.txt`, built
//! whole and written whole.
//!
//! The seed is how one image presses six cards that do not collide: `press
//! --hostname --ssh-key --wifi-ssid` regenerates the partition per unit, and
//! the device's first-boot hook applies what it finds. FAT because an operator
//! can edit the file from any laptop — plug the card in, open `seed.txt`,
//! change the hostname, eject — with no `dd`, no root, and no boot2deb
//! installed.
//!
//! **Regenerated, never mutated.** Whether at build time (the empty template),
//! at press time, or under `boot2deb seed`, the whole partition image is built
//! in memory ([`partition_image`]) and written as one unit. A partition small
//! enough to regenerate needs no in-place FAT writer and no extent bookkeeping,
//! and it makes re-personalizing an already-pressed image the same code path as
//! a fresh press.
//!
//! The text format is `key=value` lines, `#` comments, unknown keys ignored —
//! [`render`] and [`parse`] are the two directions, and the device hook's shell
//! parser follows the same grammar. An absent or empty seed means "personalize
//! nothing", so an unpersonalized image behaves exactly like one pressed with
//! no keys at all.

use crate::error::EngineError;
use boot2deb_core::press::{SEED_PARTITION_BYTES, SEED_PARTLABEL};
use ferrosys::fat::{self, FormatOptions, VolumeLabel};
use ferrosys::{Metadata, Timestamp, TreeBuilder};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// The name the seed file has inside the FAT volume — what an operator opens.
pub const SEED_FILE: &str = "seed.txt";

/// The FAT volume label, so a desktop OS mounts the card as something
/// recognizable rather than `NO NAME`.
const SEED_LABEL: &str = "B2D-SEED";

/// FAT's epoch floor (1980-01-01T00:00:00Z). A build timestamp below it — a
/// zeroed `SOURCE_DATE_EPOCH`, say — is clamped up, because the format cannot
/// represent an earlier instant and refusing the whole image over a template
/// file's mtime would invert the priorities.
const FAT_EPOCH_FLOOR: i64 = 315_532_800;

/// The keys a seed can carry. Every field optional; the default is the empty
/// seed the built image ships. Unknown keys in the file are ignored on both
/// sides, so an old image meets a newer seed (and the reverse) safely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedKeys {
    /// `hostname=` — applied via `hostnamectl` plus `/etc/hosts` at first boot.
    pub hostname: Option<String>,
    /// `authorized_key=` lines — appended to the default account's
    /// `authorized_keys`, one entry each.
    pub authorized_keys: Vec<String>,
    /// `wifi_ssid=` — the network the hook writes a NetworkManager connection
    /// profile for, on images that carry NetworkManager (every Wi-Fi-capable
    /// board's do); elsewhere the hook logs and skips. The canonical per-site
    /// value that must never sit in a committed recipe.
    pub wifi_ssid: Option<String>,
    /// `wifi_psk=` — the WPA passphrase for [`wifi_ssid`](Self::wifi_ssid);
    /// absent means an open network. Plaintext on the FAT volume by design,
    /// like the rest of the seed: the card personalizes the unit, it is not a
    /// secret store.
    pub wifi_psk: Option<String>,
    /// `static_ip=` — static IPv4 (`address/prefix[,gateway[,dns...]]`,
    /// validated by `boot2deb_core::staticip`) for the connection the seed
    /// sets up: the Wi-Fi profile when [`wifi_ssid`](Self::wifi_ssid) is
    /// present, the wired interface otherwise — through NetworkManager on
    /// images that carry it, `dhcpcd.conf` on images that carry dhcpcd, and a
    /// logged skip on images with neither. Absent means DHCP.
    pub static_ip: Option<String>,
}

impl SeedKeys {
    /// Whether this seed personalizes anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hostname.is_none()
            && self.authorized_keys.is_empty()
            && self.wifi_ssid.is_none()
            && self.wifi_psk.is_none()
            && self.static_ip.is_none()
    }
}

/// Render the seed file's text: a self-describing header, then one `key=value`
/// line per entry. The header is what someone who finds the card reads, so it
/// says what the file is and that editing it is expected.
#[must_use]
pub fn render(keys: &SeedKeys) -> String {
    let mut out = String::from(
        "# boot2deb per-unit seed. Applied once at first boot; edit freely.\n\
         # Keys: hostname=<name>   authorized_key=<ssh public key line>\n\
         #       wifi_ssid=<network name>   wifi_psk=<passphrase; omit for an open network>\n\
         #       static_ip=<address/prefix[,gateway[,dns...]]; omit for DHCP>\n\
         # A missing or empty file personalizes nothing.\n",
    );
    if let Some(hostname) = &keys.hostname {
        out.push_str("hostname=");
        out.push_str(hostname);
        out.push('\n');
    }
    for key in &keys.authorized_keys {
        out.push_str("authorized_key=");
        out.push_str(key);
        out.push('\n');
    }
    if let Some(ssid) = &keys.wifi_ssid {
        out.push_str("wifi_ssid=");
        out.push_str(ssid);
        out.push('\n');
    }
    if let Some(psk) = &keys.wifi_psk {
        out.push_str("wifi_psk=");
        out.push_str(psk);
        out.push('\n');
    }
    if let Some(ip) = &keys.static_ip {
        out.push_str("static_ip=");
        out.push_str(ip);
        out.push('\n');
    }
    out
}

/// Parse a seed file's text — the same grammar the device hook's shell parser
/// follows: `key=value` per line, whitespace-trimmed, `#` comments and blank
/// lines skipped, unknown keys ignored (a newer image may know more keys), the
/// last `hostname=` winning.
#[must_use]
pub fn parse(text: &str) -> SeedKeys {
    let mut keys = SeedKeys::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("hostname=") {
            if !v.trim().is_empty() {
                keys.hostname = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("authorized_key=") {
            if !v.trim().is_empty() {
                keys.authorized_keys.push(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("wifi_ssid=") {
            if !v.trim().is_empty() {
                keys.wifi_ssid = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("wifi_psk=") {
            if !v.trim().is_empty() {
                keys.wifi_psk = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("static_ip=") {
            if !v.trim().is_empty() {
                keys.static_ip = Some(v.trim().to_string());
            }
        }
    }
    keys
}

/// Build the whole 1 MiB partition image: a FAT12 volume carrying
/// [`SEED_FILE`] with `keys` rendered into it.
///
/// `volume_id` and `time_secs` are the two values a FAT format would otherwise
/// invent; both are supplied so the built-in seed is a function of the lock
/// (the identity derivation and the rootfs timestamp), and a press-time seed is
/// a function of its inputs. `offset_sectors` is where the partition sits on
/// the medium, recorded as the volume's hidden-sector count.
///
/// # Errors
///
/// [`EngineError::ImageGeometry`] when the format refuses — which, with a fixed
/// size and a one-file root-owned tree, means a bug rather than an input.
pub fn partition_image(
    keys: &SeedKeys,
    volume_id: u32,
    time_secs: i64,
    offset_sectors: u32,
) -> Result<Vec<u8>, EngineError> {
    // Floored to FAT's epoch, then to even seconds: the format stores times at a
    // two-second granularity, and ferrosys counts a truncated odd second as a
    // fidelity loss — rounding here keeps `AcceptedLoss::NONE` honest.
    let time = Timestamp::from_secs(time_secs.max(FAT_EPOCH_FLOOR) & !1);
    let source = TreeBuilder::new().file(
        format!("/{SEED_FILE}").into_bytes(),
        render(keys).into_bytes(),
        Metadata::new(0o644, time),
    );
    let mut options = FormatOptions::new(volume_id, time)
        .label(VolumeLabel::new(SEED_LABEL).expect("a constant label the format accepts"));
    options.hidden_sectors = offset_sectors;
    let image = fat::format(source, SEED_PARTITION_BYTES, options).map_err(|e| {
        EngineError::ImageGeometry {
            detail: format!("the seed partition failed to format: {e}"),
        }
    })?;
    debug_assert!(
        image.fidelity().is_faithful(),
        "a 0644 root file loses nothing"
    );
    Ok(image.into_bytes())
}

/// Regenerate the seed partition of an already-pressed image file (`boot2deb
/// seed`, and the personalization half of `press`): find the [`SEED_PARTLABEL`]
/// entry in the target's GPT, rebuild the partition image, and write it over
/// the old one.
///
/// The volume id is derived from the entry's own PARTUUID, so re-seeding does
/// not change the volume's identity; `time_secs` is the caller's (a wall-clock
/// re-personalization is per-unit data, not a reproducible artifact).
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the target carries no readable GPT or
/// no seed partition (an image from before the seed existed), and I/O errors
/// naming the target.
pub fn rewrite_seed(target: &Path, keys: &SeedKeys, time_secs: i64) -> Result<(), EngineError> {
    let table = crate::press::verify::read_back_table(target)?;
    let entry = table
        .iter()
        .find(|e| e.name == SEED_PARTLABEL)
        .ok_or_else(|| EngineError::ImageVerifyGpt {
            target: target.display().to_string(),
            detail: format!(
                "no `{SEED_PARTLABEL}` partition — this image predates the seed \
                 partition; press a fresh one from current artifacts"
            ),
        })?;
    let length_bytes = (entry.last_lba - entry.first_lba + 1) * 512;
    if length_bytes != SEED_PARTITION_BYTES {
        return Err(EngineError::ImageVerifyGpt {
            target: target.display().to_string(),
            detail: format!(
                "the `{SEED_PARTLABEL}` partition is {length_bytes} bytes, expected \
                 {SEED_PARTITION_BYTES} — not a seed this build writes"
            ),
        });
    }
    // The PARTUUID's leading bytes give a stable volume id without a second
    // identity source; any four bytes of it would do, and these are as good.
    let volume_id = u32::from_le_bytes(
        entry.part_guid.as_bytes()[..4]
            .try_into()
            .expect("a UUID has 16 bytes"),
    );
    let image = partition_image(
        keys,
        volume_id,
        time_secs,
        u32::try_from(entry.first_lba).unwrap_or(0),
    )?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(target)
        .map_err(|e| EngineError::io(target, e))?;
    file.seek(SeekFrom::Start(entry.first_lba * 512))
        .map_err(|e| EngineError::io(target, e))?;
    file.write_all(&image)
        .map_err(|e| EngineError::io(target, e))?;
    file.sync_all().map_err(|e| EngineError::io(target, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render → parse round-trips every key, and the empty seed renders to a
    /// file that parses back as personalizing nothing.
    #[test]
    fn render_and_parse_round_trip() {
        let keys = SeedKeys {
            hostname: Some("rk1-03".into()),
            authorized_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA opr@laptop".into(),
                "ssh-rsa AAAAB3NzaC1yc2E backup@laptop".into(),
            ],
            wifi_ssid: Some("lab net".into()),
            wifi_psk: Some("hunter22hunter22".into()),
            static_ip: Some("192.168.1.50/24,192.168.1.1,1.1.1.1".into()),
        };
        assert_eq!(parse(&render(&keys)), keys);

        let empty = SeedKeys::default();
        assert!(empty.is_empty());
        assert_eq!(parse(&render(&empty)), empty);
    }

    /// The parser survives what an operator's editor does: CRLF, stray blanks,
    /// comments, unknown keys from a newer boot2deb — and takes the last
    /// hostname, since the file is edited in place.
    #[test]
    fn the_parser_tolerates_hand_edits() {
        let text = "# my card\r\n\r\nhostname=old\r\nhostname= rk1-05 \r\n\
                    color=blue\nauthorized_key= ssh-ed25519 KEY a@b \nwifi_ssid= attic \r\n\
                    static_ip= 10.0.0.9/24 \r\n";
        let keys = parse(text);
        assert_eq!(keys.hostname.as_deref(), Some("rk1-05"));
        assert_eq!(keys.authorized_keys, vec!["ssh-ed25519 KEY a@b"]);
        assert_eq!(keys.wifi_ssid.as_deref(), Some("attic"));
        assert_eq!(keys.wifi_psk, None);
        assert_eq!(keys.static_ip.as_deref(), Some("10.0.0.9/24"));
    }

    /// The partition image is exactly the partition's size, deterministic in its
    /// inputs, and reads back through ferrosys's own FAT reader with the seed
    /// file intact — the operator-edits-it promise, checked from the read side.
    #[test]
    fn the_partition_image_is_deterministic_and_reads_back() {
        let keys = SeedKeys {
            hostname: Some("h96-01".into()),
            authorized_keys: vec!["ssh-ed25519 KEY op@x".into()],
            ..SeedKeys::default()
        };
        let a = partition_image(&keys, 0xb2d5_eed1, 1_700_000_000, 2048).unwrap();
        let b = partition_image(&keys, 0xb2d5_eed1, 1_700_000_000, 2048).unwrap();
        assert_eq!(a.len() as u64, SEED_PARTITION_BYTES);
        assert_eq!(a, b, "two builds of one seed are the same bytes");
        let c = partition_image(&keys, 0xb2d5_eed2, 1_700_000_000, 2048).unwrap();
        assert_ne!(a, c, "the volume id reaches the bytes");

        let mut reader = fat::Reader::open(std::io::Cursor::new(a.clone())).unwrap();
        let node = reader.lookup(SEED_FILE.as_bytes()).unwrap();
        let text = String::from_utf8(reader.read_data(&node).unwrap()).unwrap();
        assert_eq!(parse(&text), keys);
    }

    /// A pre-1980 build epoch clamps to FAT's floor rather than refusing the
    /// whole image over a timestamp the format cannot spell — and an odd second
    /// rounds down to the format's two-second grid rather than counting as a
    /// fidelity loss.
    #[test]
    fn out_of_grid_timestamps_clamp() {
        let img = partition_image(&SeedKeys::default(), 1, 0, 0).unwrap();
        assert_eq!(img.len() as u64, SEED_PARTITION_BYTES);
        let odd = partition_image(&SeedKeys::default(), 1, 1_700_000_001, 0).unwrap();
        assert_eq!(
            odd,
            partition_image(&SeedKeys::default(), 1, 1_700_000_000, 0).unwrap()
        );
    }

    /// `rewrite_seed` finds the partition by label, replaces exactly its range,
    /// and refuses a medium without one.
    #[test]
    fn rewrite_replaces_the_partition_in_place() {
        use gpt::disk::LogicalBlockSize;
        use gpt::{partition_types, GptConfig};

        let tmp = tempfile::tempdir().unwrap();
        let disk_path = tmp.path().join("card.img");
        let size: u64 = 8 << 20;
        {
            let f = std::fs::File::create(&disk_path).unwrap();
            f.set_len(size).unwrap();
        }
        let seed_first: u64 = 2048; // 1 MiB
        let seed_len = SEED_PARTITION_BYTES / 512;
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&disk_path)
                .unwrap();
            let mut disk = GptConfig::new()
                .writable(true)
                .logical_block_size(LogicalBlockSize::Lb512)
                .create_from_device(file, Some(uuid::Uuid::from_bytes([0xaa; 16])))
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

        // Sentinels on both sides of the seed range, to prove the write stays
        // inside it.
        let before = seed_first * 512 - 1;
        let after = (seed_first + seed_len) * 512;
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&disk_path)
                .unwrap();
            for off in [before, after] {
                f.seek(SeekFrom::Start(off)).unwrap();
                f.write_all(&[0x5a]).unwrap();
            }
        }

        let keys = SeedKeys {
            hostname: Some("c201-02".into()),
            ..SeedKeys::default()
        };
        rewrite_seed(&disk_path, &keys, 1_700_000_000).unwrap();

        // The seed range now parses as the FAT we asked for...
        let bytes = std::fs::read(&disk_path).unwrap();
        let start = usize::try_from(seed_first * 512).unwrap();
        let end = start + usize::try_from(SEED_PARTITION_BYTES).unwrap();
        let mut reader =
            fat::Reader::open(std::io::Cursor::new(bytes[start..end].to_vec())).unwrap();
        let node = reader.lookup(SEED_FILE.as_bytes()).unwrap();
        let text = String::from_utf8(reader.read_data(&node).unwrap()).unwrap();
        assert_eq!(parse(&text).hostname.as_deref(), Some("c201-02"));

        // ...and the neighbours were not touched.
        assert_eq!(bytes[usize::try_from(before).unwrap()], 0x5a);
        assert_eq!(bytes[usize::try_from(after).unwrap()], 0x5a);

        // A disk without the partition refuses with directions.
        let bare = tmp.path().join("bare.img");
        {
            let f = std::fs::File::create(&bare).unwrap();
            f.set_len(4 << 20).unwrap();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&bare)
                .unwrap();
            let mut disk = GptConfig::new()
                .writable(true)
                .logical_block_size(LogicalBlockSize::Lb512)
                .create_from_device(file, Some(uuid::Uuid::from_bytes([0xbb; 16])))
                .unwrap();
            disk.add_partition_at("rootfs", 1, 2048, 2048, partition_types::LINUX_FS, 0)
                .unwrap();
            disk.write().unwrap();
        }
        let err = rewrite_seed(&bare, &keys, 1_700_000_000).unwrap_err();
        let EngineError::ImageVerifyGpt { detail, .. } = err else {
            panic!("expected ImageVerifyGpt, got {err}");
        };
        assert!(detail.contains("press a fresh one"), "{detail}");
    }
}
