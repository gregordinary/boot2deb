//! Terminal prompts for the `new-device` scaffold wizard.
//!
//! Every helper resolves one scaffold value under the same contract: an explicit
//! flag wins outright, a terminal gets a prompt (blank takes the default), and a
//! non-interactive run takes the default silently — so the wizard and the
//! flag-driven scripted path produce the same file.

use std::io::Write as _;

/// Read one trimmed line from stdin (empty on EOF).
fn read_line() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

/// Resolve a free-text scaffold value: an explicit flag wins; otherwise prompt with
/// `default` on a terminal (blank keeps the default), or take `default` silently
/// when non-interactive.
pub(crate) fn ask_value(
    label: &str,
    provided: Option<String>,
    default: &str,
    interactive: bool,
) -> String {
    if let Some(v) = provided {
        return v;
    }
    if !interactive {
        return default.to_string();
    }
    print!("{label} [{default}]: ");
    let _ = std::io::stdout().flush();
    let line = read_line();
    if line.is_empty() {
        default.to_string()
    } else {
        line
    }
}

/// Resolve a scaffold value chosen from a closed list of `options` — a model enum
/// (SoC, boot method, layout) or a discovered id list (the SoC-compatible kernels).
///
/// An explicit flag value is parsed by `parse` and must name one of the options, so
/// a valid-but-unavailable choice (an enum variant with no layer file here, a kernel
/// that does not support the SoC) is rejected with the valid set. A terminal gets a
/// numbered menu whose blank answer picks the first option; a non-interactive run
/// takes the first option as the default. `show` renders an option for the menu, the
/// error message, and the flag-value comparison.
pub(crate) fn ask_choice<T: Clone>(
    label: &str,
    provided: Option<&str>,
    options: &[T],
    interactive: bool,
    show: impl Fn(&T) -> String,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, Box<dyn std::error::Error>> {
    if let Some(s) = provided {
        let value = parse(s)?;
        if !options.iter().any(|o| show(o) == show(&value)) {
            let valid = options.iter().map(&show).collect::<Vec<_>>().join(", ");
            return Err(
                format!("{label} '{s}' is not available here (choose one of: {valid})").into(),
            );
        }
        return Ok(value);
    }
    let default = options[0].clone();
    if !interactive {
        return Ok(default);
    }
    println!("{label}:");
    for (i, o) in options.iter().enumerate() {
        println!("  {}) {}", i + 1, show(o));
    }
    loop {
        print!("choose [1]: ");
        let _ = std::io::stdout().flush();
        let line = read_line();
        if line.is_empty() {
            return Ok(default);
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return Ok(options[n - 1].clone()),
            _ => println!("enter a number 1..{}", options.len()),
        }
    }
}

/// Prompt for the recipe's features from the SoC/arch-compatible set: a
/// comma-separated list of menu numbers (blank selects none). De-duplicates while
/// preserving the entered order.
pub(crate) fn ask_features(compatible: &[String]) -> Vec<String> {
    if compatible.is_empty() {
        println!("features: (none compatible with this SoC/arch)");
        return Vec::new();
    }
    println!("features (comma-separated numbers, blank for none):");
    for (i, f) in compatible.iter().enumerate() {
        println!("  {}) {}", i + 1, f);
    }
    print!("choose: ");
    let _ = std::io::stdout().flush();
    let mut chosen = Vec::new();
    for tok in read_line().split(',') {
        if let Ok(n) = tok.trim().parse::<usize>() {
            if (1..=compatible.len()).contains(&n) && !chosen.contains(&compatible[n - 1]) {
                chosen.push(compatible[n - 1].clone());
            }
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::model::Soc;

    /// The non-interactive paths never touch stdin, so they are directly testable:
    /// a flag value must name an available option, and an omitted one defaults.
    #[test]
    fn ask_choice_validates_a_flag_against_the_available_options() {
        let socs = [Soc::Rk3588, Soc::Rk3576];
        let show = |s: &Soc| s.as_str().to_string();
        let parse = |s: &str| s.parse::<Soc>();

        // A flag naming an available option is taken verbatim.
        let got = ask_choice("SoC", Some("rk3576"), &socs, false, show, parse).unwrap();
        assert_eq!(got, Soc::Rk3576);

        // A parseable variant that is not in the offered set names the valid ones —
        // the closed enum is wider than what a given config root actually ships.
        let err = ask_choice("SoC", Some("rk3566"), &socs, false, show, parse)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not available here") && err.contains("rk3588"), "{err}");

        // An unparseable value fails on the model's own FromStr.
        assert!(ask_choice("SoC", Some("nope"), &socs, false, show, parse).is_err());

        // Omitted + non-interactive takes the first option.
        assert_eq!(
            ask_choice("SoC", None, &socs, false, show, parse).unwrap(),
            Soc::Rk3588
        );
    }

    #[test]
    fn ask_choice_over_discovered_ids_rejects_an_unknown_one() {
        let kernels = vec!["rk3588-mainline-7.1".to_string()];
        let show = |k: &String| k.clone();
        let parse = |s: &str| Ok(s.to_string());
        assert_eq!(
            ask_choice("kernel", Some("rk3588-mainline-7.1"), &kernels, false, show, parse).unwrap(),
            "rk3588-mainline-7.1"
        );
        let err = ask_choice("kernel", Some("no-such-kernel"), &kernels, false, show, parse)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not available here"), "{err}");
    }

    #[test]
    fn ask_value_takes_the_flag_then_the_default() {
        assert_eq!(ask_value("suite", Some("sid".into()), "forky", false), "sid");
        assert_eq!(ask_value("suite", None, "forky", false), "forky");
    }
}
