//! `why-rebuild`: explain, per compile node, what the next `build` will actually redo:
//!
//! - Whether it reuses or rebuilds the cached source tree.
//! - Which pinned inputs changed, if it will rebuild.
//! - Whether the durable artifact cache lets it skip the compile entirely.
//!
//! Offline. It reads the lock, the on-disk build stamps, and the artifact store's
//! directory listing. It runs no build, and touches no network or hardware.

use crate::args::WhyRebuildArgs;
use crate::config::{device_dts_paths, fragment_paths, kmod_local_patches};
use crate::workdir::work_dir_for;
use boot2deb_core::lock::{Lock, SnapshotMode};
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::build::BuildEnv;
use boot2deb_engine::plan::{self, ArtifactStatus, NodePlan, NodeStatus};
use serde_json::json;
use std::path::PathBuf;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The build a prediction is about: the choices a `build` run makes that move the
/// compile nodes' keys, beyond what the lock and the resolution already fix.
pub(crate) struct PredictOptions {
    /// The work dir whose stamped trees the Tier-1 verdict reads.
    pub(crate) work_dir: PathBuf,
    /// The optional userspace trees the build asks for (`--userspace`).
    pub(crate) userspace: Vec<String>,
    /// The co-dev `--patches-path` checkout, when the build uses one.
    pub(crate) patches_path: Option<PathBuf>,
    /// The artifact store the build restores from, or `None` under
    /// `--no-artifact-cache`.
    pub(crate) artifact_store: Option<PathBuf>,
}

/// Predict both cache decisions for every compile node of one build of `lock`.
///
/// The one place a prediction is assembled. `why-rebuild` explains its answer, and
/// `clean --artifacts` reads the store keys off it to decide which entries some lock
/// still reaches. Both therefore ask exactly the question a `build` asks, under the
/// same environment it composes.
///
/// # Errors
///
/// A build input the prediction reads that cannot be resolved: a missing fragment,
/// `device_dts` source or kmod local patch, an `--userspace` name the SoC does not
/// declare, or a snapshot pin the lock records in a form no mirror list derives from.
pub(crate) fn predict(
    root: &ConfigRoot,
    lock: &Lock,
    build: &ResolvedBuild,
    opts: &PredictOptions,
) -> Result<Vec<NodePlan>> {
    // The kernel tree signature folds the board's device-tree sources, so the
    // prediction resolves them — an edited board `.dts` must be reported as a rebuild,
    // not a reuse.
    let device_dts = device_dts_paths(root, build)?;
    // The kernel's *output* signature folds each kconfig fragment's content, so an
    // edited fragment is an artifact-cache miss even with the tree untouched.
    let fragments = fragment_paths(root, build)?;
    // The kmod tree signatures fold the board's local compat patches, so resolve them
    // too — an edited shim must be reported as a rebuild, not a reuse.
    let kmod_local_patches = kmod_local_patches(root, build)?;
    let env = build_env(lock, build)?;
    let pf = boot2deb_engine::preflight(build.arch);
    let host_deb_arch = pf.host.debian_arch();

    // The same narrowing `build` applies: an optional tree changes what the whole
    // userspace stage layers, so a prediction under a different set answers a question
    // about nodes that build would not run.
    let userspace = crate::config::enabled_userspace(
        build
            .image
            .as_ref()
            .map(|i| i.userspace.as_slice())
            .unwrap_or(&[]),
        &opts.userspace,
    )?;
    Ok(plan::plan_nodes(&plan::PlanInputs {
        lock,
        work_dir: &opts.work_dir,
        patches_dev: opts.patches_path.is_some(),
        // Co-dev predictions fold the live-series fingerprint, so pass the checkout
        // the build reads its patches from; `None` in pinned mode.
        patches_root: opts.patches_path.as_deref(),
        userspace: &userspace,
        device_dts: &device_dts,
        device_kmods: build
            .image
            .as_ref()
            .map(|i| i.device_kmods.as_slice())
            .unwrap_or(&[]),
        kmod_local_patches: &kmod_local_patches,
        build,
        env: &env,
        fragments: &fragments,
        artifact_store: opts.artifact_store.as_deref(),
        // An app's key folds the SDK tarball this host runs. A host with no Debian
        // name has no tarball either, and no app node matches then, as for a build.
        host_arch: host_deb_arch.unwrap_or_default(),
    }))
}

/// Reconstruct the [`BuildEnv`] a build of `lock` assembles, because the output
/// signatures fold it.
///
/// Every root's identity is *derived* from the arch, suite and mirror list rather
/// than probed, so the prediction needs nothing provisioned and stays offline. The
/// only probe left is the `qemu-user` interpreter, which is a read of the kernel's
/// binfmt registration and a digest of the file it names — no network.
fn build_env(lock: &Lock, build: &ResolvedBuild) -> Result<BuildEnv> {
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
    // The archive keys the sandbox identity; it is the lock's captured snapshot mode,
    // exactly as a build with no `--snapshot` would resolve it.
    let archive = boot2deb_engine::Archive::resolve(
        &build.distro,
        lock.snapshot.as_ref(),
        lock.snapshot
            .as_ref()
            .map(|s| s.mode)
            .unwrap_or(SnapshotMode::Off),
    )?;
    Ok(BuildEnv {
        // The cross root's identity — what compiles the kernel, u-boot and modules.
        toolchain_id: host_deb_arch
            .map(|arch| {
                boot2deb_engine::build::cross_identity(arch, build.arch.debian_arch(), &archive)
            })
            .unwrap_or_default(),
        // The target-arch sandbox's identity. Empty where the build resolves no suite
        // and so stands up none, matching what the build itself would compose.
        sandbox_id: build.image.as_ref().map_or_else(String::new, |_| {
            boot2deb_engine::build::sandbox_identity(build.arch.debian_arch(), &archive, &toolchain)
        }),
        // The packaging root's identity.
        packaging_id: host_deb_arch
            .map(|arch| boot2deb_engine::build::packaging_identity(arch, &archive))
            .unwrap_or_default(),
        cross_compile,
        // Not folded into any signature (a build whose output depends on its job
        // count has a bug), so the prediction need not know the build's `--jobs`.
        jobs: None,
    })
}

/// Run `why-rebuild <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: WhyRebuildArgs,
    json_out: bool,
) -> Result<()> {
    let lock = root.lock(recipe)?;
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let work_dir = work_dir_for(root, recipe, args.work_dir);
    let nodes = predict(
        root,
        &lock,
        &build,
        &PredictOptions {
            work_dir: work_dir.clone(),
            userspace: args.userspace,
            patches_path: args.patches_path,
            artifact_store: (!args.no_artifact_cache).then(|| crate::config::artifact_cache(root)),
        },
    )?;
    if json_out {
        return print_json(root, recipe, &work_dir, &nodes);
    }

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
        let verb = tree_verb(&node.status);
        let reason = match &node.status {
            NodeStatus::Absent => "no previous build".to_string(),
            NodeStatus::Unstamped => "tree present but not stamped".to_string(),
            NodeStatus::Reuse => String::new(),
            NodeStatus::Rebuild(changes) if changes.is_empty() => "build logic changed".to_string(),
            NodeStatus::Rebuild(changes) => changes
                .iter()
                .map(|c| c.summary())
                .collect::<Vec<_>>()
                .join(", "),
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

/// The verb a node's tree verdict reads as: what the next `build` does with the tree.
/// Shared by the table and the JSON form, so the two cannot name one verdict two ways.
fn tree_verb(status: &NodeStatus) -> &'static str {
    match status {
        NodeStatus::Absent => "build",
        NodeStatus::Unstamped | NodeStatus::Rebuild(_) => "rebuild",
        NodeStatus::Reuse => "reuse",
    }
}

/// The artifact-cache verdict's token in the JSON form.
fn artifact_token(status: ArtifactStatus) -> &'static str {
    match status {
        ArtifactStatus::Hit => "hit",
        ArtifactStatus::Miss => "miss",
        ArtifactStatus::Disabled => "disabled",
        ArtifactStatus::Unknown => "unknown",
    }
}

/// One node as a JSON object: both verdicts, what moved, and the store keys the
/// node's restore reads. The keys are what a caller compares against the store's own
/// listing, so they are given whole rather than shortened.
fn node_json(node: &NodePlan) -> serde_json::Value {
    let changes: Vec<String> = match &node.status {
        NodeStatus::Rebuild(changes) => changes.iter().map(|c| c.summary()).collect(),
        _ => Vec::new(),
    };
    json!({
        "node": node.node,
        "tree": node.tree,
        "tree_status": tree_verb(&node.status),
        "unstamped": matches!(node.status, NodeStatus::Unstamped),
        "changes": changes,
        "artifact": artifact_token(node.artifact),
        "keys": node.keys.as_ref().map(|keys| {
            keys.iter()
                .map(|k| json!({"node": k.node, "signature": k.signature}))
                .collect::<Vec<_>>()
        }),
    })
}

/// The `--json` document: the builder verdict, then every node in build order. An
/// empty `nodes` array is the answer for a recipe that compiles nothing.
fn print_json(
    root: &ConfigRoot,
    recipe: &str,
    work_dir: &std::path::Path,
    nodes: &[NodePlan],
) -> Result<()> {
    let freshness = crate::builder::freshness(root);
    let builder = freshness.note().map(|note| {
        json!({
            "verdict": if freshness.is_stale() { "blocks" } else { "warns" },
            "note": note,
        })
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "recipe": recipe,
            "work_dir": work_dir,
            "builder": builder,
            "nodes": nodes.iter().map(node_json).collect::<Vec<_>>(),
        }))?
    );
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
fn scope_note(nodes: &[NodePlan], cache_disabled: bool) -> String {
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
             compiling. That store is shared across work dirs; `boot2deb clean\n      \
             --artifacts` prunes it to what the locks still reach.",
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

    fn node(name: &str, artifact: ArtifactStatus) -> NodePlan {
        NodePlan {
            node: name.into(),
            tree: PathBuf::from("/w").join(name),
            status: NodeStatus::Reuse,
            artifact,
            keys: None,
        }
    }

    #[test]
    fn the_json_node_contains_both_verdicts_and_the_full_store_keys() {
        let mut n = node("kmod:aic8800", ArtifactStatus::Miss);
        n.status = NodeStatus::Unstamped;
        n.keys = Some(vec![
            boot2deb_engine::artstore::StoreKey {
                node: "kmod:aic8800".into(),
                signature: "a".repeat(64),
            },
            boot2deb_engine::artstore::StoreKey {
                node: "kmod-fw:aic8800".into(),
                signature: "b".repeat(64),
            },
        ]);
        let v = node_json(&n);
        assert_eq!(v["tree_status"], "rebuild");
        assert_eq!(v["unstamped"], true);
        assert_eq!(v["artifact"], "miss");
        // Whole signatures: a caller matches them against the store's directory names.
        assert_eq!(v["keys"][1]["node"], "kmod-fw:aic8800");
        assert_eq!(v["keys"][1]["signature"], "b".repeat(64));
        // A key that could not be computed is null, not an empty list: "no keys" and
        // "unknown keys" mean different things to a store sweep.
        assert!(node_json(&node("ffmpeg", ArtifactStatus::Unknown))["keys"].is_null());
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
