//! `completions` and `man`: the two shell-integration artifacts, generated from the
//! same [`clap::Command`] the binary parses with.
//!
//! Neither is a second description of the tool. Both are rendered from the command
//! tree, so a flag added in [`crate::args`] is completable and documented the moment
//! it exists — the same property [`cli_reference`](super::cli_reference) gives the
//! docs page.
//!
//! They print to stdout rather than installing anything: where completions and man
//! pages belong is the packager's decision (a `.deb`'s `debian/install`, a distro's
//! `/usr/share`, a user's `~/.local`), and a tool that wrote into those directories
//! itself would be guessing at it.

use clap::CommandFactory;
use clap_complete::Shell;

/// Run `completions <shell>`: write the completion script to stdout.
pub(crate) fn completions(shell: Shell) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = crate::args::Cli::command();
    // The binary is `boot2deb`; the crate is `boot2deb-cli`. Completion scripts key on
    // the *invoked* name, so the binary's is the one to emit.
    clap_complete::generate(shell, &mut cmd, "boot2deb", &mut std::io::stdout());
    Ok(())
}

/// Run `man`: write the roff man page for `boot2deb(1)` to stdout.
///
/// The top-level page only. Each subcommand's own page would be a separate file, and a
/// tool writing several files to stdout has to invent a delimiter for them; `boot2deb
/// <command> --help` already answers the per-command question.
pub(crate) fn man() -> Result<(), Box<dyn std::error::Error>> {
    clap_mangen::Man::new(crate::args::Cli::command()).render(&mut std::io::stdout())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both generators must survive the real command tree. clap_mangen in particular
    /// walks every argument's help text, so this is the gate on a `#[arg]` shape it
    /// cannot render — which would otherwise surface as a panic in a user's shell.
    #[test]
    fn both_artifacts_render_from_the_real_command_tree() {
        let mut out = Vec::new();
        clap_mangen::Man::new(crate::args::Cli::command())
            .render(&mut out)
            .expect("the man page renders");
        let page = String::from_utf8(out).expect("roff is utf-8");
        assert!(page.contains("boot2deb"), "the page names the binary");
        // roff escapes a leading hyphen, so the flag appears as `\-\-overlay`.
        assert!(page.contains(r"\-\-overlay"), "global flags reach the page");
        assert!(
            page.contains("boot2deb\\-why\\-rebuild(1)"),
            "subcommands are cross-referenced"
        );

        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut cmd = crate::args::Cli::command();
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut cmd, "boot2deb", &mut out);
            let script = String::from_utf8(out).expect("a completion script is utf-8");
            assert!(!script.is_empty(), "{shell} produced nothing");
            // The recipe references are the awkward part to type, so the subcommands
            // that take one are the ones worth asserting reach the script.
            assert!(
                script.contains("why-rebuild"),
                "{shell} is missing a subcommand"
            );
        }
    }
}
