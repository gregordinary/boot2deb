//! Render both SBOM formats from the *shipped* RK1 configuration and write them to
//! `target/sbom/`, where CI validates them against the published JSON schemas.
//!
//! An integration test rather than a unit one for two reasons. It composes only the
//! public API — resolve a recipe, read its lock and its committed solved manifest,
//! assemble a provenance manifest, render — which is the same path a build takes, so
//! a document that renders here is one a build would produce rather than one a
//! hand-written fixture allows. And the schema check needs the documents on disk, so
//! something has to write them.
//!
//! The unit tests beside each renderer assert the *content*: which fields carry what,
//! how identifiers are formed, what the relationship graph claims. This asserts the
//! documents exist, describe the real package set, and are valid JSON; the schema
//! conformance check is the CI step that reads what this writes.

use boot2deb_core::model::Overrides;
use boot2deb_core::provenance::{
    assemble, BuildFacts, FilesystemGeometry, FilesystemProvenance, SandboxPosture,
    SandboxProvenance, SandboxStreams,
};
use boot2deb_core::sbom::{cyclonedx, spdx, Sbom};
use boot2deb_core::{manifest, resolve_recipe, ConfigRoot};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The recipe rendered: the media-accel RK1, which is the build point that exercises
/// every optional section at once — a compiled kernel with a patch series, a compiled
/// u-boot, the media-accel source trees, and rkbin blobs.
const RECIPE: &str = "turing-rk1/media-accel-forky";

/// The config tree, two directories above this crate.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels under the config root")
        .to_path_buf()
}

/// Where the rendered documents land for the CI schema check. A fixed path under the
/// workspace's `target/` rather than `CARGO_TARGET_TMPDIR`, whose per-run name the
/// workflow could not address.
fn out_dir() -> PathBuf {
    let dir = repo_root().join("target/sbom");
    std::fs::create_dir_all(&dir).expect("create the output directory");
    dir
}

/// The build facts a render needs that no committed document carries — the shape of
/// the run rather than of the build point. Fixed values, so the output is stable.
fn facts<'a>(
    archives: &'a [boot2deb_core::provenance::ArchiveProvenance],
    verified: &'a [String],
    manifest_sha256: &'a str,
    package_count: usize,
) -> BuildFacts<'a> {
    BuildFacts {
        host_arch: "x86_64",
        cross: true,
        manifest_sha256,
        package_count,
        plan: "turing-rk1-media-accel-forky.plan",
        image_bytes: 8 << 30,
        plan_sha256: "0".repeat(64).leak(),
        archives,
        user: "debian",
        password: "not-a-real-password",
        builder_version: env!("CARGO_PKG_VERSION"),
        builder_commit: None,
        builder_dirty: false,
        config_commit: None,
        config_dirty: false,
        filesystem: FilesystemProvenance {
            kind: "ext4".into(),
            policy_pin: "ferrosys-policy-pin 1\nblock_size 4096\n".into(),
            reference_geometry_pin: "ferrosys-geometry-pin 1\nblock_size 4096\n".into(),
            geometry: FilesystemGeometry {
                block_size: 4096,
                total_blocks: 2_097_152,
                total_inodes: 524_288,
                blocks_per_group: 32768,
                inodes_per_group: 8192,
                group_count: 64,
                first_data_block: 0,
                flex_bg_size: 16,
                gdt_blocks: 1,
                reserved_gdt_blocks: 1023,
                inode_table_blocks: 512,
                reserved_blocks: 20971,
                max_grow_blocks: 2_147_483_648,
            },
        },
        rootfs_verified_with: verified,
        qemu: None,
        jobs: 8,
        sandbox: SandboxProvenance {
            posture: SandboxPosture {
                root: "rootless".into(),
                identity: "userns".into(),
                network: "denied".into(),
                streams: SandboxStreams {
                    stdin: "null".into(),
                    stdout: "inherit".into(),
                    stderr: "inherit".into(),
                },
                hardening: "seccomp".into(),
                seccomp_instructions: None,
                keep_capabilities: None,
                rlimits: Vec::new(),
                landlock_fs: Vec::new(),
                landlock_net: Vec::new(),
            },
            env: Default::default(),
            mounts: Vec::new(),
        },
        build_sandbox: None,
        cross_sandbox: None,
        packaging_root: None,
    }
}

#[test]
fn both_formats_render_from_the_shipped_configuration() {
    let root = ConfigRoot::new(repo_root());
    let build = resolve_recipe(&root, RECIPE, &Overrides::default()).expect("the recipe resolves");
    let lock = root.lock(RECIPE).expect("its lock is committed");

    // The committed solved manifest beside the lock — a real 200-package set, so the
    // documents are exercised against the versions and digests Debian actually serves
    // rather than against two invented rows.
    let pin = lock.rootfs.as_ref().expect("an image build pins a rootfs");
    let manifest_path = repo_root().join("recipes/turing-rk1").join(&pin.manifest);
    let text = std::fs::read_to_string(&manifest_path).expect("the manifest is committed");
    let packages = manifest::parse(&text, &manifest_path.display().to_string()).expect("it parses");
    assert!(
        packages.len() > 100,
        "the shipped manifest should be a real package set, got {}",
        packages.len()
    );

    let verified = vec!["ferrosys-scan".to_string()];
    let image = build
        .as_image()
        .expect("the shipped media-accel recipe builds an image");
    let provenance = assemble(
        image,
        &lock,
        &facts(
            &[],
            &verified,
            pin.manifest_sha256.as_deref().unwrap_or(&"0".repeat(64)),
            packages.len(),
        ),
    );
    // Source attribution comes from the published plan, which this test has no build to
    // produce — so the empty index is exercised here, which is also what an image handed
    // over without its plan produces. The populated case is a unit test beside the
    // model, where the assertion is about content rather than about the documents
    // existing.
    let sbom = Sbom::from_provenance(
        &provenance,
        &packages,
        &BTreeMap::new(),
        "turing-rk1-media-accel-forky",
        // Fixed, so re-running writes byte-identical documents — the property the
        // `SOURCE_DATE_EPOCH` path exists to give a real run.
        "2026-01-01T00:00:00Z",
    );

    // Every package reaches the document, plus the image, the source pins and the
    // blobs — a renderer that dropped a component would still produce valid JSON.
    assert!(sbom.components.len() > packages.len());

    let dir = out_dir();
    let spdx = serde_json::to_string_pretty(&spdx::Document::render(&sbom)).expect("SPDX renders");
    let cdx =
        serde_json::to_string_pretty(&cyclonedx::Bom::render(&sbom)).expect("CycloneDX renders");
    std::fs::write(dir.join("spdx.json"), &spdx).expect("write the SPDX document");
    std::fs::write(dir.join("cyclonedx.json"), &cdx).expect("write the CycloneDX document");

    // Both parse back as JSON, and each names the package set — the floor the CI
    // schema check builds on.
    for (name, text) in [("spdx", &spdx), ("cyclonedx", &cdx)] {
        let value: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
        assert!(
            text.contains("libc6"),
            "{name} should name the packages it describes"
        );
        assert!(value.is_object());
    }
}
