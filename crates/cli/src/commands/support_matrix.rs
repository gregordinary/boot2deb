//! `support-matrix`: what each shipped recipe has been taken through.
//!
//! Reads the committed recipes and locks and formats them — the terminal table by
//! default, the docs page under `--markdown`. Resolves nothing upstream, so the
//! output is a function of the working tree.

use boot2deb_core::support::{self, Matrix};
use boot2deb_core::ConfigRoot;

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

/// Run `support-matrix`.
pub(crate) fn run(root: &ConfigRoot, markdown: bool) -> Result {
    let matrix = support::matrix(root)?;
    if markdown {
        print!("{}", support::render_markdown(&matrix));
    } else {
        print_table(&matrix);
    }
    // A recipe with no claim is omitted from the matrix, which is right for one
    // authored locally and wrong for one about to ship. Naming them on stderr keeps
    // the artifact clean while making the omission visible to whoever can judge it.
    if !matrix.unclaimed.is_empty() {
        eprintln!(
            "note: {} recipe(s) declare no [support] claim and are not in the matrix: {}",
            matrix.unclaimed.len(),
            matrix.unclaimed.join(", ")
        );
    }
    Ok(())
}

/// Render the matrix as an aligned terminal table, sized to its widest cell so a
/// long recipe or kernel name does not wrap the columns out of alignment.
fn print_table(matrix: &Matrix) {
    let cells: Vec<[String; 8]> = matrix
        .rows
        .iter()
        .map(|r| {
            [
                r.recipe.clone(),
                r.device.clone(),
                r.suite_cell().to_string(),
                r.kernel_cell(),
                r.patches_cell(),
                r.uboot_cell(),
                r.status.to_string(),
                r.date.clone(),
            ]
        })
        .collect();
    let headers = [
        "RECIPE", "DEVICE", "SUITE", "KERNEL", "PATCHES", "U-BOOT", "STATUS", "AS OF",
    ];
    let mut widths = headers.map(str::len);
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |row: &[String; 8]| {
        let mut s = String::new();
        for (i, (c, w)) in row.iter().zip(widths).enumerate() {
            // The last column is not padded: trailing whitespace on every line is
            // noise a reader's terminal and their copy-paste both carry.
            if i + 1 == row.len() {
                s.push_str(c);
            } else {
                s.push_str(&format!("{c:<w$}  "));
            }
        }
        s
    };
    println!("{}", line(&headers.map(String::from)));
    for row in &cells {
        println!("{}", line(row));
    }
}

#[cfg(test)]
mod tests {
    use crate::testsupport::{repo_root, repo_root_path};
    use boot2deb_core::support;

    /// The page under `docs/` is generated output, and the only thing keeping a
    /// generated file honest is a gate that regenerates it. Without this, a re-pinned
    /// lock or an edited claim silently leaves the published matrix describing a
    /// build that no longer exists.
    #[test]
    fn the_committed_docs_page_matches_the_generated_one() {
        let matrix = support::matrix(&repo_root()).expect("the shipped config builds a matrix");
        let path = repo_root_path().join("docs/src/reference/support-matrix.md");
        let committed =
            std::fs::read_to_string(&path).expect("the support-matrix page is committed");
        assert_eq!(
            committed,
            support::render_markdown(&matrix),
            "{} is stale — regenerate it with `boot2deb support-matrix --markdown`",
            path.display()
        );
    }

    /// Every recipe boot2deb ships makes a support claim. Shipping one without a
    /// claim would drop it from the matrix silently, which is the one way a reader
    /// can be misled by a table that is otherwise generated.
    #[test]
    fn every_shipped_recipe_declares_a_support_claim() {
        let matrix = support::matrix(&repo_root()).expect("the shipped config builds a matrix");
        assert!(
            matrix.unclaimed.is_empty(),
            "shipped recipes with no [support] claim: {}",
            matrix.unclaimed.join(", ")
        );
    }
}
