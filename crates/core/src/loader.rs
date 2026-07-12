//! Reads typed config layers from a boot2deb config root (the repo directory
//! holding `devices/`, `socs/`, `arches/`, `boot-methods/`, `kernels/`,
//! `recipes/`).
//!
//! **Layer search path & overlays.** A [`ConfigRoot`] holds an *ordered*
//! search path: the shipped root first, then zero or more out-of-tree overlay
//! directories (`--overlay <dir>`), later ones winning. A layer file present only
//! in an overlay adds a new target; a file present under a shipped name is
//! **deep-merged last-wins** over the shipped one — tables merge key-by-key with
//! the overlay winning, while a scalar or array key is replaced wholesale (the
//! simplest predictable last-wins). Each layer file is parsed to a
//! [`toml::Value`], the values are merged across the path, and the merged value is
//! deserialized into the strict `deny_unknown_fields` struct, so validation is
//! unchanged and the authored structs stay untouched. This lets a user retune one
//! device's `image_size` or add a `supported_kernel` — or drop in a whole new
//! device/soc/kernel — without forking the vendored config.

use crate::error::ConfigError;
use crate::model::*;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// A boot2deb config root — an ordered search path of directories, each holding
/// the config-layer subtrees. Lookups walk the path; overlays (later entries) win
/// over the shipped root (first entry). Tests and alternate checkouts just point
/// at a different path.
pub struct ConfigRoot {
    /// Low→high precedence: `roots[0]` is the shipped/primary root; later entries
    /// are overlays that win on merge and on single-file lookup.
    roots: Vec<PathBuf>,
}

impl ConfigRoot {
    /// Wrap a single directory as a config root (no overlays). Does not touch the
    /// filesystem; missing files surface as [`ConfigError::NotFound`] on lookup.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { roots: vec![root.into()] }
    }

    /// A primary (shipped) root plus ordered overlay directories. Overlays
    /// are listed low→high: a later overlay wins over an earlier one, and any
    /// overlay wins over the primary root.
    ///
    /// Each overlay must be an existing directory. An empty path would silently
    /// resolve every asset against the process's current directory, and a mistyped
    /// one would shadow nothing at all — in both cases the build proceeds against a
    /// config tree the operator did not intend, which is precisely the failure an
    /// overlay exists to make explicit. Both are [`ConfigError::InvalidOverlay`].
    pub fn with_overlays(
        root: impl Into<PathBuf>,
        overlays: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, ConfigError> {
        let mut roots = vec![root.into()];
        for overlay in overlays {
            let why = if overlay.as_os_str().is_empty() {
                Some("the path is empty")
            } else if !overlay.exists() {
                Some("no such directory")
            } else if !overlay.is_dir() {
                Some("not a directory")
            } else {
                None
            };
            if let Some(why) = why {
                return Err(ConfigError::InvalidOverlay {
                    path: overlay.display().to_string(),
                    why,
                });
            }
            roots.push(overlay);
        }
        Ok(Self { roots })
    }

    /// The primary (shipped) root — the base of the search path. Non-config assets
    /// resolved by direct join (blobs, fragments, overlay trees) start here; use
    /// [`find_asset`](Self::find_asset) to make those overlay-aware.
    pub fn path(&self) -> &Path {
        &self.roots[0]
    }

    /// The full search path — the primary root followed by every overlay, in
    /// low→high precedence order. For a consumer that must confirm an asset it
    /// resolved stays *within* the config tree (containment), not just find it.
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The highest-precedence existing path for a repo-relative asset (a fragment,
    /// blob, or overlay tree that is *not* a merged config layer), or `None` if no
    /// root has it. Searched high→low so an overlay's copy shadows the shipped one.
    pub fn find_asset(&self, rel: impl AsRef<Path>) -> Option<PathBuf> {
        let rel = rel.as_ref();
        self.roots
            .iter()
            .rev()
            .map(|r| r.join(rel))
            .find(|p| p.exists())
    }

    /// Resolve a *trust anchor* asset (the Debian archive keyring) that overlays
    /// must not be able to swap (TRUST-1). Unlike [`find_asset`](Self::find_asset),
    /// which lets the highest-precedence overlay win, this resolves from the primary
    /// (shipped) root only and treats an overlay copy as a fail-closed error:
    ///  - `Ok(Some(path))` — the shipped root's copy (no overlay ships one, or
    ///    `allow_overlay` is set — the explicit `--unsafe-overlay-keyring` opt-in,
    ///    in which case the highest-precedence copy wins like `find_asset`).
    ///  - `Ok(None)` — no root has it (the caller falls back to the host trust store).
    ///  - `Err(OverlayTrustAnchor)` — an overlay ships the asset and `allow_overlay`
    ///    is false: a swap attempt, refused rather than silently trusted.
    pub fn find_trust_anchor(
        &self,
        rel: impl AsRef<Path>,
        allow_overlay: bool,
    ) -> Result<Option<PathBuf>, ConfigError> {
        let rel = rel.as_ref();
        if allow_overlay {
            // Opted into the overlay explicitly: highest-precedence copy wins.
            return Ok(self.find_asset(rel));
        }
        // An overlay (any non-primary root) shipping the anchor is a swap attempt.
        if self.roots[1..].iter().any(|r| r.join(rel).exists()) {
            return Err(ConfigError::OverlayTrustAnchor {
                asset: rel.display().to_string(),
            });
        }
        let shipped = self.roots[0].join(rel);
        Ok(shipped.exists().then_some(shipped))
    }

    /// Every existing path for a repo-relative asset across the search path, in
    /// low→high precedence order — for assets that *stack* rather than shadow (a
    /// feature/layer overlay tree present in both the shipped root and an overlay,
    /// merged shipped-first so the overlay wins the last-writer semantics).
    pub fn find_asset_all(&self, rel: impl AsRef<Path>) -> Vec<PathBuf> {
        let rel = rel.as_ref();
        self.roots
            .iter()
            .map(|r| r.join(rel))
            .filter(|p| p.exists())
            .collect()
    }

    /// The root that *owns* a layer file — the highest-precedence root containing
    /// `<subdir>/<name>.toml`, or the primary root if none does. A write target
    /// derived from a layer (a recipe's lock) lands beside the file it belongs to,
    /// so `update` on an overlay recipe writes into that overlay, not the shipped
    /// tree.
    fn owning_root(&self, subdir: &str, name: &str) -> &Path {
        let rel = format!("{subdir}/{name}.toml");
        self.roots
            .iter()
            .rev()
            .find(|r| r.join(&rel).exists())
            .unwrap_or(&self.roots[0])
    }

    /// Read a config file to a string. A missing file is `Ok(None)` (the caller
    /// walks the rest of the search path and decides if it is a real
    /// [`ConfigError::NotFound`]); any other read failure is [`ConfigError::Io`].
    fn read_file(path: &Path) -> Result<Option<String>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Load a config layer, deep-merging every copy of `<subdir>/<name>.toml`
    /// found along the search path (shipped → overlays, overlay wins), then
    /// deserializing the merged value into `T`. Missing in *every* root is
    /// [`ConfigError::NotFound`].
    fn load<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        subdir: &str,
        name: &str,
    ) -> Result<T, ConfigError> {
        validate_name(kind, name)?;
        let rel = format!("{subdir}/{name}.toml");
        self.load_merged(kind, name, &rel)
    }

    /// Shared merge-and-deserialize over a repo-relative path, used by both the
    /// subdir layers ([`load`](Self::load)) and root-level `base.toml`.
    fn load_merged<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        name: &str,
        rel: &str,
    ) -> Result<T, ConfigError> {
        let mut merged: Option<toml::Value> = None;
        let mut top_path: Option<PathBuf> = None;
        let mut last_path = PathBuf::new();
        for root in &self.roots {
            let path = root.join(rel);
            last_path = path.clone();
            let Some(text) = Self::read_file(&path)? else {
                continue;
            };
            let value: toml::Value = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
            top_path = Some(path);
            merged = Some(match merged {
                Some(mut base) => {
                    merge_toml(&mut base, value);
                    base
                }
                None => value,
            });
        }
        let Some(value) = merged else {
            return Err(ConfigError::NotFound {
                kind,
                name: name.to_string(),
                path: last_path.display().to_string(),
            });
        };
        let path = top_path.unwrap_or(last_path);
        value.try_into().map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Load `devices/<name>.toml`.
    pub fn device(&self, name: &str) -> Result<DeviceLayer, ConfigError> {
        self.load("device", "devices", name)
    }
    /// Load the SoC layer for `soc` (`socs/<soc>.toml`).
    pub fn soc(&self, soc: Soc) -> Result<SocLayer, ConfigError> {
        self.load("soc", "socs", soc.as_str())
    }
    /// Load the arch layer for `arch` (`arches/<arch>.toml`).
    pub fn arch(&self, arch: Arch) -> Result<ArchLayer, ConfigError> {
        self.load("arch", "arches", arch.as_str())
    }
    /// Load the boot-method layer for `bm` (`boot-methods/<bm>.toml`).
    pub fn boot_method(&self, bm: BootMethod) -> Result<BootMethodLayer, ConfigError> {
        self.load("boot-method", "boot-methods", bm.as_str())
    }
    /// Load `kernels/<id>.toml`.
    pub fn kernel(&self, id: &str) -> Result<KernelDef, ConfigError> {
        self.load("kernel", "kernels", id)
    }
    /// Load `recipes/<name>.toml`.
    pub fn recipe(&self, name: &str) -> Result<Recipe, ConfigError> {
        self.load("recipe", "recipes", name)
    }

    /// Load `features/<name>.toml` — a composable rootfs feature.
    pub fn feature(&self, name: &str) -> Result<crate::feature::Feature, ConfigError> {
        self.load("feature", "features", name)
    }

    /// Load `base.toml` — the distro-generic rootfs substrate. Unlike the
    /// other layers it is a single file at each root, not a named file in a subdir;
    /// it deep-merges across the search path like the rest.
    pub fn base(&self) -> Result<BaseLayer, ConfigError> {
        self.load_merged("base", "base", "base.toml")
    }

    /// Load `recipes/<name>.lock` — the resolved exact pins for a recipe.
    /// `boot2deb build` reads only this; `boot2deb update` writes it. A lock is an
    /// *atomic* artifact (exact pins), not a mergeable layer, so it is read from
    /// the highest-precedence root that has it (an overlay's lock shadows a shipped
    /// one), never merged.
    pub fn lock(&self, name: &str) -> Result<crate::lock::Lock, ConfigError> {
        validate_name("lock", name)?;
        let rel = format!("recipes/{name}.lock");
        let mut last_path = PathBuf::new();
        for root in self.roots.iter().rev() {
            let path = root.join(&rel);
            last_path = path.clone();
            if let Some(text) = Self::read_file(&path)? {
                return toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
        Err(ConfigError::NotFound {
            kind: "lock",
            name: name.to_string(),
            path: last_path.display().to_string(),
        })
    }

    /// Filesystem path of `recipes/<name>.lock`, whether or not it exists — the
    /// target `boot2deb update` writes to. The lock lands in the root that *owns*
    /// the recipe (an overlay recipe's lock goes into that overlay), or the primary
    /// root if the recipe is not on the path. The name is validated first, since
    /// this is a *write* target: an unchecked `../` or absolute name would let
    /// `update` clobber a file outside `recipes/`.
    pub fn lock_path(&self, name: &str) -> Result<PathBuf, ConfigError> {
        validate_name("lock", name)?;
        Ok(self
            .owning_root("recipes", name)
            .join("recipes")
            .join(format!("{name}.lock")))
    }

    /// Filesystem path of a file that lives beside `recipe`, `recipes/<filename>` —
    /// e.g. that recipe's committed solved package manifest. Anchored to the
    /// root that *owns* `recipe`, the same way [`lock_path`](Self::lock_path) is, so
    /// an overlay recipe's manifest lands in that overlay beside its lock rather than
    /// diverging into the primary root. Both `recipe` and `filename` are validated as
    /// bare names, since this is a *write* target: an unchecked `../` or absolute name
    /// would let `build --save-manifest` write outside `recipes/`.
    pub fn recipe_sibling(&self, recipe: &str, filename: &str) -> Result<PathBuf, ConfigError> {
        validate_name("recipe", recipe)?;
        validate_name("manifest", filename)?;
        Ok(self.owning_root("recipes", recipe).join("recipes").join(filename))
    }

    /// Stems of every `*.toml` in `subdir`, unioned across the search path, sorted
    /// and de-duplicated — so an overlay's targets list alongside the shipped ones,
    /// and a target present in both (an overlay retuning a shipped device) appears
    /// once. An absent directory in a root contributes nothing; any *other*
    /// `read_dir` failure (a wrong/unreadable root, a permission error) is surfaced
    /// as [`ConfigError::Io`] rather than silently yielding a success exit (COR-19).
    pub fn list(&self, subdir: &str) -> Result<Vec<String>, ConfigError> {
        let mut names = std::collections::BTreeSet::new();
        for root in &self.roots {
            let dir = root.join(subdir);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ConfigError::Io {
                        path: dir.display().to_string(),
                        source,
                    })
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
        Ok(names.into_iter().collect())
    }
}

/// Deep-merge `overlay` into `base` with the overlay winning. Two tables
/// merge key-by-key (recursing into nested tables); anything else — a scalar, an
/// array, or a type mismatch between the two sides — replaces `base` wholesale.
/// This is the simplest predictable last-wins: a table grows/overrides field by
/// field, while an array or scalar key is set, not concatenated.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match overlay {
        toml::Value::Table(over) => {
            if let toml::Value::Table(under) = base {
                for (key, over_val) in over {
                    match under.get_mut(&key) {
                        Some(under_val) => merge_toml(under_val, over_val),
                        None => {
                            under.insert(key, over_val);
                        }
                    }
                }
            } else {
                // `base` is a scalar/array but the overlay is a table → replace.
                *base = toml::Value::Table(over);
            }
        }
        other => *base = other,
    }
}

/// Reject any name that is not a bare identifier before it joins into a filesystem
/// path. Allows `[A-Za-z0-9._-]`; rejects the empty string, a leading dot
/// (hidden files, `.`, `..`), path separators, and absolute paths — so a config
/// cross-reference or CLI argument can never traverse out of the config root.
fn validate_name(kind: &'static str, name: &str) -> Result<(), ConfigError> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ConfigError::InvalidName {
            kind,
            name: name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_pass() {
        for n in ["turing-rk1", "turing-rk1-forky", "rk3588-mainline-7.1", "a_b.c"] {
            assert!(validate_name("device", n).is_ok(), "{n} should be valid");
        }
    }

    #[test]
    fn traversal_and_absolute_names_are_rejected() {
        for n in [
            "",            // empty
            "..",          // parent
            ".hidden",     // leading dot
            "a/b",         // separator
            "../etc/x",    // traversal
            "/etc/passwd", // absolute
            "a\\b",        // backslash
            "a b",         // space
            "a:b",         // colon
        ] {
            assert!(
                matches!(
                    validate_name("recipe", n),
                    Err(ConfigError::InvalidName { .. })
                ),
                "{n:?} should be rejected"
            );
        }
    }

    #[test]
    fn lock_path_rejects_traversal() {
        let root = ConfigRoot::new("/cfg");
        assert!(root.lock_path("turing-rk1-forky").is_ok());
        assert!(matches!(
            root.lock_path("../../etc/cron.d/x"),
            Err(ConfigError::InvalidName { .. })
        ));
    }

    // ---- TOML deep-merge algebra --------------------------------------

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn merge_deep_merges_tables_key_by_key() {
        let mut base = val("a = 1\n[t]\nx = 1\ny = 1\n");
        merge_toml(&mut base, val("b = 2\n[t]\ny = 9\nz = 3\n"));
        // Top-level: base `a` kept, overlay `b` added.
        assert_eq!(base["a"].as_integer(), Some(1));
        assert_eq!(base["b"].as_integer(), Some(2));
        // Nested table merged key-by-key: x kept, y overridden, z added.
        assert_eq!(base["t"]["x"].as_integer(), Some(1));
        assert_eq!(base["t"]["y"].as_integer(), Some(9));
        assert_eq!(base["t"]["z"].as_integer(), Some(3));
    }

    #[test]
    fn merge_replaces_scalars_and_arrays_wholesale() {
        // A scalar key is overwritten; an array key is replaced, not concatenated.
        let mut base = val("n = 1\narr = [1, 2, 3]\n");
        merge_toml(&mut base, val("n = 5\narr = [9]\n"));
        assert_eq!(base["n"].as_integer(), Some(5));
        assert_eq!(base["arr"].as_array().unwrap().len(), 1);
        assert_eq!(base["arr"][0].as_integer(), Some(9));
    }

    #[test]
    fn merge_overlay_table_replaces_base_scalar() {
        // Type mismatch (base scalar, overlay table) → overlay wins wholesale.
        let mut base = val("k = 1\n");
        merge_toml(&mut base, val("[k]\ninner = 2\n"));
        assert_eq!(base["k"]["inner"].as_integer(), Some(2));
    }

    // ---- Search-path behaviour (overlays) ------------------------------------

    /// A primary root + one overlay, each optionally carrying a `base.toml` and a
    /// `recipes/<name>.toml`. Returns both tempdirs (kept alive) and the root.
    fn overlaid(
        primary_base: Option<&str>,
        overlay_base: Option<&str>,
    ) -> (tempfile::TempDir, tempfile::TempDir, ConfigRoot) {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        if let Some(b) = primary_base {
            std::fs::write(p.path().join("base.toml"), b).unwrap();
        }
        if let Some(b) = overlay_base {
            std::fs::write(o.path().join("base.toml"), b).unwrap();
        }
        let root = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        (p, o, root)
    }

    #[test]
    fn overlay_base_merges_over_shipped() {
        // Primary sets packages + exclude; overlay replaces packages (array
        // wholesale) and leaves exclude untouched (present only in primary).
        let (_p, _o, root) = overlaid(
            Some("packages = [\"a\", \"b\"]\nexclude = [\"x\"]\n"),
            Some("packages = [\"c\"]\n"),
        );
        let base = root.base().unwrap();
        assert_eq!(base.packages, vec!["c"]); // overlay array replaced wholesale
        assert_eq!(base.exclude, vec!["x"]); // untouched key survives the merge
    }

    #[test]
    fn overlay_only_file_resolves_and_lists() {
        // A recipe present only in the overlay resolves and lists alongside the
        // primary's; a name in both appears once.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("recipes")).unwrap();
        std::fs::create_dir_all(o.path().join("recipes")).unwrap();
        std::fs::write(p.path().join("recipes/shipped.toml"), "device = \"d\"\n").unwrap();
        std::fs::write(o.path().join("recipes/extra.toml"), "device = \"d\"\n").unwrap();
        // `shipped` in both roots: overlay adds a suite, must merge, not duplicate.
        std::fs::write(o.path().join("recipes/shipped.toml"), "suite = \"sid\"\n").unwrap();
        let root = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        assert_eq!(root.list("recipes").unwrap(), vec!["extra", "shipped"]);
        let extra = root.recipe("extra").unwrap();
        assert_eq!(extra.device, "d");
        // Merged: device from primary, suite from overlay.
        let shipped = root.recipe("shipped").unwrap();
        assert_eq!(shipped.device, "d");
        assert_eq!(shipped.suite.as_deref(), Some("sid"));
    }

    #[test]
    fn lock_path_targets_the_owning_root() {
        // A recipe living only in the overlay: its lock write-target lands in the
        // overlay, not the primary root.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(o.path().join("recipes")).unwrap();
        std::fs::write(o.path().join("recipes/ov.toml"), "device = \"d\"\n").unwrap();
        let root = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        let lp = root.lock_path("ov").unwrap();
        assert!(lp.starts_with(o.path()), "lock should write into the overlay: {lp:?}");
        // A recipe not on the path defaults to the primary root.
        let lp2 = root.lock_path("nowhere").unwrap();
        assert!(lp2.starts_with(p.path()));

        // The manifest sibling anchors to the same owning root as the lock, so an
        // overlay recipe's manifest lands beside its lock rather than in the primary
        // root (Finding 5). A recipe not on the path defaults to the primary root.
        let ms = root.recipe_sibling("ov", "ov.pkgs.lock").unwrap();
        assert!(ms.starts_with(o.path()), "manifest should write into the overlay: {ms:?}");
        assert!(root.recipe_sibling("nowhere", "x.pkgs.lock").unwrap().starts_with(p.path()));
        // A traversal recipe or filename is rejected as a write target.
        assert!(root.recipe_sibling("../x", "m").is_err());
        assert!(root.recipe_sibling("ov", "../m").is_err());
    }

    #[test]
    fn with_overlays_rejects_an_empty_or_missing_overlay() {
        let primary = tempfile::tempdir().unwrap();
        let good = tempfile::tempdir().unwrap();
        let root = |o: PathBuf| ConfigRoot::with_overlays(primary.path().to_path_buf(), [o]);

        // An existing directory composes the search path.
        assert_eq!(root(good.path().to_path_buf()).unwrap().search_paths().len(), 2);

        // An empty `--overlay ''` would resolve every asset against the process's
        // current directory — refused, not silently accepted.
        let err = root(PathBuf::new()).err().expect("empty overlay is refused");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "the path is empty"),
            "{err}"
        );
        // A typo'd overlay would shadow nothing, so the build would quietly use the
        // shipped config instead of the operator's.
        let err = root(primary.path().join("nope")).err().expect("missing overlay is refused");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "no such directory"),
            "{err}"
        );
        // A file is not a search path.
        let file = primary.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        let err = root(file).err().expect("a file is not an overlay");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "not a directory"),
            "{err}"
        );
    }

    #[test]
    fn find_asset_prefers_overlay_and_stacks_all() {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("blob.bin"), "primary").unwrap();
        std::fs::write(o.path().join("blob.bin"), "overlay").unwrap();
        let root = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        // Highest precedence wins.
        assert!(root.find_asset("blob.bin").unwrap().starts_with(o.path()));
        // All copies, low→high (primary first).
        let all = root.find_asset_all("blob.bin");
        assert_eq!(all.len(), 2);
        assert!(all[0].starts_with(p.path()) && all[1].starts_with(o.path()));
        assert!(root.find_asset("absent").is_none());
    }

    #[test]
    fn find_trust_anchor_refuses_overlay_shadow_by_default() {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("keyring.gpg"), "shipped").unwrap();
        // No overlay copy: the shipped anchor resolves.
        let root_no_shadow = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        let anchor = root_no_shadow.find_trust_anchor("keyring.gpg", false).unwrap().unwrap();
        assert!(anchor.starts_with(p.path()), "must resolve from the shipped root");

        // An overlay copy is a swap attempt: fail closed.
        std::fs::write(o.path().join("keyring.gpg"), "overlay").unwrap();
        let root = ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        assert!(matches!(
            root.find_trust_anchor("keyring.gpg", false),
            Err(ConfigError::OverlayTrustAnchor { .. })
        ));
        // The explicit opt-in lets the overlay's copy win (like find_asset).
        assert!(root
            .find_trust_anchor("keyring.gpg", true)
            .unwrap()
            .unwrap()
            .starts_with(o.path()));
        // Absent everywhere → None (caller falls back to the host trust store).
        assert!(root.find_trust_anchor("absent.gpg", false).unwrap().is_none());
    }
}
