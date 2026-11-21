//! The on-device scripts' shell harnesses, run under `cargo test`.
//!
//! Three scripts an image ships make decisions worth testing off the board:
//!
//! - The seed hook (`base/overlay/etc/boot2deb/first-boot.d/10-seed`), which applies
//!   hand-editable personalization.
//! - The data-volume hook, which may partition a disk.
//! - `boot2deb-install-to` (`base/overlay/usr/sbin/`), which writes an embedded image
//!   over a whole disk.
//!
//! All three are POSIX sh that runs on the device, where no boot2deb Rust code runs.
//! Each has a harness beside it that stubs the system tools the script calls and
//! asserts on what the script does with their answers. Running the harnesses here holds
//! the scripts to the same `cargo test` gate as the Rust code.
//!
//! The seed hook also repeats the press-time value checks in sh, and a test here
//! holds that copy to the Rust checks in `boot2deb_core` value by value, so the two
//! cannot drift apart.
//!
//! Each harness touches nothing outside its own temporary directory and runs from the
//! boot2deb root, which is where they look for the hook they test.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The boot2deb root: this crate's manifest directory is `crates/engine`.
fn boot2deb_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/engine sits two levels below the boot2deb root")
        .to_path_buf()
}

/// Run the harness at `rel` (relative to the boot2deb root) with `/bin/sh` and fail
/// with its whole output unless it exits 0. A host with no `/bin/sh` cannot run a
/// first-boot hook at all, so the harness is skipped there rather than failed.
fn run_harness(rel: &str) {
    let sh = Path::new("/bin/sh");
    if !sh.exists() {
        eprintln!("skipping {rel}: /bin/sh is absent");
        return;
    }
    let out = Command::new(sh)
        .arg(rel)
        .current_dir(boot2deb_root())
        .output()
        .expect("spawn /bin/sh");
    assert!(
        out.status.success(),
        "{rel} failed ({}):\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The data-volume ladder never reaches `sfdisk` or `mkfs` without positive evidence
/// that the disk is blank, writable, whole and not under root.
#[test]
fn data_volume_hook_writes_only_a_proven_blank_disk() {
    run_harness("features/data-volume/test-ladder.sh");
}

/// The seed hook applies only values the press-time checks accept, and a network
/// profile the stack refuses leaves DHCP in effect.
#[test]
fn seed_hook_applies_only_valid_values() {
    run_harness("base/test-seed.sh");
}

/// The installer writes nothing until the payload matches its record and the target
/// is established as a whole, unused disk large enough for it, and it reports an
/// install done only once the disk reads back as the recorded image.
#[test]
fn install_to_reports_done_only_for_a_verified_disk() {
    run_harness("base/test-install-to.sh");
}

/// The seed hook's own copy of the press-time checks, as sh source: the section
/// from its `# --- The press-time checks` heading to the next `# --- ` heading.
fn seed_hook_checks() -> String {
    let path = boot2deb_root().join("base/overlay/etc/boot2deb/first-boot.d/10-seed");
    let hook = std::fs::read_to_string(&path).expect("read the seed hook");
    let start = hook
        .find("# --- The press-time checks")
        .expect("the seed hook has a press-time checks section");
    let len = hook[start + 1..]
        .find("\n# --- ")
        .expect("a further section follows the checks");
    hook[start..start + 1 + len].to_string()
}

/// Whether the sh check `func` accepts `value`, run the way the hook runs it.
fn sh_accepts(checks: &str, func: &str, value: &str) -> bool {
    let script = format!("LC_ALL=C\nexport LC_ALL\n{checks}\n{func} \"$1\"\n");
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(value)
        .status()
        .expect("spawn /bin/sh")
        .success()
}

/// A `seed.txt` edited by hand reaches the board without passing through `press`,
/// so the hook repeats press's checks in sh. Every value here must get the same
/// verdict from both copies. The set holds each core module's own test vectors and
/// the boundaries a shell reimplementation gets wrong: leading zeros, byte versus
/// character lengths, C1 controls, Unicode whitespace at an edge. Invalid UTF-8,
/// which no Rust string can hold, is `base/test-seed.sh`'s to cover.
#[test]
fn seed_hook_checks_agree_with_core() {
    if !Path::new("/bin/sh").exists() {
        eprintln!("skipping: /bin/sh is absent");
        return;
    }
    use boot2deb_core::{authkeys, hostname, staticip, wifi};
    let checks = seed_hook_checks();

    let long = |c: &str, n: usize| c.repeat(n);
    let hostnames: Vec<String> = [
        "turing-rk1",
        "h96-max-m9",
        "rk1",
        "a",
        "b0ard-1",
        "board.lan",
        "b0ard-1.example.com",
        "A-Z09",
        "",
        "-rk1",
        "rk1-",
        ".rk1",
        "rk1.",
        "board..lan",
        "rk1.-a",
        "rk1-.a",
        "rk_1",
        "a/b",
        "..",
        ".",
        "rk1\n10.0.0.1 deb.debian.org",
        "rk1 deb.debian.org",
        "rk1\tdeb.debian.org",
        "rk1\r",
        "rk1#comment",
        "rk1*",
        "\u{dc}berfunk",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        long("a", 63),
        long("a", 64),
        long("a", 65),
        format!("{}.{}", long("a", 31), long("b", 32)),
        format!("{}.b", long("a", 64)),
    ])
    .collect();

    let static_ips = [
        "192.168.1.50/24",
        "10.0.0.5/32",
        "192.168.1.50/24,192.168.1.1",
        "192.168.1.50/24,192.168.1.1,1.1.1.1",
        "192.168.1.50/24,192.168.1.1,1.1.1.1,9.9.9.9",
        "0.0.0.0/1",
        "255.255.255.255/32",
        "10.0.0.9/024",
        "10.0.0.9/00000000000000000024",
        "",
        "192.168.1.50",
        "192.168.1.50/",
        "192.168.1.50/0",
        "192.168.1.50/00",
        "192.168.1.50/33",
        "192.168.1.50/256",
        "192.168.1.50/+24",
        "192.168.1.50/-1",
        "192.168.1.50/24/5",
        "192.168.256.1/24",
        "01.2.3.4/24",
        "1.2.3.04/24",
        "1.2.3.0255/24",
        "1.2.3/24",
        "1.2.3.4.5/24",
        ".1.2.3/24",
        "1..2.3/24",
        "fe80::1/64",
        "192.168.1.50/24,",
        ",192.168.1.50/24",
        "192.168.1.50/24,,1.1.1.1",
        "192.168.1.50/24,gateway",
        "192.168.1.50/24,192.168.1.1,dns",
        "192.168.1.50/24,192.168.1.1,1.1.1",
        "192.168.1.50/24,192.168.1.1,1.1.1.1,",
        "192.168.1.50 /24",
        "192.168.1.50/24\n",
    ];

    let ssids: Vec<String> = [
        "lab net",
        "\u{dc}berfunk",
        "a\\b;c",
        "#hash",
        "",
        "two\nlines",
        "tab\there",
        " padded ",
        "lab\u{1b}net",
        "lab\u{7f}net",
        "lab\u{85}net",
        "lab\u{a0}",
        "\u{3000}lab",
        "lab\u{2028}",
        "mid\u{a0}dle",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        long("x", 32),
        long("x", 33),
        long("\u{dc}", 16),
        long("\u{dc}", 17),
    ])
    .collect();

    let psks: Vec<String> = [
        "hunter22",
        "pass phrase with spaces",
        "pa\\ss;wo#rd",
        "~!@#$%^&*()",
        "",
        "short",
        "hunter2",
        "emoji-\u{1F512}pass",
        "pass\u{e9}word",
        "two\nlines-long",
        "tab\tinside-psk",
        " padded-psk",
        "padded-psk ",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        long("x", 63),
        long("x", 64),
        long("x", 65),
        long("a1", 32),
        format!("{}g", long("a", 63)),
    ])
    .collect();

    let ed25519 =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe";
    let keys: Vec<String> = vec![
        format!("{ed25519} operator@workstation"),
        ed25519.to_string(),
        format!("{ed25519} a comment with words"),
        format!("{ed25519} Jos\u{e9}@laptop"),
        "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBI/Ik3VDAC6UR9cDWp9/6U4c0XaG3BGtEOykmO5zPZ2gWhf/ScyxCsiXVfmnmLTq//7Oeq6tT5aLaWbYwqDlHes= ops@bench".to_string(),
        "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDCn/jNTTR3aZH6Ur0fDUQ/P+S270GeyIbiWo+ymUr7avzfq0yquYxvuQdmeY38eQCR2/0A4yH00nM+ieBhzGYsm9pM6cnF5zFyN9+zVYlPxUJloFOATI3DrcMJ72Qr/yJTVq2P1lGXb4VXTbV+CRjFOmry+/RqrlDPpyAbCm2P0Q== backup@bench".to_string(),
        String::new(),
        "   ".to_string(),
        format!("  {ed25519}"),
        format!("{ed25519}\n"),
        format!("{ed25519}\n{ed25519}"),
        format!("{ed25519}\r"),
        format!("{ed25519} tab\there"),
        format!("{ed25519} esc\u{1b}[2J"),
        format!("{ed25519} c1\u{85}"),
        format!("{ed25519} nbsp\u{a0}"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----"
            .to_string(),
        "-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaA==".to_string(),
        "ssh-ed25519 AAAAB3NzaC1yc2EA".to_string(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1".to_string(),
        "ssh-ed25519 not-base64!!".to_string(),
        "ssh-ed25519 AAAA=AAA".to_string(),
        "ssh-ed25519 bQ===".to_string(),
        "ssh-ed25519".to_string(),
        "ssh-dss AAAAB3NzaC1kc3MAAACBAaaa".to_string(),
        format!("restrict {ed25519}"),
        "command=\"/bin/true\"".to_string(),
        format!("ssh-ed25519  {}", &ed25519["ssh-ed25519 ".len()..]),
        format!("ssh-rsa {}", &ed25519["ssh-ed25519 ".len()..]),
    ];

    let mut disagreements = Vec::new();
    let mut compare = |func: &str, value: &str, rust: bool| {
        if sh_accepts(&checks, func, value) != rust {
            disagreements.push(format!("{func} {value:?}: core says {rust}"));
        }
    };
    for v in &hostnames {
        compare("check_hostname", v, hostname::check(v).is_ok());
    }
    for v in static_ips {
        compare("check_static_ip", v, staticip::check(v).is_ok());
    }
    for v in &ssids {
        compare("check_ssid", v, wifi::check_ssid(v).is_ok());
    }
    for v in &psks {
        compare("check_psk", v, wifi::check_psk(v).is_ok());
    }
    for v in &keys {
        compare(
            "check_authorized_key",
            v,
            authkeys::check_authorized_key(v).is_ok(),
        );
    }
    assert!(
        disagreements.is_empty(),
        "the seed hook's checks disagree with core:\n{}",
        disagreements.join("\n")
    );
}
