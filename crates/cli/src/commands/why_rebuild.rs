//! `why-rebuild`: explain, per compile node, what the next `build` will actually
//! redo — whether it reuses or rebuilds the cached source tree, which pinned inputs
//! changed if it will rebuild, and whether the durable artifact cache lets it skip
//! the compile entirely. Offline: reads the lock, the on-disk build stamps, and the
//! artifact store's directory listing; runs no build, touches no network or hardware.

use crate::args::WhyRebuildArgs;
use crate::config::{device_dts_paths, fragment_paths, kmod_local_patches};
use crate::fsutil::absolutize;
use crate::workdir::work_dir_for;
use boot2deb_core::lock::SnapshotMode;
use boot2deb_core::model::Overrides;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::build::BuildEnv;
use boot2deb_engine::plan::{self, ArtifactStatus};

/// Run `why-rebuild <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: WhyRebuildArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let lock = root.lock(recipe)?;
    // The kernel tree signature folds the board's device-tree sources, so the
    // prediction resolves the recipe to find them — an edited board `.dts` must be
    // reported as a rebuild, not a reuse.
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let device_dts = device_dts_paths(root, &build)?;
    // The kernel's *output* signature folds each kconfig fragment's content, so an
    // edited fragment is an artifact-cache miss even with the tree untouched.
    let fragments = fragment_paths(root, &build)?;
    // The kmod tree signatures fold the board's local compat patches, so resolve them
    // too — an edited shim must be reported as a rebuild, not a reuse.
    let kmod_local_patches = kmod_local_patches(root, &build)?;
    let work_dir = work_dir_for(root, recipe, args.work_dir);

    // Reconstruct the same `BuildEnv` a build assembles, because the output signatures
    // fold it. Every root's identity is *derived* from the arch, suite and mirror list
    // rather than probed, so the prediction needs nothing provisioned and stays offline;
    // the only probe left is the `qemu-user` interpreter, which is a read of the kernel's
    // binfmt registration and a digest of the file it names — no network.
    let pf = boot2deb_engine::preflight(build.arch);
    // An unmodelled host arch names no root, so it derives no identity; the prediction
    // then reports rebuilds it cannot rule out, which is the fail-safe direction, and
    // the build itself refuses such a host outright.
    let host_deb_arch = pf.host.debian_arch();
    let cross_compile =
        (host_deb_arch != Some(build.arch.debian_arch())).then(|| build.cross_compile.clone());
    let toolchain = boot2deb_engine::toolchain::HostToolchain::probe(
        pf.interpreter.then(|| build.arch.debian_arch()),
    );
    // The mirror list keys the sandbox identity; it is the lock's captured snapshot
    // mode, exactly as a build with no `--snapshot` would resolve it.
    let mirrors = boot2deb_engine::snapshot::resolve_mirrors(
        boot2deb_engine::DEFAULT_MIRROR,
        lock.snapshot.as_ref(),
        lock.snapshot
            .as_ref()
            .map(|s| s.mode)
            .unwrap_or(SnapshotMode::Off),
    )?;
    let env = BuildEnv {
        // The cross root's identity — what compiles the kernel, u-boot and modules.
        toolchain_id: host_deb_arch
            .map(|arch| {
                boot2deb_engine::build::cross_identity(
                    arch,
                    build.arch.debian_arch(),
                    &build.packaging_suite,
                    &mirrors,
                )
            })
            .unwrap_or_default(),
        // The target-arch sandbox's identity. Empty where the build resolves no suite
        // and so stands up none, matching what the build itself would compose.
        sandbox_id: build.suite.as_deref().map_or_else(String::new, |suite| {
            boot2deb_engine::build::sandbox_identity(
                build.arch.debian_arch(),
                suite,
                &mirrors,
                &toolchain,
            )
        }),
        // The packaging root's identity.
        packaging_id: host_deb_arch
            .map(|arch| {
                boot2deb_engine::build::packaging_identity(arch, &build.packaging_suite, &mirrors)
            })
            .unwrap_or_default(),
        cross_compile,
        // Not folded into any signature (a build whose output depends on its job
        // count has a bug), so the prediction need not know the build's `--jobs`.
        jobs: None,
    };
    let artifact_store =
        (!args.no_artifact_cache).then(|| absolutize(root.path().join("cache").join("artifacts")));

    let nodes = plan::plan_nodes(&plan::PlanInputs {
        lock: &lock,
        work_dir: &work_dir,
        patches_dev: args.patches_path.is_some(),
        // Co-dev predictions fold the live-series fingerprint, so pass the checkout
        // the build reads its patches from; `None` in pinned mode.
        patches_root: args.patches_path.as_deref(),
        include_libmali: args.build_libmali,
        device_dts: &device_dts,
        device_kmods: &build.device_kmods,
        kmod_local_patches: &kmod_local_patches,
        build: &build,
        env: &env,
        fragments: &fragments,
        artifact_store: artifact_store.as_deref(),
    });

    println!("why-rebuild {recipe} (work {})", work_dir.display());
    // The builder leads, because it is the one input that governs every node at once
    // and the only one a build will refuse to proceed on. It is not a compile node —
    // nothing here is cached or restored — so it sits above the list rather than in it.
    if let Some(row) = builder_row(root) {
        println!("{row}");
    }
    // A recipe can legitimately have no compile nodes at all — a board that installs
    // Debian's kernel and boots its own firmware compiles nothing, so there is nothing
    // to rebuild. Say that, rather than printing an empty list that reads as a bug.
    if nodes.is_empty() {
        println!(
            "  this recipe compiles nothing from source (the kernel is a distro package \
             and the boot method builds no bootloader), so it has no compile nodes to \
             reuse or rebuild. Its rootfs is keyed on the live package solve."
        );
        return Ok(());
    }
    for node in &nodes {
        let (verb, reason) = match &node.status {
            plan::NodeStatus::Absent => ("build", "no previous build".to_string()),
            plan::NodeStatus::Unstamped => ("rebuild", "tree present but not stamped".to_string()),
            plan::NodeStatus::Reuse => ("reuse", String::new()),
            plan::NodeStatus::Rebuild(changes) if changes.is_empty() => {
                ("rebuild", "build logic changed".to_string())
            }
            plan::NodeStatus::Rebuild(changes) => (
                "rebuild",
                changes
                    .iter()
                    .map(|c| c.summary())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        };
        // The artifact verdict leads the line when it is a hit, because it overrides
        // everything else about the node: the tree decision costs a clone, the compile
        // it skips costs tens of minutes.
        let tier2 = match node.artifact {
            ArtifactStatus::Hit => "  [artifact cache hit — compile skipped]",
            ArtifactStatus::Miss => "",
            ArtifactStatus::Disabled => "",
            ArtifactStatus::Unknown => "  [artifact cache: not predictable offline]",
        };
        if reason.is_empty() {
            println!("  {:<18} {verb}{tier2}", node.node);
        } else {
            println!("  {:<18} {verb}  ({reason}){tier2}", node.node);
        }
    }
    println!("{}", scope_note(&nodes, args.no_artifact_cache));
    Ok(())
}

/// The `builder` row, or `None` where there is no comparison to report — an installed
/// binary run against a config tree raises no question, and a row saying so on every
/// invocation would be noise in a listing whose job is to name what changed.
///
/// Phrased in this command's own vocabulary: a stale builder *blocks*, where a node
/// merely rebuilds. Saying "rebuild" here would read as "recompiles the kernel", which
/// is the one thing it does not mean.
fn builder_row(root: &ConfigRoot) -> Option<String> {
    let freshness = crate::builder::freshness(root);
    let note = freshness.note()?;
    let verb = if freshness.is_stale() {
        "blocks"
    } else {
        "warns"
    };
    Some(format!("  {:<18} {verb}  ({note})", "builder"))
}

/// The closing scope note: what each verdict above does and does not cover.
///
/// The two caches answer different questions, and conflating them misleads in the
/// expensive direction — a node can rebuild its tree and still compile nothing. So
/// the note states what the tree verdict governs, what the artifact cache governs,
/// and the one node neither predicts.
///
/// Pure, so it is unit-testable.
fn scope_note(nodes: &[plan::NodePlan], cache_disabled: bool) -> String {
    let mut s = String::from(
        "note: the per-node verdict is the *source tree*: whether the clone and patch \
         run again.\n      The compile itself is governed by the artifact cache",
    );
    if cache_disabled {
        s.push_str(
            ", which --no-artifact-cache\n      turns off — so every node above \
             recompiles. Drop the flag to see what would be restored.",
        );
    } else {
        let hits = nodes
            .iter()
            .filter(|n| n.artifact == ArtifactStatus::Hit)
            .count();
        s.push_str(&format!(
            " (<root>/cache/artifacts, keyed on\n      each node's full inputs): \
             {hits} of {} node(s) would restore stored `.deb`s and skip\n      \
             compiling. That store is shared across work dirs and survives `clean`.",
            nodes.len()
        ));
    }
    s.push_str(
        "\n      The rootfs is predicted by neither: its cache keys on the live package solve.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_engine::plan::{NodePlan, NodeStatus};
    use std::path::PathBuf;

    fn node(name: &str, artifact: ArtifactStatus) -> NodePlan {
        NodePlan {
            node: name.into(),
            tree: PathBuf::from("/w").join(name),
            status: NodeStatus::Reuse,
            artifact,
        }
    }

    #[test]
    fn the_scope_note_counts_the_compiles_the_cache_would_skip() {
        let nodes = [
            node("kernel", ArtifactStatus::Hit),
            node("uboot", ArtifactStatus::Miss),
        ];
        let s = scope_note(&nodes, false);
        assert!(s.contains("1 of 2 node(s) would restore"), "{s}");
        // And it never repeats the claim the review found wrong — that the compile
        // step always re-runs.
        assert!(!s.contains("always re-runs"), "{s}");
    }

    #[test]
    fn disabling_the_cache_is_reported_as_the_reason_nothing_restores() {
        let s = scope_note(&[node("kernel", ArtifactStatus::Disabled)], true);
        assert!(s.contains("--no-artifact-cache"), "{s}");
        assert!(s.contains("every node above recompiles"), "{s}");
        assert!(!s.contains("would restore"), "{s}");
    }
}
