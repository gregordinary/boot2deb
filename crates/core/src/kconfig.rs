//! Kernel `.config` / fragment model — parse a Kconfig file into a symbol
//! map and diff two configs over the normalized `CONFIG_*` set.
//!
//! Pure and deterministic: parsing plus set comparison, no I/O. *Generating* a
//! `.config` from a base defconfig + fragments is an engine side effect — it
//! shells out to the kernel's `merge_config.sh` + `make olddefconfig` to reuse
//! the tree's own Kconfig dependency resolution rather than reimplementing it.
//! This module is the value layer the config-parity check
//! compares with.
//!
//! Normalization follows Kconfig semantics: a symbol absent from a `.config` is
//! disabled, so *absent* and `# CONFIG_X is not set` are the same value
//! ([`Value::NotSet`]). The auto-generated banner (`# Linux/arm64 <ver> …`) and
//! section-header comments encode the kernel version+arch, not configuration, so
//! they are ignored.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The value of a Kconfig symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Disabled: written `# CONFIG_X is not set`, or absent entirely — the same
    /// thing in Kconfig, so the two forms compare equal.
    NotSet,
    /// Set to a value — `y`, `m`, a number, or a quoted string — stored verbatim
    /// as the text after the `=` (quotes included), since parity is a
    /// byte-for-byte comparison of that text.
    Set(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::NotSet => f.write_str("(not set)"),
            Value::Set(v) => f.write_str(v),
        }
    }
}

/// A parsed kernel `.config` or config fragment: symbol name → [`Value`].
///
/// Only `CONFIG_*` assignments and `# CONFIG_X is not set` lines are retained;
/// blank lines, the version banner, and section-header comments are dropped.
/// [`get`](KernelConfig::get) reads an absent symbol as [`Value::NotSet`],
/// matching Kconfig's "absent means disabled" rule, so a fragment and a full
/// `.config` are directly comparable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelConfig {
    entries: BTreeMap<String, Value>,
}

impl KernelConfig {
    /// Parse the text of a `.config` or a config fragment.
    ///
    /// Lines are matched in Kconfig's own two forms: `CONFIG_X=<value>` and
    /// `# CONFIG_X is not set`. Everything else — the banner, section comments,
    /// blank lines — is ignored. A later assignment of the same symbol wins,
    /// mirroring `.config` last-wins semantics.
    pub fn parse(text: &str) -> Self {
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            // "# CONFIG_X is not set" — a disabled symbol, not a plain comment.
            if let Some(rest) = line.strip_prefix("# CONFIG_") {
                if let Some(sym) = rest.strip_suffix(" is not set") {
                    // A hand-edited fragment may leave stray whitespace; a symbol
                    // name has none, so trim it and reject an empty / spaced name
                    // rather than record a phantom symbol (COR-14).
                    let sym = sym.trim();
                    if !sym.is_empty() && !sym.contains(char::is_whitespace) {
                        entries.insert(format!("CONFIG_{sym}"), Value::NotSet);
                    }
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // "CONFIG_X=<value>" — split on the first '=' so quoted string
            // values containing '=' are preserved verbatim. The symbol side is
            // trimmed (so `CONFIG_FOO =m` is `CONFIG_FOO`, not a spaced phantom) and
            // rejected if it still carries whitespace (COR-14).
            if let Some((sym, val)) = line.split_once('=') {
                let sym = sym.trim();
                if let Some(stripped) = sym.strip_prefix("CONFIG_") {
                    if !stripped.is_empty() && !stripped.contains(char::is_whitespace) {
                        entries.insert(sym.to_string(), Value::Set(val.trim().to_string()));
                    }
                }
            }
        }
        Self { entries }
    }

    /// The value of `symbol`, treating an absent symbol as [`Value::NotSet`].
    pub fn get(&self, symbol: &str) -> Value {
        self.entries.get(symbol).cloned().unwrap_or(Value::NotSet)
    }

    /// The explicitly-recorded symbols and their values, sorted by name. Used to
    /// check that every fragment-requested value survived dependency resolution
    /// (the clean-merge gate).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(sym, val)| (sym.as_str(), val))
    }

    /// The number of explicitly-recorded symbols (assignments plus explicit
    /// "is not set" lines). Absent-but-disabled symbols are not counted.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the config recorded no symbols.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One symbol whose value differs between two configs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// The `CONFIG_*` symbol name.
    pub symbol: String,
    /// Value in the left config.
    pub left: Value,
    /// Value in the right config.
    pub right: Value,
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} vs {}", self.symbol, self.left, self.right)
    }
}

/// Symbols whose value differs between `left` and `right`, over the union of
/// both symbol sets, sorted by name.
///
/// Absent counts as [`Value::NotSet`], so a symbol disabled in one config and
/// absent from the other is *not* reported — that is the correct Kconfig
/// reading, and it is what lets a small fragment be compared against a full
/// `.config`. An empty result is exact parity over the normalized `CONFIG_*`
/// set: what the fragment set must reproduce against the reference config.
pub fn diff(left: &KernelConfig, right: &KernelConfig) -> Vec<Diff> {
    let symbols: BTreeSet<&String> = left.entries.keys().chain(right.entries.keys()).collect();
    symbols
        .into_iter()
        .filter_map(|sym| {
            let (l, r) = (left.get(sym), right.get(sym));
            (l != r).then(|| Diff {
                symbol: sym.clone(),
                left: l,
                right: r,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_kconfig_line_forms() {
        let cfg = KernelConfig::parse(
            "#\n\
             # Automatically generated file; DO NOT EDIT.\n\
             # Linux/arm64 7.1.1 Kernel Configuration\n\
             #\n\
             CONFIG_ARM64=y\n\
             CONFIG_ROCKCHIP_MULTI_RGA=m\n\
             # CONFIG_VIDEO_ROCKCHIP_RGA is not set\n\
             CONFIG_LOG_BUF_SHIFT=18\n\
             CONFIG_LOCALVERSION=\"-rk1\"\n\
             \n\
             # Networking options\n",
        );
        assert_eq!(cfg.get("CONFIG_ARM64"), Value::Set("y".into()));
        assert_eq!(cfg.get("CONFIG_ROCKCHIP_MULTI_RGA"), Value::Set("m".into()));
        assert_eq!(cfg.get("CONFIG_VIDEO_ROCKCHIP_RGA"), Value::NotSet);
        assert_eq!(cfg.get("CONFIG_LOG_BUF_SHIFT"), Value::Set("18".into()));
        assert_eq!(cfg.get("CONFIG_LOCALVERSION"), Value::Set("\"-rk1\"".into()));
        // Banner and section comments are not symbols.
        assert_eq!(cfg.len(), 5);
    }

    #[test]
    fn hand_edited_whitespace_does_not_make_phantom_symbols() {
        // Stray spaces around `=` in a hand-edited fragment resolve to the clean
        // symbol, not a spaced phantom, and neither empty-name form is recorded.
        let cfg = KernelConfig::parse(
            "CONFIG_FOO =m\n\
             CONFIG_BAR = y\n\
             # CONFIG_BAZ  is not set\n\
             # CONFIG_ is not set\n\
             CONFIG_ =y\n",
        );
        assert_eq!(cfg.get("CONFIG_FOO"), Value::Set("m".into()));
        assert_eq!(cfg.get("CONFIG_BAR"), Value::Set("y".into()));
        assert_eq!(cfg.get("CONFIG_BAZ"), Value::NotSet);
        // No trailing-space phantom, and the two empty-name lines are dropped.
        assert_eq!(cfg.len(), 3);
        assert_eq!(cfg.get("CONFIG_FOO "), Value::NotSet);
    }

    #[test]
    fn absent_symbol_reads_as_not_set() {
        let cfg = KernelConfig::parse("CONFIG_ARM64=y\n");
        assert_eq!(cfg.get("CONFIG_NOT_THERE"), Value::NotSet);
    }

    #[test]
    fn string_value_with_equals_is_preserved() {
        let cfg = KernelConfig::parse("CONFIG_CMDLINE=\"root=/dev/mmcblk0p1 rw\"\n");
        assert_eq!(
            cfg.get("CONFIG_CMDLINE"),
            Value::Set("\"root=/dev/mmcblk0p1 rw\"".into())
        );
    }

    #[test]
    fn disabled_equals_absent_in_diff() {
        // One config disables a symbol explicitly; the other omits it. Same value.
        let a = KernelConfig::parse("CONFIG_ARM64=y\n# CONFIG_FOO is not set\n");
        let b = KernelConfig::parse("CONFIG_ARM64=y\n");
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_reports_real_value_changes_sorted() {
        let a = KernelConfig::parse("CONFIG_A=y\nCONFIG_B=m\nCONFIG_C=y\n");
        let b = KernelConfig::parse("CONFIG_A=y\n# CONFIG_B is not set\nCONFIG_C=m\n");
        let d = diff(&a, &b);
        assert_eq!(d.len(), 2);
        // Sorted by symbol name.
        assert_eq!(d[0].symbol, "CONFIG_B");
        assert_eq!(d[0].left, Value::Set("m".into()));
        assert_eq!(d[0].right, Value::NotSet);
        assert_eq!(d[1].symbol, "CONFIG_C");
        assert_eq!(d[1].left, Value::Set("y".into()));
        assert_eq!(d[1].right, Value::Set("m".into()));
    }

    #[test]
    fn identical_configs_have_no_diff() {
        let text = "CONFIG_ARM64=y\nCONFIG_ROCKCHIP_MULTI_RGA=m\n# CONFIG_FOO is not set\n";
        assert!(diff(&KernelConfig::parse(text), &KernelConfig::parse(text)).is_empty());
    }

    #[test]
    fn y_and_m_are_distinct_values() {
        let a = KernelConfig::parse("CONFIG_X=y\n");
        let b = KernelConfig::parse("CONFIG_X=m\n");
        assert_eq!(diff(&a, &b).len(), 1);
    }
}
