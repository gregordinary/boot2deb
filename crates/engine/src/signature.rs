//! Build signatures — a content hash of a node's resolved inputs, with
//! dependency signatures folded in, so the build graph is a Merkle DAG and its
//! edges are the cache-invalidation edges.
//!
//! **Tier 1.** Each built tree/artifact is stamped with its input
//! signature; on the reuse path the engine recomputes the signature and rebuilds
//! unless it matches. This replaces unsound directory-existence reuse — a reused
//! tree is otherwise never re-checked against the lock, so a changed pin/patch is
//! silently built on a stale checkout (COR-1). The bias is deliberate: a spurious
//! *miss* only wastes time, a spurious *hit* ships a stale artifact, so a node
//! folds every input that can change its tree and treats a missing/unreadable
//! stamp or any mismatch as "rebuild".
//!
//! **Diffable stamp.** The `<tree>.sig` stamp is a [`SignatureManifest`]: the rolled
//! signature *plus* the labeled input records that produced it. The rolled hash is
//! the reuse key ([`is_fresh`]); the retained records are what let `why-rebuild`
//! ([`crate::plan`]) explain a rebuild *in input terms* — "kernel.commit changed,
//! patches.commit +1" — rather than "the hash differs" (the payoff of
//! structure). [`SignatureManifest::diff`] computes that per-label delta.
//!
//! Pure: the folding + canonicalization is deterministic and unit-tested; the
//! on-disk stamp is the only I/O ([`read_manifest`] / [`write_manifest`]). The
//! hash's algebra mirrors the input's — [`SignatureBuilder::fold_ordered`] is
//! order-sensitive (patch series, last-wins fragments), [`fold_set`] is not (the
//! order-insensitive package union) — and every record is length-prefixed so
//! distinct inputs can never collide by concatenation.
//!
//! [`fold_set`]: SignatureBuilder::fold_set

use crate::error::EngineError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A node's build signature — the lowercase-hex sha256 of its canonicalized
/// resolved inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(String);

impl Signature {
    /// The full hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First 12 hex chars, for logging.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

/// One folded input, retained for explanation: a label and its canonical value(s)
/// (one value for a scalar, the ordered/sorted list for a fold). The values are
/// exactly what fed the hash, so a record diff is faithful to the signature diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The input's label (e.g. `kernel.commit`, `frags`).
    pub label: String,
    /// The canonical value(s) folded under `label`.
    pub values: Vec<String>,
}

/// Accumulates a node's inputs into a canonical byte stream (for the hash) and a
/// parallel list of labeled records (for explanation), then produces either the
/// bare [`Signature`] or the full [`SignatureManifest`].
///
/// Every `fold_*` writes length-prefixed, labeled records into the hash stream, so
/// `["a","b"]` and `["ab"]` — or a scalar `"ab"` — never produce the same stream.
/// Construct with the node name and its stage-recipe version (: a node's own
/// build logic is an input, so bump the version when the stage's logic changes to
/// force a rebuild), fold the resolved inputs, then [`finish`](Self::finish) (hash
/// only) or [`manifest`](Self::manifest) (hash + records).
pub struct SignatureBuilder {
    node: String,
    stage_version: u32,
    buf: Vec<u8>,
    records: Vec<Record>,
}

impl SignatureBuilder {
    /// Start a signature for `node` at stage-recipe version `stage_version`.
    pub fn new(node: &str, stage_version: u32) -> Self {
        let mut b = SignatureBuilder {
            node: node.to_string(),
            stage_version,
            buf: Vec::new(),
            records: Vec::new(),
        };
        b.write_field("node", node.as_bytes());
        b.write_field("stage_version", stage_version.to_string().as_bytes());
        b
    }

    /// Append one length-prefixed `(label, value)` record to the hash stream.
    /// Length-prefixing both halves is what makes the stream unambiguous under
    /// concatenation. Does **not** touch the record list — that is the caller's
    /// choice per fold kind (`node`/`stage_version` are hashed but not recorded).
    fn write_field(&mut self, label: &str, value: &[u8]) {
        self.buf.extend_from_slice(&(label.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(label.as_bytes());
        self.buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(value);
    }

    /// Fold a single scalar input (e.g. a pinned commit).
    pub fn fold_scalar(&mut self, label: &str, value: &str) -> &mut Self {
        self.write_field(label, value.as_bytes());
        self.records.push(Record {
            label: label.to_string(),
            values: vec![value.to_string()],
        });
        self
    }

    /// Fold an **order-sensitive** list: the count then each item, in order, so a
    /// reorder changes the signature (patch series, last-wins fragment merge).
    pub fn fold_ordered<S: AsRef<str>>(&mut self, label: &str, items: &[S]) -> &mut Self {
        self.write_field(label, (items.len() as u64).to_string().as_bytes());
        for it in items {
            self.write_field(label, it.as_ref().as_bytes());
        }
        self.records.push(Record {
            label: label.to_string(),
            values: items.iter().map(|i| i.as_ref().to_string()).collect(),
        });
        self
    }

    /// Fold an **order-insensitive** set: sorted + de-duplicated before folding, so
    /// a reorder or repeat leaves the signature unchanged (the package union).
    pub fn fold_set<S: AsRef<str>>(&mut self, label: &str, items: &[S]) -> &mut Self {
        let mut sorted: Vec<&str> = items.iter().map(|i| i.as_ref()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        self.fold_ordered(label, &sorted)
    }

    /// Fold a dependency node's signature in, making the graph a Merkle DAG.
    pub fn fold_dep(&mut self, dep: &Signature) -> &mut Self {
        self.write_field("dep", dep.0.as_bytes());
        self.records.push(Record {
            label: "dep".to_string(),
            values: vec![dep.0.clone()],
        });
        self
    }

    /// Hash the accumulated stream into the final [`Signature`].
    pub fn finish(&self) -> Signature {
        Signature(crate::blobs::sha256_hex(&self.buf))
    }

    /// The full [`SignatureManifest`]: the rolled [`Signature`] plus the retained
    /// input records, for stamping a tree so a later `why-rebuild` can diff it.
    pub fn manifest(&self) -> SignatureManifest {
        SignatureManifest {
            node: self.node.clone(),
            stage_version: self.stage_version,
            signature: self.finish().0,
            records: self.records.clone(),
        }
    }
}

/// A tree's stamped signature and the inputs that produced it — the on-disk
/// `<tree>.sig` document. The `signature` field is the reuse key; `records` is the
/// per-input breakdown `why-rebuild` diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureManifest {
    /// The build node this stamp belongs to (e.g. `kernel`, `userspace:mpp`).
    pub node: String,
    /// The stage-recipe version at stamp time; a bump alone changes
    /// `signature` with no record delta.
    pub stage_version: u32,
    /// The rolled sha256 hex — the reuse key.
    pub signature: String,
    /// The labeled input records that rolled into `signature`.
    pub records: Vec<Record>,
}

impl SignatureManifest {
    /// The rolled signature as a [`Signature`].
    pub fn signature(&self) -> Signature {
        Signature(self.signature.clone())
    }

    /// Whether two manifests roll to the same signature (the reuse test).
    pub fn matches(&self, other: &SignatureManifest) -> bool {
        self.signature == other.signature
    }

    /// The per-label change set from `old` to `new`. Records are grouped by
    /// label (aggregating any repeats), then compared: a label only in `new` is
    /// [`ChangeKind::Added`], only in `old` is [`ChangeKind::Removed`], and one whose
    /// values differ is [`ChangeKind::Changed`]. An empty result on differing
    /// signatures means only `node`/`stage_version` moved (a build-logic bump).
    /// Deterministic: labels are compared in sorted order.
    pub fn diff(old: &SignatureManifest, new: &SignatureManifest) -> Vec<RecordChange> {
        let group = |recs: &[Record]| -> BTreeMap<String, Vec<String>> {
            let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for r in recs {
                m.entry(r.label.clone()).or_default().extend(r.values.iter().cloned());
            }
            m
        };
        let o = group(&old.records);
        let n = group(&new.records);
        let mut labels: Vec<&String> = o.keys().chain(n.keys()).collect();
        labels.sort();
        labels.dedup();
        let mut changes = Vec::new();
        for label in labels {
            match (o.get(label), n.get(label)) {
                (Some(a), Some(b)) if a != b => changes.push(RecordChange {
                    label: label.clone(),
                    kind: ChangeKind::Changed,
                    old: a.clone(),
                    new: b.clone(),
                }),
                (Some(a), None) => changes.push(RecordChange {
                    label: label.clone(),
                    kind: ChangeKind::Removed,
                    old: a.clone(),
                    new: Vec::new(),
                }),
                (None, Some(b)) => changes.push(RecordChange {
                    label: label.clone(),
                    kind: ChangeKind::Added,
                    old: Vec::new(),
                    new: b.clone(),
                }),
                _ => {}
            }
        }
        changes
    }
}

/// How one labeled input differs between two [`SignatureManifest`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The label is present only in the new manifest.
    Added,
    /// The label is present only in the old manifest.
    Removed,
    /// The label is in both but its values differ.
    Changed,
}

/// One label's change between two manifests, with the old and new values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordChange {
    /// The input label that changed.
    pub label: String,
    /// The kind of change.
    pub kind: ChangeKind,
    /// The old value(s) (empty for [`ChangeKind::Added`]).
    pub old: Vec<String>,
    /// The new value(s) (empty for [`ChangeKind::Removed`]).
    pub new: Vec<String>,
}

impl RecordChange {
    /// A one-line human summary of the change, for `why-rebuild` output. A scalar
    /// change reads `label: old → new`; a list change reads the removed (`-`) and
    /// added (`+`) items; an add/remove names the values.
    pub fn summary(&self) -> String {
        match self.kind {
            ChangeKind::Changed if self.old.len() == 1 && self.new.len() == 1 => {
                format!("{}: {} → {}", self.label, self.old[0], self.new[0])
            }
            ChangeKind::Changed => {
                let removed: Vec<&String> = self.old.iter().filter(|v| !self.new.contains(v)).collect();
                let added: Vec<&String> = self.new.iter().filter(|v| !self.old.contains(v)).collect();
                let mut parts = Vec::new();
                for r in removed {
                    parts.push(format!("-{r}"));
                }
                for a in added {
                    parts.push(format!("+{a}"));
                }
                format!("{}: {}", self.label, parts.join(" "))
            }
            ChangeKind::Added => format!("{}: added {}", self.label, self.new.join(" ")),
            ChangeKind::Removed => format!("{}: removed {}", self.label, self.old.join(" ")),
        }
    }
}

/// The stamp path for a built tree: a `<tree>.sig` sidecar, kept *outside* the
/// tree so it never shows up as an untracked file in the git checkout (which would
/// disturb the patch-apply clean check).
pub fn stamp_path(tree: &Path) -> PathBuf {
    let mut s = tree.as_os_str().to_os_string();
    s.push(".sig");
    PathBuf::from(s)
}

/// Read a tree's stamped [`SignatureManifest`], or `None` if the stamp is absent,
/// unreadable, or not a current-format manifest — either way the caller rebuilds
/// (fail-safe). A stamp from an older format simply fails to parse and rebuilds.
pub fn read_manifest(tree: &Path) -> Option<SignatureManifest> {
    let text = std::fs::read_to_string(stamp_path(tree)).ok()?;
    toml::from_str(&text).ok()
}

/// Stamp a freshly built `tree` with its `manifest`, so the next run can check it
/// for reuse and `why-rebuild` can diff it.
pub fn write_manifest(tree: &Path, manifest: &SignatureManifest) -> Result<(), EngineError> {
    let p = stamp_path(tree);
    let text = toml::to_string(manifest).map_err(|e| EngineError::Io {
        path: p.display().to_string(),
        source: std::io::Error::other(e),
    })?;
    std::fs::write(&p, text).map_err(|s| EngineError::io(&p, s))
}

/// Whether a `tree` can be soundly reused: it exists **and** its stamp rolls to the
/// same signature as the freshly recomputed `expected` manifest. A missing tree,
/// missing/unparseable stamp, or any signature mismatch is `false` — the caller then
/// removes any stale tree and rebuilds.
pub fn is_fresh(tree: &Path, expected: &SignatureManifest) -> bool {
    tree.exists()
        && read_manifest(tree)
            .map(|m| m.signature == expected.signature)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(node: &str, f: impl FnOnce(&mut SignatureBuilder)) -> Signature {
        let mut b = SignatureBuilder::new(node, 1);
        f(&mut b);
        b.finish()
    }

    fn man(node: &str, f: impl FnOnce(&mut SignatureBuilder)) -> SignatureManifest {
        let mut b = SignatureBuilder::new(node, 1);
        f(&mut b);
        b.manifest()
    }

    #[test]
    fn same_inputs_same_signature() {
        let a = sig("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        let b = sig("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        assert_eq!(a, b);
        // 64 hex chars.
        assert_eq!(a.as_str().len(), 64);
    }

    #[test]
    fn any_input_change_changes_signature() {
        let base = sig("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        // Changed value.
        assert_ne!(base, sig("kernel", |b| {
            b.fold_scalar("commit", "abd");
        }));
        // Changed node.
        assert_ne!(base, sig("uboot", |b| {
            b.fold_scalar("commit", "abc");
        }));
        // Changed stage version.
        let bumped = {
            let mut b = SignatureBuilder::new("kernel", 2);
            b.fold_scalar("commit", "abc");
            b.finish()
        };
        assert_ne!(base, bumped);
    }

    #[test]
    fn ordered_is_order_sensitive_set_is_not() {
        let ab = sig("n", |b| {
            b.fold_ordered("frags", &["a", "b"]);
        });
        let ba = sig("n", |b| {
            b.fold_ordered("frags", &["b", "a"]);
        });
        assert_ne!(ab, ba, "ordered fold must be order-sensitive");

        let set_ab = sig("n", |b| {
            b.fold_set("pkgs", &["a", "b"]);
        });
        let set_ba = sig("n", |b| {
            b.fold_set("pkgs", &["b", "a", "a"]);
        });
        assert_eq!(set_ab, set_ba, "set fold must ignore order and repeats");
    }

    #[test]
    fn framing_prevents_concatenation_collisions() {
        // ["a","b"] must not collide with ["ab"] or the scalar "ab".
        let split = sig("n", |b| {
            b.fold_ordered("x", &["a", "b"]);
        });
        let joined = sig("n", |b| {
            b.fold_ordered("x", &["ab"]);
        });
        let scalar = sig("n", |b| {
            b.fold_scalar("x", "ab");
        });
        assert_ne!(split, joined);
        assert_ne!(split, scalar);
        assert_ne!(joined, scalar);
    }

    #[test]
    fn dep_folding_changes_signature() {
        let dep = sig("uboot", |b| {
            b.fold_scalar("commit", "u1");
        });
        let without = sig("image", |b| {
            b.fold_scalar("k", "v");
        });
        let with = sig("image", |b| {
            b.fold_scalar("k", "v");
            b.fold_dep(&dep);
        });
        assert_ne!(without, with);
    }

    #[test]
    fn stamp_round_trips_and_gates_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("linux");
        std::fs::create_dir_all(&tree).unwrap();
        let m = man("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        // No stamp yet → not fresh.
        assert!(!is_fresh(&tree, &m));
        write_manifest(&tree, &m).unwrap();
        // The stamp round-trips to an equal manifest (signature + records).
        assert_eq!(read_manifest(&tree).as_ref(), Some(&m));
        assert!(is_fresh(&tree, &m));
        // A different expected signature (inputs changed) → stale.
        let other = man("kernel", |b| {
            b.fold_scalar("commit", "xyz");
        });
        assert!(!is_fresh(&tree, &other));
        // Stamp sits beside the tree, not inside it (keeps the git tree clean).
        assert_eq!(stamp_path(&tree), tmp.path().join("linux.sig"));
        assert!(!tree.join(".sig").exists());
    }

    #[test]
    fn missing_tree_is_never_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("gone");
        let m = man("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        // Even with a stray stamp, a missing tree is not reusable.
        write_manifest(&tree, &m).unwrap();
        assert!(!is_fresh(&tree, &m));
    }

    #[test]
    fn unparseable_stamp_is_ignored() {
        // A stamp from an older/foreign format (e.g. a bare hex line) does not parse
        // as a manifest, so the tree is treated as unstamped → rebuild (fail-safe).
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("linux");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(stamp_path(&tree), "deadbeef\n").unwrap();
        assert!(read_manifest(&tree).is_none());
        let m = man("kernel", |b| {
            b.fold_scalar("commit", "abc");
        });
        assert!(!is_fresh(&tree, &m));
    }

    #[test]
    fn diff_reports_changed_added_and_removed_labels() {
        let old = man("kernel", |b| {
            b.fold_scalar("kernel.commit", "aaa");
            b.fold_scalar("patches.commit", "p1");
            b.fold_ordered("frags", &["base", "soc"]);
        });
        let new = man("kernel", |b| {
            b.fold_scalar("kernel.commit", "bbb"); // changed
            // patches.commit removed
            b.fold_ordered("frags", &["base", "soc", "accel"]); // list grew
            b.fold_scalar("patches_dev", "1"); // added
        });
        let changes = SignatureManifest::diff(&old, &new);
        // Sorted by label: frags, kernel.commit, patches.commit, patches_dev.
        let by: std::collections::HashMap<_, _> =
            changes.iter().map(|c| (c.label.as_str(), c)).collect();

        let kc = by["kernel.commit"];
        assert_eq!(kc.kind, ChangeKind::Changed);
        assert_eq!(kc.summary(), "kernel.commit: aaa → bbb");

        let pc = by["patches.commit"];
        assert_eq!(pc.kind, ChangeKind::Removed);

        let dev = by["patches_dev"];
        assert_eq!(dev.kind, ChangeKind::Added);

        let frags = by["frags"];
        assert_eq!(frags.kind, ChangeKind::Changed);
        assert_eq!(frags.summary(), "frags: +accel");
    }

    #[test]
    fn diff_is_empty_when_only_stage_version_moves() {
        // Same folded inputs, different stage-recipe version: the signatures differ
        // (a build-logic bump), but there is no input-record delta to report.
        let mut a = SignatureBuilder::new("kernel", 1);
        a.fold_scalar("commit", "abc");
        let mut b = SignatureBuilder::new("kernel", 2);
        b.fold_scalar("commit", "abc");
        let (ma, mb) = (a.manifest(), b.manifest());
        assert_ne!(ma.signature, mb.signature);
        assert!(SignatureManifest::diff(&ma, &mb).is_empty());
    }
}
