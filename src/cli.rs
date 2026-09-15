//! Command-line helpers shared by the bundled `crond` and `crontab`.

use std::io::Write;
use std::process::ExitCode;

use clap::error::ErrorKind;

/// Report a clap error with cronie's exit statuses: the version goes to
/// stdout with status 0; help and usage errors go to stderr with status 1.
/// Write errors (a closed pipe, a full disk) are ignored rather than
/// panicking.
fn report(e: clap::Error) -> ExitCode {
    if e.kind() == ErrorKind::DisplayVersion {
        let _ = e.print();
        ExitCode::SUCCESS
    } else {
        let _ = write!(std::io::stderr(), "{e}");
        ExitCode::FAILURE
    }
}

/// Parse arguments with cronie's exit statuses (see [`report`]).
pub fn parse_args<P: clap::Parser>() -> Result<P, ExitCode> {
    P::try_parse().map_err(report)
}

/// Like [`parse_args`], also returning the raw matches so callers can tell
/// the order options were given in (cronie applies some checks in `getopt`
/// order).
pub fn parse_args_with_matches<P: clap::Parser>() -> Result<(P, clap::ArgMatches), ExitCode> {
    P::command()
        .try_get_matches()
        .and_then(|m| P::from_arg_matches(&m).map(|p| (p, m)))
        .map_err(report)
}

/// cronie refuses to read a new crontab from a terminal when no file (or
/// `-`) was given, so a stray Ctrl-D cannot install an empty crontab.
pub fn stdin_file_required(file: Option<&str>, stdin_is_terminal: bool) -> bool {
    file.is_none() && stdin_is_terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_stdin_needs_a_file_argument() {
        assert!(stdin_file_required(None, true));
        assert!(!stdin_file_required(None, false));
        assert!(!stdin_file_required(Some("-"), true));
        assert!(!stdin_file_required(Some("tab"), true));
    }
}
