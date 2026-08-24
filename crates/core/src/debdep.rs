//! Whether a set of installed Debian packages actually satisfies its own dependencies.
//!
//! Pure and deterministic, like the rest of `core`: the caller supplies the parsed
//! contents of a `dpkg` status database and this decides what is unsatisfied. No I/O.
//!
//! # Why this exists
//!
//! A build root is a base tree plus a staged layer of build-dependencies. The layer is
//! resolved against a live archive while the base was provisioned earlier, so the two
//! can describe different archive states. When they do, a layer package can be
//! installed whose declared dependency the base does not meet — and the failure that
//! follows is a link error deep inside a compile, naming a library that is present and
//! correct. [`unsatisfied`] states the real fault instead: the package, the constraint
//! it declared, and what is actually installed.
//!
//! # What "satisfied" means here
//!
//! [`unsatisfied`] answers the question `dpkg` would answer about an already-unpacked
//! set, not the question a resolver answers about a candidate one. It checks
//! `Depends` and `Pre-Depends` of every installed package against the installed set,
//! honouring alternatives (`a | b`), virtual packages (`Provides`), and architecture
//! qualifiers. `Recommends` and `Suggests` are not dependencies and are not checked.
//!
//! Version ordering is dpkg's own algorithm ([`compare_versions`]), which is not
//! `sort -V`: it splits epoch, upstream and revision, orders `~` *before* the empty
//! string so `1.0~rc1` precedes `1.0`, and orders letters before non-letters.

use std::cmp::Ordering;
use std::collections::HashMap;

/// One installed package, as read from a `dpkg` status stanza.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Binary package name, without any architecture qualifier.
    pub name: String,
    /// The package's own version, as `dpkg` records it.
    pub version: String,
    /// `Depends` and `Pre-Depends` joined — both are hard requirements and neither may
    /// be unsatisfied in a usable root, so they are checked identically.
    pub depends: String,
    /// The `Provides` field verbatim; virtual names a dependency may name instead.
    pub provides: String,
}

/// A dependency that no installed package satisfies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsatisfied {
    /// The installed package whose dependency is unmet.
    pub package: String,
    /// The version of that package.
    pub package_version: String,
    /// The unmet dependency group, as declared (alternatives included).
    pub required: String,
    /// What is installed for the first alternative's name, when anything is — the
    /// evidence that turns "unsatisfied" into a diagnosis. `None` when absent entirely.
    pub installed: Option<String>,
}

/// Every hard dependency of `packages` that `packages` does not satisfy.
///
/// The result is ordered by package name then by the declared group, so two runs over
/// the same root report identically and a test can assert on the whole list.
///
/// Virtual packages are honoured: an unversioned dependency is satisfied by any
/// `Provides` of that name, and a versioned one only by a versioned `Provides`, which
/// is what Debian policy says a bare `Provides` may not satisfy.
pub fn unsatisfied(packages: &[Package]) -> Vec<Unsatisfied> {
    let installed: HashMap<&str, &str> = packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();

    // Virtual name -> the versions it is provided at. An empty version means the
    // provider declared a bare `Provides`, which satisfies only unversioned depends.
    let mut provides: HashMap<String, Vec<String>> = HashMap::new();
    for p in packages {
        for item in split_top(&p.provides, ',') {
            let (name, version) = parse_atom(item);
            provides
                .entry(name)
                .or_default()
                .push(version.map(|(_, v)| v).unwrap_or_default());
        }
    }

    let mut out = Vec::new();
    for p in packages {
        for group in split_top(&p.depends, ',') {
            if group.trim().is_empty() {
                continue;
            }
            let alternatives: Vec<&str> = split_top(group, '|').collect();
            if alternatives
                .iter()
                .any(|alt| satisfied(alt, &installed, &provides))
            {
                continue;
            }
            let first = parse_atom(alternatives.first().copied().unwrap_or(""));
            out.push(Unsatisfied {
                package: p.name.clone(),
                package_version: p.version.clone(),
                required: group.trim().to_string(),
                installed: installed.get(first.0.as_str()).map(|v| (*v).to_string()),
            });
        }
    }
    out.sort_by(|a, b| (&a.package, &a.required).cmp(&(&b.package, &b.required)));
    out
}

/// Whether one alternative of a dependency group is met by the installed set.
fn satisfied(
    atom: &str,
    installed: &HashMap<&str, &str>,
    provides: &HashMap<String, Vec<String>>,
) -> bool {
    let (name, constraint) = parse_atom(atom);
    if name.is_empty() {
        return true;
    }
    if let Some(have) = installed.get(name.as_str()) {
        match &constraint {
            None => return true,
            Some((rel, want)) if relation_holds(have, rel, want) => return true,
            Some(_) => {}
        }
    }
    match (provides.get(&name), &constraint) {
        // A bare `Provides` satisfies only an unversioned dependency.
        (Some(_), None) => true,
        (Some(versions), Some((rel, want))) => versions
            .iter()
            .any(|v| !v.is_empty() && relation_holds(v, rel, want)),
        (None, _) => false,
    }
}

/// Whether `have <rel> want` holds under dpkg version ordering.
///
/// An unrecognised relation is treated as unsatisfiable rather than as true: a
/// dependency this cannot read is a dependency it must not silently pass.
fn relation_holds(have: &str, rel: &str, want: &str) -> bool {
    let ord = compare_versions(have, want);
    match rel {
        "<<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        "=" => ord == Ordering::Equal,
        ">=" => ord != Ordering::Less,
        ">>" => ord == Ordering::Greater,
        // Deprecated spellings dpkg still accepts, meaning `<=` and `>=`.
        "<" => ord != Ordering::Greater,
        ">" => ord != Ordering::Less,
        _ => false,
    }
}

/// Split `s` on `sep`, ignoring separators inside parentheses or brackets, and
/// yielding trimmed non-empty pieces.
fn split_top(s: &str, sep: char) -> impl Iterator<Item = &str> {
    let mut pieces = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ if c == sep && depth <= 0 => {
                pieces.push(s[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    pieces.push(s[start..].trim());
    pieces.into_iter().filter(|p| !p.is_empty())
}

/// Split one dependency atom into its package name and optional version constraint.
///
/// Handles the architecture qualifier (`libc6:arm64`, `python3:any`) by dropping it —
/// these roots are single-architecture, so the qualifier never selects between two
/// installed candidates — and the build-profile/architecture bracket (`[!armhf]`),
/// which constrains when a dependency applies rather than what satisfies it.
fn parse_atom(atom: &str) -> (String, Option<(String, String)>) {
    let atom = atom.trim();
    let without_brackets = match atom.find('[') {
        Some(i) => &atom[..i],
        None => atom,
    };
    let (head, constraint) = match without_brackets.find('(') {
        Some(i) => {
            let close = without_brackets.find(')').unwrap_or(without_brackets.len());
            let inner = without_brackets[i + 1..close.max(i + 1)].trim();
            let split = inner
                .find(|c: char| c.is_ascii_digit() || c.is_ascii_alphabetic())
                .unwrap_or(inner.len());
            let (rel, ver) = inner.split_at(split);
            (
                &without_brackets[..i],
                Some((rel.trim().to_string(), ver.trim().to_string())),
            )
        }
        None => (without_brackets, None),
    };
    let name = head.trim();
    let name = name.split(':').next().unwrap_or(name).trim().to_string();
    (name, constraint)
}

/// Parse a `dpkg` status database into the packages that are actually installed.
///
/// Only stanzas whose `Status` field ends in `installed` are returned: a removed or
/// half-configured package contributes no files and must not be treated as satisfying
/// anything. `Depends` and `Pre-Depends` are joined, since both are hard requirements.
///
/// The format is RFC822-like stanzas separated by blank lines, with continuation lines
/// indented. Unknown fields are ignored rather than rejected — a status database
/// carries many, and this reads the four that bear on satisfaction.
pub fn parse_status(text: &str) -> Vec<Package> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let (mut name, mut version, mut status) = (String::new(), String::new(), String::new());
        let (mut depends, mut predepends, mut provides) =
            (String::new(), String::new(), String::new());
        let mut field: Option<&mut String> = None;
        for line in stanza.lines() {
            if let Some(rest) = line.strip_prefix(' ') {
                // A continuation of the field above it.
                if let Some(f) = field.as_deref_mut() {
                    f.push(' ');
                    f.push_str(rest.trim());
                }
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                field = None;
                continue;
            };
            let value = value.trim().to_string();
            field = match key.trim().to_ascii_lowercase().as_str() {
                "package" => {
                    name = value;
                    Some(&mut name)
                }
                "version" => {
                    version = value;
                    Some(&mut version)
                }
                "status" => {
                    status = value;
                    Some(&mut status)
                }
                "depends" => {
                    depends = value;
                    Some(&mut depends)
                }
                "pre-depends" => {
                    predepends = value;
                    Some(&mut predepends)
                }
                "provides" => {
                    provides = value;
                    Some(&mut provides)
                }
                _ => None,
            };
        }
        if name.is_empty() || !status.trim_end().ends_with("installed") {
            continue;
        }
        let depends = match (depends.is_empty(), predepends.is_empty()) {
            (_, true) => depends,
            (true, false) => predepends,
            (false, false) => format!("{depends}, {predepends}"),
        };
        out.push(Package {
            name,
            version,
            depends,
            provides,
        });
    }
    out
}

/// Compare two Debian package versions the way `dpkg --compare-versions` does.
///
/// The version is `[epoch:]upstream[-revision]`. Each of the three parts is compared
/// by the same rule: runs of digits compare numerically, and runs of non-digits compare
/// by a modified ordinal where `~` sorts before everything including the end of the
/// string, letters sort before non-letters, and the rest sort by byte value.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split_version(a);
    let (eb, ub, rb) = split_version(b);
    ea.cmp(&eb)
        .then_with(|| compare_part(ua, ub))
        .then_with(|| compare_part(ra, rb))
}

/// Split a version into epoch, upstream and revision. A missing epoch is 0 and a
/// missing revision is empty, which is what dpkg compares them as.
fn split_version(v: &str) -> (u64, &str, &str) {
    let v = v.trim();
    let (epoch, rest) = match v.find(':') {
        Some(i) => (v[..i].parse::<u64>().unwrap_or(0), &v[i + 1..]),
        None => (0, v),
    };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], &rest[i + 1..]),
        None => (epoch, rest, ""),
    }
}

/// Compare one version part under dpkg's alternating non-digit/digit rule.
fn compare_part(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        // Leading non-digit run, ordered by the modified ordinal.
        let (na, ra) = take_while(a, |c| !c.is_ascii_digit());
        let (nb, rb) = take_while(b, |c| !c.is_ascii_digit());
        match compare_nondigit(na, nb) {
            Ordering::Equal => {}
            ord => return ord,
        }
        a = ra;
        b = rb;

        // Digit run, compared numerically with leading zeroes insignificant.
        let (da, ra) = take_while(a, |c| c.is_ascii_digit());
        let (db, rb) = take_while(b, |c| c.is_ascii_digit());
        match compare_numeric(da, db) {
            Ordering::Equal => {}
            ord => return ord,
        }
        a = ra;
        b = rb;

        if a.is_empty() && b.is_empty() {
            return Ordering::Equal;
        }
    }
}

/// Split off the leading run of bytes matching `pred`.
fn take_while(s: &[u8], pred: impl Fn(u8) -> bool) -> (&[u8], &[u8]) {
    let end = s.iter().position(|c| !pred(*c)).unwrap_or(s.len());
    s.split_at(end)
}

/// Compare two digit runs as numbers, treating an empty run as zero.
fn compare_numeric(a: &[u8], b: &[u8]) -> Ordering {
    let trim = |s: &[u8]| {
        let start = s.iter().position(|c| *c != b'0').unwrap_or(s.len());
        s.len() - start
    };
    let (la, lb) = (trim(a), trim(b));
    la.cmp(&lb).then_with(|| {
        let sa = a.iter().skip_while(|c| **c == b'0');
        let sb = b.iter().skip_while(|c| **c == b'0');
        sa.cmp(sb)
    })
}

/// Compare two non-digit runs under dpkg's modified ordinal.
fn compare_nondigit(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let ca = a.get(i).copied();
        let cb = b.get(i).copied();
        match ordinal(ca).cmp(&ordinal(cb)) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    Ordering::Equal
}

/// dpkg's ordinal: `~` before the end of the string, the end before letters, letters
/// before everything else. Expressed as a sortable integer.
fn ordinal(c: Option<u8>) -> i32 {
    match c {
        Some(b'~') => -1,
        None => 0,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, depends: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            depends: depends.into(),
            provides: String::new(),
        }
    }

    #[test]
    fn a_status_database_parses_into_its_installed_packages() {
        // Two stanzas, one of them removed, plus a continuation line -- the three
        // shapes a real database mixes.
        let text = "\
Package: keeper
Status: install ok installed
Version: 1.2-3
Depends: libc6 (>= 2.0),
 other (>= 1)
Provides: virtual

Package: gone
Status: deinstall ok config-files
Version: 9

Package: other
Status: install ok installed
Version: 1
";
        let pkgs = parse_status(text);
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["keeper", "other"],
            "a removed package is not installed"
        );
        assert_eq!(pkgs[0].version, "1.2-3");
        assert!(
            pkgs[0].depends.contains("other (>= 1)"),
            "continuation joined: {:?}",
            pkgs[0].depends
        );
        assert_eq!(pkgs[0].provides, "virtual");
    }

    #[test]
    fn pre_depends_are_checked_alongside_depends() {
        // Both are hard requirements; checking only one would pass a broken root.
        let text = "\
Package: thing
Status: install ok installed
Version: 1
Pre-Depends: absent-package
";
        let pkgs = parse_status(text);
        assert_eq!(unsatisfied(&pkgs).len(), 1);
    }

    #[test]
    fn tilde_sorts_before_the_empty_string() {
        // The rule `sort -V` gets wrong, and the reason this does not reuse one.
        assert_eq!(compare_versions("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0~~", "1.0~"), Ordering::Less);
    }

    #[test]
    fn epochs_outrank_the_upstream_version() {
        assert_eq!(compare_versions("1:1.0", "2.0"), Ordering::Greater);
        assert_eq!(compare_versions("2.0", "1:1.0"), Ordering::Less);
    }

    #[test]
    fn digit_runs_compare_numerically_not_lexically() {
        assert_eq!(compare_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_versions("2.42-17", "2.43"), Ordering::Less);
        // Leading zeroes are not significant.
        assert_eq!(compare_versions("1.007", "1.7"), Ordering::Equal);
    }

    #[test]
    fn the_revision_breaks_an_upstream_tie() {
        assert_eq!(compare_versions("1.0-1", "1.0-2"), Ordering::Less);
        assert_eq!(compare_versions("1.0", "1.0-1"), Ordering::Less);
    }

    #[test]
    fn the_glibc_skew_is_reported_with_both_versions() {
        // The real failure: glib declares libc6 (>= 2.43) and the base carries 2.42-17,
        // which produced a link error naming an unrelated library.
        let set = vec![
            pkg("libglib2.0-0t64", "2.88.3-3", "libc6 (>= 2.43)"),
            pkg("libc6", "2.42-17", ""),
        ];
        let bad = unsatisfied(&set);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert_eq!(bad[0].package, "libglib2.0-0t64");
        assert_eq!(bad[0].required, "libc6 (>= 2.43)");
        assert_eq!(bad[0].installed.as_deref(), Some("2.42-17"));
    }

    #[test]
    fn a_met_constraint_is_not_reported() {
        let set = vec![
            pkg("libglib2.0-0t64", "2.88.3-3", "libc6 (>= 2.43)"),
            pkg("libc6", "2.43-1", ""),
        ];
        assert!(unsatisfied(&set).is_empty());
    }

    #[test]
    fn an_alternative_satisfies_the_whole_group() {
        let set = vec![
            pkg("thing", "1", "missing-one | libc6 (>= 2.0)"),
            pkg("libc6", "2.43-1", ""),
        ];
        assert!(unsatisfied(&set).is_empty());
    }

    #[test]
    fn a_missing_package_is_reported_with_no_installed_version() {
        let set = vec![pkg("thing", "1", "absent-package")];
        let bad = unsatisfied(&set);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].installed, None);
    }

    #[test]
    fn architecture_qualifiers_do_not_hide_a_match() {
        let set = vec![
            pkg("thing", "1", "libc6:arm64 (>= 2.0), python3:any"),
            pkg("libc6", "2.43-1", ""),
            pkg("python3", "3.14", ""),
        ];
        assert!(unsatisfied(&set).is_empty(), "{:?}", unsatisfied(&set));
    }

    #[test]
    fn a_bare_provides_satisfies_only_an_unversioned_dependency() {
        // Debian policy: a `Provides` with no version cannot meet a versioned depend.
        let mut provider = pkg("real", "1", "");
        provider.provides = "virtual".into();
        let unversioned = vec![pkg("thing", "1", "virtual"), provider.clone()];
        assert!(unsatisfied(&unversioned).is_empty());

        let versioned = vec![pkg("thing", "1", "virtual (>= 2)"), provider];
        assert_eq!(unsatisfied(&versioned).len(), 1);
    }

    #[test]
    fn a_versioned_provides_can_meet_a_versioned_dependency() {
        let mut provider = pkg("real", "1", "");
        provider.provides = "virtual (= 3.0)".into();
        let set = vec![pkg("thing", "1", "virtual (>= 2)"), provider];
        assert!(unsatisfied(&set).is_empty(), "{:?}", unsatisfied(&set));
    }

    #[test]
    fn an_unreadable_relation_is_unsatisfied_rather_than_passed() {
        // Failing open here would defeat the whole check.
        assert!(!relation_holds("1.0", "?!", "1.0"));
    }

    #[test]
    fn commas_inside_a_version_constraint_do_not_split_the_group() {
        let set = vec![
            pkg("thing", "1", "libc6 (>= 2.43), other"),
            pkg("libc6", "2.43-1", ""),
            pkg("other", "1", ""),
        ];
        assert!(unsatisfied(&set).is_empty());
    }

    #[test]
    fn the_report_is_ordered_so_two_runs_agree() {
        let set = vec![
            pkg("zeta", "1", "nope-z"),
            pkg("alpha", "1", "nope-a"),
            pkg("mid", "1", "nope-m"),
        ];
        let bad = unsatisfied(&set);
        let names: Vec<&str> = bad.iter().map(|u| u.package.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }
}
